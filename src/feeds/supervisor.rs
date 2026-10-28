use crate::cache::TransitCache;
use crate::config::{Config, FeedConfig};
use crate::feeds::hub_link::link_epoch_stops_safe;
use crate::feeds::rt_poller::{run_rt_poller, RtKind};
use crate::feeds::static_watcher::run_static_watcher;
use crate::gtfs::pack::{build_epoch_with_flash, FeedStaticBundle, StaticEpoch};
use crate::prim::{spawn_prim_pollers, PrimClient};
use crate::rt::overlay::{AlertRt, FeedRtState, RealtimeOverlay};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[derive(Debug)]
pub enum FeedEvent {
    StaticReady {
        feed_id: String,
        bundle: Arc<FeedStaticBundle>,
        sha256: Option<String>,
    },
    StaticFailed {
        feed_id: String,
        error: String,
    },
    RealtimeDelta {
        feed_id: String,
        kind: RtKind,
        state: FeedRtState,
        /// When false (default for GTFS-RT), replace the kind's map/list.
        /// When true (PRIM SIRI partial samples), upsert keys / alert ids and
        /// evict stale entries by age.
        merge: bool,
    },
}

/// Per-feed runtime status (queryable without AppState).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FeedRuntimeStatus {
    pub feed_id: String,
    pub last_static_error: Option<String>,
    pub last_static_error_at: Option<DateTime<Utc>>,
    pub last_static_ok_at: Option<DateTime<Utc>>,
    pub last_static_sha256: Option<String>,
    pub last_static_stops: Option<usize>,
    pub last_static_trips: Option<usize>,
    pub last_rt_ok_at: Option<DateTime<Utc>>,
    pub last_rt_error: Option<String>,
    pub last_rt_error_at: Option<DateTime<Utc>>,
}

static RUNTIME_STATUS: OnceLock<Arc<ArcSwap<HashMap<String, FeedRuntimeStatus>>>> = OnceLock::new();

fn status_store() -> &'static Arc<ArcSwap<HashMap<String, FeedRuntimeStatus>>> {
    RUNTIME_STATUS.get_or_init(|| Arc::new(ArcSwap::from_pointee(HashMap::new())))
}

/// Snapshot of all feed runtime statuses (for health/API later).
pub fn runtime_status() -> Arc<HashMap<String, FeedRuntimeStatus>> {
    status_store().load_full()
}

fn update_status(feed_id: &str, f: impl FnOnce(&mut FeedRuntimeStatus)) {
    let store = status_store();
    let cur = store.load();
    let mut map = (**cur).clone();
    let entry = map
        .entry(feed_id.to_string())
        .or_insert_with(|| FeedRuntimeStatus {
            feed_id: feed_id.to_string(),
            ..Default::default()
        });
    f(entry);
    store.store(Arc::new(map));
}

#[derive(Clone, Debug)]
pub struct RtVersion(pub u64);

pub struct SupervisorHandles {
    pub cancel: CancellationToken,
}

