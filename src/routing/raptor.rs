//! Multimodal RAPTOR journey planner.
//!
//! Classic multi-round RAPTOR with:
//! - transfer buffer after the first transit leg
//! - marked-stop set (only improved stops scan next round)
//! - mode filters and optional excluded trip ids
//! - access / transfer / egress walk legs in reconstruction
//! - k-best via successive departure offsets after each found journey
//! - **arrive-by**: when [`ItineraryQuery::arrive_by`] is true, `departure_at` is treated as
//!   the **arrival deadline**. Forward RAPTOR is run from sampled departures in a look-back
//!   window (default 6h); only journeys with `arrival <= deadline` are kept, ranked by later
//!   departure first. GraphQL maps `arriveBy` → this mode (`schema` sets `arrive_by` when the
//!   field is present and uses that timestamp as `departure_at`).
//! - **McRAPTOR-lite**: after collecting k-best candidates, keep the Pareto front on
//!   `(arrival↓, transfers↓, walk_distance↓, departure↑)` so dominated itineraries are
//!   dropped without erasing later alternative departures.

use chrono::{DateTime, NaiveDate, Utc};
use std::collections::{HashMap, HashSet};

use super::journey::{
    local_midnight_utc, local_seconds_since_midnight, new_journey_id, service_date_in_tz, stop_ref,
    IntermediateStop, ItineraryQuery, ItineraryResult, Journey, Leg, Place, TransitLegData,
    WalkLegData,
};
use super::walk::AccessStop;
use crate::gtfs::pack::{RouteMode, StaticEpoch};

/// How a stop was reached in a given round.
#[derive(Clone, Debug)]
enum Reach {
    /// Seeded by access walk from the query origin.
    Access {
        stop_idx: u32,
        arrival_s: u32,
        walk_duration: u32,
        walk_distance: f64,
    },
    /// Boarded a trip at `board_stop` (offset `board_off` within the trip) and alighted here.
    Transit {
        arrival_s: u32,
        trip_idx: u32,
        board_stop: u32,
        board_off: u32,
        alight_off: u32,
        /// Round that provided the boarding stop arrival (prev_round).
        prev_round: u16,
        /// Added to template `stop_times` for frequency-based trips (0 for schedule trips).
        time_shift_s: i32,
    },
    /// Footpath / walk transfer from another stop.
    Walk {
        arrival_s: u32,
        from_stop: u32,
        to_stop: u32,
        duration_s: u32,
        distance_m: f64,
        /// Labels row of this walk (same as transit that improved `from_stop`).
        /// Reconstruct follows `from_stop` on the **current** row, not this field.
        #[allow(dead_code)]
        prev_round: u16,
    },
}

impl Reach {
    fn arrival_s(&self) -> u32 {
        match self {
            Reach::Access { arrival_s, .. }
            | Reach::Transit { arrival_s, .. }
            | Reach::Walk { arrival_s, .. } => *arrival_s,
        }
    }
}

struct RaptorState {
    /// labels[round][stop] = how we reached stop in that round (None if not improved that round).
    labels: Vec<Vec<Option<Reach>>>,
}


/// Cap virtual frequency departures considered per window per board opportunity.
const MAX_FREQ_DEPS_PER_WINDOW: u32 = 32;

/// Maximum journey duration considered when searching arrive-by (look-back window).
const ARRIVE_BY_MAX_JOURNEY_S: u32 = 6 * 3600;

/// Coarse sample step when probing departures for arrive-by (seconds).
const ARRIVE_BY_SAMPLE_STEP_S: u32 = 15 * 60;

/// Extra candidates collected before Pareto / truncate (multiples of max_results).
const CANDIDATE_POOL_FACTOR: usize = 3;

/// Template base time for a trip: first stop_time departure (relative anchor for frequencies).
fn trip_template_base(epoch: &StaticEpoch, trip: &crate::gtfs::pack::GlobalTrip) -> u32 {
    if trip.stop_time_len == 0 {
        return 0;
    }
    epoch.stop_times[trip.stop_time_start as usize].departure_s
}

/// Earliest frequency-based board departure at a stop offset, if any.
/// Returns (board_dep_s, time_shift_s) where absolute times = template + time_shift.
fn earliest_freq_board(
    trip: &crate::gtfs::pack::GlobalTrip,
    template_base: u32,
    board_template_dep: u32,
    board_after: u32,
) -> Option<(u32, i32)> {
    if trip.frequency_windows.is_empty() {
        return None;
    }
    // Offset from trip start (first stop) to this board stop in the template.
    let board_offset = board_template_dep as i64 - template_base as i64;
    let mut best: Option<(u32, i32)> = None;
    for w in &trip.frequency_windows {
        if w.headway_s == 0 || w.end_s <= w.start_s {
            continue;
        }
        // Trip start T in [start_s, end_s); board dep = T + board_offset (when offset >= 0).
        // Need T + board_offset >= board_after  =>  T >= board_after - board_offset
        let need_t = (board_after as i64) - board_offset;
        let t_min = need_t.max(w.start_s as i64);
        if t_min >= w.end_s as i64 {
            continue;
        }
        // Align upward to headway grid from start_s.
        let mut t = if t_min <= w.start_s as i64 {
            w.start_s
        } else {
            let delta = (t_min as u32).saturating_sub(w.start_s);
            let steps = (delta + w.headway_s - 1) / w.headway_s;
            let aligned = w.start_s.saturating_add(steps.saturating_mul(w.headway_s));
            aligned
        };
        let mut count = 0u32;
        while t < w.end_s && count < MAX_FREQ_DEPS_PER_WINDOW {
            let dep_i = t as i64 + board_offset;
            if dep_i >= 0 {
                let dep = dep_i as u32;
                if dep >= board_after {
                    let shift = t as i32 - template_base as i32;
                    match best {
                        Some((bd, _)) if bd <= dep => {}
                        _ => best = Some((dep, shift)),
                    }
                    break; // earliest in this window
                }
            }
            t = t.saturating_add(w.headway_s);
            count += 1;
        }
    }
    best
}

