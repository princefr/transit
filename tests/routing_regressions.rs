//! Regression tests for routing correctness: after-midnight trips (GTFS times ≥ 24:00),
//! DST service-day anchors, TBR line pruning, pooled TBR scratch sizing, FLASH-TB
//! realtime transfers and walk-first departure times.

use chrono::{DateTime, TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Write};
use std::sync::Arc;
use transit::gtfs::pack::{build_epoch, StaticEpoch};
use transit::gtfs::parse::load_gtfs_bytes;
use transit::gtfs::CellBitSet;
use transit::routing::{plan_journeys, ItineraryQuery, Journey, Leg, RtTripAdjust};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn gtfs_zip(files: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default();
    for (name, body) in files {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

const AGENCY: &str = "agency_id,agency_name,agency_timezone\na1,Test,Europe/Paris\n";
const ROUTES: &str = "route_id,route_short_name,route_long_name,route_type,agency_id\n\
r1,1,Line 1,3,a1\nr2,2,Line 2,3,a1\nr3,3,Line 3,3,a1\nr4,4,Line 4,3,a1\nr5,5,Line 5,3,a1\n";
const CAL_ALL: &str = "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
S1,1,1,1,1,1,1,1,20260101,20261231\n";

fn epoch_from(stops: &str, trips: &str, stop_times: &str, calendar: &str) -> StaticEpoch {
    let bytes = gtfs_zip(&[
        ("agency.txt", AGENCY),
        ("stops.txt", stops),
        ("routes.txt", ROUTES),
        ("calendar.txt", calendar),
        ("trips.txt", trips),
        ("stop_times.txt", stop_times),
    ]);
    let bundle = load_gtfs_bytes("test", &bytes).expect("parse gtfs");
    build_epoch(vec![Arc::new(bundle)], vec![])
}

fn query(from: &str, to: &str, at: DateTime<Utc>, tbr: bool) -> ItineraryQuery {
    ItineraryQuery {
        from_stop_id: Some(from.into()),
        to_stop_id: Some(to.into()),
        from_lat: None,
        from_lon: None,
        to_lat: None,
        to_lon: None,
        departure_at: at,
        arrive_by: false,
        max_transfers: 4,
        max_results: 3,
        modes: None,
        max_walk_meters: 1000,
        walk_speed_m_s: 1.2,
        raptor_max_rounds: 6,
        default_transfer_s: 60,
        timezone: "Europe/Paris".into(),
        osrm_url: None,
        bike_from: false,
        bike_to: false,
        bike_speed_m_s: 4.2,
        max_bike_meters: 5000,
        use_tbr: tbr,
        excluded_trip_ids: HashSet::new(),
        rt_adjust: HashMap::new(),
        wheelchair: false,
    }
}

fn transit_trips(j: &Journey) -> Vec<String> {
    j.legs
        .iter()
        .filter_map(|l| match l {
            Leg::Transit(t) => Some(t.trip_id.clone()),
            _ => None,
        })
        .collect()
}

fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
}

// ---------------------------------------------------------------------------
// 1. After-midnight trips of the previous service day
// ---------------------------------------------------------------------------

const NIGHT_STOPS: &str = "stop_id,stop_name,stop_lat,stop_lon\n\
A,Alpha,48.80,2.30\nB,Beta,48.90,2.40\n";
// Friday-only service; trip leaves A at 24:45:00 (Saturday 00:45 local).
const CAL_FRIDAY: &str = "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
FRI,0,0,0,0,1,0,0,20260101,20261231\n";
const NIGHT_TRIPS: &str = "route_id,service_id,trip_id\nr1,FRI,night\n";
const NIGHT_ST: &str = "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
night,24:45:00,24:45:00,A,1\nnight,25:15:00,25:15:00,B,2\n";

#[test]
fn friday_trip_after_midnight_found_on_saturday_morning() {
    let epoch = epoch_from(NIGHT_STOPS, NIGHT_TRIPS, NIGHT_ST, CAL_FRIDAY);
    // Saturday 2026-07-25 00:30 CEST == Friday 22:30 UTC.
    let at = utc(2026, 7, 24, 22, 30);
    for tbr in [false, true] {
        let res = plan_journeys(&epoch, &query("test:A", "test:B", at, tbr));
        let j = res
            .journeys
            .iter()
            .find(|j| transit_trips(j) == vec!["test:night".to_string()])
            .unwrap_or_else(|| panic!("night trip not found (tbr={tbr}): {:?}", res.journeys));
        // 00:45 / 01:15 CEST on Saturday.
        assert_eq!(j.departure, utc(2026, 7, 24, 22, 45), "tbr={tbr}");
        assert_eq!(j.arrival, utc(2026, 7, 24, 23, 15), "tbr={tbr}");
        let Leg::Transit(t) = &j.legs[0] else { panic!() };
        assert_eq!(t.scheduled_departure, utc(2026, 7, 24, 22, 45));
    }
}

#[test]
fn friday_trip_after_midnight_arrive_by_early_deadline() {
    let epoch = epoch_from(NIGHT_STOPS, NIGHT_TRIPS, NIGHT_ST, CAL_FRIDAY);
    // Arrive by Saturday 01:30 CEST (23:30 UTC Friday).
    for tbr in [false, true] {
        let mut q = query("test:A", "test:B", utc(2026, 7, 24, 23, 30), tbr);
        q.arrive_by = true;
        let res = plan_journeys(&epoch, &q);
        assert!(
            res.journeys
                .iter()
                .any(|j| transit_trips(j) == vec!["test:night".to_string()]
                    && j.arrival == utc(2026, 7, 24, 23, 15)),
            "tbr={tbr}: {:?}",
            res.journeys
        );
    }
}

#[test]
fn after_midnight_trip_not_offered_on_wrong_day() {
    let epoch = epoch_from(NIGHT_STOPS, NIGHT_TRIPS, NIGHT_ST, CAL_FRIDAY);
    // Sunday 00:30 CEST: Saturday has no service, so nothing at 00:45.
    let at = utc(2026, 7, 25, 22, 30);
    for tbr in [false, true] {
        let res = plan_journeys(&epoch, &query("test:A", "test:B", at, tbr));
        assert!(
            res.journeys.iter().all(|j| transit_trips(j).is_empty()
                || j.departure >= utc(2026, 7, 31, 0, 0)),
            "tbr={tbr}: unexpected {:?}",
            res.journeys
        );
    }
}

// ---------------------------------------------------------------------------
// 2. DST: GTFS times are relative to noon − 12h
// ---------------------------------------------------------------------------

#[test]
fn dst_spring_forward_day_uses_noon_minus_12h_anchor() {
    let st = "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
              t1,08:30:00,08:30:00,A,1\nt1,09:00:00,09:00:00,B,2\n";
    let trips = "route_id,service_id,trip_id\nr1,S1,t1\n";
    let epoch = epoch_from(NIGHT_STOPS, trips, st, CAL_ALL);
    // 2026-03-29 (CET→CEST). 08:00 CEST == 06:00 UTC.
    for tbr in [false, true] {
        let res = plan_journeys(&epoch, &query("test:A", "test:B", utc(2026, 3, 29, 6, 0), tbr));
        let j = res.journeys.first().expect("journey");
        // 08:30 CEST == 06:30 UTC (local-midnight + 8h30 would give 07:30 UTC).
        assert_eq!(j.departure, utc(2026, 3, 29, 6, 30), "tbr={tbr}");
        assert_eq!(j.arrival, utc(2026, 3, 29, 7, 0), "tbr={tbr}");
    }
    // 2026-10-25 (CEST→CET). 08:00 CET == 07:00 UTC; 08:30 CET == 07:30 UTC.
    for tbr in [false, true] {
        let res = plan_journeys(&epoch, &query("test:A", "test:B", utc(2026, 10, 25, 7, 0), tbr));
        let j = res.journeys.first().expect("journey");
        assert_eq!(j.departure, utc(2026, 10, 25, 7, 30), "tbr={tbr}");
    }
}

// ---------------------------------------------------------------------------
// 3. TBR line pruning must not drop an earlier trip boarded at a later stop
// ---------------------------------------------------------------------------

#[test]
fn tbr_line_pruning_keeps_earlier_trip() {
    // Y and X are children of station P (60 s apart via the parent expansion).
    // Line pattern Y → X → Z. T1 passes Y before the query, X after it.
    let stops = "stop_id,stop_name,stop_lat,stop_lon,location_type,parent_station\n\
                 P,Parent,48.85,2.35,1,\n\
                 Y,Yankee,48.850,2.350,0,P\n\
                 X,Xray,48.851,2.351,0,P\n\
                 Z,Zulu,48.95,2.45,0,\n";
    let trips = "route_id,service_id,trip_id\nr1,S1,T1\nr1,S1,T2\n";
    let st = "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
              T1,07:50:00,07:50:00,Y,1\nT1,08:05:00,08:05:00,X,2\nT1,08:20:00,08:20:00,Z,3\n\
              T2,08:10:00,08:10:00,Y,1\nT2,08:25:00,08:25:00,X,2\nT2,08:40:00,08:40:00,Z,3\n";
    let epoch = epoch_from(stops, trips, st, CAL_ALL);
    // 07:55 CEST on 2026-07-27.
    let at = utc(2026, 7, 27, 5, 55);
    let raptor = plan_journeys(&epoch, &query("test:Y", "test:Z", at, false));
    let tbr = plan_journeys(&epoch, &query("test:Y", "test:Z", at, true));
    let best = utc(2026, 7, 27, 6, 20);
    assert_eq!(raptor.journeys.first().map(|j| j.arrival), Some(best));
    assert_eq!(
        tbr.journeys.first().map(|j| j.arrival),
        Some(best),
        "TBR pruned the earlier trip: {:?}",
        tbr.journeys
    );
    assert_eq!(transit_trips(&tbr.journeys[0]), vec!["test:T1".to_string()]);
}

// ---------------------------------------------------------------------------
// 4. Pooled TBR scratch must honour the query's round count
// ---------------------------------------------------------------------------

#[test]
fn tbr_pool_respects_rounds() {
    let stops = "stop_id,stop_name,stop_lat,stop_lon\n\
                 S0,S0,48.00,2.00\nS1,S1,48.10,2.10\nS2,S2,48.20,2.20\n\
                 S3,S3,48.30,2.30\nS4,S4,48.40,2.40\nS5,S5,48.50,2.50\n";
    let trips = "route_id,service_id,trip_id\n\
                 r1,S1,c1\nr2,S1,c2\nr3,S1,c3\nr4,S1,c4\nr5,S1,c5\n";
    let st = "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
              c1,08:00:00,08:00:00,S0,1\nc1,08:10:00,08:10:00,S1,2\n\
              c2,08:15:00,08:15:00,S1,1\nc2,08:25:00,08:25:00,S2,2\n\
              c3,08:30:00,08:30:00,S2,1\nc3,08:40:00,08:40:00,S3,2\n\
              c4,08:45:00,08:45:00,S3,1\nc4,08:55:00,08:55:00,S4,2\n\
              c5,09:00:00,09:00:00,S4,1\nc5,09:10:00,09:10:00,S5,2\n";
    let epoch = epoch_from(stops, trips, st, CAL_ALL);
    let at = utc(2026, 7, 27, 5, 50);
    // Warm the per-thread pool with a 4-round scratch.
    let mut small = query("test:S0", "test:S5", at, true);
    small.raptor_max_rounds = 1;
    small.max_transfers = 0;
    let _ = plan_journeys(&epoch, &small);
    // Needs 5 transit legs (≥ 5 rounds).
    let mut big = query("test:S0", "test:S5", at, true);
    big.raptor_max_rounds = 8;
    let res = plan_journeys(&epoch, &big);
    assert!(
        res.journeys.iter().any(|j| transit_trips(j).len() == 5),
        "5-leg journey missing: {:?}",
        res.journeys
    );
}

// ---------------------------------------------------------------------------
// 5. FLASH-TB (flagged) transfers use realtime-adjusted times
// ---------------------------------------------------------------------------

/// Enable flagged FLASH-TB mode with every transfer flagged (deterministic).
fn force_all_flags(epoch: &mut StaticEpoch) {
    epoch.num_cells = 1;
    epoch.stop_partition = vec![0; epoch.stops.len()];
    let mut all = CellBitSet::new(1);
    all.set_bit(0);
    epoch.flag_patterns = vec![all];
    epoch.arc_flag_pattern = vec![0; epoch.walk_edges.len()];
    for per_trip in &mut epoch.trip_transfers {
        for entries in per_trip.iter_mut() {
            for e in entries.iter_mut() {
                e.flag_pattern = 0;
            }
        }
    }
}

#[test]
fn flash_tb_transfer_respects_realtime_delay() {
    let stops = "stop_id,stop_name,stop_lat,stop_lon\n\
                 A,A,48.80,2.30\nB,B,48.85,2.35\nC,C,48.90,2.40\n";
    let trips = "route_id,service_id,trip_id\n\
                 r1,S1,t1\nr2,S1,t2a\nr2,S1,t2b\nr3,S1,slow\n";
    let st = "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
              t1,08:00:00,08:00:00,A,1\nt1,08:10:00,08:10:00,B,2\n\
              t2a,08:15:00,08:15:00,B,1\nt2a,08:25:00,08:25:00,C,2\n\
              t2b,08:30:00,08:30:00,B,1\nt2b,08:40:00,08:40:00,C,2\n\
              slow,08:00:00,08:00:00,A,1\nslow,09:30:00,09:30:00,C,2\n";
    let mut epoch = epoch_from(stops, trips, st, CAL_ALL);
    force_all_flags(&mut epoch);
    assert!(!epoch.trip_transfers.is_empty());

    let at = utc(2026, 7, 27, 5, 50); // 07:50 CEST
    let mut rt = HashMap::new();
    // t1 arrives 08:20 at B: t2a (08:15) is missed, t2b (08:30) is feasible.
    rt.insert(
        "test:t1".to_string(),
        RtTripAdjust {
            trip_delay_s: 10 * 60,
            ..Default::default()
        },
    );
    // t2b itself runs 5 minutes late → arrives C 08:45.
    rt.insert(
        "test:t2b".to_string(),
        RtTripAdjust {
            trip_delay_s: 5 * 60,
            ..Default::default()
        },
    );
    let expected_arrival = utc(2026, 7, 27, 6, 45);
    for tbr in [false, true] {
        let mut q = query("test:A", "test:C", at, tbr);
        q.rt_adjust = rt.clone();
        let res = plan_journeys(&epoch, &q);
        let j = res.journeys.first().expect("journey");
        assert_eq!(
            transit_trips(j),
            vec!["test:t1".to_string(), "test:t2b".to_string()],
            "tbr={tbr}: {:?}",
            res.journeys
        );
        assert_eq!(j.arrival, expected_arrival, "tbr={tbr}");
    }
}

// ---------------------------------------------------------------------------
// 6. Walk-first journeys depart at first boarding minus the access walk
// ---------------------------------------------------------------------------

#[test]
fn walk_first_departure_is_board_minus_walk() {
    let trips = "route_id,service_id,trip_id\nr1,S1,t1\n";
    let st = "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
              t1,08:00:00,08:00:00,A,1\nt1,08:30:00,08:30:00,B,2\n";
    let epoch = epoch_from(NIGHT_STOPS, trips, st, CAL_ALL);
    for tbr in [false, true] {
        // Leave-after 07:00 CEST from a point ~111 m south of A.
        let mut q = query("", "test:B", utc(2026, 7, 27, 5, 0), tbr);
        q.from_stop_id = None;
        q.from_lat = Some(48.799);
        q.from_lon = Some(2.30);
        let res = plan_journeys(&epoch, &q);
        let j = res
            .journeys
            .iter()
            .find(|j| !transit_trips(j).is_empty())
            .expect("journey");
        let Leg::Walk(w) = &j.legs[0] else {
            panic!("expected access walk first")
        };
        let board = utc(2026, 7, 27, 6, 0);
        assert_eq!(
            j.departure,
            board - chrono::Duration::seconds(w.duration_s as i64),
            "tbr={tbr}"
        );
        assert_eq!(j.duration_s, (j.arrival - j.departure).num_seconds());
        assert!(j.duration_s < 3600, "duration must exclude idle wait: {}", j.duration_s);
    }
}