pub fn spawn_feed_supervisor(
    config: Arc<Config>,
    epoch: Arc<ArcSwap<StaticEpoch>>,
    rt_overlay: Arc<ArcSwap<RealtimeOverlay>>,
    rt_version_tx: broadcast::Sender<RtVersion>,
    cache: TransitCache,
) -> SupervisorHandles {
    let cancel = CancellationToken::new();
    let child = cancel.child_token();
    let (tx, mut rx) = mpsc::channel::<FeedEvent>(64);

    let client = reqwest::Client::builder()
        .gzip(true)
        .pool_max_idle_per_host(4)
        .build()
        .expect("reqwest client");

    let outbound = Arc::new(Semaphore::new(config.runtime.max_outbound_rt_requests));
    let static_builds = Arc::new(Semaphore::new(
        config.runtime.max_concurrent_static_builds.max(1),
    ));
    let data_dir = config.runtime.data_dir.clone();
    let download_timeout = Duration::from_secs(config.runtime.static_download_timeout_secs);
    let rt_timeout = Duration::from_secs(config.runtime.rt_request_timeout_secs);

    // Seed status entries for enabled feeds
    for feed in config.enabled_feeds() {
        update_status(&feed.id, |_| {});
    }

    // Spawn per-feed workers
    for feed in config.enabled_feeds().cloned() {
        spawn_feed_workers(
            feed,
            data_dir.clone(),
            client.clone(),
            download_timeout,
            rt_timeout,
            outbound.clone(),
            static_builds.clone(),
            tx.clone(),
            child.clone(),
        );
    }

    // PRIM SIRI Lite continuous (SM + ET + GM) for feed id `idfm` when
    // IDFM_PRIM_API_KEY is present — independent of static GTFS load / GTFS-RT 403.
    if config.prim.should_run() {
        match PrimClient::from_env(
            &config.prim.base_url,
            &config.prim.api_key_env,
            &config.prim.user_agent,
            rt_timeout,
        ) {
            Ok(prim_client) => {
                update_status(&config.prim.feed_id, |_| {});
                spawn_prim_pollers(
                    &config.prim,
                    prim_client,
                    outbound.clone(),
                    tx.clone(),
                    child.clone(),
                );
            }
            Err(e) => {
                warn!(error = %e, "PRIM client not started");
            }
        }
    } else if config.prim.enabled {
        info!(
            env = %config.prim.api_key_env,
            "PRIM enabled in config but API key env empty — pollers not started"
        );
    }

    let routing = config.routing.clone();
    let flash_dir = data_dir.join("flash");
    let coord_cancel = child.clone();
    set_event_sender(tx.clone());
    tokio::spawn(async move {
        let mut bundles: HashMap<String, Arc<FeedStaticBundle>> = HashMap::new();
        // Per-feed RT state behind `Arc` so publishing can hand the snapshot to the
        // blocking pool without a deep clone on the async worker; mutated with
        // `Arc::make_mut` (unique again once the publish task has finished).
        let mut feed_rt: HashMap<String, Arc<FeedRtState>> = HashMap::new();
        let mut merge_seen: HashMap<String, MergeSeen> = HashMap::new();
        let mut version: u64 = 0;
        // In-flight static rebuild (old epoch id, blocking task) + coalesced follow-up.
        let mut rebuild: Option<(String, tokio::task::JoinHandle<()>)> = None;
        let mut rebuild_pending = false;

        loop {
            tokio::select! {
                _ = coord_cancel.cancelled() => break,
                res = async { (&mut rebuild.as_mut().expect("guarded").1).await }, if rebuild.is_some() => {
                    let (old_epoch_id, _) = rebuild.take().expect("guarded");
                    if let Err(e) = res {
                        error!(error = %e, "static epoch rebuild task failed");
                    }
                    let new_epoch_id = epoch.load().id.clone();
                    if old_epoch_id != new_epoch_id {
                        cache.invalidate_epoch(&old_epoch_id).await;
                    }
                    if rebuild_pending {
                        rebuild_pending = false;
                        rebuild = Some(start_rebuild(&bundles, &routing, &flash_dir, &epoch));
                    }
                }
                ev = rx.recv() => {
                    let Some(ev) = ev else { break };
                    // Coalesce a burst of queued events into a single RT publish.
                    let mut batch = vec![ev];
                    while batch.len() < 64 {
                        match rx.try_recv() {
                            Ok(e) => batch.push(e),
                            Err(_) => break,
                        }
                    }
                    let mut rt_dirty = false;
                    for ev in batch {
                        match ev {
                            FeedEvent::StaticReady { feed_id, bundle, sha256 } => {
                                info!(
                                    %feed_id,
                                    stops = bundle.stop_count(),
                                    trips = bundle.trip_count(),
                                    sha256 = sha256.as_deref().unwrap_or("-"),
                                    "static ready"
                                );
                                update_status(&feed_id, |s| {
                                    s.last_static_error = None;
                                    s.last_static_error_at = None;
                                    s.last_static_ok_at = Some(Utc::now());
                                    s.last_static_sha256 = sha256.clone();
                                    s.last_static_stops = Some(bundle.stop_count());
                                    s.last_static_trips = Some(bundle.trip_count());
                                });
                                bundles.insert(feed_id, bundle);
                                if rebuild.is_some() {
                                    rebuild_pending = true;
                                } else {
                                    rebuild = Some(start_rebuild(&bundles, &routing, &flash_dir, &epoch));
                                }
                            }
                            FeedEvent::StaticFailed { feed_id, error } => {
                                warn!(%feed_id, %error, "static failed");
                                update_status(&feed_id, |s| {
                                    s.last_static_error = Some(error);
                                    s.last_static_error_at = Some(Utc::now());
                                });
                            }
                            FeedEvent::RealtimeDelta { feed_id, kind, state, merge } => {
                                update_status(&feed_id, |s| {
                                    s.last_rt_ok_at = Some(Utc::now());
                                    s.last_rt_error = None;
                                });
                                let entry = Arc::make_mut(
                                    feed_rt
                                        .entry(feed_id.clone())
                                        .or_insert_with(|| Arc::new(FeedRtState::new(&feed_id))),
                                );
                                let seen = merge_seen.entry(feed_id).or_default();
                                apply_rt_delta(entry, seen, kind, state, merge, Utc::now());
                                rt_dirty = true;
                            }
                        }
                    }
                    if rt_dirty {
                        version = version.wrapping_add(1);
                        publish_rt(&feed_rt, version, &rt_overlay, &rt_version_tx).await;
                    }
                }
            }
        }
        info!("feed coordinator stopped");
    });

    SupervisorHandles { cancel }
}

