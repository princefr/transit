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

/// A stop counts as "on" the shape at the first vertex within this slack (metres) of
/// the best forward match — keeps loop / out-and-back shapes on the right pass.
const PROJECTION_SLACK_M: f64 = 25.0;

/// Index of the shape vertex matching (lat, lon), searching **forward** from `from`.
///
/// Picks the earliest vertex whose distance is within [`PROJECTION_SLACK_M`] of the best
/// forward distance, so a stop served on the outbound leg of an out-and-back shape is not
/// snapped to the (equally close) return leg.
pub fn forward_shape_index(shape: &[(f64, f64)], from: usize, lat: f64, lon: f64) -> usize {
    if shape.is_empty() {
        return 0;
    }
    let from = from.min(shape.len() - 1);
    let dists: Vec<f64> = shape[from..]
        .iter()
        .map(|&(slat, slon)| haversine_m(lat, lon, slat, slon))
        .collect();
    let best = dists.iter().cloned().fold(f64::INFINITY, f64::min);
    let pick = dists
        .iter()
        .position(|&d| d <= best + PROJECTION_SLACK_M)
        .unwrap_or(0);
    from + pick
}

/// Monotonic projection of a stop sequence onto a shape: one vertex index per stop
/// (`None` when the stop has no coordinates). Indices never decrease.
pub fn project_stops_monotonic(
    shape: &[(f64, f64)],
    coords: &[Option<(f64, f64)>],
) -> Vec<Option<usize>> {
    let mut cursor = 0usize;
    coords
        .iter()
        .map(|c| {
            let (lat, lon) = (*c)?;
            let i = forward_shape_index(shape, cursor, lat, lon);
            cursor = i;
            Some(i)
        })
        .collect()
}

/// Snap a measured metres-per-unit ratio to a common GTFS distance unit (m, km, ft, mi)
/// when within 25 %; otherwise keep the measured ratio.
fn snap_unit_scale(ratio: f64) -> f64 {
    const UNITS: [f64; 4] = [1.0, 1000.0, 0.3048, 1609.344];
    for u in UNITS {
        if ratio > 0.0 && (ratio / u - 1.0).abs() <= 0.25 {
            return u;
        }
    }
    ratio
}

/// Affine map `metres = a + b · d` for a trip's `shape_dist_traveled` values `d`.
///
/// GTFS leaves the unit to the producer (metres, km, feet, miles…). The scale is measured
/// from the trip's first and last stops carrying a value: their projections onto the shape
/// (metres of arc length) against their `d` span, then snapped to a common unit. Returns
/// `None` when fewer than two distinct values / projections are available.
pub fn shape_dist_affine(
    shape: &[(f64, f64)],
    cum: &[f64],
    samples: &[(f64, (f64, f64))],
) -> Option<(f64, f64)> {
    if shape.len() < 2 || samples.len() < 2 || cum.len() != shape.len() {
        return None;
    }
    let (d0, (lat0, lon0)) = samples[0];
    let (d1, (lat1, lon1)) = samples[samples.len() - 1];
    let i0 = forward_shape_index(shape, 0, lat0, lon0);
    // Last stop: search forward from the first stop's match.
    let i1 = forward_last_index(shape, i0, lat1, lon1);
    let (m0, m1) = (cum[i0], cum[i1]);
    if (d1 - d0).abs() < 1e-9 || (m1 - m0).abs() < 1.0 {
        return None;
    }
    let b = snap_unit_scale((m1 - m0) / (d1 - d0));
    if !(b.is_finite() && b > 0.0) {
        return None;
    }
    Some((m0 - b * d0, b))
}

/// Like [`forward_shape_index`] but prefers the **latest** near vertex (trip terminus on
/// a loop shape that starts and ends at the same place).
fn forward_last_index(shape: &[(f64, f64)], from: usize, lat: f64, lon: f64) -> usize {
    let from = from.min(shape.len().saturating_sub(1));
    let dists: Vec<f64> = shape[from..]
        .iter()
        .map(|&(slat, slon)| haversine_m(lat, lon, slat, slon))
        .collect();
    let best = dists.iter().cloned().fold(f64::INFINITY, f64::min);
    let pick = dists
        .iter()
        .rposition(|&d| d <= best + PROJECTION_SLACK_M)
        .unwrap_or(0);
    from + pick
}

