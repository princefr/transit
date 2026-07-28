//! Estimate vehicle geolocation from SIRI SM / ET trip updates when true GPS is absent.
//!
//! ## Idea
//!
//! Stop Monitoring does not publish continuous AVL, but each visit carries
//! **expected arrival/departure** at a stop. Combined with GTFS stop sequence
//! (and optional shapes), we can place a **virtual** marker:
//!
//! 1. **At stop** — `now` is between expected arrival and departure (or only one
//!    known call with ETA already passed and departure still ahead).
//! 2. **In transit** — `now` is between previous stop’s departure and next
//!    stop’s arrival; interpolate by **arc length** along the GTFS shape
//!    (preferred) or the full intermediate **stop-chain** polyline (not
//!    crow-flies between endpoints only).
//! 3. **Approaching (single SM call)** — only the monitored stop is known;
//!    use the previous static stop + remaining time vs scheduled run time.
//!
//! Status is always `ESTIMATED_*` (not GPS): `ESTIMATED_STOPPED_AT`,
//! `ESTIMATED_INCOMING_AT`, `ESTIMATED_ON_SHAPE`, `ESTIMATED_ON_STOP_CHAIN`,
//! or `ESTIMATED_IN_TRANSIT_TO` / `ESTIMATED_FROM_TRIP_UPDATE` fallbacks.

use super::overlay::{StopTimeRt, TripRt, VehiclePos};
use crate::gtfs::pack::StaticEpoch;
use crate::gtfs::siri_trip_map::{resolve_static_trip, TripResolveHint};
use chrono::{DateTime, Utc};

/// A vehicle position inferred from trip-update / SIRI progress.
#[derive(Debug, Clone, PartialEq)]
pub struct EstimatedVehicle {
    pub trip_id: String,
    pub lat: f64,
    pub lon: f64,
    pub delay: Option<i32>,
    pub stop_id: Option<String>,
    pub label: Option<String>,
    pub bearing: Option<f64>,
    pub current_status: Option<String>,
    pub current_stop_sequence: Option<u32>,
}

/// Estimate vehicle positions from trip updates in `feed_rt` (stop pin fallback).
///
/// Prefer [`estimate_vehicle_pos`] per trip when a [`StaticEpoch`] is available.
pub fn estimate_from_trip_updates(
    feed_rt: &super::overlay::FeedRtState,
    resolve_stop: impl Fn(&str) -> Option<(f64, f64)>,
    resolve_trip_label: impl Fn(&str) -> Option<String>,
    limit: usize,
) -> Vec<EstimatedVehicle> {
    let mut out = Vec::new();

    let mut seen_base: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut keys: Vec<&str> = Vec::new();

    for key in feed_rt.trips.keys() {
        if key.rsplit_once('@').is_some() {
            continue;
        }
        keys.push(key.as_str());
        seen_base.insert(key.as_str());
    }
    for key in feed_rt.trips.keys() {
        if let Some((base, _)) = key.rsplit_once('@') {
            if !seen_base.contains(base) {
                keys.push(key.as_str());
            }
        }
    }
    keys.sort_unstable();

    for key in keys {
        if limit > 0 && out.len() >= limit {
            break;
        }
        let Some(trip) = feed_rt.trips.get(key) else {
            continue;
        };
        if trip.canceled {
            continue;
        }

        let chosen = pick_stop_update(trip, &resolve_stop);
        let Some((stop_id, lat, lon, stop_delay)) = chosen else {
            continue;
        };

        let delay = stop_delay.or(trip.delay);
        let label = resolve_trip_label(key);

        out.push(EstimatedVehicle {
            trip_id: key.to_string(),
            lat,
            lon,
            delay,
            stop_id: Some(stop_id),
            label,
            bearing: None,
            current_status: Some("ESTIMATED_FROM_TRIP_UPDATE".into()),
            current_stop_sequence: None,
        });
    }

    out
}

/// Build an estimated [`VehiclePos`] from SIRI/GTFS-RT stop times + static geometry.
///
/// Returns `None` if canceled, no usable times/stops, or coords cannot be resolved.
pub fn estimate_vehicle_pos(
    feed_id: &str,
    trip_key: &str,
    trip: &TripRt,
    epoch: &StaticEpoch,
    now: DateTime<Utc>,
) -> Option<VehiclePos> {
    if trip.canceled || trip.stop_updates.is_empty() {
        return None;
    }

    let base_key = trip_key.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_key);
    let hint = trip_resolve_hint(trip);

    let now_ts = now.timestamp();

    // Ordered RT calls with a usable timestamp
    let mut calls: Vec<CallPoint> = trip
        .stop_updates
        .values()
        .filter_map(|u| call_point(feed_id, epoch, trip_key, u))
        .collect();
    calls.sort_by_key(|c| (c.seq, c.t_arr.unwrap_or(c.t_dep.unwrap_or(now_ts))));

    if calls.is_empty() {
        // No absolute times — fall back to highest-seq stop pin
        return pin_at_best_stop(feed_id, trip_key, trip, epoch, now, base_key);
    }

    // 1) STOPPED_AT: now between arrival and departure of a call
    for c in &calls {
        let arr = c.t_arr.or(c.t_dep);
        let dep = c.t_dep.or(c.t_arr);
        if let (Some(a), Some(d)) = (arr, dep) {
            if a <= now_ts && now_ts <= d {
                return Some(vehicle_at(
                    base_key,
                    c.lat,
                    c.lon,
                    c.stop_id.clone(),
                    Some(c.seq as u32),
                    now,
                    "STOPPED_AT",
                    None,
                    trip.delay,
                ));
            }
        }
    }

    // 2) Multi-call: interpolate between consecutive calls
    if calls.len() >= 2 {
        for w in calls.windows(2) {
            let prev = &w[0];
            let next = &w[1];
            let t0 = prev.t_dep.or(prev.t_arr).unwrap_or(now_ts);
            let t1 = next.t_arr.or(next.t_dep).unwrap_or(now_ts);
            if t0 <= now_ts && now_ts <= t1 {
                let span = (t1 - t0).max(1) as f64;
                let elapsed = (now_ts - t0).max(0) as f64;
                let frac = (elapsed / span).clamp(0.0, 1.0);
                let (lat, lon, bearing, geom) =
                    interpolate_segment(epoch, base_key, prev, next, frac, Some(&hint));
                return Some(vehicle_at(
                    base_key,
                    lat,
                    lon,
                    next.stop_id.clone(),
                    Some(next.seq as u32),
                    now,
                    geom.transit_status(),
                    bearing,
                    trip.delay,
                ));
            }
        }
        // Before first call → pin approaching first
        if let Some(first) = calls.first() {
            let first_t = first.t_arr.or(first.t_dep).unwrap_or(now_ts);
            if now_ts < first_t {
                return estimate_approaching_single(
                    feed_id, trip_key, trip, epoch, now, base_key, first,
                );
            }
        }
        // After last call → pin at last
        if let Some(last) = calls.last() {
            return Some(vehicle_at(
                base_key,
                last.lat,
                last.lon,
                last.stop_id.clone(),
                Some(last.seq as u32),
                now,
                "STOPPED_AT",
                None,
                trip.delay,
            ));
        }
    }

    // 3) Single SM MonitoredCall — typical for stop-monitoring
    if let Some(only) = calls.first() {
        return estimate_approaching_single(
            feed_id, trip_key, trip, epoch, now, base_key, only,
        );
    }

    None
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CallPoint {
    seq: u16,
    stop_id: String,
    lat: f64,
    lon: f64,
    /// Unix seconds (GTFS-RT / SIRI absolute times stored as i64)
    t_arr: Option<i64>,
    t_dep: Option<i64>,
    /// Static GTFS stop index when known
    stop_idx: Option<u32>,
}

