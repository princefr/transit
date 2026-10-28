//! Live next departures via **SIRI Stop Monitoring** (not Navitia).
//!
//! PRIM: `GET /marketplace/stop-monitoring?MonitoringRef=STIF:StopPoint:Q:…:`

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use super::client::PrimClient;
use crate::error::{Result, TransitError};
use crate::gtfs::pack::{RouteMode, StaticEpoch, StopRecord};
use crate::state::AppState;
use crate::rt::adapter::ingest_siri_stop_monitoring;
use crate::rt::overlay::FeedRtState;

/// One live departure row for GraphQL / UI.
#[derive(Debug, Clone)]
pub struct LiveDeparture {
    pub trip_id: String,
    pub route_short_name: Option<String>,
    pub route_long_name: Option<String>,
    pub trip_short_name: Option<String>,
    pub headsign: Option<String>,
    pub mode: RouteMode,
    pub scheduled_departure: DateTime<Utc>,
    pub realtime_departure: Option<DateTime<Utc>>,
    pub delay_seconds: Option<i32>,
    pub canceled: bool,
    pub platform: Option<String>,
    pub route_color: Option<String>,
    pub source: &'static str,
    pub line_ref: Option<String>,
    pub status: Option<String>,
}

/// Map a GTFS / app stop id to a PRIM SIRI `MonitoringRef`.
///
/// Examples that work:
/// - `STIF:StopPoint:Q:463041:` (already SIRI)
/// - `idfm:STIF:StopPoint:Q:463041:` → strip feed prefix
/// - raw digits `463041` → `STIF:StopPoint:Q:463041:`
///
/// **Does not** invent `StopPoint:Q:{n}` from monomodalStopPlace ids — that burned
/// PRIM quota with 429s (`monomodalStopPlace:43071` ≠ quay 43071).
pub fn monitoring_ref_from_stop_id(stop_id: &str) -> Option<String> {
    let raw = stop_id
        .strip_prefix("idfm:")
        .or_else(|| stop_id.strip_prefix("IDFM:"))
        .unwrap_or(stop_id)
        .trim();
    if raw.is_empty() {
        return None;
    }
    // Never invent StopPoint refs from place shells
    let lower = raw.to_ascii_lowercase();
    if lower.contains("monomodalstopplace")
        || lower.contains("multimodalstopplace")
        || lower.contains("stopplaceentrance")
    {
        return None;
    }
    if raw.contains("StopPoint") || raw.contains("StopArea") || raw.starts_with("STIF:") {
        let mut s = raw.to_string();
        if !s.ends_with(':') {
            s.push(':');
        }
        return Some(s);
    }
    // Bare numeric quay id only (not place ids)
    if raw.chars().all(|c| c.is_ascii_digit()) && raw.len() >= 4 {
        return Some(format!("STIF:StopPoint:Q:{raw}:"));
    }
    // IDFM:12345 style quay
    if let Some(rest) = raw.strip_prefix("IDFM:").or_else(|| raw.strip_prefix("idfm:")) {
        if rest.chars().all(|c| c.is_ascii_digit()) && rest.len() >= 4 {
            return Some(format!("STIF:StopPoint:Q:{rest}:"));
        }
    }
    None
}

fn mode_from_vehicle_mode(s: &str) -> RouteMode {
    match s.to_ascii_uppercase().as_str() {
        "METRO" | "SUBWAY" => RouteMode::Metro,
        "TRAM" | "TRAMWAY" => RouteMode::Tram,
        "BUS" | "COACH" => RouteMode::Bus,
        "RAIL" | "TRAIN" | "RER" => RouteMode::Rail,
        "FERRY" => RouteMode::Ferry,
        _ => RouteMode::Other,
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })
}

fn ref_val(v: &Value) -> Option<String> {
    v.as_str()
        .map(|s| s.to_string())
        .or_else(|| v.get("value").and_then(|x| x.as_str()).map(|s| s.to_string()))
}