/// `metres = a + b · shape_dist_traveled` for one packed trip (see [`shape_dist_affine`]).
pub fn trip_shape_dist_affine(
    epoch: &StaticEpoch,
    trip: &crate::gtfs::pack::GlobalTrip,
) -> Option<(f64, f64)> {
    let shape = epoch.shapes.get(trip.shape_id.as_ref()?)?;
    let cum = shape_cumulative_distances(shape);
    let mut first: Option<(f64, (f64, f64))> = None;
    let mut last: Option<(f64, (f64, f64))> = None;
    for off in 0..trip.stop_time_len {
        let st = epoch.stop_times.get((trip.stop_time_start + off) as usize)?;
        let Some(d) = st.shape_dist_traveled else {
            continue;
        };
        let Some(stop) = epoch.stops.get(st.stop_idx as usize) else {
            continue;
        };
        let (Some(lat), Some(lon)) = (stop.lat, stop.lon) else {
            continue;
        };
        let sample = (d as f64, (lat, lon));
        if first.is_none() {
            first = Some(sample);
        }
        last = Some(sample);
    }
    shape_dist_affine(shape, &cum, &[first?, last?])
}

/// Interpolate along a shape between two `shape_dist_traveled` marks expressed in the
/// feed's own unit, converting through the trip's affine unit map (`metres = a + b·d`).
pub fn interpolate_shape_by_feed_dist(
    shape: &[(f64, f64)],
    d0: f32,
    d1: f32,
    frac: f64,
    affine: (f64, f64),
) -> Option<(f64, f64, f64)> {
    let (a, b) = affine;
    let m0 = a + b * d0 as f64;
    let m1 = a + b * d1 as f64;
    interpolate_shape_by_dist(shape, m0 as f32, m1 as f32, frac)
}

