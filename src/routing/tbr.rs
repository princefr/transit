//! FLASH-TB: Trip-Based Public Transit Routing with Arc-Flags.
//!
//! A round-based alternative to RAPTOR that operates on **trips** rather than
//! stops.  The core algorithm follows Geisberger et al. (2012), "Trip-Based
//! Public Transit Routing", extended with **arc-flags** for transfer pruning
//! as described in Großmann et al. (2024), "FLASH-TB: Integrating Arc-Flags
//! and Trip-Based Public Transit Routing" (arXiv:2312.13146).
//!
//! ## Arc-Flags (FLASH-TB)
//!
//! The network is partitioned into geographic grid cells at pack time
//! (`pack::compute_partition`).  For each transfer edge and each cell, an
//! arc-flag is precomputed indicating whether that edge lies on a shortest
//! path to some stop in the cell (`pack::compute_arc_flags`).  During query,
//! only transfer edges whose arc-flag is set for the target cell are explored,
//! pruning the search space significantly.
//!
//! ## Key difference from RAPTOR
//!
//! RAPTOR is **stop-centric**: each round scans all *marked stops*, then for
//! each stop scans all boarding trips, then in a second phase walks transfers.
//!
//! TBR is **trip-centric**: each round scans *reached stops*, and for each stop
//! scans boarding trips.  When a trip is boarded it is **immediately** scanned
//! forward through its stop sequence (single-phase), and a per-round *trip
//! stamp* prevents re-scanning the same trip twice.  This reduces redundant
//! trip scans when many stops on the same trip are reached in the same round.
//!
//! ## Correctness
//!
//! For schedule-based trips, boarding at a later stop on the same trip never
//! yields earlier arrivals at subsequent stops (the trip schedule is fixed),
//! so skipping a re-scan is safe.  For frequency-based trips the algorithm
//! re-evaluates the boarding when a better departure is found, matching
//! RAPTOR's two-phase behaviour.
//!
//! ## k-best / arrive-by / Pareto
//!
//! TBR reuses the same k-best (successive departure offsets), arrive-by
//! (look-back sampling), and McRAPTOR-lite Pareto-front logic as RAPTOR,
//! so the two produce equivalent candidate sets.

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

use super::journey::{
    local_midnight_utc, local_seconds_since_midnight, service_date_in_tz,
    ItineraryQuery, ItineraryResult, Journey, Leg, Place, RtTripAdjust,
};
use super::walk::AccessStop;
use crate::gtfs::pack::{RouteMode, StaticEpoch};

// Re-export the RAPTOR reconstruction helpers so TBR produces identical leg
// output (same geometry, same intermediate stops, same stay-seated logic).
use super::raptor::{
    earliest_freq_board, journey_arrival_s, journey_first_departure, next_departure_s,
    pareto_front, pure_walk_journey, rt_effective_arr_s, rt_effective_dep_s, trip_template_base,
    CANDIDATE_POOL_FACTOR,
};

/// How a stop was reached in a given round — mirrors RAPTOR's `Reach`.
#[derive(Clone, Debug)]
enum TbrReach {
    Access {
        stop_idx: u32,
        arrival_s: u32,
        walk_duration: u32,
        walk_distance: f64,
    },
    Transit {
        arrival_s: u32,
        trip_idx: u32,
        board_stop: u32,
        board_off: u32,
        alight_off: u32,
        prev_round: u16,
        time_shift_s: i32,
    },
    Walk {
        arrival_s: u32,
        from_stop: u32,
        to_stop: u32,
        duration_s: u32,
        distance_m: f64,
        prev_round: u16,
    },
}

impl TbrReach {
    #[allow(dead_code)]
    fn arrival_s(&self) -> u32 {
        match self {
            TbrReach::Access { arrival_s, .. }
            | TbrReach::Transit { arrival_s, .. }
            | TbrReach::Walk { arrival_s, .. } => *arrival_s,
        }
    }
}