fn text_first(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = v.as_array() {
        for it in arr {
            if let Some(s) = it.as_str().or_else(|| it.get("value").and_then(|x| x.as_str())) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Live board + optional RT snapshot (trips + **VehicleLocation GPS**).
pub struct LiveBoardResult {
    pub departures: Vec<LiveDeparture>,
    /// Parsed SM state for overlay merge (includes real GPS when PRIM sent it).
    pub rt_delta: Option<FeedRtState>,
}

/// Fetch live board from PRIM SIRI Stop Monitoring.
pub async fn fetch_live_departures(
    client: &PrimClient,
    monitoring_ref: &str,
    limit: usize,
) -> Result<Vec<LiveDeparture>> {
    Ok(fetch_live_board(client, monitoring_ref, limit)
        .await?
        .departures)
}

// ---------------------------------------------------------------------------
// On-demand guards (GraphQL `departures(live: true)`)
//
// Every user request used to hit PRIM directly: it spent the shared SM daily
// quota and queued behind the global 700ms pacer without bound. Now:
//   1. the MonitoringRef must belong to a stop of the current static epoch
//      (when an IDFM epoch is loaded),
//   2. boards are cached in memory for `LIVE_CACHE_TTL` (also negative results),
//   3. concurrent requests for the same ref share one PRIM call,
//   4. a small global token bucket caps on-demand PRIM traffic,
//   5. the pacer wait is bounded (`ON_DEMAND_MAX_PACER_WAIT`).
// Whenever a guard declines, the function returns `Err` and the GraphQL
// resolver falls back to scheduled GTFS + RT data.
// ---------------------------------------------------------------------------

/// How long a fetched live board is reused for the same MonitoringRef.
pub const LIVE_CACHE_TTL: Duration = Duration::from_secs(30);
/// Max cached MonitoringRefs (oldest entries dropped beyond this).
const LIVE_CACHE_MAX: usize = 4096;
/// Global on-demand PRIM refill rate (tokens per second): ~1 call / 2 s.
const ON_DEMAND_RATE_PER_SEC: f64 = 0.5;
/// Burst capacity of the on-demand bucket.
const ON_DEMAND_BURST: f64 = 4.0;
/// Max time an on-demand request may wait for the shared PRIM pacer.
pub const ON_DEMAND_MAX_PACER_WAIT: Duration = Duration::from_secs(5);
/// Rows parsed and cached per board (requests truncate to their own `limit`).
const FETCH_ROWS: usize = 100;

struct CachedBoard {
    at: Instant,
    departures: Arc<Vec<LiveDeparture>>,
}

static LIVE_CACHE: LazyLock<Mutex<HashMap<String, CachedBoard>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ON_DEMAND_BUCKET: Mutex<Option<TokenBucket>> = Mutex::new(None);
static EPOCH_SOURCE: RwLock<Option<Arc<ArcSwap<StaticEpoch>>>> = RwLock::new(None);
/// (epoch id, MonitoringRefs derivable from that epoch's stops).
static KNOWN_REFS: Mutex<Option<(String, Arc<HashSet<String>>)>> = Mutex::new(None);

#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn full(now: Instant) -> Self {
        Self {
            tokens: ON_DEMAND_BURST,
            last: now,
        }
    }

    fn try_take(&mut self, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * ON_DEMAND_RATE_PER_SEC).min(ON_DEMAND_BURST);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn on_demand_try_take() -> bool {
    let now = Instant::now();
    let mut g = ON_DEMAND_BUCKET.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(|| TokenBucket::full(now)).try_take(now)
}

/// Register the live static-epoch handle so on-demand lookups can be validated.
/// Called by `AppState::new`; the most recent registration wins.
pub fn install_epoch_source(epoch: Arc<ArcSwap<StaticEpoch>>) {
    let mut g = EPOCH_SOURCE.write().unwrap_or_else(|e| e.into_inner());
    *g = Some(epoch);
}

/// Shared, lazily-created PRIM client stored in [`AppState`]. `None` when the
/// API key env var is missing/empty. Prefer this over `PrimClient::from_env`
/// per request.
pub fn shared_client(state: &AppState) -> Option<Arc<PrimClient>> {
    state
        .prim_client
        .get_or_init(|| {
            let timeout =
                Duration::from_secs(state.config.runtime.rt_request_timeout_secs.max(10));
            PrimClient::from_env(
                &state.config.prim.base_url,
                &state.config.prim.api_key_env,
                &state.config.prim.user_agent,
                timeout,
            )
            .ok()
            .map(Arc::new)
        })
        .clone()
}

/// MonitoringRefs reachable from the epoch's stops (quays + `StopArea` for
/// monomodal stop places).
fn monitoring_refs_for_epoch(epoch: &StaticEpoch) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in &epoch.stops {
        for id in [s.raw_id.as_str(), s.id.as_str()] {
            if let Some(m) = monitoring_ref_from_stop_id(id) {
                out.insert(m);
            }
            if let Some(pos) = id.find("monomodalStopPlace:") {
                let n = id[pos + "monomodalStopPlace:".len()..].trim_end_matches(':');
                if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                    out.insert(format!("STIF:StopArea:SP:{n}:"));
                }
            }
        }
    }
    out
}