fn call_point(
    feed_id: &str,
    epoch: &StaticEpoch,
    trip_key: &str,
    u: &StopTimeRt,
) -> Option<CallPoint> {
    let t_arr = u.arrival_time;
    let t_dep = u.departure_time;
    if t_arr.is_none() && t_dep.is_none() {
        return None;
    }
    let (lat, lon, stop_id, stop_idx) = resolve_stop_coords(feed_id, epoch, trip_key, u)?;
    Some(CallPoint {
        seq: u.stop_sequence,
        stop_id,
        lat,
        lon,
        t_arr,
        t_dep,
        stop_idx,
    })
}

fn resolve_stop_coords(
    feed_id: &str,
    epoch: &StaticEpoch,
    trip_key: &str,
    u: &StopTimeRt,
) -> Option<(f64, f64, String, Option<u32>)> {
    if let Some(ref sid) = u.stop_id {
        let namespaced = if sid.contains(':') {
            sid.clone()
        } else {
            format!("{feed_id}:{sid}")
        };
        for c in [sid.as_str(), namespaced.as_str()] {
            if let Some(&idx) = epoch.stop_id_to_idx.get(c) {
                if let Some(stop) = epoch.stops.get(idx as usize) {
                    if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
                        return Some((lat, lon, stop.id.clone(), Some(idx)));
                    }
                }
            }
            // SIRI STIF:StopPoint:Q:N: / StopArea → GTFS idfm:IDFM:… variants
            if let Some(digits) = extract_stop_digits(c) {
                let alts = [
                    format!("{feed_id}:{digits}"),
                    format!("{feed_id}:IDFM:{digits}"),
                    format!("{feed_id}:IDFM:monomodalStopPlace:{digits}"),
                    format!("{feed_id}:IDFM:StopPoint:{digits}"),
                    format!("IDFM:{digits}"),
                    format!("IDFM:monomodalStopPlace:{digits}"),
                    format!("StopPoint:{digits}"),
                    digits.clone(),
                ];
                for alt in &alts {
                    if let Some(&idx) = epoch.stop_id_to_idx.get(alt) {
                        if let Some(stop) = epoch.stops.get(idx as usize) {
                            if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
                                return Some((lat, lon, stop.id.clone(), Some(idx)));
                            }
                        }
                    }
                }
                // Prefer exact IDFM monomodal key patterns over full map scan
                for alt in [
                    format!("{feed_id}:IDFM:monomodalStopPlace:{digits}"),
                    format!("idfm:IDFM:monomodalStopPlace:{digits}"),
                    format!("{feed_id}:IDFM:{digits}"),
                    format!("idfm:IDFM:{digits}"),
                ] {
                    if let Some(&idx) = epoch.stop_id_to_idx.get(&alt) {
                        if let Some(stop) = epoch.stops.get(idx as usize) {
                            if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
                                return Some((lat, lon, stop.id.clone(), Some(idx)));
                            }
                        }
                    }
                }
            }
        }
    }
    // Sequence lookup on static trip (SIRI journey id rarely matches GTFS)
    let base = trip_key.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_key);
    if let Some(&trip_idx) = epoch.trip_id_to_idx.get(base) {
        if let Some(trip) = epoch.trips.get(trip_idx as usize) {
            for off in 0..trip.stop_time_len {
                let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
                if st.stop_sequence == u.stop_sequence {
                    if let Some(stop) = epoch.stops.get(st.stop_idx as usize) {
                        if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
                            return Some((lat, lon, stop.id.clone(), Some(st.stop_idx)));
                        }
                    }
                }
            }
        }
    }
    None
}