/// Per-query reusable scratch space shared across all `run_tbr` calls
/// (k-best iterations + arrive-by seeds).
struct TbrScratch<'a> {
    /// earliest arrival at each stop (u32::MAX = unreachable).
    earliest: Vec<u32>,
    /// labels[round][stop] = how we reached stop in that round (for reconstruction).
    labels: Vec<Vec<Option<TbrReach>>>,
    /// Stops written since last reset — only these are cleared between runs.
    touched: Vec<u32>,
    /// Trip indices touched in trip_boarding since last reset.
    touched_trips: Vec<u32>,
    /// trip_boarding[trip] = (board_stop, board_off, board_dep, time_shift_s)
    /// for the best boarding of this trip in the current round.
    trip_boarding: Vec<Option<(u32, u32, u32, i32)>>,
    /// Stamp for "trip already used in this round".
    trip_stamp: Vec<u32>,
    /// Stops reached in the current round (for walk transfer stage).
    reached: Vec<u32>,
    /// Stamp for "stop already reached in this round".
    stop_stamp: Vec<u32>,
    /// Stamp for "line already boarded in this round".
    line_stamp: Vec<u32>,
    /// Per-trip calendar activity for the query service date.
    active: Vec<bool>,
    /// Per-trip RT adjustment.
    rt: Vec<Option<&'a RtTripAdjust>>,
    /// Max absolute RT delay (s).
    rt_margin_s: u32,
    rounds: usize,
    stamp: u32,
}

impl<'a> TbrScratch<'a> {
    fn new(epoch: &StaticEpoch, q: &'a ItineraryQuery, date: chrono::NaiveDate) -> Self {
        let n = epoch.stops.len();
        let trip_n = epoch.trips.len();

        let mut cal_memo: HashMap<(&str, &str), bool> = HashMap::new();
        let mut active = Vec::with_capacity(trip_n);
        for trip in &epoch.trips {
            let key = (trip.feed_id.as_str(), trip.service_id.as_str());
            let a = *cal_memo.entry(key).or_insert_with(|| {
                epoch
                    .calendars
                    .get(&trip.feed_id)
                    .map(|cal| cal.is_active(&trip.service_id, date))
                    .unwrap_or(true)
            });
            active.push(a);
        }

        let mut rt: Vec<Option<&RtTripAdjust>> = vec![None; trip_n];
        let mut margin = 0u32;
        for (id, adj) in &q.rt_adjust {
            if let Some(&idx) = epoch.trip_id_to_idx.get(id) {
                rt[idx as usize] = Some(adj);
            }
            let mut m = adj.trip_delay_s.unsigned_abs();
            for &d in adj.dep_delay.values().chain(adj.arr_delay.values()) {
                m = m.max(d.unsigned_abs());
            }
            margin = margin.max(m);
        }
        let margin = margin.min(6 * 3600);

        let rounds = tbr_max_rounds(q);
        Self {
            earliest: vec![u32::MAX; n],
            labels: vec![vec![None; n]; rounds + 1],
            touched: Vec::new(),
            touched_trips: Vec::new(),
            trip_boarding: vec![None; trip_n],
            trip_stamp: vec![0; trip_n],
            reached: Vec::new(),
            stop_stamp: vec![0; n],
            line_stamp: vec![0; epoch.line_trips.len().max(1)],
            active,
            rt,
            rt_margin_s: margin,
            rounds,
            stamp: 0,
        }
    }

    fn reset(&mut self) {
        for si in self.touched.drain(..) {
            self.earliest[si as usize] = u32::MAX;
            for row in self.labels.iter_mut() {
                row[si as usize] = None;
            }
        }
        for ti in self.touched_trips.drain(..) {
            self.trip_boarding[ti as usize] = None;
        }
    }

    #[inline]
    fn set_trip_boarding(&mut self, ti: usize, data: (u32, u32, u32, i32)) {
        if self.trip_boarding[ti].is_none() {
            self.touched_trips.push(ti as u32);
        }
        self.trip_boarding[ti] = Some(data);
    }

    #[inline]
    fn improve(&mut self, round: usize, si: u32, arr: u32, reach: TbrReach) {
        if self.earliest[si as usize] == u32::MAX {
            self.touched.push(si);
        }
        self.earliest[si as usize] = arr;
        self.labels[round + 1][si as usize] = Some(reach);
    }
}

fn tbr_max_rounds(q: &ItineraryQuery) -> usize {
    (q.raptor_max_rounds
        .max(q.max_transfers.saturating_add(2))
        .max(4) as usize)
        .min(12)
        .max(1)
}

