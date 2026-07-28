//! Continuous PRIM **SIRI Lite** pollers (not Navitia).
//!
//! | API | Path | Quota | Role |
//! |-----|------|-------|------|
//! | Stop Monitoring (unitaire) | `GET /marketplace/stop-monitoring?MonitoringRef=` | 1e6/day | Next arrivals at one stop |
//! | Estimated Timetable (globale) | `GET /marketplace/estimated-timetable?LineRef=` | 1e3/day | Network sample by line |
//! | General Message | `GET /marketplace/general-message?LineRef=` | 2e4/day | Screen / traffic messages |
//!
//! Each poller respects a soft daily budget, accumulates partial LineRef/MonitoringRef
//! results, then publishes a merged snapshot into the feed supervisor.

use super::client::PrimClient;
use crate::config::PrimConfig;
use crate::feeds::rt_poller::RtKind;
use crate::feeds::FeedEvent;
use crate::rt::adapter::{ingest_realtime, RealtimeFormat};
use crate::rt::overlay::{AlertRt, FeedRtState, TripRt, VehiclePos};
use arc_swap::ArcSwap;
use chrono::{Datelike, Utc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Built-in samples (metro + RER + Transilien + tram + major bus)
// ---------------------------------------------------------------------------

/// Major IDFM line refs for sampled SIRI ET / GM.
/// Full-network ET without `LineRef` is huge (~60MB) and burns the 1k/day budget.
/// Codes from `data/idfm/referentiel-catalog.json` (STIF Line ids).
const DEFAULT_LINE_REFS: &[&str] = &[
    // --- Metro (all numbered lines + branches) ---
    "STIF:Line::C01371:", // Metro 1
    "STIF:Line::C01372:", // Metro 2
    "STIF:Line::C01373:", // Metro 3
    "STIF:Line::C01386:", // Metro 3B
    "STIF:Line::C01374:", // Metro 4
    "STIF:Line::C01375:", // Metro 5
    "STIF:Line::C01376:", // Metro 6
    "STIF:Line::C01377:", // Metro 7
    "STIF:Line::C01387:", // Metro 7B
    "STIF:Line::C01378:", // Metro 8
    "STIF:Line::C01379:", // Metro 9
    "STIF:Line::C01380:", // Metro 10
    "STIF:Line::C01381:", // Metro 11
    "STIF:Line::C01382:", // Metro 12
    "STIF:Line::C01383:", // Metro 13
    "STIF:Line::C01384:", // Metro 14
    "STIF:Line::C02874:", // Metro 15 (Grand Paris Express)
    "STIF:Line::C02833:", // Metro 18
    // --- RER ---
    "STIF:Line::C01742:", // RER A
    "STIF:Line::C01743:", // RER B
    "STIF:Line::C01727:", // RER C
    "STIF:Line::C01728:", // RER D
    "STIF:Line::C01729:", // RER E
    // --- Transilien (major suburban rail) ---
    "STIF:Line::C01737:", // H
    "STIF:Line::C01739:", // J
    "STIF:Line::C01740:", // L
    "STIF:Line::C01736:", // N
    "STIF:Line::C01730:", // P
    "STIF:Line::C01731:", // R
    "STIF:Line::C01741:", // U
    "STIF:Line::C01738:", // K
    "STIF:Line::C02711:", // V
    // --- Tram ---
    "STIF:Line::C01389:", // T1
    "STIF:Line::C01390:", // T2
    "STIF:Line::C01391:", // T3a
    "STIF:Line::C01679:", // T3b
    "STIF:Line::C01843:", // T4
    "STIF:Line::C01684:", // T5
    "STIF:Line::C01794:", // T6
    "STIF:Line::C01774:", // T7
    "STIF:Line::C01795:", // T8
    "STIF:Line::C02317:", // T9
    "STIF:Line::C02528:", // T10
    "STIF:Line::C01999:", // T11
    "STIF:Line::C02529:", // T12
    "STIF:Line::C02344:", // T13
    "STIF:Line::C02732:", // T14
    // --- Major Paris bus (core contracts; short names 20–96) ---
    "STIF:Line::C01072:", // bus 20
    "STIF:Line::C01073:", // bus 21
    "STIF:Line::C01075:", // bus 24
    "STIF:Line::C01076:", // bus 26
    "STIF:Line::C01077:", // bus 27
    "STIF:Line::C01078:", // bus 28
    "STIF:Line::C01079:", // bus 29
    "STIF:Line::C01080:", // bus 30
    "STIF:Line::C01083:", // bus 38
    "STIF:Line::C01084:", // bus 39
    "STIF:Line::C01087:", // bus 46
    "STIF:Line::C01088:", // bus 47
    "STIF:Line::C01089:", // bus 48
    "STIF:Line::C01093:", // bus 56
    "STIF:Line::C01094:", // bus 57
    "STIF:Line::C01095:", // bus 58
    "STIF:Line::C01096:", // bus 60
    "STIF:Line::C01098:", // bus 62
    "STIF:Line::C01099:", // bus 63
    "STIF:Line::C01100:", // bus 64
    "STIF:Line::C01103:", // bus 67
    "STIF:Line::C01104:", // bus 68
    "STIF:Line::C01105:", // bus 69
    "STIF:Line::C01106:", // bus 70
    "STIF:Line::C01111:", // bus 76
    "STIF:Line::C01112:", // bus 80
    "STIF:Line::C01114:", // bus 82
    "STIF:Line::C01115:", // bus 83
    "STIF:Line::C01116:", // bus 84
    "STIF:Line::C01118:", // bus 86
    "STIF:Line::C01119:", // bus 87
    "STIF:Line::C01121:", // bus 89
    "STIF:Line::C01122:", // bus 91
    "STIF:Line::C01123:", // bus 92
    "STIF:Line::C01125:", // bus 94
    "STIF:Line::C01127:", // bus 96
    "STIF:Line::C02205:", // PC (Petite Ceinture bus)
];

/// High-traffic hubs for continuous SM warm cache (when no config seeds).
/// Prefer **StopArea** form (`STIF:StopArea:SP:…:`) — aggregates quays; works on PRIM
/// (see `docs/IDFM_VEHICLE_MONITORING.md`). StopPoint refs kept as best-effort backups.
/// Keep this list **small**. PRIM short-term rate limits (HTTP 429) kick in long
/// before the daily 1M SM budget. Warm poll = 1 ref every ~60s.
const DEFAULT_SEED_STOPS: &[&str] = &[
    "STIF:StopArea:SP:43152:",  // documented working StopArea
    "STIF:StopPoint:Q:412986:", // docs sample quay
    "STIF:StopPoint:Q:463041:", // Châtelet (best-effort)
];

// ---------------------------------------------------------------------------
// Daily soft quota (UTC day)
// ---------------------------------------------------------------------------

struct DailyQuota {
    name: &'static str,
    budget: u32,
    used: AtomicU32,
    day_ord: AtomicU32,
}

impl DailyQuota {
    fn new(name: &'static str, budget: u32) -> Self {
        Self {
            name,
            budget: budget.max(1),
            used: AtomicU32::new(0),
            day_ord: AtomicU32::new(utc_day_ordinal()),
        }
    }

    fn roll_if_needed(&self) {
        let today = utc_day_ordinal();
        let prev = self.day_ord.load(Ordering::Relaxed);
        if today != prev
            && self
                .day_ord
                .compare_exchange(prev, today, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
        {
            self.used.store(0, Ordering::SeqCst);
            info!(quota = self.name, budget = self.budget, "PRIM daily quota reset");
        }
    }

    /// Try to consume one call. Returns false if budget exhausted.
    fn try_consume(&self) -> bool {
        self.roll_if_needed();
        loop {
            let u = self.used.load(Ordering::Relaxed);
            if u >= self.budget {
                return false;
            }
            if self
                .used
                .compare_exchange(u, u + 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn used(&self) -> u32 {
        self.roll_if_needed();
        self.used.load(Ordering::Relaxed)
    }

    fn remaining(&self) -> u32 {
        self.roll_if_needed();
        self.budget
            .saturating_sub(self.used.load(Ordering::Relaxed))
    }
}

/// How many ET LineRefs to fetch this tick without overspending the soft daily budget.
/// Spreads `remaining` calls evenly over ticks left in the UTC day, capped by `max_per_tick`.
fn et_batch_size(quota: &DailyQuota, interval: Duration, max_per_tick: u32) -> usize {
    let rem = quota.remaining() as u64;
    if rem == 0 {
        return 0;
    }
    // Hard cap 1 LineRef per tick — PRIM short-term limits are stricter than daily.
    let max = max_per_tick.max(1).min(1) as u64;
    let _ = interval;
    rem.min(max) as usize
}

fn utc_day_ordinal() -> u32 {
    let d = Utc::now().date_naive();
    // compact day key
    (d.year() as u32).wrapping_mul(400).wrapping_add(d.ordinal())
}

// ---------------------------------------------------------------------------
// SM interest: GraphQL live boards register here for continuous warm poll
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SmInterestInner {
    /// MonitoringRef → last registered / refreshed
    refs: HashMap<String, Instant>,
}

/// Process-wide interest set for stop-monitoring warm polling.
static SM_INTEREST: OnceLock<Mutex<SmInterestInner>> = OnceLock::new();
/// Shared soft quota for SM (on-demand + warm poller).
static SM_QUOTA: OnceLock<Arc<DailyQuota>> = OnceLock::new();

fn sm_interest() -> &'static Mutex<SmInterestInner> {
    SM_INTEREST.get_or_init(|| Mutex::new(SmInterestInner::default()))
}

/// Register a MonitoringRef for continuous stop-monitoring pulls (e.g. after GraphQL live).
pub fn register_sm_interest(monitoring_ref: &str) {
    let mon = monitoring_ref.trim();
    if mon.is_empty() {
        return;
    }
    if let Ok(mut g) = sm_interest().lock() {
        g.refs.insert(mon.to_string(), Instant::now());
        // Cap map size (viewport + live boards + seeds)
        if g.refs.len() > 512 {
            let mut pairs: Vec<_> = g.refs.iter().map(|(k, v)| (k.clone(), *v)).collect();
            pairs.sort_by_key(|(_, t)| *t);
            let drop_n = g.refs.len() - 400;
            for (k, _) in pairs.into_iter().take(drop_n) {
                g.refs.remove(&k);
            }
        }
    }
}

/// LineRef interest for viewport-biased ET (map pan).
static ET_LINE_INTEREST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn et_line_interest() -> &'static Mutex<HashMap<String, Instant>> {
    ET_LINE_INTEREST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Prefer these LineRefs on the next ET cycles (e.g. lines visible on the map).
pub fn register_et_line_interest(line_ref: &str) {
    let lr = line_ref.trim();
    if lr.is_empty() {
        return;
    }
    if let Ok(mut g) = et_line_interest().lock() {
        g.insert(lr.to_string(), Instant::now());
        if g.len() > 128 {
            let mut pairs: Vec<_> = g.iter().map(|(k, v)| (k.clone(), *v)).collect();
            pairs.sort_by_key(|(_, t)| *t);
            let drop_n = g.len() - 96;
            for (k, _) in pairs.into_iter().take(drop_n) {
                g.remove(&k);
            }
        }
    }
}

/// Build ET poll order: fresh viewport lines first, then rotate default list.
fn et_line_order(default_lines: &[String], line_idx: &mut usize, batch: usize) -> Vec<String> {
    let batch = batch.max(1);
    let mut out = Vec::with_capacity(batch);
    let mut seen = std::collections::HashSet::new();
    if let Ok(g) = et_line_interest().lock() {
        let mut pairs: Vec<_> = g.iter().map(|(k, v)| (k.clone(), *v)).collect();
        pairs.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
        for (k, _) in pairs {
            if seen.insert(k.clone()) {
                out.push(k);
            }
            if out.len() >= batch {
                return out;
            }
        }
    }
    if default_lines.is_empty() {
        return out;
    }
    while out.len() < batch {
        let line = default_lines[*line_idx % default_lines.len()].clone();
        *line_idx = line_idx.wrapping_add(1);
        if seen.insert(line.clone()) {
            out.push(line);
        } else if seen.len() >= default_lines.len() {
            // All defaults already included
            break;
        }
    }
    out
}

/// Soft-register SM stops + ET line refs for a map viewport (no PRIM I/O here).
///
/// Call from GraphQL `vehicles(bbox)` / warm endpoint. Caps registrations so pan
/// does not flood PRIM (poller still 1 SM/tick + ET budget).
pub fn register_viewport_live_interest(
    epoch: &crate::gtfs::pack::StaticEpoch,
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
) {
    let mid_lat = (min_lat + max_lat) * 0.5;
    let mid_lon = (min_lon + max_lon) * 0.5;
    // Approx radius from center to corner (m)
    let radius = crate::link::geo::haversine_m(mid_lat, mid_lon, max_lat, max_lon)
        .clamp(200.0, 6_000.0);
    // Soft SM interest near viewport center
    register_nearby_sm_interest(epoch, mid_lat, mid_lon, radius, 12);

    // Line refs: collect route_ids of trips serving stops in bbox, map to STIF Line::
    let mut line_tokens: HashMap<String, u32> = HashMap::new();
    let mut stop_idxs = Vec::new();
    for (i, s) in epoch.stops.iter().enumerate() {
        let (Some(lat), Some(lon)) = (s.lat, s.lon) else {
            continue;
        };
        if lat < min_lat || lat > max_lat || lon < min_lon || lon > max_lon {
            continue;
        }
        stop_idxs.push(i);
        if stop_idxs.len() > 800 {
            break;
        }
    }
    for &si in &stop_idxs {
        let deps = epoch
            .stop_departures
            .get(si)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        for &(trip_idx, _) in deps.iter().take(12) {
            let Some(trip) = epoch.trips.get(trip_idx as usize) else {
                continue;
            };
            // Prefer raw STIF:Line::… from route_id if present
            let rid = &trip.route_id;
            if let Some(token) = extract_stif_line_token(rid) {
                *line_tokens.entry(token).or_insert(0) += 1;
            }
        }
    }
    let mut ranked: Vec<_> = line_tokens.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    for (token, _) in ranked.into_iter().take(40) {
        // Default list uses STIF:Line::TOKEN:
        let lr = if token.starts_with("STIF:") {
            token
        } else {
            format!("STIF:Line::{token}:")
        };
        register_et_line_interest(&lr);
    }
}

fn extract_stif_line_token(route_id: &str) -> Option<String> {
    // idfm:IDFM:C01742 or idfm:STIF:Line::C01742: or …C01371
    let upper = route_id.to_ascii_uppercase();
    if let Some(i) = upper.find("LINE::") {
        let rest = &upper[i + 6..];
        let tok: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if tok.len() >= 4 {
            return Some(tok);
        }
    }
    // Token like C01742 / C01371 in the id
    for part in route_id.split(|c: char| !c.is_ascii_alphanumeric()) {
        let p = part.to_ascii_uppercase();
        if p.len() >= 5
            && p.starts_with('C')
            && p.chars().skip(1).all(|c| c.is_ascii_digit() || c.is_ascii_alphanumeric())
        {
            return Some(p);
        }
    }
    None
}

/// Register SM interest for stations near a map viewport / `near` query (best-effort).
/// Caps registrations so continuous map pan does not flood the interest set.
pub fn register_nearby_sm_interest(
    epoch: &crate::gtfs::pack::StaticEpoch,
    lat: f64,
    lon: f64,
    radius_m: f64,
    max_stops: usize,
) {
    let max_stops = max_stops.clamp(1, 32);
    let radius = radius_m.clamp(50.0, 4_000.0);
    // Prefer stations (location_type=1) — map better to StopArea-style MonitoringRefs.
    let hits = crate::search::near_stops(epoch, lat, lon, radius, max_stops, true);
    let hits = if hits.is_empty() {
        crate::search::near_stops(epoch, lat, lon, radius, max_stops, false)
    } else {
        hits
    };
    for s in hits {
        if let Some(m) = super::stop_board::monitoring_ref_from_stop_id(&s.raw_id)
            .or_else(|| super::stop_board::monitoring_ref_from_stop_id(&s.id))
        {
            register_sm_interest(&m);
        }
    }
}

/// Consume one SM quota unit for on-demand GraphQL boards. Returns false if exhausted.
pub fn sm_try_consume_quota() -> bool {
    SM_QUOTA
        .get()
        .map(|q| q.try_consume())
        .unwrap_or(true) // before pollers start, allow
}

fn list_sm_targets(seeds: &[String], max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Fresh interest first
    if let Ok(g) = sm_interest().lock() {
        let mut pairs: Vec<_> = g.refs.iter().map(|(k, v)| (k.clone(), *v)).collect();
        pairs.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
        for (k, _) in pairs {
            if seen.insert(k.clone()) {
                out.push(k);
            }
            if out.len() >= max {
                return out;
            }
        }
    }
    for s in seeds {
        let t = s.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Accumulators (merge partial LineRef / MonitoringRef responses)
// ---------------------------------------------------------------------------

struct TripAccum {
    trips: HashMap<String, TripRt>,
    max_age: Duration,
}

impl TripAccum {
    fn new(max_age: Duration) -> Self {
        Self {
            trips: HashMap::new(),
            max_age,
        }
    }

    fn merge(&mut self, incoming: HashMap<String, TripRt>) {
        let now = Utc::now();
        for (k, v) in incoming {
            self.trips.insert(k, v);
        }
        // Drop very stale trips (no stop-time updates after max_age wall clock from insert — use trip delay time if any)
        // Simple: cap map size by removing arbitrary oldest keys if huge
        if self.trips.len() > 50_000 {
            let drop: Vec<_> = self.trips.keys().take(10_000).cloned().collect();
            for k in drop {
                self.trips.remove(&k);
            }
        }
        let _ = (now, self.max_age);
    }

    fn snapshot(&self, feed_id: &str) -> FeedRtState {
        let mut s = FeedRtState::new(feed_id);
        s.trips = self.trips.clone();
        s.trip_update_count = s.trips.len();
        s.trip_updates_fetched_at = Some(Utc::now());
        s
    }
}

struct AlertAccum {
    by_id: HashMap<String, AlertRt>,
}

impl AlertAccum {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    fn merge(&mut self, alerts: Vec<AlertRt>) {
        for a in alerts {
            let key = if a.id.is_empty() {
                format!(
                    "{}|{}",
                    a.header.as_deref().unwrap_or(""),
                    a.description.as_deref().unwrap_or("")
                )
            } else {
                a.id.clone()
            };
            self.by_id.insert(key, a);
        }
        if self.by_id.len() > 5_000 {
            let drop: Vec<_> = self.by_id.keys().take(1_000).cloned().collect();
            for k in drop {
                self.by_id.remove(&k);
            }
        }
    }

    fn snapshot(&self, feed_id: &str) -> FeedRtState {
        let mut s = FeedRtState::new(feed_id);
        s.alerts = self.by_id.values().cloned().collect();
        s.alert_count = s.alerts.len();
        s.alerts_fetched_at = Some(Utc::now());
        s
    }
}

struct VehicleAccum {
    vehicles: HashMap<String, VehiclePos>,
}

impl VehicleAccum {
    fn new() -> Self {
        Self {
            vehicles: HashMap::new(),
        }
    }

    fn merge(&mut self, incoming: HashMap<String, VehiclePos>) {
        for (k, v) in incoming {
            self.vehicles.insert(k, v);
        }
        if self.vehicles.len() > 20_000 {
            let drop: Vec<_> = self.vehicles.keys().take(5_000).cloned().collect();
            for k in drop {
                self.vehicles.remove(&k);
            }
        }
    }

    fn snapshot(&self, feed_id: &str) -> FeedRtState {
        let mut s = FeedRtState::new(feed_id);
        s.vehicles = self.vehicles.clone();
        s.vehicle_count = s.vehicles.len();
        s.vehicles_fetched_at = Some(Utc::now());
        s
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

fn resolve_line_refs(cfg: &PrimConfig) -> Vec<String> {
    if !cfg.line_refs.is_empty() {
        return cfg
            .line_refs
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    DEFAULT_LINE_REFS.iter().map(|s| (*s).to_string()).collect()
}

fn resolve_seed_stops(cfg: &PrimConfig) -> Vec<String> {
    if !cfg.seed_monitoring_refs.is_empty() {
        return cfg
            .seed_monitoring_refs
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    DEFAULT_SEED_STOPS.iter().map(|s| (*s).to_string()).collect()
}

/// Spawn continuous SIRI Lite pollers when config + API key allow.
pub fn spawn_prim_pollers(
    prim: &PrimConfig,
    client: PrimClient,
    outbound: Arc<Semaphore>,
    tx: mpsc::Sender<FeedEvent>,
    cancel: CancellationToken,
) {
    let feed_id = prim.feed_id.clone();
    let lines = resolve_line_refs(prim);
    let seeds = resolve_seed_stops(prim);

    let et_q = Arc::new(DailyQuota::new("siri-et", prim.et_daily_budget));
    let gm_q = Arc::new(DailyQuota::new("siri-gm", prim.gm_daily_budget));
    let sm_q = Arc::new(DailyQuota::new("siri-sm", prim.sm_daily_budget));
    let _ = SM_QUOTA.set(sm_q.clone());

    // Seed interest from config
    for s in &seeds {
        register_sm_interest(s);
    }

    info!(
        feed_id = %feed_id,
        et_interval_s = prim.et_interval().as_secs(),
        et_lines_per_tick = prim.et_lines_per_tick,
        gm_interval_s = prim.gm_interval().as_secs(),
        sm_interval_s = prim.sm_interval().as_secs(),
        sm_max_stops_per_cycle = prim.sm_max_stops_per_cycle,
        et_budget = prim.et_daily_budget,
        gm_budget = prim.gm_daily_budget,
        sm_budget = prim.sm_daily_budget,
        lines = lines.len(),
        seed_stops = seeds.len(),
        base_url = %prim.base_url,
        "PRIM SIRI Lite pollers starting (SM + ET + GM continuous)"
    );

    // --- Estimated Timetable (globale, multi LineRef sample) ---
    {
        let client = client.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        let outbound = outbound.clone();
        let interval = prim.et_interval();
        let feed_id = feed_id.clone();
        let lines = lines.clone();
        let et_q = et_q.clone();
        let lines_per_tick = prim.et_lines_per_tick.max(1);
        tokio::spawn(async move {
            run_siri_et_poller(
                client,
                feed_id,
                lines,
                interval,
                lines_per_tick,
                et_q,
                outbound,
                tx,
                cancel,
            )
            .await;
        });
    }

    // --- General Message (screen / traffic) ---
    {
        let client = client.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        let outbound = outbound.clone();
        let interval = prim.gm_interval();
        let feed_id = feed_id.clone();
        let lines = lines.clone();
        let gm_q = gm_q.clone();
        tokio::spawn(async move {
            run_siri_gm_poller(client, feed_id, lines, interval, gm_q, outbound, tx, cancel).await;
        });
    }

    // --- Stop Monitoring (unitaire, warm interest set) ---
    {
        let client = client.clone();
        let tx = tx.clone();
        let cancel = cancel.clone();
        let outbound = outbound.clone();
        let interval = prim.sm_interval();
        let feed_id = feed_id.clone();
        let seeds = seeds.clone();
        let sm_q = sm_q.clone();
        let max_stops = prim.sm_max_stops_per_cycle.max(1) as usize;
        tokio::spawn(async move {
            run_siri_sm_poller(
                client, feed_id, seeds, interval, max_stops, sm_q, outbound, tx, cancel,
            )
            .await;
        });
    }
}

fn jitter_ms(max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    t.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17) % (max_ms + 1)
}

/// Minimal URL-encoding for LineRef / MonitoringRef (`:` → `%3A`).
pub fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ET — Prochains passages globale
// ---------------------------------------------------------------------------

async fn run_siri_et_poller(
    client: PrimClient,
    feed_id: String,
    lines: Vec<String>,
    interval: Duration,
    lines_per_tick: u32,
    quota: Arc<DailyQuota>,
    outbound: Arc<Semaphore>,
    tx: mpsc::Sender<FeedEvent>,
    cancel: CancellationToken,
) {
    if lines.is_empty() {
        warn!("PRIM SIRI ET: no LineRef list — poller idle");
        return;
    }
    let startup = Duration::from_millis(jitter_ms(2_500));
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(startup) => {}
    }

    let mut accum = TripAccum::new(Duration::from_secs(45 * 60));
    let mut veh_accum = VehicleAccum::new();
    let mut line_idx = 0usize;
    loop {
        let batch = et_batch_size(&quota, interval, lines_per_tick);
        if batch == 0 {
            warn!(
                used = quota.used(),
                budget = quota.budget,
                "PRIM SIRI ET daily budget exhausted — sleeping 1h"
            );
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(3600)) => continue,
            }
        }

        let mut any_ok = false;
        let t0 = Instant::now();
        let mut lines_ok = 0u32;
        let mut trips_this_tick = 0usize;
        let mut gps_this_tick = 0usize;

        // Prefer viewport-interested lines (map pan) then rotate the default list.
        let order = et_line_order(&lines, &mut line_idx, batch as usize);

        for line in order {
            if !quota.try_consume() {
                break;
            }
            let permit = match outbound.acquire().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let path = format!(
                "/marketplace/estimated-timetable?LineRef={}",
                urlencoding_encode(&line)
            );

            match client.get_bytes(&path).await {
                Ok(bytes) => {
                    let fid = feed_id.clone();
                    let parse = tokio::task::spawn_blocking(move || {
                        ingest_realtime(&fid, &bytes, RealtimeFormat::SiriEstimatedTimetable)
                    })
                    .await;
                    drop(permit);
                    match parse {
                        Ok(Ok(delta)) => {
                            trips_this_tick += delta.state.trip_update_count;
                            gps_this_tick += delta.state.vehicles.len();
                            accum.merge(delta.state.trips);
                            // ET may include VehicleLocation (true GPS) — keep it.
                            veh_accum.merge(delta.state.vehicles);
                            any_ok = true;
                            lines_ok += 1;
                            debug!(
                                feed_id = %feed_id,
                                line = %line,
                                line_trips = delta.state.trip_update_count,
                                "PRIM SIRI ET line ok"
                            );
                        }
                        Ok(Err(e)) => {
                            warn!(feed_id = %feed_id, line = %line, error = %e, "SIRI ET parse failed")
                        }
                        Err(e) => {
                            warn!(feed_id = %feed_id, error = %e, "SIRI ET join failed")
                        }
                    }
                }
                Err(e) => {
                    drop(permit);
                    let msg = e.to_string();
                    if msg.contains("429") || msg.to_ascii_lowercase().contains("rate limit") {
                        warn!(
                            feed_id = %feed_id,
                            line = %line,
                            "PRIM SIRI ET 429 — pause 3m"
                        );
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_secs(180)) => {}
                        }
                        break;
                    }
                    warn!(feed_id = %feed_id, line = %line, error = %e, "PRIM SIRI ET fetch failed");
                }
            }
        }

        if any_ok {
            let state = accum.snapshot(&feed_id);
            info!(
                feed_id = %feed_id,
                lines_ok,
                batch,
                trips_this_tick,
                gps_vehicles = gps_this_tick,
                total_trips = state.trip_update_count,
                used_today = quota.used(),
                remaining = quota.remaining(),
                ms = t0.elapsed().as_millis() as u64,
                "PRIM SIRI ET (globale) cycle ok"
            );
            let _ = tx
                .send(FeedEvent::RealtimeDelta {
                    feed_id: feed_id.clone(),
                    kind: RtKind::TripUpdates,
                    state,
                    merge: true,
                })
                .await;
            let veh_state = veh_accum.snapshot(&feed_id);
            if veh_state.vehicle_count > 0 {
                let _ = tx
                    .send(FeedEvent::RealtimeDelta {
                        feed_id: feed_id.clone(),
                        kind: RtKind::VehiclePositions,
                        state: veh_state,
                        merge: true,
                    })
                    .await;
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// GM — Messages affichés sur les écrans
// ---------------------------------------------------------------------------

async fn run_siri_gm_poller(
    client: PrimClient,
    feed_id: String,
    lines: Vec<String>,
    interval: Duration,
    quota: Arc<DailyQuota>,
    outbound: Arc<Semaphore>,
    tx: mpsc::Sender<FeedEvent>,
    cancel: CancellationToken,
) {
    if lines.is_empty() {
        return;
    }
    // Start quickly so info-trafic is not empty for minutes after boot.
    let startup = Duration::from_millis(jitter_ms(1_500));
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(startup) => {}
    }
    info!(
        feed_id = %feed_id,
        lines = lines.len(),
        interval_s = interval.as_secs(),
        "PRIM SIRI GM poller running"
    );

    let mut accum = AlertAccum::new();
    let mut i = 0usize;
    // Several LineRefs per cycle — many lines return empty GM; 1/90s would take hours
    // to cover the network. Budget 10k/day → ~6–8 lines/min is fine.
    const LINES_PER_TICK: usize = 8;
    loop {
        let mut tick_new = 0usize;
        for _ in 0..LINES_PER_TICK {
            if !quota.try_consume() {
                warn!(
                    used = quota.used(),
                    budget = quota.budget,
                    "PRIM SIRI GM daily budget exhausted — sleeping 1h"
                );
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
                }
                break;
            }

            let permit = match outbound.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let line = lines[i % lines.len()].clone();
            i = i.wrapping_add(1);
            let path = format!(
                "/marketplace/general-message?LineRef={}",
                urlencoding_encode(&line)
            );
            match client.get_bytes(&path).await {
                Ok(bytes) => {
                    let fid = feed_id.clone();
                    let parse = tokio::task::spawn_blocking(move || {
                        ingest_realtime(&fid, &bytes, RealtimeFormat::SiriGeneralMessage)
                    })
                    .await;
                    drop(permit);
                    match parse {
                        Ok(Ok(delta)) => {
                            let n = delta.state.alerts.len();
                            tick_new += n;
                            accum.merge(delta.state.alerts);
                            if n > 0 {
                                info!(
                                    feed_id = %feed_id,
                                    line = %line,
                                    new_alerts = n,
                                    total_alerts = accum.by_id.len(),
                                    used_today = quota.used(),
                                    "PRIM SIRI GeneralMessage ok"
                                );
                            }
                        }
                        Ok(Err(e)) => {
                            warn!(feed_id = %feed_id, line = %line, error = %e, "SIRI GM parse skip");
                        }
                        Err(e) => warn!(feed_id = %feed_id, error = %e, "SIRI GM join failed"),
                    }
                }
                Err(e) => {
                    drop(permit);
                    warn!(feed_id = %feed_id, line = %line, error = %e, "PRIM SIRI GM fetch skip");
                }
            }
        }

        // Publish accumulated IDFM alerts after each multi-line tick
        let state = accum.snapshot(&feed_id);
        if state.alert_count > 0 || tick_new > 0 {
            info!(
                feed_id = %feed_id,
                total_alerts = state.alert_count,
                new_this_tick = tick_new,
                used_today = quota.used(),
                "PRIM SIRI GM tick publish"
            );
            let _ = tx
                .send(FeedEvent::RealtimeDelta {
                    feed_id: feed_id.clone(),
                    kind: RtKind::ServiceAlerts,
                    state,
                    merge: true,
                })
                .await;
        }

        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// SM — Prochains passages unitaire (warm continuous)
// ---------------------------------------------------------------------------

async fn run_siri_sm_poller(
    client: PrimClient,
    feed_id: String,
    seeds: Vec<String>,
    interval: Duration,
    max_stops: usize,
    quota: Arc<DailyQuota>,
    outbound: Arc<Semaphore>,
    tx: mpsc::Sender<FeedEvent>,
    cancel: CancellationToken,
) {
    // PRIM enforces a short-term rate limit (429) well below the daily 1M budget.
    // Always poll **one** MonitoringRef per tick, with gap between calls.
    let interval = interval.max(Duration::from_secs(45));
    let _ = max_stops; // reserved; hard-capped to 1 to avoid 429 storms
    let startup = Duration::from_millis(jitter_ms(5_000));
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(startup) => {}
    }

    let mut trip_accum = TripAccum::new(Duration::from_secs(30 * 60));
    let mut veh_accum = VehicleAccum::new();
    let mut rr = 0usize;
    let mut backoff = Duration::from_secs(0);

    loop {
        if backoff > Duration::ZERO {
            warn!(
                secs = backoff.as_secs(),
                "PRIM SIRI SM rate-limit backoff"
            );
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
        }

        if !quota.try_consume() {
            warn!(
                used = quota.used(),
                budget = quota.budget,
                "PRIM SIRI SM daily soft budget exhausted — sleeping 30m"
            );
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1800)) => continue,
            }
        }

        let pool = list_sm_targets(&seeds, 64);
        if pool.is_empty() {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => continue,
            }
        }

        let mon = pool[rr % pool.len()].clone();
        rr = rr.wrapping_add(1);
        let t0 = Instant::now();
        let path = format!(
            "/marketplace/stop-monitoring?MonitoringRef={}",
            urlencoding_encode(&mon)
        );

        let permit = match outbound.acquire().await {
            Ok(p) => p,
            Err(_) => break,
        };

        match client.get_bytes(&path).await {
            Ok(bytes) => {
                backoff = Duration::from_secs(0);
                let fid = feed_id.clone();
                let parse = tokio::task::spawn_blocking(move || {
                    ingest_realtime(&fid, &bytes, RealtimeFormat::SiriStopMonitoring)
                })
                .await;
                drop(permit);
                match parse {
                    Ok(Ok(delta)) => {
                        let n_trips = delta.state.trips.len();
                        let n_veh = delta.state.vehicles.len();
                        trip_accum.merge(delta.state.trips);
                        veh_accum.merge(delta.state.vehicles);
                        let trip_state = trip_accum.snapshot(&feed_id);
                        let veh_state = veh_accum.snapshot(&feed_id);
                        info!(
                            feed_id = %feed_id,
                            monitoring_ref = %mon,
                            new_trips = n_trips,
                            new_vehicles = n_veh,
                            total_trips = trip_state.trip_update_count,
                            used_today = quota.used(),
                            ms = t0.elapsed().as_millis() as u64,
                            "PRIM SIRI SM (unitaire) ok"
                        );
                        if trip_state.trip_update_count > 0 {
                            let _ = tx
                                .send(FeedEvent::RealtimeDelta {
                                    feed_id: feed_id.clone(),
                                    kind: RtKind::TripUpdates,
                                    state: trip_state,
                                    merge: true,
                                })
                                .await;
                        }
                        if veh_state.vehicle_count > 0 {
                            let _ = tx
                                .send(FeedEvent::RealtimeDelta {
                                    feed_id: feed_id.clone(),
                                    kind: RtKind::VehiclePositions,
                                    state: veh_state,
                                    merge: true,
                                })
                                .await;
                        }
                    }
                    Ok(Err(e)) => {
                        debug!(feed_id = %feed_id, mon = %mon, error = %e, "SIRI SM parse skip");
                    }
                    Err(e) => warn!(feed_id = %feed_id, error = %e, "SIRI SM join failed"),
                }
            }
            Err(e) => {
                drop(permit);
                let msg = e.to_string();
                if msg.contains("429") || msg.to_ascii_lowercase().contains("rate limit") {
                    // Exponential backoff: 2m → 4m → 8m … cap 20m
                    let next = if backoff.as_secs() < 120 {
                        120
                    } else {
                        (backoff.as_secs() * 2).min(1200)
                    };
                    backoff = Duration::from_secs(next);
                    warn!(
                        feed_id = %feed_id,
                        mon = %mon,
                        backoff_s = next,
                        "PRIM SIRI SM 429 — backing off"
                    );
                } else {
                    debug!(feed_id = %feed_id, mon = %mon, error = %e, "PRIM SIRI SM fetch skip");
                }
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Catalog stub (equipment path) + SharedPrim
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PrimCatalog {
    pub note: String,
    pub fetched_at: Option<chrono::DateTime<Utc>>,
    pub error: Option<String>,
}

pub type SharedPrim = Arc<ArcSwap<PrimCatalog>>;

pub fn new_shared_prim() -> SharedPrim {
    Arc::new(ArcSwap::from_pointee(PrimCatalog::default()))
}

/// Compatibility stub: equipment/elevators catalog poller (optional).
/// Main realtime path is [`spawn_prim_pollers`] (SIRI Lite continuous).
pub fn spawn_prim_poller(
    _equipment: crate::equipment::SharedEquipment,
    prim: SharedPrim,
    _rt: Arc<ArcSwap<crate::rt::overlay::RealtimeOverlay>>,
    cancel: CancellationToken,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("PRIM catalog stub (SIRI SM/ET/GM via spawn_prim_pollers)");
        loop {
            prim.store(Arc::new(PrimCatalog {
                note: "SIRI Lite continuous: stop-monitoring + estimated-timetable + general-message"
                    .into(),
                fetched_at: Some(Utc::now()),
                error: None,
            }));
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(interval_secs.max(60))) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_blocks_after_budget() {
        let q = DailyQuota::new("test", 3);
        assert!(q.try_consume());
        assert!(q.try_consume());
        assert!(q.try_consume());
        assert!(!q.try_consume());
        assert_eq!(q.used(), 3);
    }

    #[test]
    fn url_encode_line_ref() {
        assert_eq!(
            urlencoding_encode("STIF:Line::C01742:"),
            "STIF%3ALine%3A%3AC01742%3A"
        );
    }

    #[test]
    fn register_interest_and_list() {
        register_sm_interest("STIF:StopPoint:Q:1:");
        let t = list_sm_targets(&["STIF:StopPoint:Q:2:".into()], 10);
        assert!(t.iter().any(|s| s.contains("Q:1")));
    }

    #[test]
    fn default_line_and_seed_coverage() {
        assert!(
            DEFAULT_LINE_REFS.len() >= 40,
            "expect expanded metro/RER/tram/bus sample, got {}",
            DEFAULT_LINE_REFS.len()
        );
        // Seeds stay tiny on purpose (PRIM short-term rate limit / 429).
        assert!(
            DEFAULT_SEED_STOPS.len() >= 1 && DEFAULT_SEED_STOPS.len() <= 8,
            "expect small hub seed set, got {}",
            DEFAULT_SEED_STOPS.len()
        );
        assert!(DEFAULT_LINE_REFS.iter().any(|s| s.contains("C01742"))); // RER A
        assert!(DEFAULT_LINE_REFS.iter().any(|s| s.contains("C01072"))); // bus 20
        assert!(DEFAULT_SEED_STOPS.iter().any(|s| s.contains("StopArea")));
    }

    #[test]
    fn et_batch_respects_cap() {
        let q = DailyQuota::new("et-batch", 900);
        // Always hard-capped to 1 LineRef per tick (rate-limit safety).
        let n = et_batch_size(&q, Duration::from_secs(90), 3);
        assert_eq!(n, 1, "batch={n}");
        // Exhaust
        for _ in 0..900 {
            assert!(q.try_consume());
        }
        assert_eq!(et_batch_size(&q, Duration::from_secs(90), 3), 0);
    }
}
