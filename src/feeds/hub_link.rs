//! Safer hub linking for multi-feed static epochs.
//! Uses a geographic grid to avoid O(n²) explosions; skips entirely above a station threshold.

use crate::gtfs::pack::WalkEdge;
use crate::link::haversine_m;
use std::collections::HashMap;
use tracing::{info, warn};

/// Soft advisory only — we no longer **skip** hub links for large networks.
/// Completeness of the walk graph is required for multi-mode RAPTOR (RER↔Métro).
pub const HUB_LINK_STATION_CAP: usize = 80_000;

/// Approximate cell size from radius (degrees). ~111km per degree latitude.
fn cell_deg(radius_m: f64) -> f64 {
    (radius_m * 1.25) / 111_320.0
}

fn cell_key(lat: f64, lon: f64, cell: f64) -> (i32, i32) {
    let i = (lat / cell).floor() as i32;
    let j = (lon / cell).floor() as i32;
    (i, j)
}

/// Grid-based pairing of stations within `radius_m`.
/// `points`: (global_stop_idx, lat, lon).
pub fn hub_links_grid(points: &[(usize, f64, f64)], radius_m: f64) -> Vec<(usize, usize, f64)> {
    if points.is_empty() || radius_m <= 0.0 {
        return Vec::new();
    }
    let cell = cell_deg(radius_m).max(1e-6);
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    for (pi, &(_, lat, lon)) in points.iter().enumerate() {
        grid.entry(cell_key(lat, lon, cell)).or_default().push(pi);
    }

    let mut out = Vec::new();
    for (pi, &(ia, la, loa)) in points.iter().enumerate() {
        let (ci, cj) = cell_key(la, loa, cell);
        for di in -1i32..=1 {
            for dj in -1i32..=1 {
                let Some(others) = grid.get(&(ci + di, cj + dj)) else {
                    continue;
                };
                for &pj in others {
                    if pj <= pi {
                        continue; // each unordered pair once
                    }
                    let (ib, lb, lob) = points[pj];
                    if ia == ib {
                        continue;
                    }
                    let d = haversine_m(la, loa, lb, lob);
                    if d <= radius_m && d > 1.0 {
                        let (x, y) = if ia < ib { (ia, ib) } else { (ib, ia) };
                        out.push((x, y, d));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    out
}

/// Build walk edges for hub links, or skip if too many stations.
pub fn link_epoch_stops_safe(
    stops: &[(Option<f64>, Option<f64>, bool)],
    radius_m: f64,
    walk_speed_m_s: f64,
    default_transfer_s: u32,
) -> Vec<WalkEdge> {
    let points: Vec<(usize, f64, f64)> = stops
        .iter()
        .enumerate()
        .filter_map(|(i, (lat, lon, is_station))| {
            let lat = (*lat)?;
            let lon = (*lon)?;
            if !is_station {
                return None;
            }
            Some((i, lat, lon))
        })
        .collect();

    if points.is_empty() {
        return Vec::new();
    }
    if points.len() > HUB_LINK_STATION_CAP {
        warn!(
            stations = points.len(),
            cap = HUB_LINK_STATION_CAP,
            "large hub set — still building grid links (graph completeness)"
        );
    }

    info!(stations = points.len(), radius_m, "hub link via grid index");
    let pairs = hub_links_grid(&points, radius_m);

    let mut edges = Vec::with_capacity(pairs.len().saturating_mul(2));
    for (a, b, dist) in pairs {
        let duration = ((dist / walk_speed_m_s).ceil() as u32).max(default_transfer_s.min(60));
        edges.push(WalkEdge {
            from_stop_idx: a as u32,
            to_stop_idx: b as u32,
            duration_s: duration,
            distance_m: dist,
            pathway_mode: None,
        });
        edges.push(WalkEdge {
            from_stop_idx: b as u32,
            to_stop_idx: a as u32,
            duration_s: duration,
            distance_m: dist,
            pathway_mode: None,
        });
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_close_stations_linked() {
        // ~55m apart near Paris
        let pts = vec![(0, 48.8500, 2.3500), (1, 48.8505, 2.3500)];
        let links = hub_links_grid(&pts, 250.0);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, 0);
        assert_eq!(links[0].1, 1);
    }

    #[test]
    fn far_stations_not_linked() {
        let pts = vec![(0, 48.85, 2.35), (1, 45.76, 4.86)];
        let links = hub_links_grid(&pts, 250.0);
        assert!(links.is_empty());
    }

    /// Above the station cap we still build links: the cap is advisory only —
    /// completeness of the walk graph is required for multi-mode RAPTOR.
    /// (Stations spread ~1.1km apart so the grid stays cheap; only the first
    /// two are within link radius.)
    #[test]
    fn cap_still_links_above_threshold() {
        let mut stops = Vec::new();
        for i in 0..HUB_LINK_STATION_CAP + 1 {
            let lat = 48.0 + (i as f64) * 0.01;
            stops.push((Some(lat), Some(2.0), true));
        }
        stops[1] = (Some(48.0005), Some(2.0), true); // ~55m from stops[0]
        let edges = link_epoch_stops_safe(&stops, 250.0, 1.2, 180);
        assert_eq!(
            edges.len(),
            2,
            "one bidirectional hub link pair expected above the cap"
        );
    }
}
