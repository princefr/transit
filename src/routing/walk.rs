use crate::gtfs::pack::StaticEpoch;
use crate::link::geo::haversine_m;

/// Access/egress candidate: stop index, walk duration (s), walk distance (m).
#[derive(Debug, Clone)]
pub struct AccessStop {
    pub stop_idx: u32,
    pub duration_s: u32,
    pub distance_m: f64,
    /// Optional walk polyline as `(lat, lon)` when OSRM (or similar) succeeded.
    pub geometry: Option<Vec<(f64, f64)>>,
}

/// Walk duration and distance between two stops by index.
///
/// When multiple walk edges exist, picks the cheapest by
/// [`crate::gtfs::pack::WalkEdge::effective_duration_s`] (wheelchair penalizes stairs/escalators).
pub fn walk_duration_between(
    epoch: &StaticEpoch,
    from_idx: u32,
    to_idx: u32,
    walk_speed_m_s: f64,
    max_walk_m: f64,
) -> Option<(u32, f64)> {
    walk_duration_between_opts(epoch, from_idx, to_idx, walk_speed_m_s, max_walk_m, false)
}

/// Like [`walk_duration_between`] with wheelchair pathway costs.
pub fn walk_duration_between_opts(
    epoch: &StaticEpoch,
    from_idx: u32,
    to_idx: u32,
    walk_speed_m_s: f64,
    max_walk_m: f64,
    wheelchair: bool,
) -> Option<(u32, f64)> {
    if from_idx == to_idx {
        return Some((0, 0.0));
    }
    // Prefer cheapest explicit walk edge (respect pathway_mode when wheelchair).
    let mut best: Option<(u32, f64)> = None;
    for &ei in epoch
        .walk_adj
        .get(from_idx as usize)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
    {
        let e = &epoch.walk_edges[ei];
        if e.to_stop_idx == to_idx {
            let dur = e.effective_duration_s(wheelchair);
            if best.map(|(bd, _)| dur < bd).unwrap_or(true) {
                best = Some((dur, e.distance_m));
            }
        }
    }
    if best.is_some() {
        return best;
    }
    // Fallback straight-line if both have coords
    let a = epoch.stops.get(from_idx as usize)?;
    let b = epoch.stops.get(to_idx as usize)?;
    let (la, loa) = (a.lat?, a.lon?);
    let (lb, lob) = (b.lat?, b.lon?);
    let d = haversine_m(la, loa, lb, lob);
    if d > max_walk_m {
        return None;
    }
    let speed = walk_speed_m_s.max(0.1);
    let dur = (d / speed).ceil() as u32;
    Some((dur.max(30), d))
}

/// Duration/distance from a geo point to a stop (straight-line haversine).
pub fn walk_from_geo(
    epoch: &StaticEpoch,
    stop_idx: u32,
    lat: f64,
    lon: f64,
    walk_speed_m_s: f64,
    max_walk_m: f64,
) -> Option<(u32, f64)> {
    let s = epoch.stops.get(stop_idx as usize)?;
    let (slat, slon) = (s.lat?, s.lon?);
    let d = haversine_m(lat, lon, slat, slon);
    if d > max_walk_m {
        return None;
    }
    let speed = walk_speed_m_s.max(0.1);
    let dur = (d / speed).ceil() as u32;
    Some((dur.max(1), d))
}

/// OSRM routing profile for street-level paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OsrmProfile {
    #[default]
    Foot,
    Bike,
}

impl OsrmProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Foot => "foot",
            Self::Bike => "bike",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "bike" | "bicycle" | "cycling" | "velo" | "vélo" => Self::Bike,
            _ => Self::Foot,
        }
    }
}

