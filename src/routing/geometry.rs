//! Polyline helpers for map itineraries.
//!
//! # Coordinate convention
//!
//! All geometry returned by this module uses **`[lat, lon]`** order (Leaflet /
//! Google Maps style), **not** GeoJSON `[lon, lat]`.
//!
//! `TransitLegData.geometry` stores the same as `Vec<(f64, f64)>` where each
//! pair is `(lat, lon)`. GraphQL / WASM agents should expose this as
//! `[[lat, lon], ...]` or a list of `{lat, lon}` objects and document the order
//! for map clients.

use crate::gtfs::pack::{PackedStopTime, StopRecord, TripRecord, StaticEpoch};
use crate::link::geo::haversine_m;
use crate::routing::journey::Leg;
use std::collections::HashMap;

/// Full shape polyline for a namespaced `shape_id`.
///
/// Points are **`[lat, lon]`** (Leaflet order). Empty if the shape is missing.
pub fn shape_polyline(epoch: &StaticEpoch, shape_id: &str) -> Vec<[f64; 2]> {
    epoch
        .shapes
        .get(shape_id)
        .map(|pts| pts.iter().map(|&(lat, lon)| [lat, lon]).collect())
        .unwrap_or_default()
}

/// Cumulative haversine distance along a shape polyline (meters).
pub fn shape_cumulative_distances(shape: &[(f64, f64)]) -> Vec<f64> {
    if shape.is_empty() {
        return Vec::new();
    }
    let mut cum = vec![0.0];
    for i in 0..shape.len().saturating_sub(1) {
        let d = haversine_m(shape[i].0, shape[i].1, shape[i + 1].0, shape[i + 1].1);
        cum.push(cum.last().unwrap() + d);
    }
    cum
}

/// Interpolate lat/lon/bearing at `frac` between two shape-distance marks (meters).
///
/// Handles reverse travel when `d0 > d1`. Returns `None` when distances are
/// degenerate or off the polyline span.
pub fn interpolate_shape_by_dist(
    shape: &[(f64, f64)],
    d0: f32,
    d1: f32,
    frac: f64,
) -> Option<(f64, f64, f64)> {
    if shape.len() < 2 {
        return None;
    }
    let cum = shape_cumulative_distances(shape);
    let frac = frac.clamp(0.0, 1.0);
    let (lo, hi, f) = if d0 <= d1 {
        (d0 as f64, d1 as f64, frac)
    } else {
        (d1 as f64, d0 as f64, 1.0 - frac)
    };
    if (hi - lo).abs() < 0.5 {
        return None;
    }
    let target = lo + (hi - lo) * f;
    for i in 0..cum.len().saturating_sub(1) {
        if target >= cum[i] && target <= cum[i + 1] + 1e-6 {
            let seg = (cum[i + 1] - cum[i]).max(1e-6);
            let t = ((target - cum[i]) / seg).clamp(0.0, 1.0);
            let p0 = shape[i];
            let p1 = shape[i + 1];
            let lat = p0.0 + (p1.0 - p0.0) * t;
            let lon = p0.1 + (p1.1 - p0.1) * t;
            let brg = bearing_deg(p0.0, p0.1, p1.0, p1.1);
            return Some((lat, lon, brg));
        }
    }
    let n = shape.len();
    let last = shape[n - 1];
    let prev = shape[n - 2];
    Some((last.0, last.1, bearing_deg(prev.0, prev.1, last.0, last.1)))
}

fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let phi1 = lat1.to_radians();
    let phi2 = lat2.to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let y = dlon.sin() * phi2.cos();
    let x = phi1.cos() * phi2.sin() - phi1.sin() * phi2.cos() * dlon.cos();
    let theta = y.atan2(x).to_degrees();
    (theta + 360.0) % 360.0
}

/// Fill missing `shape_dist_traveled` from packed shapes + stop coordinates.
///
/// IDFM GTFS often omits the column; shape-based vehicle estimation needs these
/// distances for accurate arc-length interpolation between stops.
pub fn backfill_shape_dist_traveled(
    trips: &[TripRecord],
    stop_times: &mut [PackedStopTime],
    stops: &[StopRecord],
    shapes: &HashMap<String, Vec<(f64, f64)>>,
) {
    for trip in trips {
        if trip.stop_time_len == 0 {
            continue;
        }
        let shape_id = match &trip.shape_id {
            Some(s) => s,
            None => continue,
        };
        let shape = match shapes.get(shape_id) {
            Some(s) if s.len() >= 2 => s.as_slice(),
            _ => continue,
        };
        let cum = shape_cumulative_distances(shape);
        for off in 0..trip.stop_time_len {
            let idx = (trip.stop_time_start + off) as usize;
            let st = &mut stop_times[idx];
            if st.shape_dist_traveled.is_some() {
                continue;
            }
            let stop = match stops.get(st.stop_idx as usize) {
                Some(s) => s,
                None => continue,
            };
            let (lat, lon) = match (stop.lat, stop.lon) {
                (Some(la), Some(lo)) => (la, lo),
                _ => continue,
            };
            let ni = nearest_shape_index(shape, lat, lon);
            st.shape_dist_traveled = Some(cum[ni] as f32);
        }
    }
}

