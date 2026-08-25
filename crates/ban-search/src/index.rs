//! Compact BAN index: addresses + street groups + prefix buckets for autocomplete.

use crate::normalize::{
    edit_distance_leq, expand_street_abbrev, normalize_query, split_number_prefix,
};
use crate::types::{AddressHit, BanError, BanStats};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use tracing::info;

/// Repetition codes (`""`, `bis`, `ter`, `quater`, single letters).
/// Stored as one byte per address to keep the record compact.
const REP_TABLE: [&str; 30] = [
    "", "bis", "ter", "quater", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
    "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
];

fn rep_code(s: &str) -> Option<u8> {
    let l = s.to_ascii_lowercase();
    REP_TABLE.iter().position(|r| *r == l).map(|i| i as u8)
}

/// One house-level record — compact (20 bytes): purely numeric house numbers
/// cover virtually all BAN rows; odd numeros go to [`IndexData::odd_addresses`]
/// so the per-street number lookup stays a plain u32 binary search.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddressRec {
    num: u32,
    rep: u8,
    street_idx: u32,
    lat: f32,
    lon: f32,
}

/// Address whose `numero` is not purely numeric (`"83-85"`, empty, …) — rare.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OddAddressRec {
    street_idx: u32,
    raw: String,
    rep: u8,
    lat: f32,
    lon: f32,
}

/// One unique street in a commune/postcode.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StreetRec {
    name: String,
    name_norm: String,
    postcode: String,
    city: String,
    city_norm: String,
    /// Centroid for street-level hits.
    lat: f32,
    lon: f32,
    /// Indices into `addresses` (sorted by `num`).
    address_ids: Vec<u32>,
    /// Indices into `odd_addresses` for this street.
    odd_ids: Vec<u32>,
}

/// Serializable index payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexData {
    streets: Vec<StreetRec>,
    addresses: Vec<AddressRec>,
    odd_addresses: Vec<OddAddressRec>,
    /// First 3 chars of street norm → street indices.
    prefix3: HashMap<String, Vec<u32>>,
    stats: BanStats,
}

/// In-memory BAN autocomplete index.
#[derive(Debug, Clone)]
pub struct BanIndex {
    data: IndexData,
}

/// Upper street count for the rare-query full scan. Country-scale indexes
/// (~2.5M streets) rely on bucket + prefix-variant coverage instead.
const FULL_SCAN_STREET_CAP: usize = 600_000;

impl BanIndex {
    pub fn empty() -> Self {
        Self {
            data: IndexData {
                streets: Vec::new(),
                addresses: Vec::new(),
                odd_addresses: Vec::new(),
                prefix3: HashMap::new(),
                stats: BanStats::default(),
            },
        }
    }

    pub fn stats(&self) -> &BanStats {
        &self.data.stats
    }

    pub fn is_ready(&self) -> bool {
        !self.data.streets.is_empty()
    }

    pub fn address_count(&self) -> usize {
        self.data.addresses.len()
    }

    pub fn street_count(&self) -> usize {
        self.data.streets.len()
    }

    /// Load a previously saved index (`index.bin`).
    pub fn load(path: &Path) -> Result<Self, BanError> {
        let f = File::open(path)?;
        let data: IndexData = bincode::deserialize_from(BufReader::new(f))
            .map_err(|e| BanError::Bincode(e.to_string()))?;
        info!(
            path = %path.display(),
            streets = data.streets.len(),
            addresses = data.addresses.len(),
            "BAN index loaded"
        );
        Ok(Self { data })
    }