fn extract_stop_digits(s: &str) -> Option<String> {
    // STIF:StopPoint:Q:412986: or …:Q:412986
    if let Some(i) = s.find(":Q:") {
        let rest = &s[i + 3..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 4 {
            return Some(digits);
        }
    }
    let digits: String = s
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if digits.len() >= 4 {
        Some(digits)
    } else {
        None
    }
}

/// Single MonitoredCall: vehicle is approaching `target` (or already past ETA).
fn estimate_approaching_single(
    _feed_id: &str,
    _trip_key: &str,
    trip: &TripRt,
    epoch: &StaticEpoch,
    now: DateTime<Utc>,
    base_key: &str,
    target: &CallPoint,
) -> Option<VehiclePos> {
    let eta = target.t_arr.or(target.t_dep)?;
    let etd = target.t_dep.or(target.t_arr)?;
    let now_ts = now.timestamp();

    // Already at / dwelling
    if now_ts >= eta && now_ts <= etd {
        return Some(vehicle_at(
            base_key,
            target.lat,
            target.lon,
            target.stop_id.clone(),
            Some(target.seq as u32),
            now,
            "STOPPED_AT",
            None,
            trip.delay,
        ));
    }
    // Departed this stop
    if now_ts > etd {
        return Some(vehicle_at(
            base_key,
            target.lat,
            target.lon,
            target.stop_id.clone(),
            Some(target.seq as u32),
            now,
            "IN_TRANSIT_TO",
            None,
            trip.delay,
        ));
    }

    // Approaching: remaining time → fraction along prev → target
    let remaining = (eta - now_ts).max(0) as f64;

    let prev = previous_static_stop(epoch, base_key, target, Some(&trip_resolve_hint(trip)));
    let Some(prev) = prev else {
        // No previous stop (first stop) — pin at target, mark incoming
        return Some(vehicle_at(
            base_key,
            target.lat,
            target.lon,
            target.stop_id.clone(),
            Some(target.seq as u32),
            now,
            "INCOMING_AT",
            None,
            trip.delay,
        ));
    };

    let scheduled_run = scheduled_run_secs(
        epoch,
        base_key,
        prev.stop_sequence,
        target.seq,
        Some(&trip_resolve_hint(trip)),
    )
        .unwrap_or(120) as f64;
    let scheduled_run = scheduled_run.max(30.0);

    // remaining large ⇒ still near previous; remaining 0 ⇒ at target
    let frac = (1.0 - (remaining / scheduled_run)).clamp(0.0, 1.0);

    let prev_call = CallPoint {
        seq: prev.stop_sequence,
        stop_id: prev.stop_id,
        lat: prev.lat,
        lon: prev.lon,
        t_arr: None,
        t_dep: None,
        stop_idx: Some(prev.stop_idx),
    };
    let (lat, lon, bearing, geom) = interpolate_segment(
        epoch,
        base_key,
        &prev_call,
        target,
        frac,
        Some(&trip_resolve_hint(trip)),
    );

    let status = if frac > 0.92 {
        "INCOMING_AT"
    } else {
        geom.transit_status()
    };

    Some(vehicle_at(
        base_key,
        lat,
        lon,
        target.stop_id.clone(),
        Some(target.seq as u32),
        now,
        status,
        bearing,
        trip.delay,
    ))
}

struct StaticStopPin {
    stop_sequence: u16,
    stop_idx: u32,
    stop_id: String,
    lat: f64,
    lon: f64,
}

fn previous_static_stop(
    epoch: &StaticEpoch,
    base_key: &str,
    target: &CallPoint,
    hint: Option<&TripResolveHint>,
) -> Option<StaticStopPin> {
    let trip = resolve_trip(epoch, base_key, hint)?;
    let mut prev: Option<StaticStopPin> = None;
    for off in 0..trip.stop_time_len {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        let stop = epoch.stops.get(st.stop_idx as usize)?;
        let (Some(lat), Some(lon)) = (stop.lat, stop.lon) else {
            continue;
        };
        let pin = StaticStopPin {
            stop_sequence: st.stop_sequence,
            stop_idx: st.stop_idx,
            stop_id: stop.id.clone(),
            lat,
            lon,
        };
        // Match target by stop_idx or sequence
        let is_target = target
            .stop_idx
            .is_some_and(|i| i == st.stop_idx)
            || st.stop_sequence == target.seq
            || stop.id == target.stop_id
            || (!stop.raw_id.is_empty() && target.stop_id.contains(stop.raw_id.as_str()));
        if is_target {
            return prev;
        }
        prev = Some(pin);
    }
    // Target not on static trip — use last static stop before estimated seq
    let mut last: Option<StaticStopPin> = None;
    for off in 0..trip.stop_time_len {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        if st.stop_sequence >= target.seq && target.seq > 0 {
            break;
        }
        let stop = match epoch.stops.get(st.stop_idx as usize) {
            Some(s) => s,
            None => continue,
        };
        let (Some(lat), Some(lon)) = (stop.lat, stop.lon) else {
            continue;
        };
        last = Some(StaticStopPin {
            stop_sequence: st.stop_sequence,
            stop_idx: st.stop_idx,
            stop_id: stop.id.clone(),
            lat,
            lon,
        });
    }
    last
}

fn scheduled_run_secs(
    epoch: &StaticEpoch,
    base_key: &str,
    from_seq: u16,
    to_seq: u16,
    hint: Option<&TripResolveHint>,
) -> Option<u32> {
    let trip = resolve_trip(epoch, base_key, hint)?;
    let mut from_dep: Option<u32> = None;
    let mut to_arr: Option<u32> = None;
    for off in 0..trip.stop_time_len {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        if st.stop_sequence == from_seq {
            from_dep = Some(st.departure_s);
        }
        if st.stop_sequence == to_seq {
            to_arr = Some(st.arrival_s);
        }
    }
    Some(to_arr?.saturating_sub(from_dep.unwrap_or(0)).max(30))
}

fn shape_dist_for_seq(
    epoch: &StaticEpoch,
    base_key: &str,
    seq: u16,
    hint: Option<&TripResolveHint>,
) -> Option<f32> {
    let trip = resolve_trip(epoch, base_key, hint)?;
    for off in 0..trip.stop_time_len {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        if st.stop_sequence == seq {
            return st.shape_dist_traveled;
        }
    }
    None
}

fn trip_resolve_hint(trip: &TripRt) -> TripResolveHint {
    let mut first_dep: Option<i64> = None;
    for u in trip.stop_updates.values() {
        let t = u.departure_time.or(u.arrival_time);
        first_dep = match (first_dep, t) {
            (None, Some(x)) => Some(x),
            (Some(a), Some(x)) => Some(a.min(x)),
            (a, None) => a,
        };
    }
    TripResolveHint {
        route_id: trip.route_id.clone(),
        first_departure_utc: first_dep,
    }
}

fn resolve_trip<'a>(
    epoch: &'a StaticEpoch,
    base_key: &str,
    hint: Option<&TripResolveHint>,
) -> Option<&'a crate::gtfs::pack::GlobalTrip> {
    resolve_static_trip(epoch, base_key, hint)
}

/// How the in-transit position was geometrically derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathGeom {
    /// Arc-length along `shapes.txt` between nearest indices.
    Shape,
    /// Arc-length along GTFS stop sequence (all intermediate stops).
    StopChain,
    /// Crow-flies between the two RT call endpoints only.
    Direct,
}