/// OSRM route between two WGS84 points (`foot` or `bike` profile).
///
/// `base_url` e.g. `https://router.project-osrm.org` (no trailing slash).
/// Timeout 2.5s; soft-fail → `None` (caller uses haversine straight line).
///
/// Returns `(duration_s, distance_m, geometry as (lat, lon) pairs)`.
pub fn osrm_route(
    base_url: &str,
    profile: OsrmProfile,
    from_lat: f64,
    from_lon: f64,
    to_lat: f64,
    to_lon: f64,
) -> Option<(u32, f64, Vec<(f64, f64)>)> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    // OSRM path coordinates are lon,lat
    let url = format!(
        "{base}/route/v1/{profile}/{from_lon},{from_lat};{to_lon},{to_lat}?overview=full&geometries=geojson",
        profile = profile.as_str(),
    );

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(2500))
        .user_agent("transit-rs/0.1 (osrm walk/bike paths)")
        .build()
        .ok()?;

    let resp = client.get(&url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().ok()?;
    let code = body.get("code").and_then(|c| c.as_str()).unwrap_or("");
    if code != "Ok" {
        return None;
    }
    let route = body.get("routes")?.as_array()?.first()?;
    let duration = route.get("duration")?.as_f64()?;
    let distance = route.get("distance")?.as_f64()?;
    let coords = route
        .get("geometry")?
        .get("coordinates")?
        .as_array()?;
    let mut geometry = Vec::with_capacity(coords.len());
    for c in coords {
        let arr = c.as_array()?;
        let lon = arr.first()?.as_f64()?;
        let lat = arr.get(1)?.as_f64()?;
        geometry.push((lat, lon));
    }
    if geometry.is_empty() {
        return None;
    }
    let duration_s = duration.ceil().max(1.0) as u32;
    Some((duration_s, distance, geometry))
}

/// OSRM **foot** route (access / transfer walks).
pub fn osrm_foot_route(
    base_url: &str,
    from_lat: f64,
    from_lon: f64,
    to_lat: f64,
    to_lon: f64,
) -> Option<(u32, f64, Vec<(f64, f64)>)> {
    osrm_route(base_url, OsrmProfile::Foot, from_lat, from_lon, to_lat, to_lon)
}

/// OSRM **bike** route (access / pure bike legs).
pub fn osrm_bike_route(
    base_url: &str,
    from_lat: f64,
    from_lon: f64,
    to_lat: f64,
    to_lon: f64,
) -> Option<(u32, f64, Vec<(f64, f64)>)> {
    osrm_route(base_url, OsrmProfile::Bike, from_lat, from_lon, to_lat, to_lon)
}

/// Optionally refine a haversine access candidate with OSRM (fail soft).
fn refine_with_osrm(
    osrm_url: Option<&str>,
    profile: OsrmProfile,
    from_lat: f64,
    from_lon: f64,
    stop_lat: f64,
    stop_lon: f64,
    haversine: AccessStop,
) -> AccessStop {
    let Some(url) = osrm_url.map(str::trim).filter(|u| !u.is_empty()) else {
        return haversine;
    };
    match osrm_route(url, profile, from_lat, from_lon, stop_lat, stop_lon) {
        Some((dur, dist, geom)) => AccessStop {
            stop_idx: haversine.stop_idx,
            duration_s: dur,
            distance_m: dist,
            geometry: Some(geom),
        },
        None => haversine,
    }
}

/// Fill street-level polylines on walk/bike legs that only have endpoints.
///
/// - Transfers (middle walk legs): always **foot**
/// - First leg: `from_profile` (foot or bike)
/// - Last leg: `to_profile` (foot or bike)
///
/// Caps external HTTP calls per journey.
pub fn enrich_non_transit_paths(
    journey: &mut crate::routing::journey::Journey,
    osrm_url: Option<&str>,
    from_profile: OsrmProfile,
    to_profile: OsrmProfile,
) {
    let Some(base) = osrm_url.map(str::trim).filter(|u| !u.is_empty()) else {
        return;
    };
    const MAX_CALLS: usize = 10;
    let mut calls = 0usize;
    let n_legs = journey.legs.len();
    for (i, leg) in journey.legs.iter_mut().enumerate() {
        if calls >= MAX_CALLS {
            break;
        }
        let crate::routing::journey::Leg::Walk(w) = leg else {
            continue;
        };
        if w.geometry.len() >= 3 {
            continue; // already a path
        }
        let (Some(fla), Some(flo), Some(tla), Some(tlo)) =
            (w.from_lat, w.from_lon, w.to_lat, w.to_lon)
        else {
            continue;
        };
        let profile = if i == 0 {
            from_profile
        } else if i + 1 == n_legs {
            to_profile
        } else {
            OsrmProfile::Foot
        };
        // Station transfers < 80 m: straight line is fine
        if w.distance_m < 80.0 && profile == OsrmProfile::Foot {
            continue;
        }
        if let Some((dur, dist, geom)) = osrm_route(base, profile, fla, flo, tla, tlo) {
            w.geometry = geom;
            if dist > 0.0 {
                w.distance_m = dist;
            }
            if profile == OsrmProfile::Bike || w.duration_s == 0 {
                w.duration_s = dur;
            }
            if profile == OsrmProfile::Bike {
                w.mode = "BIKE".into();
            } else if w.mode.is_empty() || w.mode.eq_ignore_ascii_case("BIKE") {
                // Don't leave BIKE on a foot transfer after re-enrich
                if profile == OsrmProfile::Foot && i != 0 && i + 1 != n_legs {
                    w.mode = "WALK".into();
                }
            }
            calls += 1;
        }
    }
}