// ---------------------------------------------------------------------------
// External RT submissions (on-demand GraphQL boards)
// ---------------------------------------------------------------------------

static EVENT_TX: Mutex<Option<mpsc::Sender<FeedEvent>>> = Mutex::new(None);

fn set_event_sender(tx: mpsc::Sender<FeedEvent>) {
    if let Ok(mut g) = EVENT_TX.lock() {
        *g = Some(tx);
    }
}

/// Hand a partial realtime snapshot (e.g. an on-demand SIRI Stop Monitoring board)
/// to the feed supervisor, which merges it into its authoritative per-feed state
/// (with age-based eviction) and republishes the overlay with a version bump.
///
/// Non-blocking: returns false when no supervisor is running or its queue is full
/// (the delta is dropped; warm pollers will catch up).
pub fn submit_realtime_delta(feed_id: &str, delta: FeedRtState) -> bool {
    let Some(tx) = EVENT_TX.lock().ok().and_then(|g| g.clone()) else {
        return false;
    };
    let mut ok = true;
    let FeedRtState {
        trips,
        vehicles,
        trip_updates_fetched_at,
        vehicles_fetched_at,
        ..
    } = delta;
    if !trips.is_empty() {
        let mut s = FeedRtState::new(feed_id);
        s.trip_update_count = trips.len();
        s.trips = trips;
        s.trip_updates_fetched_at = trip_updates_fetched_at.or_else(|| Some(Utc::now()));
        ok &= tx
            .try_send(FeedEvent::RealtimeDelta {
                feed_id: feed_id.to_string(),
                kind: RtKind::TripUpdates,
                state: s,
                merge: true,
            })
            .is_ok();
    }
    if !vehicles.is_empty() {
        let mut s = FeedRtState::new(feed_id);
        s.vehicle_count = vehicles.len();
        s.vehicles = vehicles;
        s.vehicles_fetched_at = vehicles_fetched_at.or_else(|| Some(Utc::now()));
        ok &= tx
            .try_send(FeedEvent::RealtimeDelta {
                feed_id: feed_id.to_string(),
                kind: RtKind::VehiclePositions,
                state: s,
                merge: true,
            })
            .is_ok();
    }
    ok
}

// ---------------------------------------------------------------------------
// RT merge + age-based eviction
// ---------------------------------------------------------------------------