    /// Persist index for fast startup.
    pub fn save(&self, path: &Path) -> Result<(), BanError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = File::create(path)?;
        bincode::serialize_into(BufWriter::new(f), &self.data)
            .map_err(|e| BanError::Bincode(e.to_string()))?;
        info!(path = %path.display(), "BAN index saved");
        Ok(())
    }

    /// Build from département CSV files in a directory (or explicit department list).
    pub fn build_from_csv_dir(
        csv_dir: &Path,
        departments: &[String],
    ) -> Result<Self, BanError> {
        let mut files = Vec::new();
        // Empty list = every BAN CSV present (all of France when complete).
        if departments.is_empty() {
            for e in std::fs::read_dir(csv_dir).map_err(BanError::from)? {
                let e = e?;
                let p = e.path();
                if crate::download::is_ban_csv(&p) {
                    let dept = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .trim_start_matches("adresses-")
                        .to_string();
                    files.push((dept, p));
                }
            }
        } else {
            for d in departments {
                let p = csv_dir.join(format!("adresses-{d}.csv"));
                if p.exists() {
                    files.push((d.clone(), p));
                }
            }
        }
        if files.is_empty() {
            return Err(BanError::index(format!(
                "no BAN CSV in {}",
                csv_dir.display()
            )));
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Self::build_from_files(&files)
    }

    fn build_from_files(files: &[(String, std::path::PathBuf)]) -> Result<Self, BanError> {
        // key: postcode|city_norm|street_norm → street_idx
        let mut street_key: HashMap<String, u32> = HashMap::new();
        let mut streets: Vec<StreetRec> = Vec::new();
        let mut addresses: Vec<AddressRec> = Vec::new();
        let mut odd_addresses: Vec<OddAddressRec> = Vec::new();
        let mut depts = Vec::new();

        for (dept, path) in files {
            depts.push(dept.clone());
            info!(%dept, path = %path.display(), "indexing BAN CSV");
            let mut rdr = csv::ReaderBuilder::new()
                .delimiter(b';')
                .has_headers(true)
                .flexible(true)
                .from_path(path)?;
            let headers = rdr.headers()?.clone();
            let col = |name: &str| headers.iter().position(|h| h == name);

            let i_num = col("numero").ok_or_else(|| BanError::index("missing numero"))?;
            let i_rep = col("rep");
            let i_voie = col("nom_voie").ok_or_else(|| BanError::index("missing nom_voie"))?;
            let i_cp = col("code_postal").ok_or_else(|| BanError::index("missing code_postal"))?;
            let i_city = col("nom_commune").ok_or_else(|| BanError::index("missing nom_commune"))?;
            let i_lon = col("lon").ok_or_else(|| BanError::index("missing lon"))?;
            let i_lat = col("lat").ok_or_else(|| BanError::index("missing lat"))?;

            let mut row_count = 0usize;
            for rec in rdr.records() {
                let rec = rec?;
                let get = |i: usize| rec.get(i).unwrap_or("").trim();
                let street = get(i_voie);
                if street.is_empty() {
                    continue;
                }
                let lat: f32 = get(i_lat).parse().unwrap_or(f32::NAN);
                let lon: f32 = get(i_lon).parse().unwrap_or(f32::NAN);
                if !lat.is_finite() || !lon.is_finite() {
                    continue;
                }
                let city = get(i_city);
                let postcode = get(i_cp);
                let num_raw = get(i_num);
                let rep_raw = i_rep.map(get).unwrap_or("");

                let street_norm = expand_street_abbrev(&normalize_query(street));
                let city_norm = normalize_query(city);
                let key = format!("{postcode}|{city_norm}|{street_norm}");

                let sidx = if let Some(&idx) = street_key.get(&key) {
                    let s = &mut streets[idx as usize];
                    // running centroid
                    let n = (s.address_ids.len() + s.odd_ids.len() + 1) as f32;
                    s.lat = (s.lat * (n - 1.0) + lat) / n;
                    s.lon = (s.lon * (n - 1.0) + lon) / n;
                    idx
                } else {
                    let idx = streets.len() as u32;
                    street_key.insert(key, idx);
                    streets.push(StreetRec {
                        name: street.to_string(),
                        name_norm: street_norm,
                        postcode: postcode.to_string(),
                        city: city.to_string(),
                        city_norm,
                        lat,
                        lon,
                        address_ids: Vec::new(),
                        odd_ids: Vec::new(),
                    });
                    idx
                };

                match (num_raw.parse::<u32>(), rep_code(rep_raw)) {
                    (Ok(num), Some(rc)) => {
                        let aidx = addresses.len() as u32;
                        streets[sidx as usize].address_ids.push(aidx);
                        addresses.push(AddressRec {
                            num,
                            rep: rc,
                            street_idx: sidx,
                            lat,
                            lon,
                        });
                    }
                    _ => {
                        // Non-numeric numero or unknown repetition — rare.
                        let oid = odd_addresses.len() as u32;
                        streets[sidx as usize].odd_ids.push(oid);
                        odd_addresses.push(OddAddressRec {
                            street_idx: sidx,
                            raw: num_raw.to_string(),
                            rep: rep_code(rep_raw).unwrap_or(0),
                            lat,
                            lon,
                        });
                    }
                }
                row_count += 1;
            }
            info!(%dept, rows = row_count, streets = streets.len(), "BAN département indexed");
        }

        // Prefix buckets (3-char) for candidate narrowing. Every significant
        // street token is indexed so mid-name tokens ("champs elysees" →
        // "avenue des champs-élysées") hit the fast path without a full scan.
        let mut prefix3: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, s) in streets.iter().enumerate() {
            let p = prefix_key(&s.name_norm, 3);
            if !p.is_empty() {
                prefix3.entry(p).or_default().push(i as u32);
            }
            for tok in s.name_norm.split_whitespace() {
                if tok.len() >= 3 && !is_street_type_token(tok) && !is_connective_token(tok) {
                    let p = prefix_key(tok, 3);
                    if !p.is_empty() {
                        prefix3.entry(p).or_default().push(i as u32);
                    }
                }
            }
            // City tokens: pure-city queries ("paris", "boulogne") and
            // city-context tokens ("fontainebleau") narrow candidates cheaply.
            // Connectives ("villiers-SUR-morin") are skipped — their buckets
            // would cover half the index and defeat the narrowing.
            for tok in s.city_norm.split_whitespace() {
                if tok.len() >= 3 && !is_connective_token(tok) {
                    let p = prefix_key(tok, 3);
                    if !p.is_empty() {
                        prefix3.entry(p).or_default().push(i as u32);
                    }
                }
            }
        }
        // dedupe buckets
        for v in prefix3.values_mut() {
            v.sort_unstable();
            v.dedup();
        }

        // Sort each street's address ids by number so house-number lookup is a
        // binary search.
        for s in &mut streets {
            s.address_ids
                .sort_by_key(|&a| addresses[a as usize].num);
        }

        let stats = BanStats {
            address_count: addresses.len(),
            street_count: streets.len(),
            departments: depts,
            built_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };

        info!(
            streets = stats.street_count,
            addresses = stats.address_count,
            "BAN index built"
        );

        Ok(Self {
            data: IndexData {
                streets,
                addresses,
                odd_addresses,
                prefix3,
                stats,
            },
        })
    }

    /// Autocomplete / search. Min query length 2 (or 1 if only a number).
    ///
    /// Hot path: candidate streets come from 3-char prefix buckets; hits are
    /// materialized (String allocations, labels) only for the final top-`limit`
    /// after scoring, so a keystroke never allocates for losers.
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<AddressHit> {
        let limit = limit.clamp(1, 30);
        let raw = query.trim();
        if raw.is_empty() {
            return vec![];
        }

        let (num_opt, street_q) = split_number_prefix(raw);
        let street_q = expand_street_abbrev(&street_q);
        let tokens: Vec<&str> = street_q.split_whitespace().collect();

        // Candidate streets: bit flags — bit n = matched the prefix bucket
        // (incl. typo variants) of the n-th significant query token. Multi-
        // token queries then score the tight intersection tier before falling
        // back to single-bucket matches ("boulevard saint michel melun" ≈
        // sai∩mic∩mel first, not sai∪mic∪mel).
        let n = self.data.streets.len();
        let mut bits = vec![0u8; n];
        let mut sig_tokens = 0usize;
        if street_q.len() >= 2 {
            for t in &tokens {
                if t.len() < 3 || is_street_type_token(t) || is_connective_token(t) {
                    continue;
                }
                if sig_tokens >= 7 {
                    break; // bit space exhausted; token still used in scoring
                }
                let b = 1u8 << sig_tokens;
                sig_tokens += 1;
                if let Some(v) = self.data.prefix3.get(&prefix_key(t, 3)) {
                    for &si in v {
                        bits[si as usize] |= b;
                    }
                }
                // Typo tolerance via prefix variants: all edit-distance-1
                // mutations of the token's first 3 chars (substitution /
                // deletion / insertion). ~150 bucket lookups per token —
                // replaces any brute-force fuzzy scan on the hot path.
                if t.len() >= 4 {
                    for v in prefix_variants(t) {
                        if let Some(bv) = self.data.prefix3.get(&v) {
                            for &si in bv {
                                bits[si as usize] |= b;
                            }
                        }
                    }
                }
            }
        }

        // Rare query: bounded exact scan over all streets. Cheap contains
        // checks only; typo coverage is handled by the variant buckets above.
        // Skipped on country-scale indexes where a full sweep costs hundreds
        // of ms — bucket + variant coverage makes it redundant there.
        let prefilter_sig: Vec<&str> = tokens
            .iter()
            .filter(|t| t.len() >= 2 && !is_street_type_token(t))
            .copied()
            .collect();
        let marked = bits.iter().any(|&b| b != 0);
        if street_q.len() >= 3
            && !marked
            && self.data.streets.len() <= FULL_SCAN_STREET_CAP
        {
            for (i, s) in self.data.streets.iter().enumerate() {
                if street_matches_exact(
                    &s.name_norm,
                    &s.city_norm,
                    &s.postcode,
                    &street_q,
                    &prefilter_sig,
                ) {
                    bits[i] = 1;
                }
            }
        }

        // Score candidates; keep only (score, street_idx) pairs.
        let want = limit * 3;
        let multi = sig_tokens >= 2;
        let score_candidate =
            |bits: &[u8], min_hits: u32, out: &mut Vec<(f32, u32)>| {
                for (i, &b) in bits.iter().enumerate() {
                    if b != 0 && (b.count_ones()) >= min_hits {
                        let street = &self.data.streets[i];
                        let sscore = street_score(street, &street_q, &tokens);
                        if sscore <= 0.0 {
                            continue;
                        }
                        match &num_opt {
                            Some((num, rep)) => {
                                // Exact house-number lookup via per-street
                                // sorted index (u32 binary search + odd list).
                                let qrep = rep_code(rep).unwrap_or(0);
                                if self.has_number(i as u32, num.parse().ok(), num, qrep) {
                                    out.push((sscore + 50.0, i as u32));
                                } else if sscore > 20.0 {
                                    // Street hit without that number — still useful.
                                    out.push((sscore, i as u32));
                                }
                            }
                            _ => out.push((sscore.max(1.0), i as u32)),
                        }
                    }
                }
            };
        let mut scored: Vec<(f32, u32)> = Vec::with_capacity(64);
        if multi {
            // Tier 1: ≥2 distinct token buckets matched.
            score_candidate(&bits, 2, &mut scored);
        }
        if scored.len() < want {
            // Tier 2: single-bucket matches (or everything, single-token).
            score_candidate(&bits, 1, &mut scored);
        }

        scored.sort_unstable_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });

        // Materialize only the visible hits. BAN can hold several street
        // records with the same display label — dedupe on the final rows only
        // (≤ `limit` strings), so over-fetch a bit before cutting off.
        let mut out: Vec<AddressHit> = Vec::with_capacity(limit);
        let mut seen_labels: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(limit);
        for (_, si) in scored.into_iter().take(limit * 4) {
            if out.len() >= limit {
                break;
            }
            let street = &self.data.streets[si as usize];
            let base = street_score(street, &street_q, &tokens);
            let hit = match &num_opt {
                Some((num, rep)) => self
                    .build_number_hit(si, num, rep, base)
                    .unwrap_or_else(|| street_hit(street, base.max(1.0))),
                None => street_hit(street, base.max(1.0)),
            };
            out.push(hit);
            if !seen_labels.insert(out.last().unwrap().label.clone()) {
                out.pop();
            }
        }
        out
    }

    /// True if street `si` has an address matching the query house number
    /// (u32 binary search over regular records + scan of rare odd numeros).
    fn has_number(&self, si: u32, qnum: Option<u32>, qraw: &str, qrep: u8) -> bool {
        let s = &self.data.streets[si as usize];
        if let Some(n) = qnum {
            let ids = &s.address_ids;
            let addrs = &self.data.addresses;
            let lo = ids.partition_point(|&ai| addrs[ai as usize].num < n);
            for &ai in &ids[lo..] {
                let a = &addrs[ai as usize];
                if a.num != n {
                    break;
                }
                if rep_ok(qrep, a.rep) {
                    return true;
                }
            }
        }
        s.odd_ids.iter().any(|&oi| {
            let o = &self.data.odd_addresses[oi as usize];
            o.raw.eq_ignore_ascii_case(qraw) && rep_ok(qrep, o.rep)
        })
    }

    /// Materialize the top house-number hit for street `si`.
    fn build_number_hit(&self, si: u32, num: &str, rep: &str, base: f32) -> Option<AddressHit> {
        let street = &self.data.streets[si as usize];
        let qrep = rep_code(rep).unwrap_or(0);
        // (display number, rep code, lat, lon, exact-rep bonus)
        let mut best: Option<(String, u8, f32, f32, f32)> = None;
        if let Ok(n) = num.parse::<u32>() {
            let ids = &street.address_ids;
            let addrs = &self.data.addresses;
            let lo = ids.partition_point(|&ai| addrs[ai as usize].num < n);
            for &ai in &ids[lo..] {
                let a = &addrs[ai as usize];
                if a.num != n {
                    break;
                }
                if !rep_ok(qrep, a.rep) {
                    continue;
                }
                let bonus = f32::from(a.rep == qrep && qrep != 0) * 10.0;
                if best.as_ref().map(|(_, _, _, _, b)| bonus > *b).unwrap_or(true) {
                    best = Some((n.to_string(), a.rep, a.lat, a.lon, bonus));
                }
            }
        }
        for &oi in &street.odd_ids {
            let o = &self.data.odd_addresses[oi as usize];
            if !o.raw.eq_ignore_ascii_case(num) || !rep_ok(qrep, o.rep) {
                continue;
            }
            let bonus = f32::from(o.rep == qrep && qrep != 0) * 10.0;
            if best.as_ref().map(|(_, _, _, _, b)| bonus > *b).unwrap_or(true) {
                best = Some((o.raw.clone(), o.rep, o.lat, o.lon, bonus));
            }
        }
        let (num_disp, arep, lat, lon, bonus) = best?;
        let rep_disp = REP_TABLE[arep as usize];
        Some(AddressHit {
            id: format!("ban:{si}:{num_disp}{rep_disp}"),
            label: format_label(&num_disp, rep_disp, &street.name, &street.postcode, &street.city),
            number: num_disp,
            rep: rep_disp.to_string(),
            street: street.name.clone(),
            postcode: street.postcode.clone(),
            city: street.city.clone(),
            lat: lat as f64,
            lon: lon as f64,
            score: base + 50.0 + bonus,
            kind: "address".into(),
        })
    }
}

