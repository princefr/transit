//! Benchmark FLASH-TB (TBR + arc-flags) vs RAPTOR on the IDFM dataset.
//!
//! Run with: cargo test --release --test bench_idfm -- --nocapture --ignored

use chrono::{TimeZone, Utc};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use transit::gtfs::pack::build_epoch;
use transit::gtfs::parse::load_gtfs_zip;
use transit::routing::{plan_journeys, ItineraryQuery};
use transit::search::search_stops;

fn idfm_zip() -> &'static Path {
    Path::new("/home/ondonda/rust/transit/data/idfm/current.zip")
}

fn make_query_geo(
    from: &transit::gtfs::pack::StopRecord,
    to: &transit::gtfs::pack::StopRecord,
    hour: u32,
    minute: u32,
) -> ItineraryQuery {
    let dep = Utc.with_ymd_and_hms(2026, 7, 30, hour, minute, 0).unwrap();
    ItineraryQuery {
        from_stop_id: Some(from.raw_id.clone()),
        to_stop_id: Some(to.raw_id.clone()),
        from_lat: from.lat,
        from_lon: from.lon,
        to_lat: to.lat,
        to_lon: to.lon,
        departure_at: dep,
        arrive_by: false,
        max_transfers: 6,
        max_results: 3,
        modes: None,
        max_walk_meters: 800,
        walk_speed_m_s: 1.2,
        raptor_max_rounds: 8,
        default_transfer_s: 120,
        timezone: "Europe/Paris".into(),
        excluded_trip_ids: Default::default(),
        rt_adjust: Default::default(),
        wheelchair: false,
        osrm_url: None,
        bike_from: false,
        bike_to: false,
        bike_speed_m_s: 4.2,
        max_bike_meters: 5000,
        use_tbr: false,
    }
}

fn make_query_tbr_geo(
    from: &transit::gtfs::pack::StopRecord,
    to: &transit::gtfs::pack::StopRecord,
    hour: u32,
    minute: u32,
) -> ItineraryQuery {
    let mut q = make_query_geo(from, to, hour, minute);
    q.use_tbr = true;
    q
}

#[test]
#[ignore]
fn bench_idfm_raptor_vs_tbr() {
    let zip_path = idfm_zip();
    if !zip_path.exists() {
        eprintln!("IDFM dataset not found at {}", zip_path.display());
        return;
    }

    eprintln!("Loading IDFM GTFS...");
    let t0 = Instant::now();
    let bundle = load_gtfs_zip("idfm", zip_path).expect("failed to load IDFM GTFS");
    eprintln!("  loaded in {:.1}s", t0.elapsed().as_secs_f64());

    eprintln!("Building epoch (includes 64-cell partition + Arc-Flags)...");
    let t0 = Instant::now();
    let epoch = build_epoch(vec![Arc::new(bundle)], vec![]);
    eprintln!("  built in {:.1}s", t0.elapsed().as_secs_f64());
    eprintln!(
        "  stops={}, trips={}, walk_edges={}, cells={}, arc_flags={}",
        epoch.stop_count(),
        epoch.trip_count(),
        epoch.walk_edges.len(),
        epoch.num_cells,
        epoch.arc_flags.len(),
    );

    let test_pairs = vec![
        ("Auber", "Nation", "Central RER A corridor"),
        ("Gare du Nord", "Massy-Palaiseau", "Suburban North-South RER B"),
        ("Chatelet", "La Defense", "High-frequency urban RER A / M1"),
        ("Saint-Lazare", "Versailles", "Regional Transilien / Suburban"),
        ("Bercy", "Montparnasse", "Inter-station Metro transfer"),
    ];

    eprintln!("\n==========================================================================");
    eprintln!(" FLASH-TB vs RAPTOR BENCHMARK ON IDFM (PARIS REGION)");
    eprintln!("==========================================================================\n");

    let iterations = 20;
    let mut total_raptor_us = 0.0;
    let mut total_tbr_us = 0.0;

    for (from_name, to_name, desc) in test_pairs {
        let origins = search_stops(&epoch, from_name, None, 5);
        let destinations = search_stops(&epoch, to_name, None, 5);

        if origins.is_empty() || destinations.is_empty() {
            eprintln!("Skipping {} -> {} (stop not found)", from_name, to_name);
            continue;
        }

        let q_raptor = make_query_geo(&origins[0], &destinations[0], 8, 30);
        let q_tbr = make_query_tbr_geo(&origins[0], &destinations[0], 8, 30);

        // Warmup
        let res_r = plan_journeys(&epoch, &q_raptor);
        let res_t = plan_journeys(&epoch, &q_tbr);

        let q_raptor_1shot = {
            let mut q = q_raptor.clone();
            q.max_results = 1;
            q
        };
        let q_tbr_1shot = {
            let mut q = q_tbr.clone();
            q.max_results = 1;
            q
        };

        // Benchmark 1-shot query (single departure pass)
        let t0 = Instant::now();
        for _ in 0..iterations {
            let _ = plan_journeys(&epoch, &q_tbr_1shot);
        }
        let t_us_1shot = t0.elapsed().as_micros() as f64 / iterations as f64;

        // Benchmark RAPTOR API pipeline
        let t0 = Instant::now();
        for _ in 0..iterations {
            let _ = plan_journeys(&epoch, &q_raptor);
        }
        let r_dur = t0.elapsed();
        let r_us = r_dur.as_micros() as f64 / iterations as f64;

        // Benchmark FLASH-TB (TBR) API pipeline (multi-departure k-best Pareto loop)
        let t0 = Instant::now();
        for _ in 0..iterations {
            let _ = plan_journeys(&epoch, &q_tbr);
        }
        let t_dur = t0.elapsed();
        let t_us = t_dur.as_micros() as f64 / iterations as f64;

        total_raptor_us += r_us;
        total_tbr_us += t_us;

        let speedup = if t_us > 0.0 { r_us / t_us } else { 1.0 };
        let match_status = match (res_r.journeys.first(), res_t.journeys.first()) {
            (Some(jr), Some(jt)) => {
                if jr.arrival == jt.arrival {
                    "MATCHED (Identical Arrival)".to_string()
                } else {
                    format!(
                        "DIFFERENT ARRIVAL (RAPTOR: {} @ {}, FLASH-TB: {} @ {})",
                        jr.departure.format("%H:%M"),
                        jr.arrival.format("%H:%M"),
                        jt.departure.format("%H:%M"),
                        jt.arrival.format("%H:%M")
                    )
                }
            }
            (None, None) => "MATCHED (No path)".to_string(),
            _ => "MISMATCHED (One found path, other did not)".to_string(),
        };

        eprintln!(
            "Route: {:<15} -> {:<18} ({})",
            from_name, to_name, desc
        );
        eprintln!(
            "  RAPTOR Full API:  {:>8.1} µs/op\n  FLASH-TB Full API: {:>8.1} µs/op | Speedup: {:>5.2}x | {}\n  FLASH-TB 1-Shot:   {:>8.1} µs/op",
            r_us, t_us, speedup, match_status, t_us_1shot
        );
        eprintln!("--------------------------------------------------------------------------");
    }

    let avg_speedup = if total_tbr_us > 0.0 {
        total_raptor_us / total_tbr_us
    } else {
        1.0
    };

    eprintln!("\n==========================================================================");
    eprintln!(
        " OVERALL MEAN: RAPTOR = {:.1} µs | FLASH-TB = {:.1} µs | Speedup = {:.2}x",
        total_raptor_us / 5.0,
        total_tbr_us / 5.0,
        avg_speedup
    );
    eprintln!("==========================================================================\n");
}
