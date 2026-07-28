//! Compact BAN index: addresses + street groups + prefix buckets for autocomplete.

use crate::normalize::{expand_street_abbrev, normalize_query, split_number_prefix};
use crate::types::{AddressHit, BanError, BanStats};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use tracing::info;

/// One house-level record (compact).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AddressRec {
    id: String,
    number: String,
    rep: String,
    street_idx: u32,
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
    /// Indices into `addresses`.
    address_ids: Vec<u32>,
}

/// Serializable index payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexData {
    streets: Vec<StreetRec>,
    addresses: Vec<AddressRec>,
    /// First 3 chars of street norm → street indices.
    prefix3: HashMap<String, Vec<u32>>,
    stats: BanStats,
}

/// In-memory BAN autocomplete index.
#[derive(Debug, Clone)]
pub struct BanIndex {
    data: IndexData,
}

impl BanIndex {
    pub fn empty() -> Self {
        Self {
            data: IndexData {
                streets: Vec::new(),
                addresses: Vec::new(),
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
        for d in departments {
            let p = csv_dir.join(format!("adresses-{d}.csv"));
            if p.exists() {
                files.push((d.clone(), p));
            }
        }
        if files.is_empty() {
            // fall back to any adresses-*.csv
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

            let i_id = col("id");
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
                let number = get(i_num).to_string();
                let rep = i_rep.map(get).unwrap_or("").to_string();
                let id = i_id.map(get).unwrap_or("").to_string();

                let street_norm = expand_street_abbrev(&normalize_query(street));
                let city_norm = normalize_query(city);
                let key = format!("{postcode}|{city_norm}|{street_norm}");

                let sidx = if let Some(&idx) = street_key.get(&key) {
                    let s = &mut streets[idx as usize];
                    // running centroid
                    let n = (s.address_ids.len() + 1) as f32;
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
                    });
                    idx
                };

                let aidx = addresses.len() as u32;
                streets[sidx as usize].address_ids.push(aidx);
                addresses.push(AddressRec {
                    id,
                    number,
                    rep,
                    street_idx: sidx,
                    lat,
                    lon,
                });
                row_count += 1;
            }
            info!(%dept, rows = row_count, streets = streets.len(), "BAN département indexed");
        }