/// Normalize a station name for clustering (« Gare d'Aulnay-sous-Bois » ≈ « Aulnay-sous-Bois »).
pub fn place_name_key(name: &str) -> String {
    let mut s = crate::search::strip_accents(name).to_lowercase();
    for prefix in [
        "gare de ",
        "gare d'",
        "gare d’",
        "station ",
        "arret ",
        "arrêt ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    // Drop mode suffixes often in GTFS names
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

pub fn is_idfm_place_pub(s: &crate::gtfs::pack::StopRecord) -> bool {
    let id = s.raw_id.to_ascii_lowercase();
    id.contains("monomodalstopplace")
        || id.contains("multimodalstopplace")
        || (id.contains("stopplace") && !id.contains("quay") && !id.contains("entrance"))
}

fn is_idfm_place_stop(s: &crate::gtfs::pack::StopRecord) -> bool {
    is_idfm_place_pub(s)
}

pub fn has_departures_pub(epoch: &StaticEpoch, idx: usize) -> bool {
    epoch
        .stop_departures
        .get(idx)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

fn has_departures(epoch: &StaticEpoch, idx: usize) -> bool {
    has_departures_pub(epoch, idx)
}

/// Expand a user-picked stop into all boardable monomodals / quays for that place.
/// End users pick « Aulnay-sous-Bois » or « Châtelet »; routing needs every RER/Métro shell.
/// Memoized place-cluster expansions. Station-shell expansion scans the whole
/// place-name index per query (~8 ms on IDFM); the result only depends on
/// (epoch, stop, walk budget), so cache it keyed by epoch id.
fn cached_place_access(
    epoch: &StaticEpoch,
    idx: u32,
    max_walk_m: f64,
    walk_speed_m_s: f64,
    limit: usize,
) -> Vec<AccessStop> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(String, u32, u64, u64, u32), Vec<AccessStop>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (
        epoch.id.clone(),
        idx,
        max_walk_m.to_bits(),
        walk_speed_m_s.to_bits(),
        limit as u32,
    );
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(&key) {
            return hit.clone();
        }
    }
    let out = expand_place_access(epoch, idx, max_walk_m, walk_speed_m_s, limit);
    if let Ok(mut map) = cache.lock() {
        if map.len() > 20_000 {
            map.clear(); // epoch rotation guard
        }
        map.insert(key, out.clone());
    }
    out
}

fn expand_place_access(
    epoch: &StaticEpoch,
    seed_idx: u32,
    max_walk_m: f64,
    walk_speed_m_s: f64,
    limit: usize,
) -> Vec<AccessStop> {
    let seed = &epoch.stops[seed_idx as usize];
    let mut out: Vec<AccessStop> = Vec::new();
    let mut push = |idx: u32, dur: u32, dist: f64| {
        if out.iter().any(|a| a.stop_idx == idx) {
            return;
        }
        if out.len() >= limit {
            return;
        }
        out.push(AccessStop {
            stop_idx: idx,
            duration_s: dur,
            distance_m: dist,
            geometry: None,
        });
    };

    push(seed_idx, 0, 0.0);
    let key = place_name_key(&seed.name);
    let seed_lat = seed.lat;
    let seed_lon = seed.lon;
    let speed = walk_speed_m_s.max(0.1);
    // Radius for "same complex" monomodals (Châtelet / Les Halles / Gare du Nord)
    let cluster_m = max_walk_m.clamp(600.0, 1200.0);

    // 1) Same-name monomodals / boardable stops (station complex by name)
    if key.len() >= 3 {
        let mut candidates: Vec<u32> = epoch
            .place_name_index
            .get(&key)
            .cloned()
            .unwrap_or_default();
        // Prefix / contains matches for variants (« Gare de Lyon » vs « Lyon Part-Dieu »)
        if candidates.len() < limit {
            // Collect matching keys first and sort them — HashMap iteration
            // order is randomly seeded per process, which made the candidate
            // set (and thus journeys) vary between runs when the result was
            // truncated by `limit`.
            let mut matched_keys: Vec<&String> = epoch
                .place_name_index
                .keys()
                .filter(|k| {
                    let k: &String = k;
                    k != &key
                        && (k.starts_with(&key)
                            || key.starts_with(k.as_str())
                            || (key.len() >= 5 && k.contains(key.as_str()))
                            || (k.len() >= 5 && key.contains(k.as_str())))
                })
                .collect();
            matched_keys.sort();
            for k in matched_keys {
                candidates.extend(epoch.place_name_index[k].iter().copied());
            }
        }
        for i in candidates {
            if i == seed_idx {
                continue;
            }
            let s = &epoch.stops[i as usize];
            if !has_departures(epoch, i as usize) && !is_idfm_place_stop(s) {
                continue;
            }
            let dist = match (seed_lat, seed_lon, s.lat, s.lon) {
                (Some(a), Some(b), Some(c), Some(d)) => {
                    crate::link::geo::haversine_m(a, b, c, d)
                }
                _ => 80.0,
            };
            if dist > 8_000.0 {
                continue;
            }
            let dur = if dist < 1.0 {
                0
            } else {
                (dist / speed).ceil() as u32
            };
            let dur = if dist < 250.0 { dur.min(90) } else { dur.max(30) };
            push(i, dur, dist);
        }
    }

    // 2) Nearby boardable + monomodals (geographic complex)
    if let (Some(lat), Some(lon)) = (seed_lat, seed_lon) {
        let points = epoch.geo_boardable_points_view();
        let near = crate::link::geo::nearby(&points, lat, lon, cluster_m, limit.saturating_mul(2));
        for (i, dist) in near {
            if i as u32 == seed_idx {
                continue;
            }
            let dur = (dist / speed).ceil() as u32;
            push(i as u32, dur.max(30), dist);
        }
    }

    // 3) GTFS parent/child siblings
    let stop = &epoch.stops[seed_idx as usize];
    if let Some(ref p) = stop.parent_id {
        if let Some(children) = epoch.children_by_parent.get(p) {
            for &i in children {
                let s = &epoch.stops[i as usize];
                if s.parent_id.as_deref() == Some(p.as_str()) || s.id == *p {
                    if has_departures(epoch, i as usize) || is_idfm_place_stop(s) {
                        push(i, 60, 50.0);
                    }
                }
            }
        }
    }
    if let Some(children) = epoch.children_by_parent.get(&stop.id) {
        for &i in children {
            if epoch.stops[i as usize].parent_id.as_deref() == Some(&stop.id)
                && has_departures(epoch, i as usize)
            {
                push(i, 60, 50.0);
            }
        }
    }

    // Prefer boardable monomodals first for RAPTOR efficiency
    out.sort_by(|a, b| {
        let score = |idx: u32| {
            let s = &epoch.stops[idx as usize];
            let mut sc = 0i32;
            if is_idfm_place_stop(s) {
                sc += 100;
            }
            if has_departures(epoch, idx as usize) {
                sc += 50;
            }
            sc
        };
        score(b.stop_idx)
            .cmp(&score(a.stop_idx))
            .then_with(|| a.duration_s.cmp(&b.duration_s))
    });
    out.truncate(limit);
    out.dedup_by_key(|x| x.stop_idx);
    out
}

/// Expand origin or destination: stop id and/or geo snap.
///
/// When the user selects any station / monomodal / quay, we **expand to the whole place
/// cluster** (all monomodals + boardable stops for that city/station). End users never
/// need to pick a platform.
///
/// When `lat`/`lon` are provided (door-to-door), snaps to nearby stops via haversine.
pub fn resolve_access_stops(
    epoch: &StaticEpoch,
    stop_id: Option<&str>,
    lat: Option<f64>,
    lon: Option<f64>,
    max_walk_m: f64,
    walk_speed_m_s: f64,
    limit: usize,
    osrm_url: Option<&str>,
) -> Vec<AccessStop> {
    resolve_access_stops_profile(
        epoch,
        stop_id,
        lat,
        lon,
        max_walk_m,
        walk_speed_m_s,
        limit,
        osrm_url,
        OsrmProfile::Foot,
    )
}

/// Like [`resolve_access_stops`] with an explicit OSRM profile (foot / bike).
pub fn resolve_access_stops_profile(
    epoch: &StaticEpoch,
    stop_id: Option<&str>,
    lat: Option<f64>,
    lon: Option<f64>,
    max_walk_m: f64,
    walk_speed_m_s: f64,
    limit: usize,
    osrm_url: Option<&str>,
    profile: OsrmProfile,
) -> Vec<AccessStop> {
    if let Some(id) = stop_id {
        if let Some(&idx) = epoch.stop_id_to_idx.get(id) {
            let seed = &epoch.stops[idx as usize];
            // Full place-cluster expansion only for real station shells (IDFM monomodal etc.).
            // Plain GTFS quays keep classic expansion so unit fixtures still transfer at B.
            if is_idfm_place_stop(seed) || seed.location_type == 1 {
                let cap = limit.max(24).min(64);
                return cached_place_access(epoch, idx, max_walk_m, walk_speed_m_s, cap);
            }
            // Classic: seed + parent/child siblings — via the precomputed
            // children_by_parent index (linear scans over 48k stops cost ~10ms).
            let mut out = vec![AccessStop {
                stop_idx: idx,
                duration_s: 0,
                distance_m: 0.0,
                geometry: None,
            }];
            let stop = &epoch.stops[idx as usize];
            if let Some(ref p) = stop.parent_id {
                if let Some(siblings) = epoch.children_by_parent.get(p.as_str()) {
                    for &i in siblings {
                        if i != idx {
                            out.push(AccessStop {
                                stop_idx: i,
                                duration_s: 60,
                                distance_m: 50.0,
                                geometry: None,
                            });
                        }
                    }
                }
                // The parent station record itself (id == parent_id), when present.
                if let Some(&pi) = epoch.stop_id_to_idx.get(p.as_str()) {
                    if pi != idx {
                        out.push(AccessStop {
                            stop_idx: pi,
                            duration_s: 60,
                            distance_m: 50.0,
                            geometry: None,
                        });
                    }
                }
            }
            if let Some(children) = epoch.children_by_parent.get(stop.id.as_str()) {
                for &i in children {
                    if i != idx {
                        out.push(AccessStop {
                            stop_idx: i,
                            duration_s: 60,
                            distance_m: 50.0,
                            geometry: None,
                        });
                    }
                }
            }
            out.truncate(limit.max(8));
            return out;
        }
    }
    if let (Some(lat), Some(lon)) = (lat, lon) {
        // Borrowed when the epoch was packed (production); cloned only in tests.
        let owned;
        let points: &[(usize, f64, f64)] = match epoch.geo_stop_points_ref() {
            Some(p) => p,
            None => {
                owned = epoch.geo_stop_points_view();
                &owned
            }
        };
        let near = crate::link::geo::nearby(points, lat, lon, max_walk_m, limit);
        let speed = walk_speed_m_s.max(0.1);
        // Cap OSRM calls: refine at most the nearest few candidates.
        const OSRM_REFINE_LIMIT: usize = 6;
        return near
            .into_iter()
            .enumerate()
            .map(|(rank, (idx, dist))| {
                let dur = (dist / speed).ceil() as u32;
                let base = AccessStop {
                    stop_idx: idx as u32,
                    duration_s: dur.max(1),
                    distance_m: dist,
                    geometry: None,
                };
                if rank >= OSRM_REFINE_LIMIT {
                    return base;
                }
                let s = &epoch.stops[idx];
                let (slat, slon) = match (s.lat, s.lon) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return base,
                };
                refine_with_osrm(osrm_url, profile, lat, lon, slat, slon, base)
            })
            .collect();
    }
    Vec::new()
}

