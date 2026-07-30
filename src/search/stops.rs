use crate::gtfs::pack::{RouteMode, StaticEpoch, StopRecord};
use crate::link::geo::{haversine_m, nearby};

/// Fold common French/Latin accents for accent-insensitive matching.
/// Does not depend on unicode-normalization; covers the usual GTFS French set.
pub fn strip_accents(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let mapped = match c {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'À' | 'Á' | 'Â' | 'Ä' | 'Ã' | 'Å' => 'A',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'È' | 'É' | 'Ê' | 'Ë' => 'E',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
            'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
            'Ò' | 'Ó' | 'Ô' | 'Ö' | 'Õ' => 'O',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
            'ý' | 'ÿ' => 'y',
            'Ý' | 'Ÿ' => 'Y',
            'ç' => 'c',
            'Ç' => 'C',
            'ñ' => 'n',
            'Ñ' => 'N',
            'œ' => {
                out.push('o');
                out.push('e');
                continue;
            }
            'Œ' => {
                out.push('O');
                out.push('E');
                continue;
            }
            'æ' => {
                out.push('a');
                out.push('e');
                continue;
            }
            'Æ' => {
                out.push('A');
                out.push('E');
                continue;
            }
            other => other,
        };
        out.push(mapped);
    }
    out
}

pub fn normalize_query(s: &str) -> String {
    strip_accents(s).to_lowercase()
}

/// Per-stop precomputed search data (built at pack time) so autocomplete does
/// not re-normalize every stop name/id on each keystroke.
#[derive(Debug, Clone)]
pub struct StopSearchEntry {
    pub name_norm: String,
    pub id_norm: String,
    pub place_kind: i32,
    pub has_dep: bool,
}

/// Build the search index for an epoch (called from `pack::build_epoch`).
pub fn build_stop_search_index(epoch: &StaticEpoch) -> Vec<StopSearchEntry> {
    epoch
        .stops
        .iter()
        .enumerate()
        .map(|(i, s)| StopSearchEntry {
            name_norm: normalize_query(&s.name),
            id_norm: normalize_query(&s.raw_id),
            place_kind: place_kind_score(s),
            has_dep: epoch
                .stop_departures
                .get(i)
                .map(|v| !v.is_empty())
                .unwrap_or(false),
        })
        .collect()
}

/// Match quality for ranking (higher is better).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchKind {
    None = 0,
    Substring = 1,
    WordBoundary = 2,
    Prefix = 3,
}

fn best_match_kind(haystack: &str, needle: &str) -> MatchKind {
    if needle.is_empty() {
        return MatchKind::None;
    }
    if haystack.starts_with(needle) {
        return MatchKind::Prefix;
    }
    // word boundary: needle at start of a token (after non-alphanumeric)
    let bytes = haystack.as_bytes();
    let n = needle.as_bytes();
    let mut i = 0usize;
    while i + n.len() <= bytes.len() {
        if bytes[i..].starts_with(n) {
            let at_boundary = i == 0
                || !bytes[i - 1].is_ascii_alphanumeric();
            if at_boundary {
                return MatchKind::WordBoundary;
            }
        }
        // advance one UTF-8 char if possible, else one byte
        let ch = haystack[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        i += ch;
    }
    if haystack.contains(needle) {
        MatchKind::Substring
    } else {
        MatchKind::None
    }
}

/// IDFM / GTFS place preference for passenger-facing search.
/// monomodalStopPlace (one mode at a station) > multimodalStopPlace > station > quay.
pub fn place_kind_score(s: &StopRecord) -> i32 {
    let id = s.raw_id.to_ascii_lowercase();
    if id.contains("monomodalstopplace") {
        400
    } else if id.contains("multimodalstopplace") {
        350
    } else if id.contains("stopplace") && !id.contains("quay") {
        300
    } else if s.location_type == 1 {
        200
    } else if s.is_station() {
        120
    } else {
        0
    }
}

pub fn is_place_like(s: &StopRecord) -> bool {
    place_kind_score(s) >= 120
}

/// IDFM monomodal / multimodal / stop-place shells (not quays).
pub fn is_idfm_place_stop(s: &StopRecord) -> bool {
    let id = s.raw_id.to_ascii_lowercase();
    id.contains("monomodalstopplace")
        || id.contains("multimodalstopplace")
        || (id.contains("stopplace") && !id.contains("quay") && !id.contains("entrance"))
}

/// Normalize a station name for clustering (« Gare d'Aulnay-sous-Bois » ≈ « Aulnay-sous-Bois »).
pub fn place_name_key(name: &str) -> String {
    let mut s = strip_accents(name).to_lowercase();
    for prefix in [
        "gare de ",
        "gare d'",
        "gare d'",
        "station ",
        "arret ",
        "arrêt ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    for suffix in [" rer", " metro", " métro", " tram", " bus"] {
        if let Some(rest) = s.strip_suffix(suffix) {
            s = rest.to_string();
        }
    }
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && *t != "de" && *t != "la" && *t != "le" && *t != "les")
        .collect::<Vec<_>>()
        .join(" ")
}