/// Repetition filter on compact codes: empty query accepts anything; otherwise
/// exact code match or single-letter query matches longer forms ("b" ↔ "bis").
fn rep_ok(qrep: u8, arep: u8) -> bool {
    if qrep == 0 || qrep == arep {
        return true;
    }
    let q = REP_TABLE[qrep as usize];
    q.len() == 1 && REP_TABLE[arep as usize].starts_with(q)
}

fn prefix_key(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    s.chars().take(n).collect()
}

fn is_street_type_token(t: &str) -> bool {    matches!(
        t,
        "rue"
            | "avenue"
            | "boulevard"
            | "place"
            | "impasse"
            | "allee"
            | "chemin"
            | "route"
            | "square"
            | "cours"
            | "quai"
            | "faubourg"
            | "passage"
            | "residence"
            | "lotissement"
            | "sentier"
            | "villa"
            | "cite"
            | "promenade"
            | "esplanade"
            | "voie"
            | "de"
            | "du"
            | "des"
            | "la"
            | "le"
            | "les"
            | "et"
    )
}

/// Generic connectives (mostly in commune names, e.g. « Villiers-sur-Morin »)
/// whose 3-char prefix buckets would cover a huge share of the index and
/// defeat candidate narrowing.
fn is_connective_token(t: &str) -> bool {
    matches!(t, "sur" | "sous" | "sans" | "les" | "aux" | "l")
}