/// Public entry: plan up to `max_results` diverse journeys.
///
/// # Arrive-by (GraphQL `arriveBy`)
///
/// When `q.arrive_by` is true, `q.departure_at` is the **latest acceptable arrival**
/// (not “leave after”). GraphQL sets this when the client sends `arriveBy` (and uses that
/// timestamp as `departure_at`). Implementation: sample forward RAPTOR seeds in
/// `[deadline − 6h, deadline)`, keep journeys with `arrival ≤ deadline`, prefer **later
/// departures**, then McRAPTOR-lite Pareto on `(arrival, transfers, walk_distance)`.
pub fn plan_journeys(epoch: &StaticEpoch, q: &ItineraryQuery) -> ItineraryResult {
    let date = service_date_in_tz(q.departure_at, &q.timezone);
    let dep_s0 = local_seconds_since_midnight(q.departure_at, &q.timezone);
    let midnight = local_midnight_utc(date, &q.timezone);

    let osrm = q.osrm_url.as_deref().filter(|u| !u.trim().is_empty());
    // Larger access set for IDFM place clusters (monomodals per station/city).
    let access_n = if epoch.trip_count() > 80_000 { 40 } else { 16 };
    let from_profile = if q.bike_from {
        super::walk::OsrmProfile::Bike
    } else {
        super::walk::OsrmProfile::Foot
    };
    let to_profile = if q.bike_to {
        super::walk::OsrmProfile::Bike
    } else {
        super::walk::OsrmProfile::Foot
    };
    let origins = super::walk::resolve_access_stops_profile(
        epoch,
        q.from_stop_id.as_deref(),
        q.from_lat,
        q.from_lon,
        q.access_max_from_m(),
        q.access_speed_from_m_s(),
        access_n,
        osrm,
        from_profile,
    );
    let targets = super::walk::resolve_access_stops_profile(
        epoch,
        q.to_stop_id.as_deref(),
        q.to_lat,
        q.to_lon,
        q.access_max_to_m(),
        q.access_speed_to_m_s(),
        access_n,
        osrm,
        to_profile,
    );

    if epoch.stops.is_empty() || origins.is_empty() || targets.is_empty() {
        return empty_result(epoch, q);
    }

    let egress: HashMap<u32, AccessStop> = targets
        .iter()
        .map(|a| (a.stop_idx, a.clone()))
        .collect();
    let origin_map: HashMap<u32, AccessStop> = origins
        .iter()
        .map(|a| (a.stop_idx, a.clone()))
        .collect();

    // Large multi-feed epochs (IDFM): cap k tightly — each extra result re-runs
    // RAPTOR and can starve the server (browser then sees "Failed to fetch").
    let max_k = if epoch.trip_count() > 80_000 {
        (q.max_results.max(1) as usize).min(4)
    } else {
        q.max_results.max(1) as usize
    };
    let pool_cap = if epoch.trip_count() > 80_000 {
        // Small pool: prefer latency over many near-duplicate itineraries.
        (max_k + 1).max(max_k)
    } else {
        (max_k * CANDIDATE_POOL_FACTOR).max(max_k)
    };

    let mut journeys = if q.arrive_by {
        plan_arrive_by(
            epoch,
            q,
            date,
            dep_s0,
            midnight,
            &origins,
            &targets,
            &egress,
            &origin_map,
            pool_cap,
        )
    } else {
        plan_depart_after(
            epoch,
            q,
            date,
            dep_s0,
            midnight,
            &origins,
            &targets,
            &egress,
            &origin_map,
            pool_cap,
            None, // no arrival deadline
        )
    };

    // McRAPTOR-lite: drop dominated (arrival, transfers, walk) itineraries.
    // Multi-mode (RER→Métro) relies on a **complete walk/transfer graph** built at pack
    // time — not hardcoded gare names or via-hub stitching.
    journeys = pareto_front(journeys);

    if q.arrive_by {
        // Prefer leaving later while still meeting the deadline; then arrival, transfers, walk.
        journeys.sort_by(|a, b| {
            b.departure
                .cmp(&a.departure)
                .then_with(|| a.arrival.cmp(&b.arrival))
                .then_with(|| a.transfers.cmp(&b.transfers))
                .then_with(|| {
                    a.walk_distance_m
                        .partial_cmp(&b.walk_distance_m)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.duration_s.cmp(&b.duration_s))
        });
    } else {
        journeys.sort_by(|a, b| {
            a.arrival
                .cmp(&b.arrival)
                .then_with(|| a.duration_s.cmp(&b.duration_s))
                .then_with(|| a.transfers.cmp(&b.transfers))
                .then_with(|| a.departure.cmp(&b.departure))
                .then_with(|| {
                    a.walk_distance_m
                        .partial_cmp(&b.walk_distance_m)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
    }
    journeys.truncate(max_k);

    // Street-level walk/bike polylines (OSRM) for map display.
    for j in &mut journeys {
        let n = j.legs.len();
        for (i, leg) in j.legs.iter_mut().enumerate() {
            if let Leg::Walk(w) = leg {
                if i == 0 && q.bike_from {
                    w.mode = "BIKE".into();
                } else if i + 1 == n && q.bike_to {
                    w.mode = "BIKE".into();
                } else if w.mode.is_empty() {
                    w.mode = "WALK".into();
                }
            }
        }
        super::walk::enrich_non_transit_paths(j, osrm, from_profile, to_profile);
    }

    ItineraryResult {
        from: place_from_query_from(epoch, q),
        to: place_from_query_to(epoch, q),
        computed_at: Utc::now(),
        static_epoch_id: epoch.id.clone(),
        realtime_age_s: None,
        realtime_degraded: false,
        journeys,
    }
}

/// Depart-after k-best: RAPTOR from `dep_s0`, then successive later departures.
///
/// When `arrive_deadline_s` is set, only journeys with local arrival ≤ deadline are kept.
fn plan_depart_after(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    date: NaiveDate,
    dep_s0: u32,
    midnight: DateTime<Utc>,
    origins: &[AccessStop],
    targets: &[AccessStop],
    egress: &HashMap<u32, AccessStop>,
    origin_map: &HashMap<u32, AccessStop>,
    pool_cap: usize,
    arrive_deadline_s: Option<u32>,
) -> Vec<Journey> {
    let mut journeys = Vec::new();

    if let Some(mut j) = pure_walk_journey(epoch, q, origins, targets, dep_s0, midnight) {
        if arrive_deadline_s.map(|d| journey_arrival_s(&j, midnight) <= d).unwrap_or(true) {
            super::fare::apply_fare_estimate(epoch, &mut j);
            journeys.push(j);
        }
    }

    let mut dep_s = dep_s0;
    let mut seen_keys: HashSet<(i64, i64, u32)> = HashSet::new();
    let mut stagnant = 0u32;
    // Bound search so arrive-by coarse samples cannot loop forever.
    let mut iterations = 0u32;
    let max_iterations = (pool_cap as u32).saturating_mul(8).max(16);

    while journeys.len() < pool_cap && stagnant < 8 && iterations < max_iterations {
        iterations += 1;
        if let Some(deadline) = arrive_deadline_s {
            if dep_s >= deadline {
                break;
            }
        }

        let Some((state, target_idx, total_arr)) =
            run_raptor(epoch, q, date, dep_s, origins, egress)
        else {
            break;
        };

        if let Some(deadline) = arrive_deadline_s {
            if total_arr > deadline {
                // Earliest arrival from this seed is already too late — later seeds worse.
                // For a single seed this is correct; for arrive-by multi-seed caller advances.
                break;
            }
        }

        let Some(mut journey) = reconstruct(
            epoch,
            &state,
            target_idx,
            &egress[&target_idx],
            total_arr,
            midnight,
            origin_map,
            q,
            dep_s,
        ) else {
            // Path found in RAPTOR but chain rebuild failed — try a later seed
            // instead of aborting the whole depart-after loop.
            stagnant += 1;
            dep_s = dep_s.saturating_add(60);
            continue;
        };

        let j_dep = journey_first_departure(&journey).unwrap_or(journey.departure);
        let j_arr = journey.arrival;
        let key = (j_dep.timestamp(), j_arr.timestamp(), journey.transfers);
        if !seen_keys.insert(key) {
            if let Some(next) = next_departure_s(&journey, midnight, dep_s) {
                if next <= dep_s {
                    stagnant += 1;
                    dep_s = dep_s.saturating_add(60);
                } else {
                    dep_s = next;
                    stagnant = 0;
                }
            } else {
                stagnant += 1;
                dep_s = dep_s.saturating_add(60);
            }
            continue;
        }

        if let Some(fd) = journey_first_departure(&journey) {
            journey.departure = fd;
            journey.duration_s = (journey.arrival - journey.departure).num_seconds();
        }

        if let Some(deadline) = arrive_deadline_s {
            if journey_arrival_s(&journey, midnight) > deadline {
                if let Some(next) = next_departure_s(&journey, midnight, dep_s) {
                    dep_s = if next <= dep_s {
                        dep_s.saturating_add(60)
                    } else {
                        next
                    };
                } else {
                    break;
                }
                stagnant += 1;
                continue;
            }
        }

        super::fare::apply_fare_estimate(epoch, &mut journey);
        journeys.push(journey);

        if let Some(next) = next_departure_s(journeys.last().unwrap(), midnight, dep_s) {
            if next <= dep_s {
                dep_s = dep_s.saturating_add(60);
            } else {
                dep_s = next;
            }
            stagnant = 0;
        } else {
            break;
        }
    }

    journeys
}

/// Arrive-by: sample departures in a look-back window and keep journeys arriving by deadline.
fn plan_arrive_by(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    date: NaiveDate,
    deadline_s: u32,
    midnight: DateTime<Utc>,
    origins: &[AccessStop],
    targets: &[AccessStop],
    egress: &HashMap<u32, AccessStop>,
    origin_map: &HashMap<u32, AccessStop>,
    pool_cap: usize,
) -> Vec<Journey> {
    let earliest = deadline_s.saturating_sub(ARRIVE_BY_MAX_JOURNEY_S);
    let mut journeys: Vec<Journey> = Vec::new();
    let mut seen_keys: HashSet<(i64, i64, u32)> = HashSet::new();

    // Pure walk timed to arrive by the deadline (leave as late as needed).
    if let Some(mut j) = pure_walk_journey(epoch, q, origins, targets, earliest, midnight) {
        let dur = j.duration_s.max(0) as u32;
        if dur <= deadline_s {
            let leave_s = deadline_s.saturating_sub(dur);
            let dep = midnight + chrono::Duration::seconds(leave_s as i64);
            let arr = midnight + chrono::Duration::seconds(deadline_s as i64);
            j.departure = dep;
            j.arrival = arr;
            j.duration_s = (arr - dep).num_seconds();
            super::fare::apply_fare_estimate(epoch, &mut j);
            let key = (
                j.departure.timestamp(),
                j.arrival.timestamp(),
                j.transfers,
            );
            seen_keys.insert(key);
            journeys.push(j);
        }
    }

    // Coarse samples from early → late; each seed runs a short k-best with deadline filter.
    // Also always include a seed at `earliest` so long journeys are reachable.
    let mut sample = earliest;
    let mut samples: Vec<u32> = Vec::new();
    while sample < deadline_s {
        samples.push(sample);
        sample = sample.saturating_add(ARRIVE_BY_SAMPLE_STEP_S);
        if samples.len() > 48 {
            break;
        }
    }
    // Last chance: try just before deadline for very short trips.
    if samples.last().copied().unwrap_or(u32::MAX) + 60 < deadline_s {
        samples.push(deadline_s.saturating_sub(60));
    }

    // Per-sample budget so total work stays bounded.
    let per_sample = ((pool_cap / samples.len().max(1)) + 2).max(2);

    for seed in samples {
        let batch = plan_depart_after(
            epoch,
            q,
            date,
            seed,
            midnight,
            origins,
            targets,
            egress,
            origin_map,
            per_sample,
            Some(deadline_s),
        );
        for j in batch {
            let key = (
                journey_first_departure(&j)
                    .unwrap_or(j.departure)
                    .timestamp(),
                j.arrival.timestamp(),
                j.transfers,
            );
            if !seen_keys.insert(key) {
                continue;
            }
            if journey_arrival_s(&j, midnight) > deadline_s {
                continue;
            }
            journeys.push(j);
            if journeys.len() >= pool_cap * 2 {
                break;
            }
        }
        if journeys.len() >= pool_cap * 2 {
            break;
        }
    }

    journeys
}

/// Local seconds-since-midnight of journey arrival (clamped for same service day).
fn journey_arrival_s(j: &Journey, midnight: DateTime<Utc>) -> u32 {
    let secs = (j.arrival - midnight).num_seconds();
    if secs < 0 {
        0
    } else {
        secs as u32
    }
}

/// McRAPTOR-lite: keep only non-dominated journeys on
/// `(arrival↓, transfers↓, walk_distance↓)` plus **departure↑** so a later train with a later
/// arrival is not dropped as “dominated junk” by an earlier service (k-best diversity).
///
/// Journey A dominates B if A is ≤/≥ on all criteria and strictly better on at least one.
fn pareto_front(journeys: Vec<Journey>) -> Vec<Journey> {
    if journeys.len() <= 1 {
        return journeys;
    }
    let mut out: Vec<Journey> = Vec::with_capacity(journeys.len());
    for j in journeys {
        let dominated = out.iter().any(|o| dominates(o, &j));
        if dominated {
            continue;
        }
        // Drop anything already kept that `j` dominates.
        out.retain(|o| !dominates(&j, o));
        out.push(j);
    }
    out
}

/// True if `a` dominates `b` on the multi-criteria bag.
fn dominates(a: &Journey, b: &Journey) -> bool {
    let arr_le = a.arrival <= b.arrival;
    let tr_le = a.transfers <= b.transfers;
    let walk_le = a.walk_distance_m <= b.walk_distance_m + f64::EPSILON;
    // Later (or equal) departure is better: less waiting at origin for the same arrival quality.
    let dep_ge = a.departure >= b.departure;
    let strict = a.arrival < b.arrival
        || a.transfers < b.transfers
        || a.walk_distance_m < b.walk_distance_m - f64::EPSILON
        || a.departure > b.departure;
    arr_le && tr_le && walk_le && dep_ge && strict
}

fn journey_first_departure(j: &Journey) -> Option<DateTime<Utc>> {
    match j.legs.first()? {
        Leg::Walk(_) => Some(j.departure),
        Leg::Transit(t) => Some(t.scheduled_departure),
    }
}

fn next_departure_s(journey: &Journey, midnight: DateTime<Utc>, current_dep_s: u32) -> Option<u32> {
    // After the first transit departure (+1s), search again for a later option.
    for leg in &journey.legs {
        if let Leg::Transit(t) = leg {
            let since = (t.scheduled_departure - midnight).num_seconds();
            if since < 0 {
                return Some(current_dep_s.saturating_add(60));
            }
            let board_s = since as u32;
            return Some(board_s.saturating_add(1));
        }
    }
    None
}

fn pure_walk_journey(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    origins: &[AccessStop],
    targets: &[AccessStop],
    dep_s: u32,
    midnight: DateTime<Utc>,
) -> Option<Journey> {
    // Same stop as origin and destination.
    let mut best: Option<(u32, f64, u32, u32)> = None; // dur, dist, from_idx, to_idx
    for o in origins {
        for t in targets {
            if o.stop_idx == t.stop_idx {
                let dur = o.duration_s.saturating_add(t.duration_s);
                let dist = o.distance_m + t.distance_m;
                if best.map(|b| dur < b.0).unwrap_or(true) {
                    best = Some((dur, dist, o.stop_idx, t.stop_idx));
                }
            }
        }
    }
    // Also allow walking between different access/egress stops if within max walk.
    if best.is_none() {
        for o in origins {
            for t in targets {
                if o.stop_idx == t.stop_idx {
                    continue;
                }
                if let Some((wd, wdist)) = super::walk::walk_duration_between_opts(
                    epoch,
                    o.stop_idx,
                    t.stop_idx,
                    q.walk_speed_m_s,
                    q.max_walk_meters as f64,
                    q.wheelchair,
                ) {
                    let dur = o.duration_s.saturating_add(wd).saturating_add(t.duration_s);
                    let dist = o.distance_m + wdist + t.distance_m;
                    if best.map(|b| dur < b.0).unwrap_or(true) {
                        best = Some((dur, dist, o.stop_idx, t.stop_idx));
                    }
                }
            }
        }
    }

    let (dur, dist, from_idx, to_idx) = best?;
    // Only return pure walk if query is stop-to-stop nearby or same; skip if zero-distance same and no walk needed
    // when from and to are different stops with a long walk we still return it.
    let from = stop_ref(epoch, from_idx);
    let to = stop_ref(epoch, to_idx);
    let from_place = place_from_query_from(epoch, q);
    let to_place = place_from_query_to(epoch, q);

    let mut legs = Vec::new();
    // Access from query origin coords if different from stop
    if origins
        .iter()
        .find(|a| a.stop_idx == from_idx)
        .map(|a| a.duration_s > 0)
        .unwrap_or(false)
    {
        let a = origins.iter().find(|a| a.stop_idx == from_idx).unwrap();
        legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
            from_name: from_place.name.clone().unwrap_or_else(|| "origin".into()),
            to_name: from.name.clone(),
            from_stop_id: from_place.stop_id.clone(),
            to_stop_id: Some(from.stop_id.clone()),
            distance_m: a.distance_m,
            duration_s: a.duration_s,
            from_lat: from_place.lat,
            from_lon: from_place.lon,
            to_lat: from.lat,
            to_lon: from.lon,
            geometry: super::walk::walk_leg_geometry(
                a.geometry.as_deref(),
                from_place.lat,
                from_place.lon,
                from.lat,
                from.lon,
            ),
        }));
    }

    if from_idx != to_idx {
        if let Some((wd, wdist)) = super::walk::walk_duration_between_opts(
            epoch,
            from_idx,
            to_idx,
            q.walk_speed_m_s,
            q.max_walk_meters as f64,
            q.wheelchair,
        ) {
            legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
                from_name: from.name.clone(),
                to_name: to.name.clone(),
                from_stop_id: Some(from.stop_id.clone()),
                to_stop_id: Some(to.stop_id.clone()),
                distance_m: wdist,
                duration_s: wd,
                from_lat: from.lat,
                from_lon: from.lon,
                to_lat: to.lat,
                to_lon: to.lon,
                geometry: super::walk::walk_leg_geometry(
                    None, from.lat, from.lon, to.lat, to.lon,
                ),
            }));
        }
    }

    if targets
        .iter()
        .find(|a| a.stop_idx == to_idx)
        .map(|a| a.duration_s > 0)
        .unwrap_or(false)
    {
        let e = targets.iter().find(|a| a.stop_idx == to_idx).unwrap();
        let egress_geom = e.geometry.as_ref().map(|g| {
            let mut r = g.clone();
            r.reverse();
            r
        });
        legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
            from_name: to.name.clone(),
            to_name: to_place.name.clone().unwrap_or_else(|| "destination".into()),
            from_stop_id: Some(to.stop_id.clone()),
            to_stop_id: to_place.stop_id.clone(),
            distance_m: e.distance_m,
            duration_s: e.duration_s,
            from_lat: to.lat,
            from_lon: to.lon,
            to_lat: to_place.lat,
            to_lon: to_place.lon,
            geometry: super::walk::walk_leg_geometry(
                egress_geom.as_deref(),
                to.lat,
                to.lon,
                to_place.lat,
                to_place.lon,
            ),
        }));
    }

    if legs.is_empty() {
        // Same stop, zero walk — still a trivial journey.
        legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
            from_name: from.name.clone(),
            to_name: to.name.clone(),
            from_stop_id: Some(from.stop_id),
            to_stop_id: Some(to.stop_id),
            distance_m: 0.0,
            duration_s: 0,
            from_lat: from.lat,
            from_lon: from.lon,
            to_lat: to.lat,
            to_lon: to.lon,
            geometry: super::walk::walk_leg_geometry(
                None, from.lat, from.lon, to.lat, to.lon,
            ),
        }));
    }

    let departure = midnight + chrono::Duration::seconds(dep_s as i64);
    let arrival = midnight + chrono::Duration::seconds(dep_s.saturating_add(dur) as i64);
    Some(Journey {
        id: new_journey_id(),
        departure,
        arrival,
        duration_s: (arrival - departure).num_seconds(),
        transfers: 0,
        walk_distance_m: dist,
        realtime_status: "SCHEDULED".into(),
        legs,
        alert_headers: vec![],
        fare_amount: None,
        fare_currency: None,
        fare_note: None,
    })
}

