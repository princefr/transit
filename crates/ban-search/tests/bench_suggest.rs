//! Benchmark `BanIndex::suggest` against the real index when present.
//!
//! Run with:
//!   cargo test --release -p ban-search --test bench_suggest -- --nocapture --ignored
//!
//! Skips (ok) when `data/ban/index.bin` does not exist.

use ban_search::BanIndex;
use std::path::PathBuf;
use std::time::Instant;

fn index_path() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ban/index.bin");
    p.is_file().then_some(p)
}

/// Query mix: prefixes, multi-token, typos, house numbers, city, postcode.
const QUERIES: &[&str] = &[
    "ru",
    "rue",
    "rue de",
    "rue de la gare",
    "gare",
    "gare de fontainebleau",
    "chateau",
    "chateau de fontainebleau",
    "melun",
    "melu",
    "fontainebleau",
    "rivolli",
    "av jean jaures",
    "bd saint michel",
    "boulevard saint michel melun",
    "les quinze",
    "12 rue de la gare",
    "8 chemin des vignes",
    "5 avenue de la gare 77000",
    "77",
    "77000",
    "77000 melun",
    "place du marche",
    "impasse",
    "voie romaine",
    "saint",
    "saint as",
    "la chapelle la reine",
    "montigny le bretonneux", // not in 77? still exercises scan path
    "villiers sur morin",
];

#[test]
#[ignore]
fn bench_suggest() {
    let Some(path) = index_path() else {
        eprintln!("skipping: no data/ban/index.bin — run `make ban-index` first");
        return;
    };
    let t0 = Instant::now();
    let idx = BanIndex::load(&path).expect("load index");
    eprintln!(
        "index loaded in {:?} (streets={} addresses={})",
        t0.elapsed(),
        idx.street_count(),
        idx.address_count()
    );

    let iters = 20;
    // Warmup (page in, populate caches).
    for q in QUERIES {
        std::hint::black_box(idx.suggest(q, 8));
    }

    let mut per_query = vec![Vec::new(); QUERIES.len()];
    let total = Instant::now();
    for _ in 0..iters {
        for (i, q) in QUERIES.iter().enumerate() {
            let t = Instant::now();
            let hits = idx.suggest(q, 8);
            let d = t.elapsed();
            std::hint::black_box(&hits);
            per_query[i].push(d);
        }
    }
    let wall = total.elapsed();

    let n = QUERIES.len() * iters;
    let mut all: Vec<u128> = per_query.iter().flatten().map(|d| d.as_nanos()).collect();
    all.sort_unstable();
    let avg = wall.as_nanos() / n as u128;
    let p50 = all[n / 2];
    let p95 = all[(n as f32 * 0.95) as usize % n];
    let max = *all.last().unwrap();
    eprintln!("\n=== suggest bench ({n} calls, limit=8) ===");
    eprintln!(
        "wall   {wall:?}   avg {} µs   p50 {} µs   p95 {} µs   max {} ms",
        avg / 1000,
        p50 / 1000,
        p95 / 1000,
        max / 1_000_000
    );

    eprintln!("\nper-query avg:");
    let mut rows: Vec<(u128, &str)> = QUERIES
        .iter()
        .zip(&per_query)
        .map(|(q, ds)| (ds.iter().map(|d| d.as_nanos()).sum::<u128>() / iters as u128, *q))
        .collect();
    rows.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
    for (us, q) in rows.iter().take(10) {
        eprintln!("  {q:<32} {:>6} µs", us / 1000);
    }
}