/// Exact (no fuzzy) match used by the bounded fallback scan — must stay cheap.
/// `sig` holds the significant query tokens, precomputed once per query.
fn street_matches_exact(
    name_norm: &str,
    city_norm: &str,
    postcode: &str,
    q: &str,
    sig: &[&str],
) -> bool {
    if q.is_empty() {
        return true;
    }
    if name_norm.contains(q) || city_norm.contains(q) || postcode.starts_with(q) {
        return true;
    }
    if sig.is_empty() {
        return name_norm.contains(q);
    }
    sig.iter()
        .all(|t| name_norm.contains(*t) || city_norm.contains(*t))
}

/// Typo-tolerant token match: `tok` is within a small edit distance of some
/// word of `hay` (both normalized). Used only during scoring of bucketed
/// candidates — candidate collection itself stays exact via prefix variants.
fn fuzzy_token_match(tok: &str, hay_norm: &str) -> bool {
    let n_chars = tok.chars().count();
    if n_chars < 4 {
        return false;
    }
    let max_d = if n_chars >= 7 { 2 } else { 1 };
    hay_norm
        .split_whitespace()
        .any(|w| w.chars().count() >= 3 && edit_distance_leq(tok, w, max_d))
}

/// All single-edit variants (substitution / deletion / insertion) of the first
/// 3 chars of a token, used to look up typo'd tokens in the prefix buckets.
/// Bounded: ≤ ~150 keys per token, all ASCII letters.
fn prefix_variants(tok: &str) -> impl Iterator<Item = String> {
    let head: Vec<char> = tok.chars().take(3).collect();
    let rest: String = tok.chars().skip(3).collect();
    let mut out = Vec::with_capacity(26 * (head.len() + 1) + head.len());
    for i in 0..head.len() {
        // substitutions
        for c in b'a'..=b'z' {
            if c as char != head[i] {
                let mut v: String = head[..i].iter().collect();
                v.push(c as char);
                v.extend(head[i + 1..].iter());
                out.push(v + &rest);
            }
        }
        // deletions
        let d: String = head[..i].iter().chain(head[i + 1..].iter()).collect();
        out.push(d + &rest);
    }
    // insertions before/after each of the first 3 positions
    for i in 0..=head.len() {
        for c in b'a'..=b'z' {
            let mut v: String = head[..i].iter().collect();
            v.push(c as char);
            v.extend(head[i..].iter());
            out.push(v + &rest);
        }
    }
    out.into_iter()
}