fn nearest_shape_index(shape: &[(f64, f64)], lat: f64, lon: f64) -> usize {
    let mut best_i = 0usize;
    let mut best_d = f64::INFINITY;
    for (i, &(slat, slon)) in shape.iter().enumerate() {
        let d = haversine_m(lat, lon, slat, slon);
        if d < best_d {
            best_d = d;
            best_i = i;
        }
    }
    best_i
}

/// Board→alight geometry for a transit leg.
///
/// `from_stop_off` / `to_stop_off` are **offsets within the trip's stop_times**
/// (same as RAPTOR board_off / alight_off).
///
/// Prefer a slice of the trip's GTFS shape (nearest shape points to board/alight
/// stop coordinates). If slicing is degenerate or no shape exists, fall back to
/// straight segments through stop coordinates between board and alight (inclusive).
///
/// Points are **`[lat, lon]`**.
pub fn leg_geometry(
    epoch: &StaticEpoch,
    trip_id: &str,
    from_stop_off: u32,
    to_stop_off: u32,
) -> Vec<[f64; 2]> {
    let Some(&trip_i) = epoch.trip_id_to_idx.get(trip_id) else {
        return Vec::new();
    };
    let trip = &epoch.trips[trip_i as usize];
    if trip.stop_time_len == 0 && trip.shape_id.is_none() {
        return Vec::new();
    }
    // When stop_time_len is 0 but shape exists, still return shape below.
    let (from_off, to_off) = if trip.stop_time_len == 0 {
        (0u32, 0u32)
    } else {
        let a = from_stop_off.min(trip.stop_time_len - 1);
        let b = to_stop_off.min(trip.stop_time_len - 1);
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    };

    let board_stop = if trip.stop_time_len > 0 {
        let st = &epoch.stop_times[(trip.stop_time_start + from_off) as usize];
        epoch.stops.get(st.stop_idx as usize)
    } else {
        None
    };
    let alight_stop = if trip.stop_time_len > 0 {
        let st = &epoch.stop_times[(trip.stop_time_start + to_off) as usize];
        epoch.stops.get(st.stop_idx as usize)
    } else {
        None
    };

    if let Some(ref shape_id) = trip.shape_id {
        if let Some(shape) = epoch.shapes.get(shape_id) {
            if shape.len() >= 2 {
                if let (Some(bs), Some(as_)) = (board_stop, alight_stop) {
                    if let (Some(blat), Some(blon), Some(alat), Some(alon)) =
                        (bs.lat, bs.lon, as_.lat, as_.lon)
                    {
                        let mut i0 = nearest_shape_index(shape, blat, blon);
                        let mut i1 = nearest_shape_index(shape, alat, alon);
                        if i0 > i1 {
                            std::mem::swap(&mut i0, &mut i1);
                        }
                        let slice = &shape[i0..=i1];
                        if slice.len() >= 2 {
                            return slice.iter().map(|&(lat, lon)| [lat, lon]).collect();
                        }
                        // Degenerate slice: use full shape.
                        return shape.iter().map(|&(lat, lon)| [lat, lon]).collect();
                    }
                }
                // Missing stop coords or no stop_times: full shape.
                return shape.iter().map(|&(lat, lon)| [lat, lon]).collect();
            }
            if shape.len() == 1 {
                return vec![[shape[0].0, shape[0].1]];
            }
        }
    }

    // Fallback: polyline through stop coords between board and alight.
    let mut out = Vec::new();
    if trip.stop_time_len == 0 {
        return out;
    }
    for off in from_off..=to_off {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        if let Some(s) = epoch.stops.get(st.stop_idx as usize) {
            if let (Some(lat), Some(lon)) = (s.lat, s.lon) {
                if out
                    .last()
                    .map(|p: &[f64; 2]| (p[0] - lat).abs() > 1e-9 || (p[1] - lon).abs() > 1e-9)
                    .unwrap_or(true)
                {
                    out.push([lat, lon]);
                }
            }
        }
    }
    out
}