impl PathGeom {
    fn transit_status(self) -> &'static str {
        match self {
            PathGeom::Shape => "ON_SHAPE",
            PathGeom::StopChain => "ON_STOP_CHAIN",
            PathGeom::Direct => "IN_TRANSIT_TO",
        }
    }
}

/// Interpolate along GTFS shape (preferred), else full stop-chain, else chord.
///
/// `frac` 0 → pin at `prev`, 1 → pin at `next`. Intermediate vertices with
/// missing coordinates are skipped.
fn interpolate_segment(
    epoch: &StaticEpoch,
    base_key: &str,
    prev: &CallPoint,
    next: &CallPoint,
    frac: f64,
    hint: Option<&TripResolveHint>,
) -> (f64, f64, Option<f64>, PathGeom) {
    let frac = frac.clamp(0.0, 1.0);

    // Exact endpoints
    if frac <= 0.0 {
        let brg = bearing_deg(prev.lat, prev.lon, next.lat, next.lon);
        return (prev.lat, prev.lon, Some(brg), PathGeom::Direct);
    }
    if frac >= 1.0 {
        let brg = bearing_deg(prev.lat, prev.lon, next.lat, next.lon);
        return (next.lat, next.lon, Some(brg), PathGeom::Direct);
    }

    let stop_chain = build_stop_chain_polyline(epoch, base_key, prev, next, hint);

    // Prefer shape when trip has shape_id: arc-length by shape_dist_traveled (GTFS
    // standard), else nearest-index slice between RT call endpoints.
    if let Some(trip) = resolve_trip(epoch, base_key, hint) {
        if let Some(ref sid) = trip.shape_id {
            if let Some(pts) = epoch.shapes.get(sid) {
                if pts.len() >= 2 {
                    let d0 = shape_dist_for_seq(epoch, base_key, prev.seq, hint);
                    let d1 = shape_dist_for_seq(epoch, base_key, next.seq, hint);
                    if let (Some(d0), Some(d1)) = (d0, d1) {
                        if let Some((lat, lon, brg)) =
                            crate::routing::geometry::interpolate_shape_by_dist(pts, d0, d1, frac)
                        {
                            return (lat, lon, Some(brg), PathGeom::Shape);
                        }
                    }
                    if let Some((lat, lon, brg)) = interpolate_polyline(
                        pts,
                        prev.lat,
                        prev.lon,
                        next.lat,
                        next.lon,
                        frac,
                    ) {
                        return (lat, lon, Some(brg), PathGeom::Shape);
                    }
                    // Multi-branch / bad nearest indices → fall through to stop-chain
                }
            }
        }
    }

    if let Some(ref chain) = stop_chain {
        if chain.len() >= 2 {
            if let Some((lat, lon, brg)) = interpolate_along_vertices(chain, frac) {
                return (lat, lon, Some(brg), PathGeom::StopChain);
            }
        }
    }

    // Crow-flies fallback (no intermediate geometry)
    let lat = prev.lat + (next.lat - prev.lat) * frac;
    let lon = prev.lon + (next.lon - prev.lon) * frac;
    let bearing = bearing_deg(prev.lat, prev.lon, next.lat, next.lon);
    (lat, lon, Some(bearing), PathGeom::Direct)
}

/// Build lat/lon vertices for every static stop from `prev` through `next`
/// (inclusive), ordered by stop_sequence. Missing coords are skipped.
fn build_stop_chain_polyline(
    epoch: &StaticEpoch,
    base_key: &str,
    prev: &CallPoint,
    next: &CallPoint,
    hint: Option<&TripResolveHint>,
) -> Option<Vec<(f64, f64)>> {
    let trip = resolve_trip(epoch, base_key, hint)?;

    let seq_lo = prev.seq.min(next.seq);
    let seq_hi = prev.seq.max(next.seq);

    // Collect stops whose sequence is in [lo, hi], in trip order
    let mut chain: Vec<(u16, f64, f64)> = Vec::new();
    for off in 0..trip.stop_time_len {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        if st.stop_sequence < seq_lo || st.stop_sequence > seq_hi {
            continue;
        }
        let Some(stop) = epoch.stops.get(st.stop_idx as usize) else {
            continue;
        };
        let (Some(lat), Some(lon)) = (stop.lat, stop.lon) else {
            continue; // skip vertices without coords
        };
        // Avoid duplicate consecutive identical points
        if let Some((_, plat, plon)) = chain.last() {
            if (*plat - lat).abs() < 1e-9 && (*plon - lon).abs() < 1e-9 {
                continue;
            }
        }
        chain.push((st.stop_sequence, lat, lon));
    }

    // Ensure endpoints from RT calls are present even if static match failed
    if chain.is_empty() {
        return Some(vec![(prev.lat, prev.lon), (next.lat, next.lon)]);
    }

    // If prev/next seq order is reverse of static order, reverse chain
    let mut pts: Vec<(f64, f64)> = chain.into_iter().map(|(_, la, lo)| (la, lo)).collect();

    // Snap first/last to RT call coords (authoritative for endpoints)
    if prev.seq <= next.seq {
        if let Some(first) = pts.first_mut() {
            *first = (prev.lat, prev.lon);
        }
        if let Some(last) = pts.last_mut() {
            *last = (next.lat, next.lon);
        }
    } else {
        pts.reverse();
        if let Some(first) = pts.first_mut() {
            *first = (prev.lat, prev.lon);
        }
        if let Some(last) = pts.last_mut() {
            *last = (next.lat, next.lon);
        }
    }

    // If we only resolved one endpoint from static, pad with both calls
    if pts.len() < 2 {
        return Some(vec![(prev.lat, prev.lon), (next.lat, next.lon)]);
    }
    Some(pts)
}