fn street_score(street: &StreetRec, q: &str, tokens: &[&str]) -> f32 {
    if q.is_empty() {
        return 1.0;
    }
    // Whole-query match on the street name.
    let mut name_score = 0.0f32;
    if street.name_norm == q {
        name_score += 100.0;
    } else if street.name_norm.starts_with(q) {
        name_score += 80.0;
    } else if street.name_norm.contains(q) {
        name_score += 50.0;
    }

    let mut matched = 0usize;
    let mut sig_total = 0usize;
    let mut context = 0.0f32;
    for t in tokens {
        if t.len() < 2 || is_street_type_token(t) {
            continue;
        }
        sig_total += 1;
        let mut tok_name = 0.0f32;
        if street.name_norm == *t {
            tok_name = 40.0;
        } else if street.name_norm.starts_with(t) || contains_word(&street.name_norm, t) {
            tok_name = 15.0;
        } else if street.name_norm.contains(t) {
            tok_name = 8.0;
        } else if fuzzy_token_match(t, &street.name_norm) {
            tok_name = 10.0;
        }
        if tok_name > 0.0 {
            matched += 1;
            name_score += tok_name;
        }
        // City / postcode evidence is context, not identity (exact only —
        // fuzzy here would run per candidate and dominate the hot path).
        if street.city_norm.contains(t) {
            context += 8.0;
        }
        if !street.postcode.is_empty() && street.postcode.starts_with(t) {
            context += 20.0;
        }
    }

    // Matching every significant token beats partial matches.
    let coverage = if sig_total == 0 {
        1.0
    } else {
        matched as f32 / sig_total as f32
    };
    let total = (name_score + context) * (0.5 + 0.5 * coverage);

    if name_score <= 0.0 {
        // No street-name evidence — city/postcode-only matches ("paris",
        // "75001") stay available for broad queries but always rank below
        // anything that actually matched the street name.
        return (total * 0.3).min(9.0);
    }
    // Bonus for more addresses (importance proxy).
    total + (street.address_ids.len() as f32).min(50.0) * 0.05
}