/// Scheduled departure + GTFS-RT delay (seconds since midnight).
fn rt_effective_dep_s(
    q: &ItineraryQuery,
    trip_id: &str,
    st: &crate::gtfs::pack::PackedStopTime,
) -> u32 {
    let delay = q
        .rt_adjust
        .get(trip_id)
        .map(|a| a.dep_delay_for(st.stop_sequence))
        .unwrap_or(0);
    (st.departure_s as i64 + delay as i64).max(0) as u32
}

/// Alight time: prefer stop-level RT arrival delay, else propagate board `time_shift`.
fn rt_effective_arr_s(
    q: &ItineraryQuery,
    trip_id: &str,
    st: &crate::gtfs::pack::PackedStopTime,
    time_shift: i32,
) -> u32 {
    if let Some(a) = q.rt_adjust.get(trip_id) {
        if a.arr_delay.contains_key(&st.stop_sequence)
            || a.dep_delay.contains_key(&st.stop_sequence)
        {
            let d = a.arr_delay_for(st.stop_sequence);
            return (st.arrival_s as i64 + d as i64).max(0) as u32;
        }
    }
    (st.arrival_s as i64 + time_shift as i64).max(0) as u32
}

/// Stop-based transit stage.
fn raptor_transit_stop_based(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    round: usize,
    round_stamp: u32,
    date: NaiveDate,
    marked: &HashSet<u32>,
    best: &mut [u32],
    best_dest: &mut u32,
    labels: &mut [Vec<Option<Reach>>],
    next_marked: &mut HashSet<u32>,
    egress: &HashMap<u32, AccessStop>,
    cal_active: &mut HashMap<(String, String, NaiveDate), bool>,
    boarded_gen: &mut [u32],
    boarded_data: &mut [(u32, u32, u32, i32)],
    mode_ok: &impl Fn(RouteMode) -> bool,
    trip_ok: &impl Fn(&crate::gtfs::pack::GlobalTrip) -> bool,
) {
    for &stop_idx in marked {
        let arrival_here = best[stop_idx as usize];
        if arrival_here == u32::MAX {
            continue;
        }
        let board_after = if round == 0 {
            arrival_here
        } else {
            arrival_here.saturating_add(q.default_transfer_s)
        };
        if board_after >= *best_dest {
            continue;
        }

        for &(trip_idx, st_off) in epoch
            .stop_departures
            .get(stop_idx as usize)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
        {
            let trip = &epoch.trips[trip_idx as usize];
            if !mode_ok(trip.mode) || !trip_ok(trip) {
                continue;
            }
            let cal_key = (
                trip.feed_id.clone(),
                trip.service_id.clone(),
                date,
            );
            let active = *cal_active.entry(cal_key).or_insert_with(|| {
                epoch
                    .calendars
                    .get(&trip.feed_id)
                    .map(|cal| cal.is_active(&trip.service_id, date))
                    .unwrap_or(true)
            });
            if !active {
                continue;
            }

            let st_board = &epoch.stop_times[(trip.stop_time_start + st_off) as usize];
            if st_board.pickup_type == 1 {
                continue;
            }
            if q.rt_adjust
                .get(&trip.id)
                .is_some_and(|a| a.is_skipped(st_board.stop_sequence))
            {
                continue;
            }

            let (board_dep, time_shift) = if !trip.frequency_windows.is_empty() {
                let base = trip_template_base(epoch, trip);
                let sched_dep = rt_effective_dep_s(q, &trip.id, st_board);
                match earliest_freq_board(trip, base, sched_dep, board_after) {
                    Some(v) => v,
                    None => continue,
                }
            } else {
                let dep_rt = rt_effective_dep_s(q, &trip.id, st_board);
                if dep_rt < board_after {
                    continue;
                }
                let shift = dep_rt as i64 - st_board.departure_s as i64;
                (dep_rt, shift as i32)
            };
            if board_dep >= *best_dest {
                continue;
            }

            let ti = trip_idx as usize;
            if boarded_gen[ti] != round_stamp {
                boarded_gen[ti] = round_stamp;
                boarded_data[ti] = (stop_idx, st_off, board_dep, time_shift);
            } else {
                let (_, prev_off, prev_dep, _) = boarded_data[ti];
                if board_dep < prev_dep || (board_dep == prev_dep && st_off < prev_off) {
                    boarded_data[ti] = (stop_idx, st_off, board_dep, time_shift);
                }
            }
        }
    }

    for (trip_idx, trip) in epoch.trips.iter().enumerate() {
        if boarded_gen[trip_idx] != round_stamp {
            continue;
        }
        let (board_stop, board_off, _board_dep, time_shift) = boarded_data[trip_idx];
        for off in (board_off + 1)..trip.stop_time_len {
            let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
            if st.drop_off_type == 1 {
                continue;
            }
            if q.rt_adjust
                .get(&trip.id)
                .is_some_and(|a| a.is_skipped(st.stop_sequence))
            {
                continue;
            }
            let arr = rt_effective_arr_s(q, &trip.id, st, time_shift);
            if arr >= *best_dest {
                break;
            }
            let si = st.stop_idx as usize;

            if arr < best[si] {
                best[si] = arr;
                labels[round + 1][si] = Some(Reach::Transit {
                    arrival_s: arr,
                    trip_idx: trip_idx as u32,
                    board_stop,
                    board_off,
                    alight_off: off,
                    prev_round: round as u16,
                    time_shift_s: time_shift,
                });
                next_marked.insert(st.stop_idx);
                if let Some(eg) = egress.get(&st.stop_idx) {
                    let total = arr.saturating_add(eg.duration_s);
                    if total < *best_dest {
                        *best_dest = total;
                    }
                }
            }
        }
    }
}