/// True when `ref_q` belongs to the current epoch, or when validation is not
/// possible (no epoch registered / loaded, or the epoch has no IDFM stops).
fn monitoring_ref_known(ref_q: &str) -> bool {
    let src = EPOCH_SOURCE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(src) = src else {
        return true;
    };
    let epoch = src.load_full();
    if epoch.stops.is_empty() {
        return true;
    }
    let set = {
        let mut g = KNOWN_REFS.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some((id, set)) if *id == epoch.id => set.clone(),
            _ => {
                let set = Arc::new(monitoring_refs_for_epoch(&epoch));
                *g = Some((epoch.id.clone(), set.clone()));
                set
            }
        }
    };
    set.is_empty() || set.contains(ref_q)
}

fn cache_get(ref_q: &str, now: Instant) -> Option<Arc<Vec<LiveDeparture>>> {
    let g = LIVE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    g.get(ref_q)
        .filter(|c| now.saturating_duration_since(c.at) < LIVE_CACHE_TTL)
        .map(|c| c.departures.clone())
}

fn cache_put(ref_q: &str, departures: Arc<Vec<LiveDeparture>>, now: Instant) {
    let mut g = LIVE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if g.len() >= LIVE_CACHE_MAX {
        g.retain(|_, c| now.saturating_duration_since(c.at) < LIVE_CACHE_TTL);
        if g.len() >= LIVE_CACHE_MAX {
            g.clear();
        }
    }
    g.insert(
        ref_q.to_string(),
        CachedBoard {
            at: now,
            departures,
        },
    );
}