/// Merged trips whose last stop time is older than this are dropped.
const TRIP_PAST_GRACE_SECS: i64 = 2 * 3600;
/// Merged trips / alerts not refreshed by any partial snapshot for this long are dropped.
const TRIP_UNSEEN_MAX_SECS: i64 = 2 * 3600;
const ALERT_UNSEEN_MAX_SECS: i64 = 6 * 3600;
/// Vehicle positions older than this are dropped (matches overlay `merge_feed_delta`).
const VEHICLE_MAX_AGE_SECS: i64 = 15 * 60;
/// Hard caps per merged feed; evict least-recently-seen / oldest first.
const MERGED_TRIPS_CAP: usize = 100_000;
const MERGED_ALERTS_CAP: usize = 10_000;
const MERGED_VEHICLES_CAP: usize = 50_000;

/// Last time each merged trip / alert key was present in an incoming partial snapshot.
#[derive(Debug, Default)]
struct MergeSeen {
    trips: HashMap<String, DateTime<Utc>>,
    alerts: HashMap<String, DateTime<Utc>>,
}

/// Apply one RT delta to a feed's state. `merge == false` replaces the kind
/// (full GTFS-RT snapshot); `merge == true` upserts (PRIM SIRI partial samples)
/// and then evicts stale entries so the merged state stays bounded.
fn apply_rt_delta(
    entry: &mut FeedRtState,
    seen: &mut MergeSeen,
    kind: RtKind,
    state: FeedRtState,
    merge: bool,
    now: DateTime<Utc>,
) {
    match kind {
        RtKind::TripUpdates => {
            if merge {
                for (k, v) in state.trips {
                    seen.trips.insert(k.clone(), now);
                    entry.trips.insert(k, v);
                }
                evict_stale_trips(entry, seen, now);
                entry.trip_update_count = entry.trips.len();
                entry.trip_updates_fetched_at = state
                    .trip_updates_fetched_at
                    .or(entry.trip_updates_fetched_at);
                entry.header_timestamp = state.header_timestamp.or(entry.header_timestamp);
            } else {
                seen.trips.clear();
                entry.trips = state.trips;
                entry.trip_updates_fetched_at = state.trip_updates_fetched_at;
                entry.trip_update_count = state.trip_update_count;
                entry.header_timestamp = state.header_timestamp;
            }
        }
        RtKind::VehiclePositions => {
            if merge {
                for (k, v) in state.vehicles {
                    entry.vehicles.insert(k, v);
                }
                evict_stale_vehicles(entry, now);
                entry.vehicle_count = entry.vehicles.len();
                entry.vehicles_fetched_at = state.vehicles_fetched_at.or(entry.vehicles_fetched_at);
            } else {
                entry.vehicles = state.vehicles;
                entry.vehicles_fetched_at = state.vehicles_fetched_at;
                entry.vehicle_count = state.vehicle_count;
            }
        }
        RtKind::ServiceAlerts => {
            if merge {
                // Upsert by alert key
                let mut by_id: HashMap<String, AlertRt> =
                    entry.alerts.drain(..).map(|a| (a.merge_key(), a)).collect();
                for a in state.alerts {
                    let k = a.merge_key();
                    seen.alerts.insert(k.clone(), now);
                    by_id.insert(k, a);
                }
                evict_stale_alerts(&mut by_id, seen, now);
                entry.alerts = by_id.into_values().collect();
                entry.alert_count = entry.alerts.len();
                entry.alerts_fetched_at = state.alerts_fetched_at.or(entry.alerts_fetched_at);
            } else {
                seen.alerts.clear();
                entry.alerts = state.alerts;
                entry.alerts_fetched_at = state.alerts_fetched_at;
                entry.alert_count = state.alert_count;
            }
        }
    }
}