fn run_raptor(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    date: NaiveDate,
    dep_s: u32,
    origins: &[AccessStop],
    egress: &HashMap<u32, AccessStop>,
) -> Option<(RaptorState, u32, u32)> {
    let n = epoch.stops.len();
    let trip_n = epoch.trips.len();
    // Need enough rounds for transit→walk→transit (RER then Métro). Do not starve
    // multi-mode by min(max_transfers+1) when max_transfers is low.
    let max_rounds = (q
        .raptor_max_rounds
        .max(q.max_transfers.saturating_add(2))
        .max(4) as usize)
        .min(12)
        .max(1);

    let mut best = vec![u32::MAX; n];
    let mut labels: Vec<Vec<Option<Reach>>> = vec![vec![None; n]; max_rounds + 1];
    let mut best_dest = u32::MAX;

    // Round 0: access.
    let mut marked: HashSet<u32> = HashSet::new();
    for o in origins {
        let arr = dep_s.saturating_add(o.duration_s);
        let si = o.stop_idx as usize;
        if arr < best[si] {
            best[si] = arr;
            labels[0][si] = Some(Reach::Access {
                stop_idx: o.stop_idx,
                arrival_s: arr,
                walk_duration: o.duration_s,
                walk_distance: o.distance_m,
            });
            marked.insert(o.stop_idx);
        }
        if let Some(eg) = egress.get(&o.stop_idx) {
            let total = arr.saturating_add(eg.duration_s);
            if total < best_dest {
                best_dest = total;
            }
        }
    }
    if marked.is_empty() {
        return None;
    }

    let mode_ok = |m: RouteMode| {
        q.modes
            .as_ref()
            .map(|ms| ms.is_empty() || ms.contains(&m))
            .unwrap_or(true)
    };

    let trip_ok = |trip: &crate::gtfs::pack::GlobalTrip| {
        if q.excluded_trip_ids.contains(&trip.id) {
            return false;
        }
        // Soft wheelchair filter: exclude explicitly inaccessible trips (GTFS 2).
        if q.wheelchair && trip.wheelchair == 2 {
            return false;
        }
        true
    };

    // Per-run caches (hot on IDFM-sized epochs).
    let mut cal_active: HashMap<(String, String, NaiveDate), bool> = HashMap::new();
    let mut boarded_gen: Vec<u32> = vec![0; trip_n];
    let mut boarded_data: Vec<(u32, u32, u32, i32)> = vec![(0, 0, 0, 0); trip_n];
    let mut round_stamp: u32 = 0;

    for round in 0..max_rounds {
        if marked.is_empty() {
            break;
        }

        round_stamp = round_stamp.wrapping_add(1);
        let mut next_marked: HashSet<u32> = HashSet::new();

        raptor_transit_stop_based(
            epoch,
            q,
            round,
            round_stamp,
            date,
            &marked,
            &mut best,
            &mut best_dest,
            &mut labels,
            &mut next_marked,
            egress,
            &mut cal_active,
            &mut boarded_gen,
            &mut boarded_data,
            &mode_ok,
            &trip_ok,
        );

        // --- Walk transfer stage from stops improved by transit this round ---
        let transit_improved: Vec<u32> = next_marked.iter().copied().collect();
        for stop_idx in transit_improved {
            let arrival_here = best[stop_idx as usize];
            for &ei in epoch
                .walk_adj
                .get(stop_idx as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                let e = &epoch.walk_edges[ei];
                let walk_dur = e.effective_duration_s(q.wheelchair);
                let arr = arrival_here.saturating_add(walk_dur);
                if arr >= best_dest {
                    continue;
                }
                let ti = e.to_stop_idx as usize;
                if arr < best[ti] {
                    best[ti] = arr;
                    labels[round + 1][ti] = Some(Reach::Walk {
                        arrival_s: arr,
                        from_stop: stop_idx,
                        to_stop: e.to_stop_idx,
                        duration_s: walk_dur,
                        distance_m: e.distance_m,
                        prev_round: (round + 1) as u16,
                    });
                    next_marked.insert(e.to_stop_idx);
                    if let Some(eg) = egress.get(&e.to_stop_idx) {
                        let total = arr.saturating_add(eg.duration_s);
                        if total < best_dest {
                            best_dest = total;
                        }
                    }
                }
            }
        }

        marked = next_marked;
    }

    // Best target
    let mut best_target: Option<(u32, u32, u32)> = None; // total, stop_idx, arr_at_stop
    for (t_idx, eg) in egress {
        let arr_at_stop = best[*t_idx as usize];
        if arr_at_stop == u32::MAX {
            continue;
        }
        let total = arr_at_stop.saturating_add(eg.duration_s);
        if best_target.map(|b| total < b.0).unwrap_or(true) {
            best_target = Some((total, *t_idx, arr_at_stop));
        }
    }

    let (total_arr, target_idx, _) = best_target?;
    let _ = best; // consumed during search only
    Some((RaptorState { labels }, target_idx, total_arr))
}