fn in_flight_lock(ref_q: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut g = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if g.len() >= LIVE_CACHE_MAX {
        // Entries orphaned by cancelled requests: nobody holds them any more.
        g.retain(|_, l| Arc::strong_count(l) > 1);
    }
    g.entry(ref_q.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn in_flight_release(ref_q: &str, lock: Arc<tokio::sync::Mutex<()>>) {
    let mut g = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    // Map holds one reference, `lock` the other → nobody else is waiting.
    if Arc::strong_count(&lock) <= 2 {
        g.remove(ref_q);
    }
}

fn board_from_cached(deps: &[LiveDeparture], limit: usize) -> LiveBoardResult {
    LiveBoardResult {
        departures: deps.iter().take(limit).cloned().collect(),
        // Overlay was already merged by the request that fetched it.
        rt_delta: None,
    }
}

/// Like [`fetch_live_departures`] but also returns RT delta for map GPS merge.
///
/// Guarded (epoch validation, 30s cache, in-flight dedupe, on-demand rate
/// limit, bounded pacer wait). `rt_delta` is only `Some` for the request that
/// actually called PRIM; cached answers return `None`. Any `Err` means "no live
/// data now" and callers should serve scheduled data.
pub async fn fetch_live_board(
    client: &PrimClient,
    monitoring_ref: &str,
    limit: usize,
) -> Result<LiveBoardResult> {
    let mut ref_q = monitoring_ref.trim().to_string();
    if !ref_q.ends_with(':') {
        ref_q.push(':');
    }
    if ref_q.len() > 128 || !monitoring_ref_known(&ref_q) {
        return Err(TransitError::StopNotFound(format!(
            "MonitoringRef not in current static epoch: {}",
            ref_q.chars().take(64).collect::<String>()
        )));
    }
    // Keep this stop in the continuous SM warm set
    super::register_sm_interest(&ref_q);

    if let Some(deps) = cache_get(&ref_q, Instant::now()) {
        return Ok(board_from_cached(&deps, limit));
    }

    let lock = in_flight_lock(&ref_q);
    let result = {
        let _guard = lock.lock().await;
        // Another request may have fetched it while we waited.
        if let Some(deps) = cache_get(&ref_q, Instant::now()) {
            Ok(board_from_cached(&deps, limit))
        } else if !on_demand_try_take() {
            Err(TransitError::Http(
                "PRIM on-demand rate limit reached; serving scheduled data".into(),
            ))
        } else if !super::sm_try_consume_quota() {
            Err(TransitError::Http(
                "PRIM stop-monitoring daily soft budget exhausted".into(),
            ))
        } else {
            let fetched = fetch_live_board_uncached(client, &ref_q, FETCH_ROWS).await;
            let now = Instant::now();
            match fetched {
                Ok(mut board) => {
                    cache_put(&ref_q, Arc::new(board.departures.clone()), now);
                    board.departures.truncate(limit);
                    Ok(board)
                }
                Err(e) => {
                    // Negative cache: don't retry a failing ref for every request.
                    cache_put(&ref_q, Arc::new(Vec::new()), now);
                    Err(e)
                }
            }
        }
    };
    in_flight_release(&ref_q, lock);
    result
}

async fn fetch_live_board_uncached(
    client: &PrimClient,
    ref_q: &str,
    limit: usize,
) -> Result<LiveBoardResult> {
    let path = format!(
        "/marketplace/stop-monitoring?MonitoringRef={}",
        crate_url_encode(ref_q)
    );
    let bytes = client
        .get_bytes_bounded(&path, ON_DEMAND_MAX_PACER_WAIT)
        .await?;

    // Parse once for overlay (trips + optional VehicleLocation → real GPS)
    let rt_delta = ingest_siri_stop_monitoring("idfm", &bytes).ok();

    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|e| TransitError::Parse(format!("stop-monitoring json: {e}")))?;
    let sd = root
        .get("Siri")
        .and_then(|s| s.get("ServiceDelivery"))
        .unwrap_or(&root);
    let deliveries = sd
        .get("StopMonitoringDelivery")
        .and_then(|d| d.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let mut out = Vec::new();
    for del in deliveries {
        if del.get("ErrorCondition").is_some() {
            let err = del
                .pointer("/ErrorCondition/ErrorInformation/ErrorText")
                .and_then(|x| x.as_str())
                .unwrap_or("SIRI error");
            return Err(TransitError::Http(format!("stop-monitoring: {err}")));
        }
        let visits = del
            .get("MonitoredStopVisit")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        for visit in visits {
            let mvj = match visit.get("MonitoredVehicleJourney") {
                Some(m) => m,
                None => continue,
            };
            let trip_raw = mvj
                .pointer("/FramedVehicleJourneyRef/DatedVehicleJourneyRef")
                .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| ref_val(v)))
                .or_else(|| mvj.get("DatedVehicleJourneyRef").and_then(ref_val))
                .unwrap_or_else(|| format!("siri-{}", out.len()));
            let line = mvj.get("LineRef").and_then(ref_val);
            let headsign = mvj
                .get("DestinationName")
                .and_then(text_first)
                .or_else(|| mvj.get("DestinationShortName").and_then(text_first))
                .or_else(|| mvj.get("DirectionName").and_then(text_first));
            let line_name = mvj.get("PublishedLineName").and_then(text_first);
            let mode = mvj
                .get("VehicleMode")
                .and_then(|m| {
                    if let Some(a) = m.as_array() {
                        a.first().and_then(|x| x.as_str())
                    } else {
                        m.as_str()
                    }
                })
                .map(mode_from_vehicle_mode)
                .unwrap_or(RouteMode::Other);

            let call = mvj.get("MonitoredCall");
            let exp = call
                .and_then(|c| c.get("ExpectedDepartureTime").or_else(|| c.get("ExpectedArrivalTime")))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);
            let aimed = call
                .and_then(|c| c.get("AimedDepartureTime").or_else(|| c.get("AimedArrivalTime")))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);
            let status = call
                .and_then(|c| c.get("DepartureStatus").or_else(|| c.get("ArrivalStatus")))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let canceled = status
                .as_deref()
                .map(|s| s.to_ascii_lowercase().contains("cancel"))
                .unwrap_or(false);
            let delay = match (aimed, exp) {
                (Some(a), Some(e)) => Some((e - a).num_seconds() as i32),
                _ => None,
            };
            let when = exp.or(aimed).unwrap_or_else(Utc::now);
            let platform = call
                .and_then(|c| c.get("DeparturePlatformName").or_else(|| c.get("ArrivalPlatformName")))
                .and_then(text_first);

            out.push(LiveDeparture {
                trip_id: format!("idfm:{trip_raw}"),
                route_short_name: line_name.clone(),
                route_long_name: line_name,
                trip_short_name: None,
                headsign,
                mode,
                scheduled_departure: aimed.unwrap_or(when),
                realtime_departure: exp.or(Some(when)),
                delay_seconds: delay,
                canceled,
                platform,
                route_color: None,
                source: "siri-stop-monitoring",
                line_ref: line,
                status,
            });
            if out.len() >= limit {
                break;
            }
        }
        if out.len() >= limit {
            break;
        }
    }

    out.sort_by_key(|d| d.realtime_departure.unwrap_or(d.scheduled_departure));
    out.truncate(limit);
    Ok(LiveBoardResult {
        departures: out,
        rt_delta,
    })
}

