pub mod geo;

use crate::gtfs::pack::{FeedStaticBundle, WalkEdge};
use geo::hub_links;
use std::sync::Arc;

/// Build cross-feed (and same-feed nearby) walk edges between stops.
pub fn link_feeds(bundles: &[Arc<FeedStaticBundle>], radius_m: f64, walk_speed_m_s: f64) -> Vec<WalkEdge> {
    // Prefer link_epoch_stops after merge (global indices).
    let _ = (bundles, radius_m, walk_speed_m_s);
    Vec::new()
}

/// Link station stops already in a flat list (global indices) via grid-indexed hub_links.
pub fn link_epoch_stops(
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

    let pairs = hub_links(&points, radius_m);
    let mut edges = Vec::new();
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

pub use geo::{haversine_m, nearby, GeoGrid};