/// Legacy helper: (stop_idx, walk_seconds) for callers that only need duration.
pub fn resolve_access_stops_durations(
    epoch: &StaticEpoch,
    stop_id: Option<&str>,
    lat: Option<f64>,
    lon: Option<f64>,
    max_walk_m: f64,
    walk_speed_m_s: f64,
    limit: usize,
) -> Vec<(u32, u32)> {
    resolve_access_stops(
        epoch,
        stop_id,
        lat,
        lon,
        max_walk_m,
        walk_speed_m_s,
        limit,
        None,
    )
    .into_iter()
    .map(|a| (a.stop_idx, a.duration_s))
    .collect()
}

/// Polyline for a walk leg: prefer OSRM geometry, else straight endpoints.
pub fn walk_leg_geometry(
    geometry: Option<&[(f64, f64)]>,
    from_lat: Option<f64>,
    from_lon: Option<f64>,
    to_lat: Option<f64>,
    to_lon: Option<f64>,
) -> Vec<(f64, f64)> {
    if let Some(g) = geometry {
        if g.len() >= 2 {
            return g.to_vec();
        }
    }
    let mut out = Vec::with_capacity(2);
    if let (Some(lat), Some(lon)) = (from_lat, from_lon) {
        out.push((lat, lon));
    }
    if let (Some(lat), Some(lon)) = (to_lat, to_lon) {
        if out
            .last()
            .map(|&(a, b)| (a - lat).abs() > 1e-12 || (b - lon).abs() > 1e-12)
            .unwrap_or(true)
        {
            out.push((lat, lon));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::pack::StopRecord;
    use std::collections::HashMap;

    fn stop(id: &str, lat: f64, lon: f64) -> StopRecord {
        StopRecord {
            id: id.into(),
            feed_id: "t".into(),
            raw_id: id.into(),
            name: id.into(),
            lat: Some(lat),
            lon: Some(lon),
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
        }
    }

    fn tiny_epoch() -> StaticEpoch {
        let mut epoch = StaticEpoch::empty();
        epoch.stops = vec![
            stop("t:a", 48.8566, 2.3522), // Paris centre-ish
            stop("t:b", 48.8600, 2.3400),
        ];
        epoch.stop_id_to_idx = HashMap::from([("t:a".into(), 0), ("t:b".into(), 1)]);
        epoch
    }

    #[test]
    fn haversine_geo_access_without_osrm() {
        let epoch = tiny_epoch();
        // Point near stop a (~100m-ish)
        let access = resolve_access_stops(
            &epoch,
            None,
            Some(48.8560),
            Some(2.3520),
            2000.0,
            1.2,
            8,
            None, // no OSRM — pure haversine
        );
        assert!(!access.is_empty());
        assert_eq!(access[0].stop_idx, 0);
        assert!(access[0].duration_s >= 1);
        assert!(access[0].distance_m > 0.0);
        assert!(access[0].geometry.is_none());
    }

    #[test]
    fn walk_from_geo_haversine_fallback() {
        let epoch = tiny_epoch();
        let (dur, dist) = walk_from_geo(&epoch, 0, 48.8560, 2.3520, 1.2, 2000.0).unwrap();
        assert!(dur >= 1);
        assert!(dist > 0.0 && dist < 200.0);
    }

    #[test]
    fn empty_osrm_url_skips_http() {
        // Empty / whitespace URL must not attempt network.
        assert!(osrm_foot_route("", 48.85, 2.35, 48.86, 2.36).is_none());
        assert!(osrm_foot_route("   ", 48.85, 2.35, 48.86, 2.36).is_none());
        assert!(osrm_bike_route("", 48.85, 2.35, 48.86, 2.36).is_none());
    }

    #[test]
    fn walk_leg_geometry_endpoints_fallback() {
        let g = walk_leg_geometry(None, Some(1.0), Some(2.0), Some(3.0), Some(4.0));
        assert_eq!(g, vec![(1.0, 2.0), (3.0, 4.0)]);
        let g2 = walk_leg_geometry(Some(&[(5.0, 6.0), (7.0, 8.0)]), Some(1.0), Some(2.0), None, None);
        assert_eq!(g2, vec![(5.0, 6.0), (7.0, 8.0)]);
    }

    #[test]
    fn stop_id_access_zero_walk() {
        let epoch = tiny_epoch();
        let access = resolve_access_stops(
            &epoch,
            Some("t:a"),
            None,
            None,
            2000.0,
            1.2,
            8,
            None,
        );
        assert_eq!(access.len(), 1);
        assert_eq!(access[0].duration_s, 0);
        assert!(access[0].geometry.is_none());
    }
}