/// Public entry: plan up to `max_results` diverse journeys using Trip-Based
/// Routing.  Mirrors `plan_journeys` (RAPTOR) but dispatches to `run_tbr`.
pub fn plan_journeys_tbr(epoch: &StaticEpoch, q: &ItineraryQuery) -> ItineraryResult {
    let date = service_date_in_tz(q.departure_at, &q.timezone);
    let dep_s0 = local_seconds_since_midnight(q.departure_at, &q.timezone);
    let midnight = local_midnight_utc(date, &q.timezone);

    let osrm = q.osrm_url.as_deref().filter(|u| !u.trim().is_empty());
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

    let max_k = if epoch.trip_count() > 80_000 {
        (q.max_results.max(1) as usize).min(4)
    } else {
        q.max_results.max(1) as usize
    };
    let pool_cap = if epoch.trip_count() > 80_000 {
        (max_k + 1).max(max_k)
    } else {
        (max_k * CANDIDATE_POOL_FACTOR).max(max_k)
    };

    let mut scratch = TbrScratch::new(epoch, q, date);

    let mut journeys = if q.arrive_by {
        plan_arrive_by_tbr(
            epoch, q, dep_s0, midnight, &origins, &targets, &egress, &origin_map, pool_cap,
            &mut scratch,
        )
    } else {
        plan_depart_after_tbr(
            epoch, q, dep_s0, midnight, &origins, &targets, &egress, &origin_map, pool_cap,
            None, &mut scratch,
        )
    };

    journeys = pareto_front(journeys);

    if q.arrive_by {
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

/// Depart-after k-best using TBR.
fn plan_depart_after_tbr(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    dep_s0: u32,
    midnight: DateTime<Utc>,
    origins: &[AccessStop],
    targets: &[AccessStop],
    egress: &HashMap<u32, AccessStop>,
    origin_map: &HashMap<u32, AccessStop>,
    pool_cap: usize,
    arrive_deadline_s: Option<u32>,
    scratch: &mut TbrScratch,
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
    let mut iterations = 0u32;
    let max_iterations = (pool_cap as u32).saturating_mul(8).max(16);

    while journeys.len() < pool_cap && stagnant < 8 && iterations < max_iterations {
        iterations += 1;
        if let Some(deadline) = arrive_deadline_s {
            if dep_s >= deadline {
                break;
            }
        }

        let Some((total_arr, target_idx)) = run_tbr(epoch, q, dep_s, origins, egress, scratch)
        else {
            break;
        };

        if let Some(deadline) = arrive_deadline_s {
            if total_arr > deadline {
                break;
            }
        }

        let Some(mut journey) = reconstruct_tbr(
            epoch,
            &scratch.labels,
            target_idx,
            &egress[&target_idx],
            total_arr,
            midnight,
            origin_map,
            q,
            dep_s,
        ) else {
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

/// Arrive-by using TBR: sample departures in a look-back window.
fn plan_arrive_by_tbr(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    deadline_s: u32,
    midnight: DateTime<Utc>,
    origins: &[AccessStop],
    targets: &[AccessStop],
    egress: &HashMap<u32, AccessStop>,
    origin_map: &HashMap<u32, AccessStop>,
    pool_cap: usize,
    scratch: &mut TbrScratch,
) -> Vec<Journey> {
    let earliest = deadline_s.saturating_sub(super::raptor::ARRIVE_BY_MAX_JOURNEY_S);
    let mut journeys: Vec<Journey> = Vec::new();
    let mut seen_keys: HashSet<(i64, i64, u32)> = HashSet::new();

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

    let mut samples: Vec<u32> = Vec::new();
    let mut sample = earliest;
    while sample < deadline_s {
        samples.push(sample);
        sample = sample.saturating_add(super::raptor::ARRIVE_BY_SAMPLE_STEP_S);
        if samples.len() > 48 {
            break;
        }
    }
    if samples.last().copied().unwrap_or(u32::MAX) + 60 < deadline_s {
        samples.push(deadline_s.saturating_sub(60));
    }

    let per_sample = ((pool_cap / samples.len().max(1)) + 2).max(2);

    for seed in samples {
        let batch = plan_depart_after_tbr(
            epoch,
            q,
            seed,
            midnight,
            origins,
            targets,
            egress,
            origin_map,
            per_sample,
            Some(deadline_s),
            scratch,
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

/// Core TBR round-based scan.
///
/// Returns `(total_arrival_at_best_target, best_target_stop_idx)`.
fn run_tbr(
    epoch: &StaticEpoch,
    q: &ItineraryQuery,
    dep_s: u32,
    origins: &[AccessStop],
    egress: &HashMap<u32, AccessStop>,
    s: &mut TbrScratch,
) -> Option<(u32, u32)> {
    s.reset();
    let mut best_dest = u32::MAX;

    // FLASH-TB: compute target cell bitmask from egress stops.
    // Only transfer edges with arc-flags set for at least one target cell
    // are explored during the walk transfer phase.
    let mut target_cells = crate::gtfs::cell_bitset::CellBitSet::new(epoch.num_cells);
    for (t_idx, _) in egress {
        if let Some(&cell) = epoch.stop_partition.get(*t_idx as usize) {
            target_cells.set_bit(cell as usize);
        }
    }
    let use_arc_flags = !target_cells.is_empty() && !epoch.arc_flags.is_empty();

    // Round 0: access.
    s.stamp = s.stamp.wrapping_add(1);
    let round0_stamp = s.stamp;
    for o in origins {
        let arr = dep_s.saturating_add(o.duration_s);
        if arr < s.earliest[o.stop_idx as usize] {
            // Access labels go at labels[0] (mirrors RAPTOR's labels[0] for access).
            if s.earliest[o.stop_idx as usize] == u32::MAX {
                s.touched.push(o.stop_idx);
            }
            s.earliest[o.stop_idx as usize] = arr;
            s.labels[0][o.stop_idx as usize] = Some(TbrReach::Access {
                stop_idx: o.stop_idx,
                arrival_s: arr,
                walk_duration: o.duration_s,
                walk_distance: o.distance_m,
            });
            s.reached.push(o.stop_idx);
            s.stop_stamp[o.stop_idx as usize] = round0_stamp;
        }
        if let Some(eg) = egress.get(&o.stop_idx) {
            let total = arr.saturating_add(eg.duration_s);
            if total < best_dest {
                best_dest = total;
            }
        }
    }
    if s.reached.is_empty() {
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
        if q.wheelchair && trip.wheelchair == 2 {
            return false;
        }
        true
    };

    for round in 0..s.rounds {
        if s.reached.is_empty() {
            break;
        }

        s.stamp = s.stamp.wrapping_add(1);
        let round_stamp = s.stamp;
        let mut next_reached: Vec<u32> = Vec::new();

        // --- Trip scanning phase ---
        // Collect reached stops first to avoid borrow conflict with scan_trip_forward.
        let reached: Vec<u32> = std::mem::take(&mut s.reached);
        for &stop_idx in &reached {
            let arrival_here = s.earliest[stop_idx as usize];
            if arrival_here == u32::MAX {
                continue;
            }
            let board_after = if round == 0 {
                arrival_here
            } else {
                arrival_here.saturating_add(q.default_transfer_s)
            };
            if board_after >= best_dest {
                continue;
            }

            let deps = epoch
                .stop_departures
                .get(stop_idx as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            if deps.is_empty() {
                continue;
            }

            let has_freq = epoch
                .stop_has_freq
                .get(stop_idx as usize)
                .copied()
                .unwrap_or_else(|| {
                    deps.iter()
                        .any(|&(t, _)| !epoch.trips[t as usize].frequency_windows.is_empty())
                });

            let start = if has_freq {
                0
            } else {
                let min_dep = board_after.saturating_sub(s.rt_margin_s);
                deps.partition_point(|&(t, off)| {
                    let trip = &epoch.trips[t as usize];
                    epoch.stop_times[(trip.stop_time_start + off) as usize].departure_s < min_dep
                })
            };

            for &(trip_idx, st_off) in &deps[start..] {
                let ti = trip_idx as usize;
                let line_id = epoch.line_of_trip.get(ti).copied().unwrap_or(u32::MAX);
                let trip = &epoch.trips[ti];
                let st_board = &epoch.stop_times[(trip.stop_time_start + st_off) as usize];

                if !has_freq && st_board.departure_s >= best_dest.saturating_add(s.rt_margin_s) {
                    break;
                }
                if !mode_ok(trip.mode) || !trip_ok(trip) {
                    continue;
                }
                if !s.active[ti] {
                    continue;
                }
                if line_id != u32::MAX {
                    if s.line_stamp.get(line_id as usize).copied().unwrap_or(0) == round_stamp {
                        continue;
                    }
                }
                if st_board.pickup_type == 1 {
                    continue;
                }
                let adj = s.rt[ti];
                if adj.is_some_and(|a| a.is_skipped(st_board.stop_sequence)) {
                    continue;
                }

                // Compute effective boarding departure + time shift.
                let (board_dep, time_shift) = if !trip.frequency_windows.is_empty() {
                    let base = trip_template_base(epoch, trip);
                    let sched_dep = rt_effective_dep_s(adj, st_board);
                    match earliest_freq_board(trip, base, sched_dep, board_after) {
                        Some(v) => v,
                        None => continue,
                    }
                } else {
                    let dep_rt = rt_effective_dep_s(adj, st_board);
                    if dep_rt < board_after {
                        continue;
                    }
                    let shift = dep_rt as i64 - st_board.departure_s as i64;
                    (dep_rt, shift as i32)
                };
                if board_dep >= best_dest {
                    continue;
                }

                // TBR: check if trip already used this round.
                if s.trip_stamp[ti] == round_stamp {
                    // Trip already boarded this round — check if this boarding
                    // is better (earlier departure or earlier stop offset).
                    let (_, prev_off, prev_dep, _) =
                        s.trip_boarding[ti].unwrap_or((0, 0, u32::MAX, 0));
                    if board_dep < prev_dep
                        || (board_dep == prev_dep && st_off < prev_off)
                    {
                        s.set_trip_boarding(ti, (stop_idx, st_off, board_dep, time_shift));
                        // Re-scan forward from this boarding point.
                        scan_trip_forward(
                            epoch,
                            q,
                            ti,
                            trip,
                            st_off,
                            time_shift,
                            adj,
                            &mut best_dest,
                            egress,
                            s,
                            round,
                            round_stamp,
                            &mut next_reached,
                            &target_cells,
                            use_arc_flags,
                        );
                    }
                    continue;
                }

                // New trip this round — board and scan.
                s.trip_stamp[ti] = round_stamp;
                if line_id != u32::MAX {
                    if let Some(ls) = s.line_stamp.get_mut(line_id as usize) {
                        *ls = round_stamp;
                    }
                }
                s.set_trip_boarding(ti, (stop_idx, st_off, board_dep, time_shift));
                scan_trip_forward(
                    epoch,
                    q,
                    ti,
                    trip,
                    st_off,
                    time_shift,
                    adj,
                    &mut best_dest,
                    egress,
                    s,
                    round,
                    round_stamp,
                    &mut next_reached,
                    &target_cells,
                    use_arc_flags,
                );
            }
        }

        // --- Walk transfer phase ---
        let transit_reached: Vec<u32> = next_reached.drain(..).collect();
        for stop_idx in &transit_reached {
            let arrival_here = s.earliest[*stop_idx as usize];
            for &ei in epoch
                .walk_adj
                .get(*stop_idx as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                let e = &epoch.walk_edges[ei];
                // FLASH-TB: skip transfer edges that don't lead to any target cell.
                if use_arc_flags {
                    if let Some(flags) = epoch.arc_flags.get(ei) {
                        if !flags.intersects(&target_cells) {
                            continue;
                        }
                    }
                }
                let walk_dur = e.effective_duration_s(q.wheelchair);
                let arr = arrival_here.saturating_add(walk_dur);
                if arr >= best_dest {
                    continue;
                }
                let ti = e.to_stop_idx;
                if arr < s.earliest[ti as usize] {
                    s.improve(
                        round,
                        ti,
                        arr,
                        TbrReach::Walk {
                            arrival_s: arr,
                            from_stop: *stop_idx,
                            to_stop: e.to_stop_idx,
                            duration_s: walk_dur,
                            distance_m: e.distance_m,
                            prev_round: (round + 1) as u16,
                        },
                    );
                    if s.stop_stamp[ti as usize] != round_stamp {
                        s.stop_stamp[ti as usize] = round_stamp;
                        s.reached.push(ti);
                    }
                    if let Some(eg) = egress.get(&ti) {
                        let total = arr.saturating_add(eg.duration_s);
                        if total < best_dest {
                            best_dest = total;
                        }
                    }
                }
            }
        }

        // Prepare next round's reached set: transit-reached + walk-reached.
        // s.reached already contains walk-reached stops from the walk phase above.
        // Add transit-reached stops (no overlap with walk-reached due to stamp check).
        for &si in &transit_reached {
            if s.stop_stamp[si as usize] == round_stamp {
                s.reached.push(si);
            }
        }
    }

    // Best target
    let mut best_target: Option<(u32, u32)> = None;
    for (t_idx, eg) in egress {
        let arr_at_stop = s.earliest[*t_idx as usize];
        if arr_at_stop == u32::MAX {
            continue;
        }
        let total = arr_at_stop.saturating_add(eg.duration_s);
        if best_target.map(|b| total < b.0).unwrap_or(true) {
            best_target = Some((total, *t_idx));
        }
    }

    best_target
}

/// Scan forward through a trip from `board_off`, updating arrival times at
/// all subsequent stops.  Writes `TbrReach::Transit` labels and updates
/// `best_dest` / `next_reached` / `stop_stamp`.
#[inline]
fn scan_trip_forward(
    epoch: &StaticEpoch,
    _q: &ItineraryQuery,
    ti: usize,
    trip: &crate::gtfs::pack::GlobalTrip,
    board_off: u32,
    time_shift: i32,
    adj: Option<&RtTripAdjust>,
    best_dest: &mut u32,
    egress: &HashMap<u32, AccessStop>,
    s: &mut TbrScratch,
    round: usize,
    round_stamp: u32,
    next_reached: &mut Vec<u32>,
    target_cells: &crate::gtfs::cell_bitset::CellBitSet,
    use_arc_flags: bool,
) {
    let board_stop = s.trip_boarding[ti].map(|(bs, _, _, _)| bs).unwrap_or(0);
    for off in (board_off + 1)..trip.stop_time_len {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        if st.drop_off_type == 1 {
            continue;
        }
        if adj.is_some_and(|a| a.is_skipped(st.stop_sequence)) {
            continue;
        }

        let arr = rt_effective_arr_s(adj, st, time_shift);
        if arr >= *best_dest {
            break;
        }
        let si = st.stop_idx;

        if arr < s.earliest[si as usize] {
            s.improve(
                round,
                si,
                arr,
                TbrReach::Transit {
                    arrival_s: arr,
                    trip_idx: ti as u32,
                    board_stop,
                    board_off,
                    alight_off: off,
                    prev_round: round as u16,
                    time_shift_s: time_shift,
                },
            );
            if s.stop_stamp[si as usize] != round_stamp {
                s.stop_stamp[si as usize] = round_stamp;
                next_reached.push(si);
            }
            if let Some(eg) = egress.get(&si) {
                let total = arr.saturating_add(eg.duration_s);
                if total < *best_dest {
                    *best_dest = total;
                }
            }
        }
    }

    // FLASH-TB: precomputed trip-to-trip transfers with arc-flag pruning
    if use_arc_flags {
        for off in (board_off + 1)..trip.stop_time_len {
            let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
            if st.drop_off_type == 1 { continue; }
            let arr = rt_effective_arr_s(adj, st, time_shift);
            if arr >= *best_dest { break; }
            
            if let Some(transfers) = epoch.trip_transfers.get(ti).and_then(|v| v.get(off as usize)) {
                for entry in transfers {
                    if !entry.arc_flags.intersects(target_cells) {
                        continue; // arc-flag prune this trip transfer
                    }
                    let t2 = entry.target_trip as usize;
                    if t2 >= s.active.len() || !s.active[t2] { continue; }
                    
                    let trip2 = &epoch.trips[t2];
                    let board_st = &epoch.stop_times[(trip2.stop_time_start + entry.target_board_off) as usize];
                    let effective_board = arr.saturating_add(entry.min_transfer_s);
                    if board_st.departure_s < effective_board { continue; }
                    if board_st.departure_s >= *best_dest { continue; }
                    
                    let si2 = board_st.stop_idx;
                    if board_st.departure_s < s.earliest[si2 as usize] {
                        s.improve(
                            round, si2, board_st.departure_s,
                            TbrReach::Walk {
                                arrival_s: board_st.departure_s,
                                from_stop: st.stop_idx,
                                to_stop: si2,
                                duration_s: entry.min_transfer_s,
                                distance_m: 0.0,
                                prev_round: (round + 1) as u16,
                            },
                        );
                        if s.stop_stamp[si2 as usize] != round_stamp {
                            s.stop_stamp[si2 as usize] = round_stamp;
                            next_reached.push(si2);
                        }
                        if let Some(eg) = egress.get(&si2) {
                            let total = board_st.departure_s.saturating_add(eg.duration_s);
                            if total < *best_dest {
                                *best_dest = total;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Reconstruct a journey from TBR labels.
///
/// Reuses the RAPTOR `reconstruct` logic by converting `TbrReach` labels into
/// the same `Reach` representation RAPTOR uses, then calling the existing
/// reconstruction path.  This guarantees identical leg output (geometry,
/// intermediate stops, stay-seated detection).
fn reconstruct_tbr(
    epoch: &StaticEpoch,
    labels: &[Vec<Option<TbrReach>>],
    target_idx: u32,
    egress: &AccessStop,
    total_arr: u32,
    midnight: DateTime<Utc>,
    origins: &HashMap<u32, AccessStop>,
    q: &ItineraryQuery,
    query_dep_s: u32,
) -> Option<Journey> {
    // Convert TbrReach labels into RAPTOR Reach labels for reconstruction.
    use super::raptor::Reach;

    let n_rounds = labels.len();
    let mut raptor_labels: Vec<Vec<Option<Reach>>> =
        vec![vec![None; epoch.stops.len()]; n_rounds];

    for (r, row) in labels.iter().enumerate() {
        for (si, lab) in row.iter().enumerate() {
            if let Some(lab) = lab {
                raptor_labels[r][si] = Some(match lab {
                    TbrReach::Access {
                        stop_idx,
                        arrival_s,
                        walk_duration,
                        walk_distance,
                    } => Reach::Access {
                        stop_idx: *stop_idx,
                        arrival_s: *arrival_s,
                        walk_duration: *walk_duration,
                        walk_distance: *walk_distance,
                    },
                    TbrReach::Transit {
                        arrival_s,
                        trip_idx,
                        board_stop,
                        board_off,
                        alight_off,
                        prev_round,
                        time_shift_s,
                    } => Reach::Transit {
                        arrival_s: *arrival_s,
                        trip_idx: *trip_idx,
                        board_stop: *board_stop,
                        board_off: *board_off,
                        alight_off: *alight_off,
                        prev_round: *prev_round,
                        time_shift_s: *time_shift_s,
                    },
                    TbrReach::Walk {
                        arrival_s,
                        from_stop,
                        to_stop,
                        duration_s,
                        distance_m,
                        prev_round,
                    } => Reach::Walk {
                        arrival_s: *arrival_s,
                        from_stop: *from_stop,
                        to_stop: *to_stop,
                        duration_s: *duration_s,
                        distance_m: *distance_m,
                        prev_round: *prev_round,
                    },
                });
            }
        }
    }

    // Delegate to RAPTOR's reconstruction.
    super::raptor::reconstruct(
        epoch,
        &raptor_labels,
        target_idx,
        egress,
        total_arr,
        midnight,
        origins,
        q,
        query_dep_s,
    )
}

fn place_from_query_from(epoch: &StaticEpoch, q: &ItineraryQuery) -> Place {
    super::raptor::place_from_query_from(epoch, q)
}

fn place_from_query_to(epoch: &StaticEpoch, q: &ItineraryQuery) -> Place {
    super::raptor::place_from_query_to(epoch, q)
}

fn empty_result(epoch: &StaticEpoch, q: &ItineraryQuery) -> ItineraryResult {
    super::raptor::empty_result(epoch, q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::calendar::ServiceCalendar;
    use crate::gtfs::pack::{
        FrequencyWindow, GlobalTrip, PackedStopTime, RouteMode, StaticEpoch, StopRecord,
        WalkEdge,
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

    /// Tiny network (same fixture as RAPTOR tests):
    ///   A --trip1--> B --trip1--> C
    ///   B --trip2--> D
    ///   A --trip3--> C (later direct)
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
        let start = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2030, 12, 31).unwrap();
        cal.add_regular(
            "svc".into(),
            start,
            end,
            true, true, true, true, true, true, true,
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
            use_tbr: true,
        }
    }

    /// Helper: run both RAPTOR and TBR and assert they produce equivalent
    /// first-journey arrival times.
    fn assert_tbr_matches_raptor(epoch: &StaticEpoch, q: &ItineraryQuery) {
        let mut q_raptor = q.clone();
        q_raptor.use_tbr = false;
        let res_r = super::super::raptor::plan_journeys(epoch, &q_raptor);
        let res_t = plan_journeys_tbr(epoch, q);

        // Both should find the same number of journeys (or TBR finds at least 1
        // when RAPTOR does).
        if res_r.journeys.is_empty() {
            assert!(
                res_t.journeys.is_empty(),
                "TBR found journeys but RAPTOR didn't"
            );
            return;
        }

        assert!(
            !res_t.journeys.is_empty(),
            "RAPTOR found journeys but TBR didn't"
        );

        // First journey arrival should match.
        let arr_r = res_r.journeys[0].arrival;
        let arr_t = res_t.journeys[0].arrival;
        assert_eq!(
            arr_r, arr_t,
            "TBR arrival {:?} != RAPTOR arrival {:?} for {}→{}",
            arr_t, arr_r,
            q.from_stop_id.as_deref().unwrap_or("?"),
            q.to_stop_id.as_deref().unwrap_or("?")
        );

        // Same trip sequence.
        let trips_r: Vec<&str> = res_r.journeys[0]
            .legs
            .iter()
            .filter_map(|l| match l {
                Leg::Transit(t) => Some(t.trip_id.as_str()),
                _ => None,
            })
            .collect();
        let trips_t: Vec<&str> = res_t.journeys[0]
            .legs
            .iter()
            .filter_map(|l| match l {
                Leg::Transit(t) => Some(t.trip_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            trips_r, trips_t,
            "TBR trip sequence {:?} != RAPTOR {:?} for {}→{}",
            trips_t, trips_r,
            q.from_stop_id.as_deref().unwrap_or("?"),
            q.to_stop_id.as_deref().unwrap_or("?")
        );
    }

    #[test]
    fn tbr_direct_trip_a_to_b() {
        let epoch = fixture_epoch();
        let q = query("test:A", "test:B", 7, 50);
        let res = plan_journeys_tbr(&epoch, &q);
        assert!(!res.journeys.is_empty(), "expected direct journey A→B");
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
        }
        assert_eq!(j.transfers, 0);
    }

    #[test]
    fn tbr_matches_raptor_direct_a_to_b() {
        let epoch = fixture_epoch();
        let q = query("test:A", "test:B", 7, 50);
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_one_transfer_a_to_d() {
        let epoch = fixture_epoch();
        let q = query("test:A", "test:D", 7, 50);
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_direct_a_to_c() {
        let epoch = fixture_epoch();
        let q = query("test:A", "test:C", 7, 50);
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_k_best() {
        let epoch = fixture_epoch();
        let mut q = query("test:A", "test:C", 7, 50);
        q.max_results = 3;
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_no_path() {
        let mut epoch = fixture_epoch();
        epoch.stops.push(stop("test:E", "Epsilon", 49.0, 3.0));
        epoch
            .stop_id_to_idx
            .insert("test:E".into(), (epoch.stops.len() - 1) as u32);
        epoch.stop_departures.push(Vec::new());
        epoch.walk_adj.push(Vec::new());

        let q = query("test:A", "test:E", 7, 50);
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_excluded_trip() {
        let epoch = fixture_epoch();
        let mut q = query("test:A", "test:B", 7, 50);
        q.excluded_trip_ids.insert("test:t1".into());
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_mode_filter() {
        let epoch = fixture_epoch();
        let mut q = query("test:A", "test:C", 7, 50);
        q.modes = Some(vec![RouteMode::Metro]);
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_wheelchair() {
        let mut epoch = fixture_epoch();
        epoch.trips[0].wheelchair = 2;
        epoch.trips[2].wheelchair = 2;
        let mut q = query("test:A", "test:B", 7, 50);
        q.wheelchair = true;
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_walk_transfer() {
        let mut epoch = fixture_epoch();
        let b2 = epoch.stops.len() as u32;
        epoch.stops.push(stop("test:B2", "Bravo 2", 48.001, 2.001));
        epoch.stop_id_to_idx.insert("test:B2".into(), b2);
        epoch.stop_departures.push(Vec::new());
        epoch.walk_adj.push(Vec::new());

        let t2_start = epoch.trips[1].stop_time_start as usize;
        epoch.stop_times[t2_start].stop_idx = b2;

        epoch.stop_departures = vec![Vec::new(); epoch.stops.len()];
        for (trip_idx, trip) in epoch.trips.iter().enumerate() {
            for off in 0..trip.stop_time_len {
                let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
                if st.pickup_type != 1 {
                    epoch.stop_departures[st.stop_idx as usize].push((trip_idx as u32, off));
                }
            }
        }

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
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_frequency() {
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
        let start = chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2030, 12, 31).unwrap();
        cal.add_regular(
            "svc".into(),
            start, end, true, true, true, true, true, true, true,
        );
        epoch.calendars = HashMap::from([("test".into(), Arc::new(cal))]);
        epoch.walk_adj = vec![Vec::new(); epoch.stops.len()];

        let q = query("test:A", "test:B", 7, 50);
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_rt_delay() {
        use crate::routing::RtTripAdjust;
        let epoch = fixture_epoch();
        let q0 = query("test:A", "test:B", 7, 50);
        assert_tbr_matches_raptor(&epoch, &q0);

        let adj = RtTripAdjust {
            trip_delay_s: 30 * 60,
            ..Default::default()
        };
        let mut q = query("test:A", "test:B", 7, 50);
        q.rt_adjust = HashMap::from([("test:t1".into(), adj)]);
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_matches_raptor_arrive_by() {
        let epoch = fixture_epoch();
        let deadline = Utc.with_ymd_and_hms(2026, 7, 27, 8, 30, 0).unwrap();
        let mut q = query("test:A", "test:B", 8, 30);
        q.arrive_by = true;
        q.departure_at = deadline;
        assert_tbr_matches_raptor(&epoch, &q);
    }

    #[test]
    fn tbr_timing_smoke() {
        use std::time::Instant;
        let epoch = fixture_epoch();
        let q = query("test:A", "test:D", 7, 50);
        let start = Instant::now();
        for _ in 0..200 {
            let res = plan_journeys_tbr(&epoch, &q);
            assert!(!res.journeys.is_empty());
        }
        let elapsed = start.elapsed();
        eprintln!(
            "tbr fixture 200x A→D: {:?} ({:.0} µs/op)",
            elapsed,
            elapsed.as_micros() as f64 / 200.0
        );
    }
}
