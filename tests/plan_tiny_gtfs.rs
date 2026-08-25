//! End-to-end: tiny GTFS zip → parse → build_epoch → plan_journeys finds a trip.

use chrono::{TimeZone, Utc};
use std::collections::HashSet;
use std::io::{Cursor, Write};
use std::sync::Arc;
use transit::gtfs::pack::build_epoch;
use transit::gtfs::parse::load_gtfs_bytes;
use transit::routing::{plan_journeys, ItineraryQuery};
use transit::search::{near, search_stops, search_stops_filtered};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn tiny_gtfs_zip() -> Vec<u8> {
    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);
    let opts = SimpleFileOptions::default();

    zip.start_file("agency.txt", opts).unwrap();
    zip.write_all(b"agency_id,agency_name,agency_timezone\na1,Test Agency,Europe/Paris\n")
        .unwrap();

    zip.start_file("stops.txt", opts).unwrap();
    zip.write_all(
        b"stop_id,stop_name,stop_lat,stop_lon,location_type,parent_station\n\
          A,Station A,48.86,2.35,1,\n\
          A1,Platform A1,48.86,2.35,0,A\n\
          B,Station B,45.76,4.86,1,\n\
          B1,Platform B1,45.76,4.86,0,B\n",
    )
    .unwrap();

    zip.start_file("routes.txt", opts).unwrap();
    zip.write_all(
        b"route_id,route_short_name,route_long_name,route_type,agency_id\nr1,TGV,Paris-Lyon,2,a1\n",
    )
    .unwrap();

    zip.start_file("calendar.txt", opts).unwrap();
    zip.write_all(
        b"service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
          S1,1,1,1,1,1,1,1,20260101,20261231\n",
    )
    .unwrap();

    zip.start_file("trips.txt", opts).unwrap();
    zip.write_all(b"route_id,service_id,trip_id,trip_headsign\nr1,S1,t1,Lyon\n")
        .unwrap();

    zip.start_file("stop_times.txt", opts).unwrap();
    zip.write_all(
        b"trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
          t1,08:00:00,08:00:00,A1,1\n\
          t1,10:00:00,10:00:00,B1,2\n",
    )
    .unwrap();

    zip.finish().unwrap().into_inner()
}

fn sample_query(from: &str, to: &str) -> ItineraryQuery {
    // 2026-03-15 07:00 Europe/Paris ≈ 06:00 UTC (CET)
    let departure_at = Utc.with_ymd_and_hms(2026, 3, 15, 6, 0, 0).unwrap();
    ItineraryQuery {
        from_stop_id: Some(from.into()),
        to_stop_id: Some(to.into()),
        from_lat: None,
        from_lon: None,
        to_lat: None,
        to_lon: None,
        departure_at,
        arrive_by: false,
        max_transfers: 4,
        max_results: 5,
        modes: None,
        max_walk_meters: 2000,
        walk_speed_m_s: 1.2,
        raptor_max_rounds: 6,
        default_transfer_s: 120,
        timezone: "Europe/Paris".into(),
        osrm_url: None,
            bike_from: false,
            bike_to: false,
            bike_speed_m_s: 4.2,
            max_bike_meters: 5000,
            use_tbr: false,
            excluded_trip_ids: HashSet::new(),
            excluded_lines: HashSet::new(),
        rt_adjust: Default::default(),
        wheelchair: false,
    }
}

#[test]
fn load_build_and_plan_a_to_b() {
    let bytes = tiny_gtfs_zip();
    let bundle = load_gtfs_bytes("test", &bytes).expect("parse tiny gtfs");
    assert_eq!(bundle.stops.len(), 4);
    assert_eq!(bundle.trips.len(), 1);

    let epoch = build_epoch(vec![Arc::new(bundle)], vec![]);
    assert_eq!(epoch.stop_count(), 4);
    assert_eq!(epoch.trip_count(), 1);

    // Search should find Station A; station-only drops platforms
    let hits = search_stops(&epoch, "station a", None, 10);
    assert!(hits.iter().any(|s| s.name == "Station A"));
    let stations_only = search_stops_filtered(&epoch, "station", None, 10, true);
    assert!(stations_only.iter().all(|s| s.is_station()));

    // Near Paris should return Station A first
    let near_hits = near(&epoch, 48.86, 2.35, 5_000.0, 5, true);
    assert!(!near_hits.is_empty());
    assert_eq!(near_hits[0].0.raw_id, "A");

    // Journey from A (parent) or A1 → B / B1
    for (from, to) in [
        ("test:A1", "test:B1"),
        ("test:A", "test:B"),
        ("test:A", "test:B1"),
    ] {
        let q = sample_query(from, to);
        let result = plan_journeys(&epoch, &q);
        assert!(
            !result.journeys.is_empty(),
            "expected a journey for {from} → {to}, got none (degraded={})",
            result.realtime_degraded
        );
        let j = &result.journeys[0];
        assert!(j.duration_s > 0, "duration should be positive");
        assert!(
            j.legs.iter().any(|leg| matches!(leg, transit::routing::Leg::Transit(_))),
            "expected at least one transit leg for {from} → {to}"
        );
    }
}