/// True if `needle` occurs in `hay` at a word boundary (space-prefixed).
/// Allocation-free replacement for `hay.contains(&format!(" {needle}"))`.
fn contains_word(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0usize;
    while let Some(pos) = hay[from..].find(needle) {
        let idx = from + pos;
        if idx > 0 && hay.as_bytes()[idx - 1] == b' ' {
            return true;
        }
        from = idx + needle.len();
        if from >= hay.len() {
            break;
        }
    }
    false
}

fn street_hit(street: &StreetRec, score: f32) -> AddressHit {
    let label = if street.postcode.is_empty() {
        format!("{}, {}", street.name, street.city)
    } else {
        format!("{} {} {}", street.name, street.postcode, street.city)
    };
    AddressHit {
        id: format!(
            "street:{}:{}:{}",
            street.postcode, street.city_norm, street.name_norm
        ),
        label,
        number: String::new(),
        rep: String::new(),
        street: street.name.clone(),
        postcode: street.postcode.clone(),
        city: street.city.clone(),
        lat: street.lat as f64,
        lon: street.lon as f64,
        score,
        kind: "street".into(),
    }
}

fn format_label(num: &str, rep: &str, street: &str, postcode: &str, city: &str) -> String {
    let mut head = String::new();
    if !num.is_empty() && num != "0" {
        head.push_str(num);
        if !rep.is_empty() {
            head.push(' ');
            head.push_str(rep);
        }
        head.push(' ');
    }
    if postcode.is_empty() {
        format!("{head}{street}, {city}")
    } else {
        format!("{head}{street} {postcode} {city}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn sample_csv(dir: &Path) {
        let p = dir.join("adresses-75.csv");
        let mut f = File::create(&p).unwrap();
        writeln!(
            f,
            "id;id_fantoir;numero;rep;nom_voie;code_postal;code_insee;nom_commune;code_insee_ancienne_commune;nom_ancienne_commune;x;y;lon;lat;type_position;alias;nom_ld;libelle_acheminement;nom_afnor;source_position;source_nom_voie;certification_commune;cad_parcelles"
        )
        .unwrap();
        // 23 fields: …nom_commune;anc;anc_name;x;y;lon;lat;type;…
        writeln!(
            f,
            "75104_1234_00012;;12;;Rue de Rivoli;75001;75101;Paris;;;;;2.3364;48.8606;entrée;;;;Paris;RUE DE RIVOLI;commune;commune;1;"
        )
        .unwrap();
        writeln!(
            f,
            "75104_1234_00014;;14;;Rue de Rivoli;75001;75101;Paris;;;;;2.3370;48.8607;entrée;;;;Paris;RUE DE RIVOLI;commune;commune;1;"
        )
        .unwrap();
        writeln!(
            f,
            "75109_9999_00001;;1;;Boulevard Haussmann;75009;75109;Paris;;;;;2.3310;48.8738;entrée;;;;Paris;RUE DE RIVOLI;commune;commune;1;"
        )
        .unwrap();
        writeln!(
            f,
            "75104_8888_00002;;2;;Avenue des Champs-Elysées;75008;75108;Paris;;;;;2.3010;48.8690;entrée;;;;Paris;AVENUE DES CHAMPS-ELYSEES;commune;commune;1;"
        )
        .unwrap();
        writeln!(
            f,
            "77000_1111_00005;;5;;Rue de la Gare;77000;77021;Melun;;;;;2.6600;48.5400;entrée;;;;Melun;RUE DE LA GARE;commune;commune;1;"
        )
        .unwrap();
        writeln!(
            f,
            "75005_2222_00010;;10;;Boulevard Saint-Michel;75005;75105;Paris;;;;;2.3430;48.8470;entrée;;;;Paris;BOULEVARD SAINT-MICHEL;commune;commune;1;"
        )
        .unwrap();
        writeln!(
            f,
            "75014_3333_00020;;20;;Boulevard Saint-Jacques;75014;75114;Paris;;;;;2.3340;48.8320;entrée;;;;Paris;BOULEVARD SAINT-JACQUES;commune;commune;1;"
        )
        .unwrap();
    }

    #[test]
    fn builds_and_suggests() {
        let dir = tempfile::tempdir().unwrap();
        sample_csv(dir.path());
        let idx = BanIndex::build_from_csv_dir(dir.path(), &["75".into()]).unwrap();
        assert!(idx.address_count() >= 3);
        let hits = idx.suggest("12 rue de rivoli", 5);
        assert!(!hits.is_empty(), "expected hits, got none");
        assert!(
            hits[0].label.to_ascii_lowercase().contains("rivoli"),
            "{:?}",
            hits[0]
        );
        assert!(hits[0].lat > 48.0);
        let street = idx.suggest("haussmann paris", 5);
        assert!(street.iter().any(|h| h.street.to_ascii_lowercase().contains("haussmann")));
    }

    fn build_sample() -> BanIndex {
        let dir = tempfile::tempdir().unwrap();
        sample_csv(dir.path());
        BanIndex::build_from_csv_dir(dir.path(), &["75".into()]).unwrap()
    }

    #[test]
    fn mid_name_token_found() {
        let idx = build_sample();
        // "champs" is not the first word of the street name — must still match.
        let hits = idx.suggest("champs elysees", 5);
        assert!(
            hits.iter().any(|h| h.street.contains("Champs")),
            "{:?}",
            hits.iter().map(|h| &h.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn typo_tolerant_and_name_beats_city_only() {
        let idx = build_sample();
        let hits = idx.suggest("rivolli", 5);
        assert!(
            !hits.is_empty(),
            "typo in street name should still match"
        );
        assert_eq!(hits[0].street, "Rue de Rivoli", "got {:?}", hits[0]);
    }

    #[test]
    fn city_only_query_ranked_below_name_match() {
        let idx = build_sample();
        let broad = idx.suggest("paris", 5);
        assert!(!broad.is_empty(), "city-only query stays useful");
        for h in &broad {
            assert!(h.score <= 9.0, "city-only hit demoted: {:?}", h);
        }
        // A name match always outranks city-only evidence.
        let mixed = idx.suggest("rivoli paris", 3);
        assert!(
            mixed[0].street.to_ascii_lowercase().contains("rivoli"),
            "{:?}",
            mixed
        );
    }

    #[test]
    fn full_token_coverage_outranks_partial() {
        let idx = build_sample();
        let hits = idx.suggest("boulevard saint michel", 5);
        assert!(
            hits[0].label.contains("Saint-Michel"),
            "full coverage first, got {:?}",
            hits[0]
        );
    }

    #[test]
    fn irrelevant_query_returns_nothing() {
        let idx = build_sample();
        assert!(idx.suggest("qqqzzzz", 5).is_empty());
        assert!(idx.suggest("xyzzy wprld", 5).is_empty());
    }
}