/// Arc-length interpolation along a vertex list. `frac` in [0, 1].
fn interpolate_along_vertices(pts: &[(f64, f64)], frac: f64) -> Option<(f64, f64, f64)> {
    if pts.is_empty() {
        return None;
    }
    if pts.len() == 1 || frac <= 0.0 {
        let p = pts[0];
        let brg = if pts.len() >= 2 {
            bearing_deg(p.0, p.1, pts[1].0, pts[1].1)
        } else {
            0.0
        };
        return Some((p.0, p.1, brg));
    }
    if frac >= 1.0 {
        let n = pts.len();
        let p = pts[n - 1];
        let prev = pts[n - 2];
        return Some((p.0, p.1, bearing_deg(prev.0, prev.1, p.0, p.1)));
    }

    let mut cum = vec![0.0f64];
    for i in 0..pts.len() - 1 {
        let d = haversine_m(pts[i].0, pts[i].1, pts[i + 1].0, pts[i + 1].1);
        cum.push(cum.last().unwrap() + d);
    }
    let total = *cum.last()?;
    if total < 1.0 {
        // Degenerate — linear in index space
        let lat = pts[0].0 + (pts.last()?.0 - pts[0].0) * frac;
        let lon = pts[0].1 + (pts.last()?.1 - pts[0].1) * frac;
        return Some((
            lat,
            lon,
            bearing_deg(pts[0].0, pts[0].1, pts.last()?.0, pts.last()?.1),
        ));
    }
    let target = total * frac.clamp(0.0, 1.0);
    for i in 0..cum.len().saturating_sub(1) {
        if target >= cum[i] && target <= cum[i + 1] + 1e-9 {
            let seg = (cum[i + 1] - cum[i]).max(1e-6);
            let t = ((target - cum[i]) / seg).clamp(0.0, 1.0);
            let p0 = pts[i];
            let p1 = pts[i + 1];
            let lat = p0.0 + (p1.0 - p0.0) * t;
            let lon = p0.1 + (p1.1 - p0.1) * t;
            let brg = bearing_deg(p0.0, p0.1, p1.0, p1.1);
            return Some((lat, lon, brg));
        }
    }
    let p = *pts.last()?;
    let prev = pts[pts.len() - 2];
    Some((p.0, p.1, bearing_deg(prev.0, prev.1, p.0, p.1)))
}

/// Find nearest shape indices to prev/next and lerp along cumulative distance.
///
/// Returns `None` when the nearest-index span looks wrong for multi-branch
/// shapes (path much longer than crow-flies), so caller can use stop-chain.
fn interpolate_polyline(
    pts: &[(f64, f64)],
    prev_lat: f64,
    prev_lon: f64,
    next_lat: f64,
    next_lon: f64,
    frac: f64,
) -> Option<(f64, f64, f64)> {
    let i0 = nearest_idx(pts, prev_lat, prev_lon);
    let i1 = nearest_idx(pts, next_lat, next_lon);
    if i0 == i1 {
        // Ambiguous on multi-branch or same vertex — let stop-chain handle it
        return None;
    }
    let (a, b) = if i0 < i1 { (i0, i1) } else { (i1, i0) };
    // cumulative length along [a..=b]
    let mut cum = vec![0.0f64];
    for i in a..b {
        let d = haversine_m(pts[i].0, pts[i].1, pts[i + 1].0, pts[i + 1].1);
        cum.push(cum.last().unwrap() + d);
    }
    let total = *cum.last()?;
    if total < 1.0 {
        return None;
    }

    // Multi-branch heuristic: if shape path is wildly longer than straight line
    // between stops, nearest indices likely jumped branches — reject.
    let chord = haversine_m(prev_lat, prev_lon, next_lat, next_lon).max(1.0);
    if total > chord * 4.0 && total > 500.0 {
        return None;
    }

    // If shape index order is reverse of travel, flip frac
    let f = if i0 < i1 { frac } else { 1.0 - frac };
    let slice: Vec<(f64, f64)> = pts[a..=b].to_vec();
    // Re-lerp with travel-direction frac on the slice (always low→high index)
    interpolate_along_vertices(&slice, f.clamp(0.0, 1.0))
}