/// Fill missing `shape_dist_traveled` from packed shapes + stop coordinates.
///
/// IDFM GTFS often omits the column; shape-based vehicle estimation needs these
/// distances for accurate arc-length interpolation between stops.
///
/// - Stops are projected onto the shape **monotonically** (forward search from the
///   previous stop), so loop and out-and-back shapes keep the right pass.
/// - Trips with no values get arc-length metres.
/// - Partially filled trips are completed **in the feed's own unit**: the unit is measured
///   from the provided values (see [`shape_dist_affine`]); when it cannot be measured the
///   gaps are left empty rather than mixing metres into km/ft values.
/// - Fully filled trips are left untouched.
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
        let start = trip.stop_time_start as usize;
        let end = start + trip.stop_time_len as usize;
        let Some(sts) = stop_times.get_mut(start..end) else {
            continue;
        };
        let missing = sts.iter().filter(|st| st.shape_dist_traveled.is_none()).count();
        if missing == 0 {
            continue;
        }
        let coords: Vec<Option<(f64, f64)>> = sts
            .iter()
            .map(|st| {
                let s = stops.get(st.stop_idx as usize)?;
                Some((s.lat?, s.lon?))
            })
            .collect();
        let cum = shape_cumulative_distances(shape);
        let proj = project_stops_monotonic(shape, &coords);

        // metres → feed unit for the values we write.
        let to_feed: Box<dyn Fn(f64) -> f64> = if missing == sts.len() {
            Box::new(|m| m)
        } else {
            let samples: Vec<(f64, (f64, f64))> = sts
                .iter()
                .zip(&coords)
                .filter_map(|(st, c)| Some((st.shape_dist_traveled? as f64, (*c)?)))
                .collect();
            match shape_dist_affine(shape, &cum, &samples) {
                Some((a, b)) => Box::new(move |m| (m - a) / b),
                None => continue,
            }
        };
        for (st, p) in sts.iter_mut().zip(proj) {
            if st.shape_dist_traveled.is_some() {
                continue;
            }
            if let Some(i) = p {
                st.shape_dist_traveled = Some(to_feed(cum[i]) as f32);
            }
        }
    }
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
                        // Monotonic projection of the trip's stops up to the
                        // alight stop, so loop / out-and-back shapes slice the
                        // pass actually travelled.
                        let _ = (blat, blon, alat, alon);
                        let coords: Vec<Option<(f64, f64)>> = (0..=to_off)
                            .map(|off| {
                                let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
                                let s = epoch.stops.get(st.stop_idx as usize)?;
                                Some((s.lat?, s.lon?))
                            })
                            .collect();
                        let proj = project_stops_monotonic(shape, &coords);
                        let (Some(mut i0), Some(mut i1)) =
                            (proj[from_off as usize], proj[to_off as usize])
                        else {
                            return shape.iter().map(|&(lat, lon)| [lat, lon]).collect();
                        };
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

    fn st(stop_idx: u32, seq: u16, dist: Option<f32>) -> PackedStopTime {
        PackedStopTime {
            stop_idx,
            arrival_s: seq as u32 * 60,
            departure_s: seq as u32 * 60,
            stop_sequence: seq.into(),
            pickup_type: 0,
            drop_off_type: 0,
            stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: dist,
        }
    }

    fn trip_rec(len: u32) -> TripRecord {
        TripRecord {
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
            stop_time_len: len,
        }
    }

    /// Straight north-south shape, 0.01° lat ≈ 1112 m per step.
    fn line_shape() -> Vec<(f64, f64)> {
        (0..=4).map(|i| (48.0 + 0.01 * i as f64, 2.0)).collect()
    }

    #[test]
    fn backfill_partial_trip_keeps_feed_unit_km() {
        let shapes = HashMap::from([("t:sh".to_string(), line_shape())]);
        let stops = vec![
            stop("t:a", 48.00, 2.0),
            stop("t:b", 48.02, 2.0),
            stop("t:c", 48.04, 2.0),
        ];
        let len_km = haversine_m(48.0, 2.0, 48.04, 2.0) as f32 / 1000.0;
        // Provided in km at both ends, missing in the middle.
        let mut sts = vec![st(0, 1, Some(0.0)), st(1, 2, None), st(2, 3, Some(len_km))];
        backfill_shape_dist_traveled(&[trip_rec(3)], &mut sts, &stops, &shapes);
        let mid = sts[1].shape_dist_traveled.expect("filled");
        // Half-way, in km (≈ 2.22), not metres (≈ 2224).
        assert!((mid - len_km / 2.0).abs() < 0.05, "mid={mid} len_km={len_km}");
    }

    #[test]
    fn backfill_out_and_back_is_monotonic() {
        // Out to the north and back: stops A (start), B (far end), A' (back at start).
        let mut shape = line_shape();
        let mut back = line_shape();
        back.reverse();
        shape.extend(back.into_iter().skip(1));
        let shapes = HashMap::from([("t:sh".to_string(), shape)]);
        let stops = vec![
            stop("t:a", 48.00, 2.0),
            stop("t:m", 48.02, 2.0),
            stop("t:b", 48.04, 2.0),
            stop("t:m2", 48.02, 2.0),
            stop("t:a2", 48.00, 2.0),
        ];
        let mut sts: Vec<PackedStopTime> = (0..5).map(|i| st(i, i as u16 + 1, None)).collect();
        backfill_shape_dist_traveled(&[trip_rec(5)], &mut sts, &stops, &shapes);
        let d: Vec<f32> = sts.iter().map(|s| s.shape_dist_traveled.unwrap()).collect();
        for w in d.windows(2) {
            assert!(w[1] > w[0], "not monotonic: {d:?}");
        }
        // Return-leg midpoint is 3/4 of the way, not snapped to the outbound pass.
        assert!((d[3] / d[4] - 0.75).abs() < 0.01, "{d:?}");
    }

    #[test]
    fn affine_detects_km_and_converts() {
        let shape = line_shape();
        let cum = shape_cumulative_distances(&shape);
        let total = *cum.last().unwrap();
        let samples = [(0.0, (48.0, 2.0)), (total / 1000.0, (48.04, 2.0))];
        let (a, b) = shape_dist_affine(&shape, &cum, &samples).unwrap();
        assert_eq!(b, 1000.0);
        assert!(a.abs() < 1e-6);
        // Interpolating at half-way with km marks lands on the middle vertex.
        let (lat, _, _) =
            interpolate_shape_by_feed_dist(&shape, 0.0, (total / 1000.0) as f32, 0.5, (a, b))
                .unwrap();
        assert!((lat - 48.02).abs() < 1e-3, "lat={lat}");
    }

    #[test]
    fn leg_geometry_on_loop_slices_travelled_pass() {
        let mut epoch = StaticEpoch::empty();
        let mut shape = line_shape();
        let mut back = line_shape();
        back.reverse();
        shape.extend(back.into_iter().skip(1));
        epoch.shapes.insert("t:sh".into(), shape);
        epoch.stops = vec![
            stop("t:a", 48.00, 2.0),
            stop("t:b", 48.04, 2.0),
            stop("t:m", 48.02, 2.0),
        ];
        epoch.stop_times = vec![st(0, 1, None), st(1, 2, None), st(2, 3, None)];
        epoch.trips = vec![base_trip(Some("t:sh".into()), 3)];
        epoch.trip_id_to_idx.insert("t:trip".into(), 0);
        // Board at the far end B, alight at M on the way back: the slice must be the
        // return pass (B → M), i.e. 3 vertices going south.
        let geo = leg_geometry(&epoch, "t:trip", 1, 2);
        assert_eq!(geo.len(), 3, "{geo:?}");
        assert!((geo[0][0] - 48.04).abs() < 1e-9);
        assert!((geo[2][0] - 48.02).abs() < 1e-9);
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