/// Optional stop record for GraphQL stop field (empty shell if unknown).
pub fn shell_stop(stop_id: &str, name: Option<String>) -> StopRecord {
    StopRecord {
        id: stop_id.to_string(),
        feed_id: "idfm".into(),
        raw_id: stop_id.to_string(),
        name: name.unwrap_or_else(|| stop_id.to_string()),
        lat: None,
        lon: None,
        parent_id: None,
        location_type: 0,
        platform_code: None,
        wheelchair: 0,
        stop_code: None,
        stop_desc: None,
        zone_id: None,
        stop_url: None,
        stop_timezone: None,
        level_id: None,
    }
}

fn crate_url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitoring_ref_from_digits() {
        assert_eq!(
            monitoring_ref_from_stop_id("463041").as_deref(),
            Some("STIF:StopPoint:Q:463041:")
        );
        assert_eq!(
            monitoring_ref_from_stop_id("idfm:STIF:StopPoint:Q:463041:").as_deref(),
            Some("STIF:StopPoint:Q:463041:")
        );
    }

    #[test]
    fn token_bucket_limits_burst_then_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::full(t0);
        let burst = ON_DEMAND_BURST as usize;
        for _ in 0..burst {
            assert!(b.try_take(t0));
        }
        assert!(!b.try_take(t0), "bucket must be empty after burst");
        // 2s at 0.5 tokens/s → one more token.
        let t1 = t0 + Duration::from_secs(2);
        assert!(b.try_take(t1));
        assert!(!b.try_take(t1));
    }

    #[test]
    fn live_cache_hit_within_ttl_only() {
        let key = "STIF:StopPoint:Q:test-cache-ttl:";
        let t0 = Instant::now();
        cache_put(key, Arc::new(Vec::new()), t0);
        assert!(cache_get(key, t0 + Duration::from_secs(1)).is_some());
        assert!(cache_get(key, t0 + LIVE_CACHE_TTL + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn epoch_refs_include_quays_and_stop_areas() {
        let mut epoch = StaticEpoch::empty();
        let mut quay = shell_stop("idfm:IDFM:463041", None);
        quay.raw_id = "IDFM:463041".into();
        let mut place = shell_stop("idfm:IDFM:monomodalStopPlace:43071", None);
        place.raw_id = "IDFM:monomodalStopPlace:43071".into();
        epoch.stops = vec![quay, place];
        let refs = monitoring_refs_for_epoch(&epoch);
        assert!(refs.contains("STIF:StopPoint:Q:463041:"));
        assert!(refs.contains("STIF:StopArea:SP:43071:"));
        assert!(!refs.contains("STIF:StopPoint:Q:999999:"));
    }
}