fn reconstruct(
    epoch: &StaticEpoch,
    state: &RaptorState,
    target_idx: u32,
    egress: &AccessStop,
    total_arr: u32,
    midnight: DateTime<Utc>,
    origins: &HashMap<u32, AccessStop>,
    q: &ItineraryQuery,
    query_dep_s: u32,
) -> Option<Journey> {
    // Find latest round with a label at target (prefer earliest arrival among labels).
    let mut best_round: Option<usize> = None;
    let mut best_arr = u32::MAX;
    for r in 0..state.labels.len() {
        if let Some(lab) = &state.labels[r][target_idx as usize] {
            let a = lab.arrival_s();
            if a < best_arr || (a == best_arr && best_round.map(|br| r > br).unwrap_or(true)) {
                // Prefer fewer rounds for same arrival (fewer transfers).
                if a < best_arr {
                    best_arr = a;
                    best_round = Some(r);
                } else if a == best_arr {
                    // keep smaller round
                    if best_round.map(|br| r < br).unwrap_or(true) {
                        best_round = Some(r);
                    }
                }
            }
        }
    }
    // Also accept target if only in best[] from a walk in some round — labels should cover it.
    let mut round = best_round? as i32;
    let mut stop = target_idx;
    let mut chain: Vec<Reach> = Vec::new();

    while round >= 0 {
        let lab = state.labels[round as usize][stop as usize]
            .clone()
            .or_else(|| {
                // Search earlier rounds for a label at this stop with same best arrival.
                for r in (0..=round as usize).rev() {
                    if let Some(l) = &state.labels[r][stop as usize] {
                        return Some(l.clone());
                    }
                }
                None
            })?;
        chain.push(lab.clone());
        match lab {
            Reach::Access { .. } => break,
            Reach::Transit {
                board_stop,
                prev_round,
                ..
            } => {
                stop = board_stop;
                round = prev_round as i32;
            }
            Reach::Walk {
                from_stop,
                ..
            } => {
                // Walk is written into the same labels row as the transit (or prior
                // walk) that improved `from_stop` this RAPTOR round (`labels[round+1]`).
                // Do **not** jump to `prev_round` — that row holds the *previous*
                // round and is empty for mid-journey transfer hubs, which broke
                // multi-leg reconstruction (RER→walk→Métro).
                stop = from_stop;
            }
        }
    }
    chain.reverse();
    if chain.is_empty() {
        return None;
    }

    let mut legs: Vec<Leg> = Vec::new();
    let mut walk_distance = 0.0f64;
    let mut transit_count = 0u32;
    let from_place = place_from_query_from(epoch, q);
    let to_place = place_from_query_to(epoch, q);

    // Access walk leg before first transit/walk if origin access duration > 0.
    if let Some(first) = chain.first() {
        match first {
            Reach::Access {
                stop_idx,
                walk_duration,
                walk_distance: wd,
                ..
            } if *walk_duration > 0 => {
                let to = stop_ref(epoch, *stop_idx);
                let access = origins.get(stop_idx);
                legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
                    from_name: from_place.name.clone().unwrap_or_else(|| "origin".into()),
                    to_name: to.name.clone(),
                    from_stop_id: from_place.stop_id.clone(),
                    to_stop_id: Some(to.stop_id.clone()),
                    distance_m: *wd,
                    duration_s: *walk_duration,
                    from_lat: from_place.lat,
                    from_lon: from_place.lon,
                    to_lat: to.lat,
                    to_lon: to.lon,
                    geometry: super::walk::walk_leg_geometry(
                        access.and_then(|a| a.geometry.as_deref()),
                        from_place.lat,
                        from_place.lon,
                        to.lat,
                        to.lon,
                    ),
                }));
                walk_distance += wd;
            }
            Reach::Transit { board_stop, .. } | Reach::Walk { from_stop: board_stop, .. } => {
                if let Some(o) = origins.get(board_stop) {
                    if o.duration_s > 0 {
                        let to = stop_ref(epoch, *board_stop);
                        legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
                            from_name: from_place.name.clone().unwrap_or_else(|| "origin".into()),
                            to_name: to.name.clone(),
                            from_stop_id: from_place.stop_id.clone(),
                            to_stop_id: Some(to.stop_id.clone()),
                            distance_m: o.distance_m,
                            duration_s: o.duration_s,
                            from_lat: from_place.lat,
                            from_lon: from_place.lon,
                            to_lat: to.lat,
                            to_lon: to.lon,
                            geometry: super::walk::walk_leg_geometry(
                                o.geometry.as_deref(),
                                from_place.lat,
                                from_place.lon,
                                to.lat,
                                to.lon,
                            ),
                        }));
                        walk_distance += o.distance_m;
                    }
                }
            }
            Reach::Access { .. } => {}
        }
    }

    for lab in &chain {
        match lab {
            Reach::Access { .. } => {
                // Already emitted as access walk if needed.
            }
            Reach::Transit {
                trip_idx,
                board_stop,
                board_off,
                alight_off,
                time_shift_s,
                ..
            } => {
                let trip = &epoch.trips[*trip_idx as usize];
                let st_board =
                    &epoch.stop_times[(trip.stop_time_start + *board_off) as usize];
                let st_alight =
                    &epoch.stop_times[(trip.stop_time_start + *alight_off) as usize];
                // Frequency trips use template stop_times; wall-clock offset is in time_shift.
                // Timed trips keep pure GTFS schedule here — RT enrichment applies delays once.
                let shift = if trip.frequency_windows.is_empty() {
                    0i64
                } else {
                    *time_shift_s as i64
                };
                let from = stop_ref(epoch, *board_stop);
                let to = stop_ref(epoch, st_alight.stop_idx);

                let mut intermediate = Vec::new();
                for off in (*board_off + 1)..*alight_off {
                    let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
                    intermediate.push(IntermediateStop {
                        stop: stop_ref(epoch, st.stop_idx),
                        scheduled_arrival: midnight
                            + chrono::Duration::seconds(st.arrival_s as i64 + shift),
                        scheduled_departure: midnight
                            + chrono::Duration::seconds(st.departure_s as i64 + shift),
                        realtime_arrival: None,
                        realtime_departure: None,
                    });
                }

                let board_hs = epoch
                    .headsign_pool
                    .get(st_board.stop_headsign_idx as usize)
                    .filter(|s| !s.is_empty())
                    .cloned();
                let stop_headsign = board_hs.or_else(|| trip.headsign.clone());

                let geom = crate::routing::geometry::leg_geometry(
                    epoch,
                    &trip.id,
                    *board_off,
                    *alight_off,
                )
                .into_iter()
                .map(|p| (p[0], p[1]))
                .collect();

                // Stay-seated: consecutive transit legs share feed + non-empty block_id.
                let same_vehicle = legs.iter().rev().find_map(|l| match l {
                    Leg::Transit(prev) => {
                        let prev_trip = epoch
                            .trip_id_to_idx
                            .get(&prev.trip_id)
                            .and_then(|&i| epoch.trips.get(i as usize));
                        match prev_trip {
                            Some(pt)
                                if pt.feed_id == trip.feed_id
                                    && pt.block_id.is_some()
                                    && pt.block_id == trip.block_id =>
                            {
                                Some(true)
                            }
                            _ => Some(false),
                        }
                    }
                    _ => None,
                })
                .unwrap_or(false);

                legs.push(Leg::Transit(TransitLegData {
                    mode: trip.mode.as_str().to_string(),
                    route_short_name: trip.route_short_name.clone(),
                    route_long_name: trip.route_long_name.clone(),
                    route_id: trip.route_id.clone(),
                    agency_name: trip.agency_name.clone(),
                    trip_id: trip.id.clone(),
                    trip_short_name: trip.short_name.clone(),
                    headsign: trip.headsign.clone(),
                    direction_id: trip.direction_id,
                    route_color: trip.route_color.clone(),
                    route_text_color: trip.route_text_color.clone(),
                    stop_headsign,
                    wheelchair: trip.wheelchair,
                    bikes_allowed: trip.bikes_allowed,
                    from,
                    to,
                    from_stop_sequence: st_board.stop_sequence,
                    to_stop_sequence: st_alight.stop_sequence,
                    scheduled_departure: midnight
                        + chrono::Duration::seconds(st_board.departure_s as i64 + shift),
                    scheduled_arrival: midnight
                        + chrono::Duration::seconds(st_alight.arrival_s as i64 + shift),
                    realtime_departure: None,
                    realtime_arrival: None,
                    delay_departure_s: None,
                    delay_arrival_s: None,
                    canceled: false,
                    vehicle_lat: None,
                    vehicle_lon: None,
                    vehicle_updated_at: None,
                    intermediate_stops: intermediate,
                    geometry: geom,
                    same_vehicle,
                }));
                transit_count += 1;
            }
            Reach::Walk {
                from_stop,
                to_stop,
                duration_s,
                distance_m,
                ..
            } => {
                let from = stop_ref(epoch, *from_stop);
                let to = stop_ref(epoch, *to_stop);
                walk_distance += distance_m;
                legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
                    from_name: from.name.clone(),
                    to_name: to.name.clone(),
                    from_stop_id: Some(from.stop_id),
                    to_stop_id: Some(to.stop_id),
                    distance_m: *distance_m,
                    duration_s: *duration_s,
                    from_lat: from.lat,
                    from_lon: from.lon,
                    to_lat: to.lat,
                    to_lon: to.lon,
                    geometry: super::walk::walk_leg_geometry(
                        None, from.lat, from.lon, to.lat, to.lon,
                    ),
                }));
            }
        }
    }

    // Egress walk
    if egress.duration_s > 0 {
        let from_stop = stop_ref(epoch, target_idx);
        let egress_geom = egress.geometry.as_ref().map(|g| {
            let mut r = g.clone();
            r.reverse();
            r
        });
        legs.push(Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
            from_name: from_stop.name.clone(),
            to_name: to_place
                .name
                .clone()
                .unwrap_or_else(|| "destination".into()),
            from_stop_id: Some(from_stop.stop_id),
            to_stop_id: to_place.stop_id.clone(),
            distance_m: egress.distance_m,
            duration_s: egress.duration_s,
            from_lat: from_stop.lat,
            from_lon: from_stop.lon,
            to_lat: to_place.lat,
            to_lon: to_place.lon,
            geometry: super::walk::walk_leg_geometry(
                egress_geom.as_deref(),
                from_stop.lat,
                from_stop.lon,
                to_place.lat,
                to_place.lon,
            ),
        }));
        walk_distance += egress.distance_m;
    }

    let transfers = transit_count.saturating_sub(1);

    // Journey departure: query departure time (includes waiting at first stop).
    let departure = midnight + chrono::Duration::seconds(query_dep_s as i64);
    let arrival = midnight + chrono::Duration::seconds(total_arr as i64);

    // Tighten departure to first transit departure minus access walk for display.
    let mut disp_dep = departure;
    if let Some(Leg::Transit(t)) = legs.iter().find(|l| matches!(l, Leg::Transit(_))) {
        // If first leg is transit, departure is board time; if walk then transit, keep query dep
        // or access start.
        if matches!(legs.first(), Some(Leg::Transit(_))) {
            disp_dep = t.scheduled_departure;
        }
    }

    Some(Journey {
        id: new_journey_id(),
        departure: disp_dep,
        arrival,
        duration_s: (arrival - disp_dep).num_seconds(),
        transfers,
        walk_distance_m: walk_distance,
        realtime_status: "SCHEDULED".into(),
        legs,
        alert_headers: vec![],
        fare_amount: None,
        fare_currency: None,
        fare_note: None,
    })
}

fn place_from_query_from(epoch: &StaticEpoch, q: &ItineraryQuery) -> Place {
    if let Some(ref id) = q.from_stop_id {
        if let Some(s) = epoch.get_stop(id) {
            return Place {
                stop_id: Some(s.id.clone()),
                name: Some(s.name.clone()),
                lat: s.lat,
                lon: s.lon,
            };
        }
    }
    Place {
        stop_id: q.from_stop_id.clone(),
        name: None,
        lat: q.from_lat,
        lon: q.from_lon,
    }
}

fn place_from_query_to(epoch: &StaticEpoch, q: &ItineraryQuery) -> Place {
    if let Some(ref id) = q.to_stop_id {
        if let Some(s) = epoch.get_stop(id) {
            return Place {
                stop_id: Some(s.id.clone()),
                name: Some(s.name.clone()),
                lat: s.lat,
                lon: s.lon,
            };
        }
    }
    Place {
        stop_id: q.to_stop_id.clone(),
        name: None,
        lat: q.to_lat,
        lon: q.to_lon,
    }
}