fn evict_stale_trips(entry: &mut FeedRtState, seen: &mut MergeSeen, now: DateTime<Utc>) {
    let now_ts = now.timestamp();
    let unseen_cutoff = now - chrono::Duration::seconds(TRIP_UNSEEN_MAX_SECS);
    entry.trips.retain(|k, t| {
        // Keys inherited from an earlier full snapshot have no record: start now.
        let last_seen = *seen.trips.entry(k.clone()).or_insert(now);
        let ended = t
            .last_stop_time()
            .is_some_and(|ts| ts < now_ts - TRIP_PAST_GRACE_SECS);
        !ended && last_seen >= unseen_cutoff
    });
    seen.trips.retain(|k, _| entry.trips.contains_key(k));
    if entry.trips.len() > MERGED_TRIPS_CAP {
        let drop_n = entry.trips.len() - MERGED_TRIPS_CAP;
        for k in oldest_keys(&seen.trips, drop_n) {
            entry.trips.remove(&k);
            seen.trips.remove(&k);
        }
    }
}

fn evict_stale_vehicles(entry: &mut FeedRtState, now: DateTime<Utc>) {
    let cutoff = now - chrono::Duration::seconds(VEHICLE_MAX_AGE_SECS);
    entry.vehicles.retain(|_, v| v.updated_at >= cutoff);
    if entry.vehicles.len() > MERGED_VEHICLES_CAP {
        let drop_n = entry.vehicles.len() - MERGED_VEHICLES_CAP;
        let ages: HashMap<String, DateTime<Utc>> = entry
            .vehicles
            .iter()
            .map(|(k, v)| (k.clone(), v.updated_at))
            .collect();
        for k in oldest_keys(&ages, drop_n) {
            entry.vehicles.remove(&k);
        }
    }
}

fn evict_stale_alerts(
    by_id: &mut HashMap<String, AlertRt>,
    seen: &mut MergeSeen,
    now: DateTime<Utc>,
) {
    let now_ts = now.timestamp();
    let unseen_cutoff = now - chrono::Duration::seconds(ALERT_UNSEEN_MAX_SECS);
    by_id.retain(|k, a| {
        let last_seen = *seen.alerts.entry(k.clone()).or_insert(now);
        !a.has_ended(now_ts) && last_seen >= unseen_cutoff
    });
    seen.alerts.retain(|k, _| by_id.contains_key(k));
    if by_id.len() > MERGED_ALERTS_CAP {
        let drop_n = by_id.len() - MERGED_ALERTS_CAP;
        for k in oldest_keys(&seen.alerts, drop_n) {
            by_id.remove(&k);
            seen.alerts.remove(&k);
        }
    }
}

/// The `n` keys with the oldest timestamps.
fn oldest_keys(ages: &HashMap<String, DateTime<Utc>>, n: usize) -> Vec<String> {
    let mut by_age: Vec<(DateTime<Utc>, &String)> = ages.iter().map(|(k, t)| (*t, k)).collect();
    by_age.sort_unstable();
    by_age.into_iter().take(n).map(|(_, k)| k.clone()).collect()
}

/// Build + finalize the overlay on the blocking pool (deep clone of feed state and
/// index rebuilds are CPU-heavy), then swap it in and notify subscribers.
async fn publish_rt(
    feed_rt: &HashMap<String, Arc<FeedRtState>>,
    version: u64,
    rt_overlay: &ArcSwap<RealtimeOverlay>,
    rt_version_tx: &broadcast::Sender<RtVersion>,
) {
    let snapshot: Vec<(String, Arc<FeedRtState>)> = feed_rt
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    match tokio::task::spawn_blocking(move || build_overlay(snapshot, version)).await {
        Ok(overlay) => {
            rt_overlay.store(Arc::new(overlay));
            let _ = rt_version_tx.send(RtVersion(version));
        }
        Err(e) => error!(error = %e, version, "RT overlay build task failed"),
    }
}

fn build_overlay(feeds: Vec<(String, Arc<FeedRtState>)>, version: u64) -> RealtimeOverlay {
    let mut overlay = RealtimeOverlay {
        version,
        feeds: feeds
            .into_iter()
            .map(|(k, v)| {
                let mut st = Arc::unwrap_or_clone(v);
                st.feed_id = k.clone();
                (k, st)
            })
            .collect(),
        canceled_trip_ids: Default::default(),
        rt_adjust: Default::default(),
    };
    crate::routing::finalize_realtime_overlay(&mut overlay);
    overlay
}

