use crate::cache::TransitCache;
use crate::config::{Config, FeedConfig};
use crate::feeds::hub_link::link_epoch_stops_safe;
use crate::feeds::rt_poller::{run_rt_poller, RtKind};
use crate::feeds::static_watcher::run_static_watcher;
use crate::gtfs::pack::{build_epoch_with_flash, FeedStaticBundle, StaticEpoch};
use crate::prim::{spawn_prim_pollers, PrimClient};
use crate::rt::overlay::{FeedRtState, RealtimeOverlay};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
        /// When true (default for GTFS-RT), replace the kind's map/list.
        /// When false (PRIM SIRI partial samples), merge keys / alert ids.
        #[allow(dead_code)]
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
    tokio::spawn(async move {
        let mut bundles: HashMap<String, Arc<FeedStaticBundle>> = HashMap::new();
        let mut feed_rt: HashMap<String, FeedRtState> = HashMap::new();
        let mut version: u64 = 0;

        loop {
            tokio::select! {
                _ = coord_cancel.cancelled() => break,
                ev = rx.recv() => {
                    let Some(ev) = ev else { break };
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
                            let old_epoch_id = epoch.load().id.clone();
                            rebuild_epoch(&bundles, &routing, &flash_dir, &epoch);
                            let new_epoch_id = epoch.load().id.clone();
                            if old_epoch_id != new_epoch_id {
                                cache.invalidate_epoch(&old_epoch_id).await;
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
                            let entry = feed_rt.entry(feed_id.clone()).or_insert_with(|| FeedRtState::new(&feed_id));
                            match kind {
                                RtKind::TripUpdates => {
                                    if merge {
                                        for (k, v) in state.trips {
                                            entry.trips.insert(k, v);
                                        }
                                        entry.trip_update_count = entry.trips.len();
                                        entry.trip_updates_fetched_at = state
                                            .trip_updates_fetched_at
                                            .or(entry.trip_updates_fetched_at);
                                        entry.header_timestamp =
                                            state.header_timestamp.or(entry.header_timestamp);
                                    } else {
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
                                        entry.vehicle_count = entry.vehicles.len();
                                        entry.vehicles_fetched_at = state
                                            .vehicles_fetched_at
                                            .or(entry.vehicles_fetched_at);
                                    } else {
                                        entry.vehicles = state.vehicles;
                                        entry.vehicles_fetched_at = state.vehicles_fetched_at;
                                        entry.vehicle_count = state.vehicle_count;
                                    }
                                }
                                RtKind::ServiceAlerts => {
                                    if merge {
                                        // Upsert by alert id
                                        let mut by_id: HashMap<String, _> = entry
                                            .alerts
                                            .drain(..)
                                            .map(|a| (a.id.clone(), a))
                                            .collect();
                                        for a in state.alerts {
                                            let k = if a.id.is_empty() {
                                                format!(
                                                    "{}|{}",
                                                    a.header.as_deref().unwrap_or(""),
                                                    a.description.as_deref().unwrap_or("")
                                                )
                                            } else {
                                                a.id.clone()
                                            };
                                            by_id.insert(k, a);
                                        }
                                        entry.alerts = by_id.into_values().collect();
                                        entry.alert_count = entry.alerts.len();
                                        entry.alerts_fetched_at = state
                                            .alerts_fetched_at
                                            .or(entry.alerts_fetched_at);
                                    } else {
                                        entry.alerts = state.alerts;
                                        entry.alerts_fetched_at = state.alerts_fetched_at;
                                        entry.alert_count = state.alert_count;
                                    }
                                }
                            }
                            version = version.wrapping_add(1);
                            let mut overlay = RealtimeOverlay {
                                version,
                                feeds: feed_rt.clone(),
                                canceled_trip_ids: Default::default(),
                                rt_adjust: Default::default(),
                            };
                            for (k, v) in overlay.feeds.iter_mut() {
                                v.feed_id = k.clone();
                            }
                            crate::routing::finalize_realtime_overlay(&mut overlay);
                            rt_overlay.store(Arc::new(overlay));
                            let _ = rt_version_tx.send(RtVersion(version));
                        }
                    }
                }
            }
        }
        info!("feed coordinator stopped");
    });

    SupervisorHandles { cancel }
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

fn rebuild_epoch(
    bundles: &HashMap<String, Arc<FeedStaticBundle>>,
    routing: &crate::config::RoutingConfig,
    flash_dir: &std::path::Path,
    epoch_slot: &ArcSwap<StaticEpoch>,
) {
    let list: Vec<Arc<FeedStaticBundle>> = bundles.values().cloned().collect();
    if list.is_empty() {
        return;
    }
    // Preliminary epoch only provides coordinates for hub linking — skip the
    // expensive flag preprocessing entirely.
    let preliminary = build_epoch_with_flash(
        list.clone(),
        vec![],
        crate::gtfs::pack::FlashMode::Skip,
    );
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
    let epoch = build_epoch_with_flash(
        list,
        hub,
        crate::gtfs::pack::FlashMode::Persist(flash_dir),
    );
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