/// Case- and accent-insensitive stop search.
/// Ranking: prefix > word boundary > substring; **monomodal / multimodal places** preferred;
/// results collapsed to **one row per display name** (no quay spam).
pub fn search_stops(
    epoch: &StaticEpoch,
    query: &str,
    modes: Option<&[RouteMode]>,
    limit: usize,
) -> Vec<StopRecord> {
    search_stops_filtered(epoch, query, modes, limit, false)
}

/// Like [`search_stops`] with an optional station-only filter.
pub fn search_stops_filtered(
    epoch: &StaticEpoch,
    query: &str,
    modes: Option<&[RouteMode]>,
    limit: usize,
    is_station_only: bool,
) -> Vec<StopRecord> {
    let q = normalize_query(query.trim());
    if q.len() < 2 {
        return vec![];
    }
    // Packed epochs carry a precomputed search index (zero per-query allocs);
    // unit-test epochs fall back to on-the-fly normalization.
    let pre = if epoch.stop_search.len() == epoch.stops.len() {
        Some(epoch.stop_search.as_slice())
    } else {
        None
    };
    let mut scored: Vec<(i32, &StopRecord)> = Vec::new();
    for (i, s) in epoch.stops.iter().enumerate() {
        if is_station_only && !s.is_station() {
            continue;
        }
        let name_store;
        let id_store;
        let (name_n, id_n, place_kind, has_dep) = if let Some(pre) = pre {
            let e = &pre[i];
            (
                e.name_norm.as_str(),
                e.id_norm.as_str(),
                e.place_kind,
                e.has_dep,
            )
        } else {
            name_store = normalize_query(&s.name);
            id_store = normalize_query(&s.raw_id);
            let pk = place_kind_score(s);
            let hd = epoch
                .stop_departures
                .get(i)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            (name_store.as_str(), id_store.as_str(), pk, hd)
        };
        let kind_name = best_match_kind(name_n, &q);
        let kind_id = best_match_kind(id_n, &q);
        let kind = kind_name.max(kind_id);
        if kind == MatchKind::None {
            continue;
        }

        let mut score = match kind {
            MatchKind::Prefix => 300,
            MatchKind::WordBoundary => 200,
            MatchKind::Substring => 100,
            MatchKind::None => 0,
        };
        // Prefer name matches over id-only matches
        if kind_name >= kind_id && kind_name != MatchKind::None {
            score += 30;
        }
        // IDFM: monomodal / multimodal stop places beat numeric quays
        score += place_kind;
        // Prefer shorter names on equal match quality
        score -= (name_n.len() as i32).min(40);
        // Prefer boardable monomodals; demote empty place shells (no departures)
        if has_dep {
            score += 80;
        } else if place_kind >= 300 {
            score -= 350; // empty monomodal (e.g. wrong Nation shell)
        }
        let _ = modes; // mode filter needs route↔stop index; reserved
        scored.push((score, s));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));

    // Collapse: one result per normalized name (keep highest score → monomodal wins)
    let mut seen_names = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(limit);
    for (score, s) in scored {
        let key = normalize_query(&s.name);
        if key.is_empty() {
            continue;
        }
        // Always keep distinct monomodals if same name is rare; primarily drop quay spam
        if place_kind_score(s) < 120 && seen_names.contains(&key) {
            continue;
        }
        if place_kind_score(s) >= 120 {
            // places: one monomodal/multimodal per name
            if !seen_names.insert(key) {
                continue;
            }
        } else if !seen_names.insert(key) {
            continue;
        } else {
            let _ = score;
        }
        out.push(s.clone());
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Search stops by geographic radius; results sorted by distance (nearest first).
pub fn near(
    epoch: &StaticEpoch,
    lat: f64,
    lon: f64,
    radius_m: f64,
    limit: usize,
    is_station_only: bool,
) -> Vec<(StopRecord, f64)> {
    let points = epoch.geo_stop_points_view();

    nearby(&points, lat, lon, radius_m, limit)
        .into_iter()
        .filter_map(|(idx, dist)| {
            let s = epoch.stops.get(idx)?;
            if is_station_only && !s.is_station() {
                return None;
            }
            Some((s.clone(), dist))
        })
        .collect()
}

/// Convenience: same as [`near`] but returns only stop records (distance discarded).
pub fn near_stops(
    epoch: &StaticEpoch,
    lat: f64,
    lon: f64,
    radius_m: f64,
    limit: usize,
    is_station_only: bool,
) -> Vec<StopRecord> {
    near(epoch, lat, lon, radius_m, limit, is_station_only)
        .into_iter()
        .map(|(s, _)| s)
        .collect()
}

/// Distance in meters between two stops when both have coordinates.
pub fn stop_distance_m(a: &StopRecord, b: &StopRecord) -> Option<f64> {
    Some(haversine_m(a.lat?, a.lon?, b.lat?, b.lon?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::pack::StaticEpoch;

    fn stop(id: &str, name: &str, loc: u8, lat: f64, lon: f64) -> StopRecord {
        StopRecord {
            id: id.into(),
            feed_id: "t".into(),
            raw_id: id.into(),
            name: name.into(),
            lat: Some(lat),
            lon: Some(lon),
            parent_id: None,
            location_type: loc,
            platform_code: None,
            wheelchair: 0,
            stop_code: None,
            stop_desc: None,
            level_id: None,
        zone_id: None,
        stop_url: None,
        stop_timezone: None,
        }
    }

    fn epoch_with(stops: Vec<StopRecord>) -> StaticEpoch {
        let mut e = StaticEpoch::empty();
        for (i, s) in stops.into_iter().enumerate() {
            e.stop_id_to_idx.insert(s.id.clone(), i as u32);
            e.stops.push(s);
        }
        e
    }

    #[test]
    fn strip_accents_french() {
        assert_eq!(strip_accents("Châtelet"), "Chatelet");
        assert_eq!(strip_accents("Évry"), "Evry");
        assert_eq!(normalize_query("Gare de Lyon"), "gare de lyon");
        assert_eq!(normalize_query("  Gare de Lyon  ".trim()), "gare de lyon");
    }

    #[test]
    fn prefix_beats_substring() {
        let e = epoch_with(vec![
            stop("1", "Lyon Perrache", 1, 45.7, 4.8),
            stop("2", "Villeurbanne Lyon", 0, 45.7, 4.9),
            stop("3", "Paris Gare de Lyon", 1, 48.8, 2.4),
        ]);
        let hits = search_stops(&e, "lyon", None, 10);
        assert!(!hits.is_empty());
        // "Lyon Perrache" is prefix; should rank first among these
        assert_eq!(hits[0].name, "Lyon Perrache");
    }

    #[test]
    fn collapse_prefers_monomodal_place() {
        let mono = StopRecord {
            id: "idfm:IDFM:monomodalStopPlace:1".into(),
            feed_id: "idfm".into(),
            raw_id: "IDFM:monomodalStopPlace:1".into(),
            name: "Aulnay-sous-Bois".into(),
            lat: Some(48.93),
            lon: Some(2.49),
            parent_id: None,
            location_type: 0,
            platform_code: None,
            wheelchair: 0,
            stop_code: None,
            stop_desc: None,
            level_id: None,
            zone_id: None,
            stop_url: None,
            stop_timezone: None,
        };
        let quay = StopRecord {
            id: "idfm:IDFM:999".into(),
            feed_id: "idfm".into(),
            raw_id: "IDFM:999".into(),
            name: "Aulnay-sous-Bois".into(),
            lat: Some(48.93),
            lon: Some(2.49),
            parent_id: None,
            location_type: 0,
            platform_code: None,
            wheelchair: 0,
            stop_code: None,
            stop_desc: None,
            level_id: None,
            zone_id: None,
            stop_url: None,
            stop_timezone: None,
        };
        let mut e = epoch_with(vec![quay, mono]);
        // Real epochs have departures; without them the "empty place shell"
        // penalty intentionally demotes the monomodal below the quay.
        e.stop_departures = vec![vec![(0, 0)], vec![(0, 0)]];
        let aul = search_stops(&e, "aulnay-sous-bois", None, 10);
        assert_eq!(
            aul.len(),
            1,
            "collapse to one name: {:?}",
            aul.iter().map(|s| &s.id).collect::<Vec<_>>()
        );
        assert!(
            aul[0].raw_id.to_lowercase().contains("monomodal"),
            "prefer monomodal got {}",
            aul[0].id
        );
    }

    #[test]
    fn accent_insensitive() {
        let e = epoch_with(vec![stop("1", "Châtelet", 1, 48.85, 2.34)]);
        let hits = search_stops(&e, "chatelet", None, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Châtelet");
    }

    #[test]
    fn station_only_filter() {
        let e = epoch_with(vec![
            stop("A", "Alpha Station", 1, 48.0, 2.0),
            StopRecord {
                id: "p".into(),
                feed_id: "t".into(),
                raw_id: "p".into(),
                name: "Alpha Platform".into(),
                lat: Some(48.0),
                lon: Some(2.0),
                parent_id: Some("A".into()),
                location_type: 0,
                platform_code: Some("1".into()),
                wheelchair: 0,
                stop_code: None,
                stop_desc: None,
                level_id: None,
            zone_id: None,
            stop_url: None,
            stop_timezone: None,
            },
        ]);
        let all = search_stops_filtered(&e, "alpha", None, 10, false);
        assert_eq!(all.len(), 2);
        let stations = search_stops_filtered(&e, "alpha", None, 10, true);
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].name, "Alpha Station");
    }

    #[test]
    fn near_sorted_by_distance() {
        let e = epoch_with(vec![
            stop("far", "Far", 1, 48.90, 2.40),
            stop("near", "Near", 1, 48.8601, 2.3501),
        ]);
        let hits = near(&e, 48.86, 2.35, 50_000.0, 10, false);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0.name, "Near");
        assert!(hits[0].1 < hits[1].1);
    }
}