fn nearest_idx(pts: &[(f64, f64)], lat: f64, lon: f64) -> usize {
    let mut best = 0usize;
    let mut best_d = f64::MAX;
    for (i, p) in pts.iter().enumerate() {
        let d = (p.0 - lat).hypot(p.1 - lon);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let rlat1 = lat1.to_radians();
    let rlat2 = lat2.to_radians();
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + rlat1.cos() * rlat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().asin()
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

fn vehicle_at(
    base_key: &str,
    lat: f64,
    lon: f64,
    stop_id: String,
    seq: Option<u32>,
    now: DateTime<Utc>,
    status: &str,
    bearing: Option<f64>,
    _delay: Option<i32>,
) -> VehiclePos {
    // Distinct from real GPS: UI should label as estimated (`ESTIMATED_*` prefix).
    let current_status = match status {
        "STOPPED_AT" => "ESTIMATED_STOPPED_AT",
        "INCOMING_AT" => "ESTIMATED_INCOMING_AT",
        "ON_SHAPE" => "ESTIMATED_ON_SHAPE",
        "ON_STOP_CHAIN" => "ESTIMATED_ON_STOP_CHAIN",
        "IN_TRANSIT_TO" => "ESTIMATED_IN_TRANSIT_TO",
        other if other.starts_with("ESTIMATED_") => other,
        other => {
            // Unknown raw status → prefix for UI filters
            return VehiclePos {
                trip_id: Some(base_key.to_string()),
                lat,
                lon,
                bearing,
                speed: None,
                updated_at: now,
                current_stop_id: Some(stop_id),
                label: None,
                vehicle_id: None,
                license_plate: None,
                occupancy: None,
                current_status: Some(format!("ESTIMATED_{other}")),
                current_stop_sequence: seq,
                congestion: None,
                occupancy_percentage: None,
            };
        }
    };
    VehiclePos {
        trip_id: Some(base_key.to_string()),
        lat,
        lon,
        bearing,
        speed: None,
        updated_at: now,
        current_stop_id: Some(stop_id),
        label: None,
        vehicle_id: None,
        license_plate: None,
        occupancy: None,
        current_status: Some(current_status.into()),
        current_stop_sequence: seq,
        congestion: None,
        occupancy_percentage: None,
    }
}

fn pin_at_best_stop(
    feed_id: &str,
    trip_key: &str,
    trip: &TripRt,
    epoch: &StaticEpoch,
    now: DateTime<Utc>,
    base_key: &str,
) -> Option<VehiclePos> {
    let mut best: Option<&StopTimeRt> = None;
    let mut best_score = i64::MIN;
    for u in trip.stop_updates.values() {
        let has_signal = u.arrival_delay.is_some()
            || u.departure_delay.is_some()
            || u.arrival_time.is_some()
            || u.departure_time.is_some();
        let score = (if has_signal { 1_000_000i64 } else { 0 }) + u.stop_sequence as i64;
        if score > best_score {
            best_score = score;
            best = Some(u);
        }
    }
    let su = best?;
    let (lat, lon, stop_id, _) = resolve_stop_coords(feed_id, epoch, trip_key, su)?;
    Some(vehicle_at(
        base_key,
        lat,
        lon,
        stop_id,
        Some(su.stop_sequence as u32),
        now,
        "IN_TRANSIT_TO",
        None,
        trip.delay,
    ))
}

fn pick_stop_update(
    trip: &TripRt,
    resolve_stop: &impl Fn(&str) -> Option<(f64, f64)>,
) -> Option<(String, f64, f64, Option<i32>)> {
    let mut with_id: Vec<&StopTimeRt> = trip
        .stop_updates
        .values()
        .filter(|u| u.stop_id.as_ref().is_some_and(|s| !s.is_empty()))
        .collect();

    if with_id.is_empty() {
        return None;
    }

    with_id.sort_by_key(|u| std::cmp::Reverse(u.stop_sequence));

    for u in &with_id {
        let sid = u.stop_id.as_deref().unwrap();
        if let Some((lat, lon)) = resolve_stop(sid) {
            let delay = u.departure_delay.or(u.arrival_delay);
            return Some((sid.to_string(), lat, lon, delay));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::pack::{PackedStopTime, RouteMode, StaticEpoch, StopRecord};
    use crate::rt::overlay::{FeedRtState, StopTimeRt, TripRt};
    use std::collections::HashMap;

    fn stop(seq: u16, id: &str, arr_delay: Option<i32>) -> (u16, StopTimeRt) {
        (
            seq,
            StopTimeRt {
                stop_sequence: seq,
                stop_id: Some(id.to_string()),
                arrival_delay: arr_delay,
                departure_delay: None,
                ..Default::default()
            },
        )
    }

    fn trip_with(stops: Vec<(u16, StopTimeRt)>, delay: Option<i32>, canceled: bool) -> TripRt {
        TripRt {
            delay,
            canceled,
            stop_updates: stops.into_iter().collect(),
            ..Default::default()
        }
    }

    fn resolver(map: HashMap<&'static str, (f64, f64)>) -> impl Fn(&str) -> Option<(f64, f64)> {
        move |id: &str| map.get(id).copied()
    }

    #[test]
    fn estimates_from_highest_sequence() {
        let mut feed = FeedRtState::new("idf");
        let t = trip_with(
            vec![
                stop(1, "idf:A", Some(0)),
                stop(5, "idf:B", Some(120)),
                stop(3, "idf:C", Some(60)),
            ],
            Some(30),
            false,
        );
        feed.insert_trip("idf", "T1", t);

        let coords = HashMap::from([
            ("idf:A", (48.0, 2.0)),
            ("idf:B", (48.1, 2.1)),
            ("idf:C", (48.05, 2.05)),
        ]);
        let out = estimate_from_trip_updates(
            &feed,
            resolver(coords),
            |_| Some("Line 1".into()),
            0,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trip_id, "idf:T1");
        assert_eq!(out[0].stop_id.as_deref(), Some("idf:B"));
        assert!((out[0].lat - 48.1).abs() < 1e-9);
        assert_eq!(out[0].delay, Some(120));
    }

    #[test]
    fn skips_canceled_and_unresolvable() {
        let mut feed = FeedRtState::new("idf");
        feed.insert_trip(
            "idf",
            "CX",
            trip_with(vec![stop(1, "idf:A", Some(0))], None, true),
        );
        feed.insert_trip(
            "idf",
            "NOCOORD",
            trip_with(vec![stop(1, "idf:UNKNOWN", Some(0))], None, false),
        );
        feed.insert_trip(
            "idf",
            "OK",
            trip_with(vec![stop(2, "idf:A", Some(10))], None, false),
        );

        let coords = HashMap::from([("idf:A", (1.0, 2.0))]);
        let out = estimate_from_trip_updates(&feed, resolver(coords), |_| None, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trip_id, "idf:OK");
    }

    #[test]
    fn prefers_undated_keys_once() {
        let mut feed = FeedRtState::new("sncf");
        let t = trip_with(vec![stop(1, "sncf:S", Some(5))], None, false);
        let mut dated = t.clone();
        dated.start_date = Some("20240115".into());
        feed.insert_trip("sncf", "TRIP", dated);

        let coords = HashMap::from([("sncf:S", (45.0, 5.0))]);
        let out = estimate_from_trip_updates(&feed, resolver(coords), |_| None, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trip_id, "sncf:TRIP");
    }

    #[test]
    fn falls_back_to_trip_delay() {
        let mut feed = FeedRtState::new("x");
        feed.insert_trip(
            "x",
            "T",
            trip_with(vec![stop(1, "x:S", None)], Some(99), false),
        );
        let coords = HashMap::from([("x:S", (0.0, 0.0))]);
        let out = estimate_from_trip_updates(&feed, resolver(coords), |_| None, 0);
        assert_eq!(out[0].delay, Some(99));
    }

    #[test]
    fn respects_limit() {
        let mut feed = FeedRtState::new("f");
        for i in 0..5 {
            feed.insert_trip(
                "f",
                &format!("T{i}"),
                trip_with(vec![stop(1, "f:S", Some(0))], None, false),
            );
        }
        let coords = HashMap::from([("f:S", (1.0, 1.0))]);
        let out = estimate_from_trip_updates(&feed, resolver(coords), |_| None, 2);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn skips_highest_seq_if_unresolvable_uses_next() {
        let mut feed = FeedRtState::new("f");
        feed.insert_trip(
            "f",
            "T",
            trip_with(
                vec![
                    stop(10, "f:GHOST", Some(0)),
                    stop(3, "f:REAL", Some(15)),
                ],
                None,
                false,
            ),
        );
        let coords = HashMap::from([("f:REAL", (9.0, 8.0))]);
        let out = estimate_from_trip_updates(&feed, resolver(coords), |_| None, 0);
        assert_eq!(out[0].stop_id.as_deref(), Some("f:REAL"));
    }

    fn mini_epoch() -> StaticEpoch {
        use crate::gtfs::pack::GlobalTrip;
        let mut epoch = StaticEpoch::empty();
        epoch.id = "test".into();
        epoch.stops = vec![
            StopRecord {
                id: "f:A".into(),
                feed_id: "f".into(),
                raw_id: "A".into(),
                name: "A".into(),
                lat: Some(48.0),
                lon: Some(2.0),
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
            },
            StopRecord {
                id: "f:B".into(),
                feed_id: "f".into(),
                raw_id: "B".into(),
                name: "B".into(),
                lat: Some(48.1),
                lon: Some(2.1),
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
            },
        ];
        epoch.trips = vec![GlobalTrip {
            id: "f:T1".into(),
            feed_id: "f".into(),
            route_id: "f:R1".into(),
            service_id: "svc".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Bus,
            route_short_name: "1".into(),
            route_long_name: "Line 1".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 3,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len: 2,
            frequency_windows: vec![],
        }];
        epoch.stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 36000,
                departure_s: 36000,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 36600, // 10 min later
                departure_s: 36600,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
        ];
        epoch.stop_id_to_idx.insert("f:A".into(), 0);
        epoch.stop_id_to_idx.insert("f:B".into(), 1);
        epoch.trip_id_to_idx.insert("f:T1".into(), 0);
        epoch
    }

    #[test]
    fn siri_sm_interpolates_approaching() {
        let epoch = mini_epoch();
        let now = Utc::now();
        let now_ts = now.timestamp();
        // ETA at B in 5 minutes → halfway of 10 min scheduled segment
        let mut trip = TripRt::default();
        trip.stop_updates.insert(
            2,
            StopTimeRt {
                stop_sequence: 2,
                stop_id: Some("f:B".into()),
                arrival_time: Some(now_ts + 300),
                departure_time: Some(now_ts + 330),
                ..Default::default()
            },
        );
        let pos = estimate_vehicle_pos("f", "f:T1", &trip, &epoch, now).expect("pos");
        assert!(pos.lat > 48.0 && pos.lat < 48.1);
        assert!(pos.lon > 2.0 && pos.lon < 2.1);
        assert!(
            pos.current_status
                .as_deref()
                .unwrap()
                .starts_with("ESTIMATED_")
        );
    }

    #[test]
    fn siri_sm_stopped_when_in_dwell() {
        let epoch = mini_epoch();
        let now = Utc::now();
        let now_ts = now.timestamp();
        let mut trip = TripRt::default();
        trip.stop_updates.insert(
            2,
            StopTimeRt {
                stop_sequence: 2,
                stop_id: Some("f:B".into()),
                arrival_time: Some(now_ts - 10),
                departure_time: Some(now_ts + 20),
                ..Default::default()
            },
        );
        let pos = estimate_vehicle_pos("f", "f:T1", &trip, &epoch, now).expect("pos");
        assert!((pos.lat - 48.1).abs() < 1e-9);
        assert_eq!(pos.current_status.as_deref(), Some("ESTIMATED_STOPPED_AT"));
    }

    #[test]
    fn multi_call_interpolates_between() {
        let epoch = mini_epoch();
        let now = Utc::now();
        let now_ts = now.timestamp();
        let mut trip = TripRt::default();
        trip.stop_updates.insert(
            1,
            StopTimeRt {
                stop_sequence: 1,
                stop_id: Some("f:A".into()),
                departure_time: Some(now_ts - 300),
                arrival_time: Some(now_ts - 320),
                ..Default::default()
            },
        );
        trip.stop_updates.insert(
            2,
            StopTimeRt {
                stop_sequence: 2,
                stop_id: Some("f:B".into()),
                arrival_time: Some(now_ts + 300),
                departure_time: Some(now_ts + 330),
                ..Default::default()
            },
        );
        let pos = estimate_vehicle_pos("f", "f:T1", &trip, &epoch, now).expect("pos");
        // Midway between A and B (2-stop chain = chord)
        assert!((pos.lat - 48.05).abs() < 0.02);
        assert!(
            matches!(
                pos.current_status.as_deref(),
                Some("ESTIMATED_ON_STOP_CHAIN")
                    | Some("ESTIMATED_IN_TRANSIT_TO")
                    | Some("ESTIMATED_ON_SHAPE")
            ),
            "got {:?}",
            pos.current_status
        );
    }

    /// L-shaped stop chain A→B→C: mid time-fraction must sit near corner B,
    /// not on the A–C chord.
    fn l_shaped_epoch() -> StaticEpoch {
        use crate::gtfs::pack::GlobalTrip;
        let mut epoch = StaticEpoch::empty();
        epoch.id = "test".into();
        // A (0,0) → B (0,0.1) east → C (0.1, 0.1) north  (lat, lon degrees ~11km)
        epoch.stops = vec![
            StopRecord {
                id: "f:A".into(),
                feed_id: "f".into(),
                raw_id: "A".into(),
                name: "A".into(),
                lat: Some(48.0),
                lon: Some(2.0),
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
            },
            StopRecord {
                id: "f:B".into(),
                feed_id: "f".into(),
                raw_id: "B".into(),
                name: "B".into(),
                lat: Some(48.0),
                lon: Some(2.1),
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
            },
            StopRecord {
                id: "f:C".into(),
                feed_id: "f".into(),
                raw_id: "C".into(),
                name: "C".into(),
                lat: Some(48.1),
                lon: Some(2.1),
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
            },
            // D missing coords — must be skipped when in chain
            StopRecord {
                id: "f:D".into(),
                feed_id: "f".into(),
                raw_id: "D".into(),
                name: "D".into(),
                lat: None,
                lon: None,
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
            },
        ];
        epoch.trips = vec![GlobalTrip {
            id: "f:T1".into(),
            feed_id: "f".into(),
            route_id: "f:R1".into(),
            service_id: "svc".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Bus,
            route_short_name: "1".into(),
            route_long_name: "Line 1".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 3,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len: 3,
            frequency_windows: vec![],
        }];
        epoch.stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 36000,
                departure_s: 36000,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 36300,
                departure_s: 36300,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 2,
                arrival_s: 36600,
                departure_s: 36600,
                stop_sequence: 3,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
        ];
        epoch.stop_id_to_idx.insert("f:A".into(), 0);
        epoch.stop_id_to_idx.insert("f:B".into(), 1);
        epoch.stop_id_to_idx.insert("f:C".into(), 2);
        epoch.stop_id_to_idx.insert("f:D".into(), 3);
        epoch.trip_id_to_idx.insert("f:T1".into(), 0);
        epoch
    }

    #[test]
    fn stop_chain_mid_frac_not_on_endpoint_chord() {
        let epoch = l_shaped_epoch();
        let now = Utc::now();
        let now_ts = now.timestamp();
        // RT only reports A and C (skipping intermediate B in feed)
        let mut trip = TripRt::default();
        trip.stop_updates.insert(
            1,
            StopTimeRt {
                stop_sequence: 1,
                stop_id: Some("f:A".into()),
                departure_time: Some(now_ts - 300),
                arrival_time: Some(now_ts - 320),
                ..Default::default()
            },
        );
        trip.stop_updates.insert(
            3,
            StopTimeRt {
                stop_sequence: 3,
                stop_id: Some("f:C".into()),
                arrival_time: Some(now_ts + 300),
                departure_time: Some(now_ts + 330),
                ..Default::default()
            },
        );
        let pos = estimate_vehicle_pos("f", "f:T1", &trip, &epoch, now).expect("pos");
        // Arc-length midpoint of A→B→C is at B (equal legs ~east then ~north)
        assert!(
            (pos.lat - 48.0).abs() < 0.02,
            "lat should stay near B (48.0), got {}",
            pos.lat
        );
        assert!(
            (pos.lon - 2.1).abs() < 0.02,
            "lon should stay near B (2.1), got {}",
            pos.lon
        );
        // Must NOT be on A–C diagonal (~48.05, 2.05)
        let chord_lat = 48.05;
        let chord_lon = 2.05;
        let dist_to_chord = (pos.lat - chord_lat).hypot(pos.lon - chord_lon);
        assert!(
            dist_to_chord > 0.03,
            "position too close to A–C chord: ({}, {})",
            pos.lat,
            pos.lon
        );
        assert_eq!(
            pos.current_status.as_deref(),
            Some("ESTIMATED_ON_STOP_CHAIN")
        );
    }

    #[test]
    fn stop_chain_frac_snaps_endpoints() {
        let epoch = l_shaped_epoch();
        let prev = CallPoint {
            seq: 1,
            stop_id: "f:A".into(),
            lat: 48.0,
            lon: 2.0,
            t_arr: None,
            t_dep: None,
            stop_idx: Some(0),
        };
        let next = CallPoint {
            seq: 3,
            stop_id: "f:C".into(),
            lat: 48.1,
            lon: 2.1,
            t_arr: None,
            t_dep: None,
            stop_idx: Some(2),
        };
        let (lat0, lon0, _, _) = interpolate_segment(&epoch, "f:T1", &prev, &next, 0.0, None);
        assert!((lat0 - 48.0).abs() < 1e-12 && (lon0 - 2.0).abs() < 1e-12);
        let (lat1, lon1, _, _) = interpolate_segment(&epoch, "f:T1", &prev, &next, 1.0, None);
        assert!((lat1 - 48.1).abs() < 1e-12 && (lon1 - 2.1).abs() < 1e-12);
    }

    #[test]
    fn shape_interpolation_preferred_when_present() {
        let mut epoch = l_shaped_epoch();
        // Dense L-shape matching stops
        epoch.shapes.insert(
            "f:sh1".into(),
            vec![
                (48.0, 2.0),
                (48.0, 2.05),
                (48.0, 2.1),
                (48.05, 2.1),
                (48.1, 2.1),
            ],
        );
        epoch.trips[0].shape_id = Some("f:sh1".into());

        let prev = CallPoint {
            seq: 1,
            stop_id: "f:A".into(),
            lat: 48.0,
            lon: 2.0,
            t_arr: None,
            t_dep: None,
            stop_idx: Some(0),
        };
        let next = CallPoint {
            seq: 3,
            stop_id: "f:C".into(),
            lat: 48.1,
            lon: 2.1,
            t_arr: None,
            t_dep: None,
            stop_idx: Some(2),
        };
        let (lat, lon, _, geom) = interpolate_segment(&epoch, "f:T1", &prev, &next, 0.5, None);
        assert_eq!(geom, PathGeom::Shape);
        // Mid arc ≈ corner at B
        assert!((lat - 48.0).abs() < 0.03, "lat={lat}");
        assert!((lon - 2.1).abs() < 0.03, "lon={lon}");
    }

    #[test]
    fn reverse_shape_direction_flips_frac() {
        let mut epoch = l_shaped_epoch();
        // Shape stored opposite to travel (C → A)
        epoch.shapes.insert(
            "f:sh_rev".into(),
            vec![(48.1, 2.1), (48.0, 2.1), (48.0, 2.0)],
        );
        epoch.trips[0].shape_id = Some("f:sh_rev".into());
        let prev = CallPoint {
            seq: 1,
            stop_id: "f:A".into(),
            lat: 48.0,
            lon: 2.0,
            t_arr: None,
            t_dep: None,
            stop_idx: Some(0),
        };
        let next = CallPoint {
            seq: 3,
            stop_id: "f:C".into(),
            lat: 48.1,
            lon: 2.1,
            t_arr: None,
            t_dep: None,
            stop_idx: Some(2),
        };
        // Small frac: near A (start of travel)
        let (lat, lon, _, geom) = interpolate_segment(&epoch, "f:T1", &prev, &next, 0.1, None);
        assert_eq!(geom, PathGeom::Shape);
        assert!((lat - 48.0).abs() < 0.02, "lat={lat}");
        assert!((lon - 2.0).abs() < 0.05, "lon={lon}");
    }

    #[test]
    fn canceled_trip_yields_none() {
        let epoch = mini_epoch();
        let mut trip = TripRt::default();
        trip.canceled = true;
        trip.stop_updates.insert(
            1,
            StopTimeRt {
                stop_sequence: 1,
                stop_id: Some("f:A".into()),
                arrival_time: Some(Utc::now().timestamp()),
                ..Default::default()
            },
        );
        assert!(estimate_vehicle_pos("f", "f:T1", &trip, &epoch, Utc::now()).is_none());
    }

    #[test]
    fn bearing_eastish() {
        let b = bearing_deg(48.0, 2.0, 48.0, 2.1);
        assert!(b > 80.0 && b < 100.0);
    }
}