fn empty_result(epoch: &StaticEpoch, q: &ItineraryQuery) -> ItineraryResult {
    ItineraryResult {
        from: place_from_query_from(epoch, q),
        to: place_from_query_to(epoch, q),
        computed_at: Utc::now(),
        static_epoch_id: epoch.id.clone(),
        realtime_age_s: None,
        realtime_degraded: false,
        journeys: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::calendar::ServiceCalendar;
    use crate::gtfs::pack::{
        FrequencyWindow, GlobalTrip, PackedStopTime, RouteMode, StaticEpoch,
        StopRecord, WalkEdge,
    };
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn stop(id: &str, name: &str, lat: f64, lon: f64) -> StopRecord {
        StopRecord {
            id: id.into(),
            feed_id: "test".into(),
            raw_id: id.into(),
            name: name.into(),
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

    /// Tiny network:
    ///   A --trip1--> B --trip1--> C
    ///   B --trip2--> D
    /// walk B <-> X (transfer hub sibling)
    fn fixture_epoch() -> StaticEpoch {
        let mut epoch = StaticEpoch::empty();
        epoch.id = "fixture".into();

        epoch.stops = vec![
            stop("test:A", "Alpha", 48.86, 2.30),
            stop("test:B", "Beta", 48.87, 2.31),
            stop("test:C", "Gamma", 48.88, 2.32),
            stop("test:D", "Delta", 48.875, 2.315),
        ];
        for (i, s) in epoch.stops.iter().enumerate() {
            epoch.stop_id_to_idx.insert(s.id.clone(), i as u32);
        }

        // Trip 1: A@8:00 -> B@8:10 -> C@8:25
        // Trip 2: B@8:20 -> D@8:35  (transfer at B from trip1)
        epoch.stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 8 * 3600,
                departure_s: 8 * 3600,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 8 * 3600 + 10 * 60,
                departure_s: 8 * 3600 + 10 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 2,
                arrival_s: 8 * 3600 + 25 * 60,
                departure_s: 8 * 3600 + 25 * 60,
                stop_sequence: 3,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            // trip2
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 8 * 3600 + 20 * 60,
                departure_s: 8 * 3600 + 20 * 60,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 3,
                arrival_s: 8 * 3600 + 35 * 60,
                departure_s: 8 * 3600 + 35 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            // Later direct A->C trip3 for k-best
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 9 * 3600,
                departure_s: 9 * 3600,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 2,
                arrival_s: 9 * 3600 + 20 * 60,
                departure_s: 9 * 3600 + 20 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
        ];

        epoch.trips = vec![
            GlobalTrip {
                id: "test:t1".into(),
                feed_id: "test".into(),
                route_id: "test:r1".into(),
                service_id: "svc".into(),
                headsign: Some("To C".into()),
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
                agency_name: Some("TestBus".into()),
                stop_time_start: 0,
                stop_time_len: 3,
            frequency_windows: vec![],
            },
            GlobalTrip {
                id: "test:t2".into(),
                feed_id: "test".into(),
                route_id: "test:r2".into(),
                service_id: "svc".into(),
                headsign: Some("To D".into()),
                short_name: None,
                direction_id: None,
                wheelchair: 0,
                bikes_allowed: 0,
                block_id: None,
                shape_id: None,
                mode: RouteMode::Metro,
                route_short_name: "M".into(),
                route_long_name: "Metro M".into(),
                route_color: None,
                route_text_color: None,
                route_type_raw: 1,
                agency_name: Some("TestMetro".into()),
                stop_time_start: 3,
                stop_time_len: 2,
            frequency_windows: vec![],
            },
            GlobalTrip {
                id: "test:t3".into(),
                feed_id: "test".into(),
                route_id: "test:r1".into(),
                service_id: "svc".into(),
                headsign: Some("Express C".into()),
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
                agency_name: Some("TestBus".into()),
                stop_time_start: 5,
                stop_time_len: 2,
            frequency_windows: vec![],
            },
        ];
        for (i, t) in epoch.trips.iter().enumerate() {
            epoch.trip_id_to_idx.insert(t.id.clone(), i as u32);
        }

        epoch.stop_departures = vec![Vec::new(); epoch.stops.len()];
        for (trip_idx, trip) in epoch.trips.iter().enumerate() {
            for off in 0..trip.stop_time_len {
                let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
                if st.pickup_type != 1 {
                    epoch.stop_departures[st.stop_idx as usize].push((trip_idx as u32, off));
                }
            }
        }

        let mut cal = ServiceCalendar::new();
        let start = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2030, 12, 31).unwrap();
        cal.add_regular(
            "svc".into(),
            start,
            end,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
        );
        epoch.calendars = HashMap::from([("test".into(), Arc::new(cal))]);

        epoch.walk_edges = vec![];
        epoch.walk_adj = vec![Vec::new(); epoch.stops.len()];
        epoch
    }

    fn query(from: &str, to: &str, hour: u32, minute: u32) -> ItineraryQuery {
        let dep = Utc.with_ymd_and_hms(2026, 7, 27, hour, minute, 0).unwrap();
        ItineraryQuery {
            from_stop_id: Some(from.into()),
            to_stop_id: Some(to.into()),
            from_lat: None,
            from_lon: None,
            to_lat: None,
            to_lon: None,
            departure_at: dep,
            arrive_by: false,
            max_transfers: 4,
            max_results: 5,
            modes: None,
            max_walk_meters: 800,
            walk_speed_m_s: 1.2,
            raptor_max_rounds: 6,
            default_transfer_s: 60,
            timezone: "UTC".into(),
            excluded_trip_ids: HashSet::new(),
            rt_adjust: std::collections::HashMap::new(),
            wheelchair: false,
            osrm_url: None,
            bike_from: false,
            bike_to: false,
            bike_speed_m_s: 4.2,
            max_bike_meters: 5000,
        }
    }

    #[test]
    fn rt_delay_shifts_boarding_time() {
        use crate::routing::RtTripAdjust;
        use std::collections::HashMap;
        let epoch = fixture_epoch();
        // Without RT: board t1 A at 08:00, query at 07:50 → ok
        let q0 = query("test:A", "test:B", 7, 50);
        assert!(!plan_journeys(&epoch, &q0).journeys.is_empty());

        // With +30 min trip delay, board becomes 08:30 — still reachable from 07:50
        let adj = RtTripAdjust {
            trip_delay_s: 30 * 60,
            ..Default::default()
        };
        let mut q = query("test:A", "test:B", 7, 50);
        q.rt_adjust = HashMap::from([("test:t1".into(), adj.clone())]);
        let res = plan_journeys(&epoch, &q);
        assert!(
            !res.journeys.is_empty(),
            "delayed trip should still be boardable from 07:50"
        );
        // Arrival should be shifted by ~30 min vs undelayed (B was 08:10 → ~08:40)
        let arr0 = plan_journeys(&epoch, &q0).journeys[0].arrival;
        let arr_d = res.journeys[0].arrival;
        assert!(
            (arr_d - arr0).num_seconds() >= 25 * 60,
            "RT delay should push arrival later: undelayed={arr0} delayed={arr_d}"
        );

        // After delayed departure (08:35), t1 A→B is no longer boardable
        let mut q_late = query("test:A", "test:B", 8, 35);
        q_late.rt_adjust = HashMap::from([("test:t1".into(), adj)]);
        let res_late = plan_journeys(&epoch, &q_late);
        let used_t1 = res_late.journeys.iter().any(|j| {
            j.legs.iter().any(|l| matches!(l, crate::routing::Leg::Transit(t) if t.trip_id == "test:t1"))
        });
        assert!(
            !used_t1,
            "should not board t1 after its delayed departure time"
        );
    }

    #[test]
    fn direct_trip_a_to_b() {
        let epoch = fixture_epoch();
        let q = query("test:A", "test:B", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(
            !res.journeys.is_empty(),
            "expected direct journey A→B"
        );
        let j = &res.journeys[0];
        let transit: Vec<_> = j
            .legs
            .iter()
            .filter(|l| matches!(l, Leg::Transit(_)))
            .collect();
        assert_eq!(transit.len(), 1);
        if let Leg::Transit(t) = transit[0] {
            assert_eq!(t.trip_id, "test:t1");
            assert_eq!(t.from.stop_id, "test:A");
            assert_eq!(t.to.stop_id, "test:B");
            assert!(t.intermediate_stops.is_empty());
        }
        assert_eq!(j.transfers, 0);
    }

    #[test]
    fn one_transfer_a_to_d() {
        let epoch = fixture_epoch();
        let q = query("test:A", "test:D", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(!res.journeys.is_empty(), "expected A→B→D with transfer");
        let j = &res.journeys[0];
        let transit: Vec<_> = j
            .legs
            .iter()
            .filter_map(|l| match l {
                Leg::Transit(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(transit.len(), 2, "legs: {:?}", j.legs.len());
        assert_eq!(transit[0].trip_id, "test:t1");
        assert_eq!(transit[1].trip_id, "test:t2");
        assert_eq!(j.transfers, 1);
        // Intermediate stop B on trip1 when going A→C would appear; A→B has none.
        assert!(transit[0].intermediate_stops.is_empty());
    }

    /// Multi-mode style: alight trip1 at B, **walk** to B2, board trip to D.
    /// Regression: reconstruct used to jump to the wrong labels row on Walk and
    /// drop RER→walk→Métro journeys.
    #[test]
    fn one_transfer_via_walk_edge() {
        let mut epoch = fixture_epoch();
        // B2: metro board a short walk from B (index 1).
        let b2 = epoch.stops.len() as u32;
        epoch.stops.push(stop("test:B2", "Bravo 2", 48.001, 2.001));
        epoch.stop_id_to_idx.insert("test:B2".into(), b2);
        epoch.stop_departures.push(Vec::new());
        epoch.walk_adj.push(Vec::new());

        // Replace trip2 board stop B → B2.
        let t2_start = epoch.trips[1].stop_time_start as usize;
        epoch.stop_times[t2_start].stop_idx = b2;
        // Rebuild stop_departures for B / B2.
        epoch.stop_departures = vec![Vec::new(); epoch.stops.len()];
        for (trip_idx, trip) in epoch.trips.iter().enumerate() {
            for off in 0..trip.stop_time_len {
                let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
                if st.pickup_type != 1 {
                    epoch.stop_departures[st.stop_idx as usize].push((trip_idx as u32, off));
                }
            }
        }
        // Bidirectional walk B ↔ B2.
        let ei = epoch.walk_edges.len();
        epoch.walk_edges.push(WalkEdge {
            from_stop_idx: 1,
            to_stop_idx: b2,
            duration_s: 90,
            distance_m: 100.0,
            pathway_mode: None,
        });
        epoch.walk_edges.push(WalkEdge {
            from_stop_idx: b2,
            to_stop_idx: 1,
            duration_s: 90,
            distance_m: 100.0,
            pathway_mode: None,
        });
        epoch.walk_adj = vec![Vec::new(); epoch.stops.len()];
        for (i, e) in epoch.walk_edges.iter().enumerate() {
            epoch.walk_adj[e.from_stop_idx as usize].push(i);
        }
        let _ = ei;

        let q = query("test:A", "test:D", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(
            !res.journeys.is_empty(),
            "expected A→B walk→B2→D, got empty"
        );
        let j = &res.journeys[0];
        let transit: Vec<_> = j
            .legs
            .iter()
            .filter_map(|l| match l {
                Leg::Transit(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(transit.len(), 2, "legs: {:?}", j.legs);
        assert_eq!(transit[0].to.stop_id, "test:B");
        assert_eq!(transit[1].from.stop_id, "test:B2");
        assert!(
            j.legs.iter().any(|l| matches!(l, Leg::Walk(_))),
            "expected walk transfer leg"
        );
    }

    #[test]
    fn direct_with_intermediate_a_to_c() {
        let epoch = fixture_epoch();
        let q = query("test:A", "test:C", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(!res.journeys.is_empty());
        let j = &res.journeys[0];
        let t = j
            .legs
            .iter()
            .find_map(|l| match l {
                Leg::Transit(t) => Some(t),
                _ => None,
            })
            .expect("transit leg");
        assert_eq!(t.trip_id, "test:t1");
        assert_eq!(t.intermediate_stops.len(), 1);
        assert_eq!(t.intermediate_stops[0].stop.stop_id, "test:B");
    }

    #[test]
    fn no_path_returns_empty() {
        let mut epoch = fixture_epoch();
        // Isolated stop E with no trips
        epoch.stops.push(stop("test:E", "Epsilon", 49.0, 3.0));
        epoch
            .stop_id_to_idx
            .insert("test:E".into(), (epoch.stops.len() - 1) as u32);
        epoch.stop_departures.push(Vec::new());
        epoch.walk_adj.push(Vec::new());

        let q = query("test:A", "test:E", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(
            res.journeys.is_empty(),
            "expected no path to isolated stop, got {}",
            res.journeys.len()
        );
    }

    #[test]
    fn excluded_trip_skips_boarding() {
        let epoch = fixture_epoch();
        let mut q = query("test:A", "test:B", 7, 50);
        q.excluded_trip_ids.insert("test:t1".into());
        // t3 is A→C only, not B — so A→B should be empty
        let res = plan_journeys(&epoch, &q);
        assert!(
            res.journeys.is_empty(),
            "t1 excluded should remove A→B service"
        );
    }

    #[test]
    fn mode_filter_blocks_bus() {
        let epoch = fixture_epoch();
        let mut q = query("test:A", "test:C", 7, 50);
        q.modes = Some(vec![RouteMode::Metro]);
        let res = plan_journeys(&epoch, &q);
        assert!(
            res.journeys.is_empty(),
            "metro-only should not reach C from A"
        );
    }

    #[test]
    fn k_best_returns_multiple() {
        let epoch = fixture_epoch();
        let mut q = query("test:A", "test:C", 7, 50);
        q.max_results = 3;
        let res = plan_journeys(&epoch, &q);
        assert!(res.journeys.len() >= 2, "expected t1 and later t3");
        // First should arrive earlier or equal
        assert!(res.journeys[0].arrival <= res.journeys[1].arrival);
    }

    #[test]
    fn drop_off_forbidden_cannot_alight() {
        // Only trip: A -> B with drop_off_type=1 at B (no alighting).
        let mut epoch = StaticEpoch::empty();
        epoch.id = "drop_off".into();
        epoch.stops = vec![
            stop("test:A", "Alpha", 48.86, 2.30),
            stop("test:B", "Beta", 48.87, 2.31),
        ];
        for (i, s) in epoch.stops.iter().enumerate() {
            epoch.stop_id_to_idx.insert(s.id.clone(), i as u32);
        }
        epoch.stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 8 * 3600,
                departure_s: 8 * 3600,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 8 * 3600 + 10 * 60,
                departure_s: 8 * 3600 + 10 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 1, // no alighting
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
        ];
        epoch.trips = vec![GlobalTrip {
            id: "test:t_drop".into(),
            feed_id: "test".into(),
            route_id: "test:r1".into(),
            service_id: "svc".into(),
            headsign: Some("No Alight".into()),
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
            agency_name: Some("TestBus".into()),
            stop_time_start: 0,
            stop_time_len: 2,
        frequency_windows: vec![],
        }];
        epoch.trip_id_to_idx.insert("test:t_drop".into(), 0);
        epoch.stop_departures = vec![Vec::new(); epoch.stops.len()];
        for off in 0..2u32 {
            let st = &epoch.stop_times[off as usize];
            if st.pickup_type != 1 {
                epoch.stop_departures[st.stop_idx as usize].push((0, off));
            }
        }
        let mut cal = ServiceCalendar::new();
        let start = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2030, 12, 31).unwrap();
        cal.add_regular(
            "svc".into(),
            start,
            end,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
        );
        epoch.calendars = HashMap::from([("test".into(), Arc::new(cal))]);
        epoch.walk_adj = vec![Vec::new(); epoch.stops.len()];

        let q = query("test:A", "test:B", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(
            res.journeys.is_empty(),
            "drop_off_type=1 at B must prevent alighting A→B"
        );
    }

    #[test]
    fn arrive_by_returns_journey_before_deadline() {
        let epoch = fixture_epoch();
        // Deadline 08:30: trip1 A→B arrives 08:10 — valid. Later options after deadline filtered.
        let deadline = Utc.with_ymd_and_hms(2026, 7, 27, 8, 30, 0).unwrap();
        let mut q = query("test:A", "test:B", 8, 30);
        q.arrive_by = true;
        q.departure_at = deadline;
        let res = plan_journeys(&epoch, &q);
        assert!(
            !res.journeys.is_empty(),
            "expected at least one journey arriving by 08:30"
        );
        for j in &res.journeys {
            assert!(
                j.arrival <= deadline,
                "journey arrival {:?} must be ≤ deadline {:?}",
                j.arrival,
                deadline
            );
        }
        // Direct t1 A@08:00→B@08:10 should be preferred among valid options.
        let first = &res.journeys[0];
        let transit: Vec<_> = first
            .legs
            .iter()
            .filter(|l| matches!(l, Leg::Transit(_)))
            .collect();
        assert!(!transit.is_empty());
        if let Leg::Transit(t) = transit[0] {
            assert_eq!(t.trip_id, "test:t1");
            assert!(t.scheduled_arrival <= deadline);
        }
    }

    #[test]
    fn arrive_by_rejects_journeys_after_deadline() {
        let epoch = fixture_epoch();
        // Deadline 08:05: nothing from A can reach B (first arrival 08:10).
        let deadline = Utc.with_ymd_and_hms(2026, 7, 27, 8, 5, 0).unwrap();
        let mut q = query("test:A", "test:B", 8, 5);
        q.arrive_by = true;
        q.departure_at = deadline;
        let res = plan_journeys(&epoch, &q);
        assert!(
            res.journeys.is_empty(),
            "no journey should arrive by 08:05, got {}",
            res.journeys.len()
        );
    }

    #[test]
    fn pareto_front_drops_dominated() {
        let late = Utc.with_ymd_and_hms(2026, 7, 27, 9, 0, 0).unwrap();
        let early = Utc.with_ymd_and_hms(2026, 7, 27, 8, 0, 0).unwrap();
        let dep = Utc.with_ymd_and_hms(2026, 7, 27, 7, 0, 0).unwrap();
        let better = Journey {
            id: "better".into(),
            departure: dep,
            arrival: early,
            duration_s: 3600,
            transfers: 0,
            walk_distance_m: 100.0,
            realtime_status: "SCHEDULED".into(),
            legs: vec![],
            alert_headers: vec![],
            fare_amount: None,
            fare_currency: None,
            fare_note: None,
        };
        // Same or earlier departure, strictly worse on arrival/transfers/walk → dominated.
        let dominated = Journey {
            id: "worse".into(),
            departure: dep,
            arrival: late,
            duration_s: 7200,
            transfers: 1,
            walk_distance_m: 500.0,
            realtime_status: "SCHEDULED".into(),
            legs: vec![],
            alert_headers: vec![],
            fare_amount: None,
            fare_currency: None,
            fare_note: None,
        };
        // Trade-off: later arrival but fewer transfers — keep both.
        let tradeoff = Journey {
            id: "tradeoff".into(),
            departure: dep,
            arrival: late,
            duration_s: 7200,
            transfers: 0,
            walk_distance_m: 50.0,
            realtime_status: "SCHEDULED".into(),
            legs: vec![],
            alert_headers: vec![],
            fare_amount: None,
            fare_currency: None,
            fare_note: None,
        };
        let front = pareto_front(vec![dominated.clone(), better.clone(), tradeoff.clone()]);
        let ids: HashSet<_> = front.iter().map(|j| j.id.as_str()).collect();
        assert!(ids.contains("better"), "non-dominated better kept");
        assert!(
            !ids.contains("worse"),
            "dominated journey must be removed"
        );
        assert!(
            ids.contains("tradeoff"),
            "trade-off on walk/arrival should survive"
        );
    }

    #[test]
    fn wheelchair_filter_excludes_inaccessible_trip() {
        let mut epoch = fixture_epoch();
        // Make the only A→B trip wheelchair=2 (not allowed).
        epoch.trips[0].wheelchair = 2;
        // Also block later A→C trip3 so no alternate path to B.
        epoch.trips[2].wheelchair = 2;

        let mut q = query("test:A", "test:B", 7, 50);
        q.wheelchair = true;
        let res = plan_journeys(&epoch, &q);
        assert!(
            res.journeys.is_empty(),
            "wheelchair=2 trips must be excluded when query.wheelchair=true"
        );

        // Without filter, same network still works.
        q.wheelchair = false;
        let res_ok = plan_journeys(&epoch, &q);
        assert!(
            !res_ok.journeys.is_empty(),
            "without wheelchair filter, journey should exist"
        );
    }

    #[test]
    fn frequency_trip_can_be_boarded() {
        // Relative template: A@0:00, B@0:10. Frequency every 15 min from 08:00–09:00.
        let mut epoch = StaticEpoch::empty();
        epoch.id = "freq".into();
        epoch.stops = vec![
            stop("test:A", "Alpha", 48.86, 2.30),
            stop("test:B", "Beta", 48.87, 2.31),
        ];
        for (i, s) in epoch.stops.iter().enumerate() {
            epoch.stop_id_to_idx.insert(s.id.clone(), i as u32);
        }
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
                arrival_s: 10 * 60,
                departure_s: 10 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
        ];
        epoch.trips = vec![GlobalTrip {
            id: "test:tfreq".into(),
            feed_id: "test".into(),
            route_id: "test:r1".into(),
            service_id: "svc".into(),
            headsign: Some("Freq B".into()),
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Bus,
            route_short_name: "F".into(),
            route_long_name: "Freq".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 3,
            agency_name: Some("TestBus".into()),
            stop_time_start: 0,
            stop_time_len: 2,
            frequency_windows: vec![FrequencyWindow {
                start_s: 8 * 3600,
                end_s: 9 * 3600,
                headway_s: 15 * 60,
            }],
        }];
        epoch.trip_id_to_idx.insert("test:tfreq".into(), 0);
        epoch.stop_departures = vec![Vec::new(); epoch.stops.len()];
        for off in 0..2u32 {
            let st = &epoch.stop_times[off as usize];
            if st.pickup_type != 1 {
                epoch.stop_departures[st.stop_idx as usize].push((0, off));
            }
        }
        let mut cal = ServiceCalendar::new();
        let start = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2030, 12, 31).unwrap();
        cal.add_regular(
            "svc".into(),
            start,
            end,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
        );
        epoch.calendars = HashMap::from([("test".into(), Arc::new(cal))]);
        epoch.walk_adj = vec![Vec::new(); epoch.stops.len()];

        // Query at 07:50 → first virtual departure 08:00, arrive B 08:10.
        let q = query("test:A", "test:B", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(!res.journeys.is_empty(), "frequency trip should be boardable");
        let t = res.journeys[0]
            .legs
            .iter()
            .find_map(|l| match l {
                Leg::Transit(t) => Some(t),
                _ => None,
            })
            .expect("transit");
        assert_eq!(t.trip_id, "test:tfreq");
        assert_eq!(t.from.stop_id, "test:A");
        assert_eq!(t.to.stop_id, "test:B");
        let dep = Utc.with_ymd_and_hms(2026, 7, 27, 8, 0, 0).unwrap();
        let arr = Utc.with_ymd_and_hms(2026, 7, 27, 8, 10, 0).unwrap();
        assert_eq!(t.scheduled_departure, dep);
        assert_eq!(t.scheduled_arrival, arr);
    }

    #[test]
    fn block_id_marks_same_vehicle_on_second_leg() {
        // A --t1--> B --t2--> C, same block_id, timed for stay-seated transfer.
        let mut epoch = StaticEpoch::empty();
        epoch.id = "block".into();
        epoch.stops = vec![
            stop("test:A", "Alpha", 48.86, 2.30),
            stop("test:B", "Beta", 48.87, 2.31),
            stop("test:C", "Gamma", 48.88, 2.32),
        ];
        for (i, s) in epoch.stops.iter().enumerate() {
            epoch.stop_id_to_idx.insert(s.id.clone(), i as u32);
        }
        epoch.stop_times = vec![
            // t1 A@8:00 -> B@8:10
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 8 * 3600,
                departure_s: 8 * 3600,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 8 * 3600 + 10 * 60,
                departure_s: 8 * 3600 + 10 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            // t2 B@8:12 -> C@8:25 (after transfer buffer 60s from 8:10)
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 8 * 3600 + 12 * 60,
                departure_s: 8 * 3600 + 12 * 60,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 2,
                arrival_s: 8 * 3600 + 25 * 60,
                departure_s: 8 * 3600 + 25 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
            },
        ];
        let mk = |id: &str, start: u32, len: u32, block: Option<&str>| GlobalTrip {
            id: id.into(),
            feed_id: "test".into(),
            route_id: "test:r1".into(),
            service_id: "svc".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: block.map(|s| s.into()),
            shape_id: None,
            mode: RouteMode::Bus,
            route_short_name: "1".into(),
            route_long_name: "Line".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 3,
            agency_name: None,
            stop_time_start: start,
            stop_time_len: len,
            frequency_windows: vec![],
        };
        epoch.trips = vec![
            mk("test:t1", 0, 2, Some("BLOCK-X")),
            mk("test:t2", 2, 2, Some("BLOCK-X")),
        ];
        for (i, t) in epoch.trips.iter().enumerate() {
            epoch.trip_id_to_idx.insert(t.id.clone(), i as u32);
        }
        epoch.stop_departures = vec![Vec::new(); epoch.stops.len()];
        for (trip_idx, trip) in epoch.trips.iter().enumerate() {
            for off in 0..trip.stop_time_len {
                let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
                if st.pickup_type != 1 {
                    epoch.stop_departures[st.stop_idx as usize].push((trip_idx as u32, off));
                }
            }
        }
        let mut cal = ServiceCalendar::new();
        let start = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2030, 12, 31).unwrap();
        cal.add_regular(
            "svc".into(),
            start,
            end,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
        );
        epoch.calendars = HashMap::from([("test".into(), Arc::new(cal))]);
        epoch.walk_adj = vec![Vec::new(); epoch.stops.len()];

        let q = query("test:A", "test:C", 7, 50);
        let res = plan_journeys(&epoch, &q);
        assert!(!res.journeys.is_empty());
        let transit: Vec<_> = res.journeys[0]
            .legs
            .iter()
            .filter_map(|l| match l {
                Leg::Transit(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(transit.len(), 2, "expected two transit legs");
        assert!(!transit[0].same_vehicle, "first leg is never same_vehicle");
        assert!(
            transit[1].same_vehicle,
            "second leg should flag stay-seated (same block_id)"
        );
    }

    /// Two platforms linked by stairs (fast) and elevator (slower). With wheelchair=true,
    /// stairs cost ×100 so the elevator path is chosen.
    /// Multiple trips on the same stop pattern board the earliest departure that
    /// satisfies `board_after`.
    #[test]
    fn picks_earliest_trip_on_pattern() {
        let mut epoch = fixture_epoch();
        // Add t4: same A→B→C pattern as t1 but departs 08:05 at A (later than t1's 08:00).
        let st_start = epoch.stop_times.len() as u32;
        epoch.stop_times.extend([
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 8 * 3600 + 5 * 60,
                departure_s: 8 * 3600 + 5 * 60,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 8 * 3600 + 15 * 60,
                departure_s: 8 * 3600 + 15 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 2,
                arrival_s: 8 * 3600 + 30 * 60,
                departure_s: 8 * 3600 + 30 * 60,
                stop_sequence: 3,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
        ]);
        let t4_idx = epoch.trips.len() as u32;
        epoch.trips.push(GlobalTrip {
            id: "test:t4".into(),
            feed_id: "test".into(),
            route_id: "test:r1".into(),
            service_id: "svc".into(),
            headsign: Some("Later C".into()),
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
            agency_name: Some("TestBus".into()),
            stop_time_start: st_start,
            stop_time_len: 3,
            frequency_windows: vec![],
        });
        epoch.trip_id_to_idx.insert("test:t4".into(), t4_idx);
        epoch.stop_departures[0].push((t4_idx, 0));

        let q = query("test:A", "test:C", 7, 50);
        let res = plan_journeys(&epoch, &q);
        let t = res.journeys[0]
            .legs
            .iter()
            .find_map(|l| match l {
                Leg::Transit(t) => Some(t),
                _ => None,
            })
            .expect("transit");
        assert_eq!(
            t.trip_id, "test:t1",
            "should board earliest trip on pattern"
        );
    }

    #[test]
    fn uses_later_trip_when_earliest_excluded() {
        let mut epoch = fixture_epoch();
        let st_start = epoch.stop_times.len() as u32;
        epoch.stop_times.extend([
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 8 * 3600 + 5 * 60,
                departure_s: 8 * 3600 + 5 * 60,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 8 * 3600 + 15 * 60,
                departure_s: 8 * 3600 + 15 * 60,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 2,
                arrival_s: 8 * 3600 + 30 * 60,
                departure_s: 8 * 3600 + 30 * 60,
                stop_sequence: 3,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
        ]);
        let t4_idx = epoch.trips.len() as u32;
        epoch.trips.push(GlobalTrip {
            id: "test:t4".into(),
            feed_id: "test".into(),
            route_id: "test:r1".into(),
            service_id: "svc".into(),
            headsign: Some("Later C".into()),
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
            agency_name: Some("TestBus".into()),
            stop_time_start: st_start,
            stop_time_len: 3,
            frequency_windows: vec![],
        });
        epoch.trip_id_to_idx.insert("test:t4".into(), t4_idx);
        epoch.stop_departures[0].push((t4_idx, 0));

        let mut q = query("test:A", "test:C", 7, 50);
        q.excluded_trip_ids.insert("test:t1".into());
        let res = plan_journeys(&epoch, &q);
        let t = res.journeys[0]
            .legs
            .iter()
            .find_map(|l| match l {
                Leg::Transit(t) => Some(t),
                _ => None,
            })
            .expect("transit");
        assert_eq!(
            t.trip_id, "test:t4",
            "should fall through to next trip when earliest is excluded"
        );
    }

    #[test]
    fn raptor_fixture_timing_smoke() {
        use std::time::Instant;
        let epoch = fixture_epoch();
        let q = query("test:A", "test:D", 7, 50);
        let start = Instant::now();
        for _ in 0..200 {
            let res = plan_journeys(&epoch, &q);
            assert!(!res.journeys.is_empty());
        }
        let elapsed = start.elapsed();
        eprintln!(
            "raptor fixture 200x A→D: {:?} ({:.0} µs/op)",
            elapsed,
            elapsed.as_micros() as f64 / 200.0
        );
    }

    #[test]
    fn wheelchair_prefers_elevator_over_stairs() {
        use crate::gtfs::pack::WalkEdge;

        let mut epoch = StaticEpoch::empty();
        epoch.id = "wc_path".into();
        // P1 --stairs 30s--> Hub --elevator 90s--> P2
        // P1 --elevator 90s--> Hub --stairs 30s--> P2  (same edges bidirectional)
        // Also direct: P1 --stairs 40s--> P2 and P1 --elevator 120s--> P2
        epoch.stops = vec![
            stop("test:P1", "Platform 1", 48.86, 2.35),
            stop("test:P2", "Platform 2", 48.86, 2.351),
        ];
        for (i, s) in epoch.stops.iter().enumerate() {
            epoch.stop_id_to_idx.insert(s.id.clone(), i as u32);
        }
        // No transit — pure walk between platforms via walk edges.
        epoch.walk_edges = vec![
            WalkEdge {
                from_stop_idx: 0,
                to_stop_idx: 1,
                duration_s: 40,
                distance_m: 40.0,
                pathway_mode: Some(2), // stairs
            },
            WalkEdge {
                from_stop_idx: 0,
                to_stop_idx: 1,
                duration_s: 120,
                distance_m: 40.0,
                pathway_mode: Some(5), // elevator
            },
            WalkEdge {
                from_stop_idx: 1,
                to_stop_idx: 0,
                duration_s: 40,
                distance_m: 40.0,
                pathway_mode: Some(2),
            },
            WalkEdge {
                from_stop_idx: 1,
                to_stop_idx: 0,
                duration_s: 120,
                distance_m: 40.0,
                pathway_mode: Some(5),
            },
        ];
        epoch.walk_adj = vec![vec![0, 1], vec![2, 3]];

        // Without wheelchair: stairs (40s) win.
        let mut q = query("test:P1", "test:P2", 8, 0);
        q.wheelchair = false;
        let res_fast = plan_journeys(&epoch, &q);
        assert!(
            !res_fast.journeys.is_empty(),
            "expected pure walk journey"
        );
        let walk_s = res_fast.journeys[0]
            .legs
            .iter()
            .filter_map(|l| match l {
                Leg::Walk(w) => Some(w.duration_s),
                _ => None,
            })
            .sum::<u32>();
        assert!(
            walk_s <= 40,
            "non-wheelchair should use stairs (~40s), got {walk_s}"
        );

        // With wheelchair: stairs become 4000s, elevator 120s wins.
        q.wheelchair = true;
        let res_wc = plan_journeys(&epoch, &q);
        assert!(!res_wc.journeys.is_empty(), "wheelchair journey");
        let walk_wc = res_wc.journeys[0]
            .legs
            .iter()
            .filter_map(|l| match l {
                Leg::Walk(w) => Some(w.duration_s),
                _ => None,
            })
            .sum::<u32>();
        assert!(
            walk_wc >= 100 && walk_wc < 1000,
            "wheelchair should prefer elevator (~120s), not stairs×100; got {walk_wc}"
        );
    }
}