/// Stitch geometry across journey legs (walk endpoints + transit polylines).
///
/// Points are **`[lat, lon]`**.
pub fn journey_geometry(legs: &[Leg]) -> Vec<[f64; 2]> {
    let mut out = Vec::new();
    for leg in legs {
        match leg {
            Leg::Transit(t) => {
                for &(lat, lon) in &t.geometry {
                    if out.last() != Some(&[lat, lon]) {
                        out.push([lat, lon]);
                    }
                }
            }
            Leg::Walk(w) => {
                if w.geometry.len() >= 2 {
                    for &(lat, lon) in &w.geometry {
                        if out.last() != Some(&[lat, lon]) {
                            out.push([lat, lon]);
                        }
                    }
                } else {
                    if let (Some(lat), Some(lon)) = (w.from_lat, w.from_lon) {
                        if out.last() != Some(&[lat, lon]) {
                            out.push([lat, lon]);
                        }
                    }
                    if let (Some(lat), Some(lon)) = (w.to_lat, w.to_lon) {
                        if out.last() != Some(&[lat, lon]) {
                            out.push([lat, lon]);
                        }
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::pack::{GlobalTrip, PackedStopTime, RouteMode, StaticEpoch, StopRecord, TripRecord};
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

    fn base_trip(shape_id: Option<String>, stop_time_len: u32) -> GlobalTrip {
        GlobalTrip {
            id: "t:trip".into(),
            feed_id: "t".into(),
            route_id: "t:r".into(),
            service_id: "svc".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id,
            mode: RouteMode::Bus,
            route_short_name: "1".into(),
            route_long_name: "".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 3,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len,
            frequency_windows: vec![],
        }
    }

    #[test]
    fn stop_chain_fallback() {
        let mut epoch = StaticEpoch::empty();
        epoch.stops = vec![stop("t:a", 1.0, 2.0), stop("t:b", 3.0, 4.0)];
        epoch.stop_id_to_idx.insert("t:a".into(), 0);
        epoch.stop_id_to_idx.insert("t:b".into(), 1);
        epoch.stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 0,
                departure_s: 0,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 60,
                departure_s: 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
        ];
        epoch.trips = vec![base_trip(None, 2)];
        epoch.trip_id_to_idx.insert("t:trip".into(), 0);
        let geo = leg_geometry(&epoch, "t:trip", 0, 1);
        assert_eq!(geo, vec![[1.0, 2.0], [3.0, 4.0]]);
    }

    #[test]
    fn uses_packed_shape() {
        let mut epoch = StaticEpoch::empty();
        epoch.shapes = HashMap::from([("t:sh".into(), vec![(10.0, 20.0), (11.0, 21.0)])]);
        epoch.trips = vec![base_trip(Some("t:sh".into()), 0)];
        epoch.trip_id_to_idx.insert("t:trip".into(), 0);
        let geo = leg_geometry(&epoch, "t:trip", 0, 0);
        assert_eq!(geo, vec![[10.0, 20.0], [11.0, 21.0]]);
        assert_eq!(
            shape_polyline(&epoch, "t:sh"),
            vec![[10.0, 20.0], [11.0, 21.0]]
        );
    }

    #[test]
    fn backfill_shape_dist_from_geometry() {
        let shapes = HashMap::from([(
            "t:sh".into(),
            vec![(48.0, 2.0), (48.01, 2.0), (48.02, 2.0)],
        )]);
        let stops = vec![stop("t:a", 48.0, 2.0), stop("t:b", 48.02, 2.0)];
        let trips = vec![TripRecord {
            id: "t:trip".into(),
            feed_id: "t".into(),
            route_id: "t:r".into(),
            service_id: "svc".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: Some("t:sh".into()),
            stop_time_start: 0,
            stop_time_len: 2,
        }];
        let mut stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 0,
                departure_s: 0,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 60,
                departure_s: 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
        ];
        backfill_shape_dist_traveled(&trips, &mut stop_times, &stops, &shapes);
        assert!(stop_times[0].shape_dist_traveled.is_some());
        assert!(stop_times[1].shape_dist_traveled.is_some());
        assert!(
            stop_times[1].shape_dist_traveled.unwrap()
                > stop_times[0].shape_dist_traveled.unwrap()
        );
    }

    #[test]
    fn slices_shape_between_stops() {
        let mut epoch = StaticEpoch::empty();
        epoch.stops = vec![
            stop("t:a", 48.86, 2.35),
            stop("t:b", 45.76, 4.86),
        ];
        epoch.shapes.insert(
            "t:sh".into(),
            vec![
                (48.86, 2.35),
                (47.5, 3.0),
                (46.5, 4.0),
                (45.76, 4.86),
            ],
        );
        epoch.stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 0,
                departure_s: 0,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 100,
                departure_s: 100,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
        ];
        epoch.trips = vec![base_trip(Some("t:sh".into()), 2)];
        epoch.trip_id_to_idx.insert("t:trip".into(), 0);
        let geo = leg_geometry(&epoch, "t:trip", 0, 1);
        assert!(geo.len() >= 2);
        assert!((geo[0][0] - 48.86).abs() < 0.01);
        assert!((geo.last().unwrap()[0] - 45.76).abs() < 0.01);
    }
}