        // Prefix buckets (3-char) for candidate narrowing.
        let mut prefix3: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, s) in streets.iter().enumerate() {
            let p = prefix_key(&s.name_norm, 3);
            if !p.is_empty() {
                prefix3.entry(p).or_default().push(i as u32);
            }
            // also first significant token (≥3 chars)
            if let Some(tok) = s
                .name_norm
                .split_whitespace()
                .find(|t| t.len() >= 3 && !is_street_type_token(t))
            {
                let p = prefix_key(tok, 3);
                prefix3.entry(p).or_default().push(i as u32);
            }
        }
        // dedupe buckets
        for v in prefix3.values_mut() {
            v.sort_unstable();
            v.dedup();
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
                prefix3,
                stats,
            },
        })
    }

    /// Autocomplete / search. Min query length 2 (or 1 if only a number).
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<AddressHit> {
        let limit = limit.clamp(1, 30);
        let raw = query.trim();
        if raw.is_empty() {
            return vec![];
        }

        let (num_opt, street_q) = split_number_prefix(raw);
        let street_q = expand_street_abbrev(&street_q);
        let tokens: Vec<&str> = street_q.split_whitespace().collect();

        // Collect candidate street indices.
        let mut candidates: Vec<u32> = Vec::new();
        if street_q.len() >= 2 {
            let p = prefix_key(&street_q, 3);
            if let Some(v) = self.data.prefix3.get(&p) {
                candidates.extend(v.iter().copied());
            }
            // token prefixes
            for t in &tokens {
                if t.len() >= 3 && !is_street_type_token(t) {
                    let p = prefix_key(t, 3);
                    if let Some(v) = self.data.prefix3.get(&p) {
                        candidates.extend(v.iter().copied());
                    }
                }
            }
        }

        // Fallback: scan all streets if few candidates (short / rare query).
        if candidates.len() < 32 && street_q.len() >= 2 {
            for (i, s) in self.data.streets.iter().enumerate() {
                if street_matches(&s.name_norm, &s.city_norm, &s.postcode, &street_q, &tokens) {
                    candidates.push(i as u32);
                }
            }
        }

        candidates.sort_unstable();
        candidates.dedup();

        let mut scored: Vec<(f32, AddressHit)> = Vec::new();

        for si in candidates {
            let street = &self.data.streets[si as usize];
            let sscore = street_score(street, &street_q, &tokens);
            if sscore <= 0.0 && street_q.len() >= 2 {
                continue;
            }

            if let Some((ref num, ref rep)) = num_opt {
                // House number search on this street.
                let mut best: Option<(f32, AddressHit)> = None;
                for &ai in &street.address_ids {
                    let a = &self.data.addresses[ai as usize];
                    if a.number != *num {
                        continue;
                    }
                    let rep_ok = rep.is_empty()
                        || a.rep.eq_ignore_ascii_case(rep)
                        || (rep.len() == 1
                            && a.rep
                                .chars()
                                .next()
                                .map(|c| c.to_ascii_lowercase().to_string())
                                == Some(rep.clone()));
                    if !rep_ok && !a.rep.is_empty() && !rep.is_empty() {
                        continue;
                    }
                    let mut score = sscore + 50.0;
                    if a.rep.eq_ignore_ascii_case(rep) {
                        score += 10.0;
                    }
                    let label = format_label(&a.number, &a.rep, &street.name, &street.postcode, &street.city);
                    let hit = AddressHit {
                        id: a.id.clone(),
                        label,
                        number: a.number.clone(),
                        rep: a.rep.clone(),
                        street: street.name.clone(),
                        postcode: street.postcode.clone(),
                        city: street.city.clone(),
                        lat: a.lat as f64,
                        lon: a.lon as f64,
                        score,
                        kind: "address".into(),
                    };
                    if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                        best = Some((score, hit));
                    }
                }
                if let Some(b) = best {
                    scored.push(b);
                } else if sscore > 20.0 {
                    // Street hit without that number — still useful.
                    scored.push((
                        sscore,
                        street_hit(street, sscore),
                    ));
                }
            } else {
                // Street-level (or best sample address as proxy).
                scored.push((sscore.max(1.0), street_hit(street, sscore.max(1.0))));
                // Also surface a few representative house numbers for strong matches.
                if sscore >= 40.0 {
                    for &ai in street.address_ids.iter().take(3) {
                        let a = &self.data.addresses[ai as usize];
                        if a.number.is_empty() || a.number == "0" {
                            continue;
                        }
                        let label = format_label(
                            &a.number,
                            &a.rep,
                            &street.name,
                            &street.postcode,
                            &street.city,
                        );
                        scored.push((
                            sscore - 5.0,
                            AddressHit {
                                id: a.id.clone(),
                                label,
                                number: a.number.clone(),
                                rep: a.rep.clone(),
                                street: street.name.clone(),
                                postcode: street.postcode.clone(),
                                city: street.city.clone(),
                                lat: a.lat as f64,
                                lon: a.lon as f64,
                                score: sscore - 5.0,
                                kind: "address".into(),
                            },
                        ));
                    }
                }
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // Dedupe by label
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (_, h) in scored {
            if seen.insert(h.label.clone()) {
                out.push(h);
            }
            if out.len() >= limit {
                break;
            }
        }
        out
    }
}

fn prefix_key(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    s.chars().take(n).collect()
}

fn is_street_type_token(t: &str) -> bool {
    matches!(
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

fn street_matches(
    name_norm: &str,
    city_norm: &str,
    postcode: &str,
    q: &str,
    tokens: &[&str],
) -> bool {
    if q.is_empty() {
        return true;
    }
    if name_norm.contains(q) || city_norm.contains(q) || postcode.starts_with(q) {
        return true;
    }
    // all significant tokens present
    let sig: Vec<&&str> = tokens
        .iter()
        .filter(|t| t.len() >= 2 && !is_street_type_token(t))
        .collect();
    if sig.is_empty() {
        return name_norm.contains(q);
    }
    sig.iter().all(|t| name_norm.contains(**t) || city_norm.contains(**t))
}

fn street_score(street: &StreetRec, q: &str, tokens: &[&str]) -> f32 {
    if q.is_empty() {
        return 1.0;
    }
    let mut score = 0.0f32;
    if street.name_norm == q {
        score += 100.0;
    } else if street.name_norm.starts_with(q) {
        score += 80.0;
    } else if street.name_norm.contains(q) {
        score += 50.0;
    }
    for t in tokens {
        if t.len() < 2 || is_street_type_token(t) {
            continue;
        }
        if street.name_norm.starts_with(t) || street.name_norm.contains(&format!(" {t}")) {
            score += 15.0;
        } else if street.name_norm.contains(t) {
            score += 8.0;
        }
        if street.city_norm.contains(t) {
            score += 12.0;
        }
        if street.postcode.starts_with(t) {
            score += 20.0;
        }
    }
    // Prefer Paris / well-known cities slightly for short queries? skip.
    // Bonus for more addresses (importance proxy).
    score += (street.address_ids.len() as f32).min(50.0) * 0.05;
    score
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
            "75109_9999_00001;;1;;Boulevard Haussmann;75009;75109;Paris;;;;;2.3310;48.8738;entrée;;;;Paris;BOULEVARD HAUSSMANN;commune;commune;1;"
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
}