fn spawn_feed_workers(
    feed: FeedConfig,
    data_dir: std::path::PathBuf,
    client: reqwest::Client,
    download_timeout: Duration,
    rt_timeout: Duration,
    outbound: Arc<Semaphore>,
    static_builds: Arc<Semaphore>,
    tx: mpsc::Sender<FeedEvent>,
    cancel: CancellationToken,
) {
    let f = feed.clone();
    let c = cancel.clone();
    let cl = client.clone();
    let txx = tx.clone();
    let dd = data_dir;
    let builds = static_builds;
    tokio::spawn(async move {
        run_static_watcher(f, dd, cl, download_timeout, builds, txx, c).await;
    });

    if let Some(url) = feed.trip_updates_url.clone() {
        let f = feed.clone();
        let c = cancel.clone();
        let cl = client.clone();
        let txx = tx.clone();
        let out = outbound.clone();
        let poll = feed.rt_poll_interval_secs;
        tokio::spawn(async move {
            run_rt_poller(
                f,
                RtKind::TripUpdates,
                url,
                cl,
                rt_timeout,
                poll,
                out,
                txx,
                c,
            )
            .await;
        });
    }
    if let Some(url) = feed.service_alerts_url.clone() {
        let f = feed.clone();
        let c = cancel.clone();
        let cl = client.clone();
        let txx = tx.clone();
        let out = outbound.clone();
        let poll = feed.rt_poll_interval_secs;
        tokio::spawn(async move {
            run_rt_poller(
                f,
                RtKind::ServiceAlerts,
                url,
                cl,
                rt_timeout,
                poll,
                out,
                txx,
                c,
            )
            .await;
        });
    }
    if let Some(url) = feed.vehicle_positions_url.clone() {
        let f = feed.clone();
        let c = cancel.clone();
        let cl = client.clone();
        let txx = tx.clone();
        let out = outbound.clone();
        let poll = feed.rt_poll_interval_secs;
        tokio::spawn(async move {
            run_rt_poller(
                f,
                RtKind::VehiclePositions,
                url,
                cl,
                rt_timeout,
                poll,
                out,
                txx,
                c,
            )
            .await;
        });
    }
}

/// Kick off an epoch rebuild on the blocking pool (epoch build, hub linking,
/// edge sorting and FLASH persistence are CPU + disk heavy).
fn start_rebuild(
    bundles: &HashMap<String, Arc<FeedStaticBundle>>,
    routing: &crate::config::RoutingConfig,
    flash_dir: &std::path::Path,
    epoch: &Arc<ArcSwap<StaticEpoch>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let old_epoch_id = epoch.load().id.clone();
    let list: Vec<Arc<FeedStaticBundle>> = bundles.values().cloned().collect();
    let routing = routing.clone();
    let flash_dir = flash_dir.to_path_buf();
    let epoch = epoch.clone();
    let handle = tokio::task::spawn_blocking(move || {
        rebuild_epoch(list, &routing, &flash_dir, &epoch);
    });
    (old_epoch_id, handle)
}

fn rebuild_epoch(
    list: Vec<Arc<FeedStaticBundle>>,
    routing: &crate::config::RoutingConfig,
    flash_dir: &std::path::Path,
    epoch_slot: &ArcSwap<StaticEpoch>,
) {
    if list.is_empty() {
        return;
    }
    // Preliminary epoch only provides coordinates for hub linking — skip the
    // expensive flag preprocessing entirely.
    let preliminary =
        build_epoch_with_flash(list.clone(), vec![], crate::gtfs::pack::FlashMode::Skip);
    // Hub-link stations **and** monomodal/boardable places so the walk graph is complete
    // (IDFM monomodals are often location_type=0 with trips — still transfer nodes).
    let stop_coords: Vec<(Option<f64>, Option<f64>, bool)> = preliminary
        .stops
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let id = s.raw_id.to_ascii_lowercase();
            let place = id.contains("monomodalstopplace") || id.contains("multimodalstopplace");
            let boardable = preliminary
                .stop_departures
                .get(i)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            let link = s.is_station() || place || boardable;
            (s.lat, s.lon, link)
        })
        .collect();
    let hub = link_epoch_stops_safe(
        &stop_coords,
        // Slightly larger than default so monomodal RER ↔ Métro at large stations connect
        (routing.hub_link_radius_m as f64).max(400.0),
        routing.walk_speed_m_s,
        routing.default_transfer_s,
    );
    // Cap directed edges — keep **shortest** first so central monomodal↔Métro links
    // are not dropped when the suburban long-tail would fill the cap.
    let hub: Vec<_> = if hub.len() > 400_000 {
        warn!(
            edges = hub.len(),
            "hub link edges capped at 400000 (keeping shortest)"
        );
        let mut hub = hub;
        hub.sort_by(|a, b| {
            a.distance_m
                .partial_cmp(&b.distance_m)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hub.truncate(400_000);
        hub
    } else {
        hub
    };
    let epoch = build_epoch_with_flash(list, hub, crate::gtfs::pack::FlashMode::Persist(flash_dir));
    crate::gtfs::load_progress::set_phase(crate::gtfs::load_progress::PHASE_READY);
    info!(
        epoch_id = %epoch.id,
        stops = epoch.stop_count(),
        trips = epoch.trip_count(),
        walk_edges = epoch.walk_edges.len(),
        "static epoch swapped"
    );
    epoch_slot.store(Arc::new(epoch));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rt::overlay::{StopTimeRt, TripRt, VehiclePos};

    fn trip_ending_at(ts: i64) -> TripRt {
        let mut t = TripRt::default();
        t.stop_updates.insert(
            1,
            StopTimeRt {
                arrival_time: Some(ts),
                ..Default::default()
            },
        );
        t
    }

    fn vehicle(at: DateTime<Utc>) -> VehiclePos {
        VehiclePos {
            trip_id: None,
            lat: 48.85,
            lon: 2.35,
            bearing: None,
            speed: None,
            updated_at: at,
            current_stop_id: None,
            label: None,
            vehicle_id: None,
            license_plate: None,
            occupancy: None,
            current_status: None,
            current_stop_sequence: None,
            congestion: None,
            occupancy_percentage: None,
        }
    }

    fn alert(id: &str, end: Option<i64>) -> AlertRt {
        AlertRt {
            id: id.into(),
            header: Some("h".into()),
            description: None,
            severity: "INFO".into(),
            informed_stop_ids: vec![],
            informed_route_ids: vec![],
            informed_trip_ids: vec![],
            cause: None,
            effect: None,
            url: None,
            active_start: None,
            active_end: end,
            active_periods: vec![(None, end)],
            header_translations: vec![],
        }
    }

    fn trips_delta(trips: Vec<(&str, TripRt)>) -> FeedRtState {
        let mut s = FeedRtState::new("idfm");
        for (k, t) in trips {
            s.trips.insert(k.to_string(), t);
        }
        s
    }

    #[test]
    fn merge_evicts_trips_past_last_stop_time() {
        let now = Utc::now();
        let mut entry = FeedRtState::new("idfm");
        let mut seen = MergeSeen::default();
        let delta = trips_delta(vec![
            ("idfm:old", trip_ending_at(now.timestamp() - 3 * 3600)),
            ("idfm:live", trip_ending_at(now.timestamp() + 600)),
            ("idfm:no_times", TripRt::default()),
        ]);
        apply_rt_delta(&mut entry, &mut seen, RtKind::TripUpdates, delta, true, now);
        assert!(!entry.trips.contains_key("idfm:old"));
        assert!(entry.trips.contains_key("idfm:live"));
        assert!(entry.trips.contains_key("idfm:no_times"));
        assert_eq!(entry.trip_update_count, 2);
        assert_eq!(seen.trips.len(), 2);
    }

    #[test]
    fn merge_evicts_trips_unseen_for_too_long() {
        let t0 = Utc::now();
        let mut entry = FeedRtState::new("idfm");
        let mut seen = MergeSeen::default();
        let delta = trips_delta(vec![
            ("idfm:a", TripRt::default()),
            ("idfm:b", trip_ending_at(t0.timestamp() + 10 * 3600)),
        ]);
        apply_rt_delta(&mut entry, &mut seen, RtKind::TripUpdates, delta, true, t0);
        assert_eq!(entry.trips.len(), 2);

        // 3h later only `c` is refreshed: `a` and `b` were not seen for > 2h.
        let t1 = t0 + chrono::Duration::hours(3);
        let delta = trips_delta(vec![("idfm:c", TripRt::default())]);
        apply_rt_delta(&mut entry, &mut seen, RtKind::TripUpdates, delta, true, t1);
        assert_eq!(entry.trips.keys().collect::<Vec<_>>(), vec!["idfm:c"]);
        assert_eq!(entry.trip_update_count, 1);
    }

    #[test]
    fn replace_resets_and_does_not_evict() {
        let now = Utc::now();
        let mut entry = FeedRtState::new("gtfs");
        let mut seen = MergeSeen::default();
        let mut delta = trips_delta(vec![(
            "gtfs:old",
            trip_ending_at(now.timestamp() - 5 * 3600),
        )]);
        delta.trip_update_count = 1;
        apply_rt_delta(
            &mut entry,
            &mut seen,
            RtKind::TripUpdates,
            delta,
            false,
            now,
        );
        assert!(entry.trips.contains_key("gtfs:old"));
        assert!(seen.trips.is_empty());
    }

    #[test]
    fn merge_evicts_stale_vehicles() {
        let now = Utc::now();
        let mut entry = FeedRtState::new("idfm");
        let mut seen = MergeSeen::default();
        let mut delta = FeedRtState::new("idfm");
        delta
            .vehicles
            .insert("old".into(), vehicle(now - chrono::Duration::minutes(20)));
        delta.vehicles.insert("new".into(), vehicle(now));
        apply_rt_delta(
            &mut entry,
            &mut seen,
            RtKind::VehiclePositions,
            delta,
            true,
            now,
        );
        assert_eq!(entry.vehicles.keys().collect::<Vec<_>>(), vec!["new"]);
        assert_eq!(entry.vehicle_count, 1);
    }

    #[test]
    fn merge_evicts_ended_and_unseen_alerts() {
        let t0 = Utc::now();
        let ts = t0.timestamp();
        let mut entry = FeedRtState::new("idfm");
        let mut seen = MergeSeen::default();
        let mut delta = FeedRtState::new("idfm");
        delta.alerts = vec![
            alert("ended", Some(ts - 60)),
            alert("open", None),
            alert("future_end", Some(ts + 3600)),
        ];
        apply_rt_delta(
            &mut entry,
            &mut seen,
            RtKind::ServiceAlerts,
            delta,
            true,
            t0,
        );
        let mut ids: Vec<_> = entry.alerts.iter().map(|a| a.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["future_end", "open"]);

        // Re-send "open" 7h later; "future_end" has ended and was not refreshed.
        let t1 = t0 + chrono::Duration::hours(7);
        let mut delta = FeedRtState::new("idfm");
        delta.alerts = vec![alert("open", None)];
        apply_rt_delta(
            &mut entry,
            &mut seen,
            RtKind::ServiceAlerts,
            delta,
            true,
            t1,
        );
        let ids: Vec<_> = entry.alerts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["open"]);
        assert_eq!(entry.alert_count, 1);
    }

    #[test]
    fn oldest_keys_picks_by_timestamp() {
        let now = Utc::now();
        let mut ages = HashMap::new();
        ages.insert("c".to_string(), now);
        ages.insert("a".to_string(), now - chrono::Duration::minutes(10));
        ages.insert("b".to_string(), now - chrono::Duration::minutes(5));
        assert_eq!(
            oldest_keys(&ages, 2),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn build_overlay_sets_version_and_feed_ids() {
        let feeds = vec![("idfm".to_string(), Arc::new(FeedRtState::new("")))];
        let ov = build_overlay(feeds, 7);
        assert_eq!(ov.version, 7);
        assert_eq!(ov.feeds["idfm"].feed_id, "idfm");
    }
}
