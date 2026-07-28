use crate::config::FeedConfig;
use crate::feeds::download::download_bytes_with_headers;
use crate::feeds::FeedEvent;
use crate::rt::adapter::{ingest_realtime, RealtimeFormat, RealtimeKind};
use crate::rt::decode::{apply_service_alerts, apply_trip_updates, apply_vehicle_positions};
use crate::rt::overlay::FeedRtState;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Decode payloads larger than this off the async runtime.
const SPAWN_BLOCKING_THRESHOLD: usize = 1_048_576; // 1 MiB

#[derive(Clone, Copy, Debug)]
pub enum RtKind {
    TripUpdates,
    VehiclePositions,
    ServiceAlerts,
}

impl RtKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TripUpdates => "trip_updates",
            Self::VehiclePositions => "vehicle_positions",
            Self::ServiceAlerts => "service_alerts",
        }
    }
}

/// Decode GTFS-RT **or** SIRI into [`FeedRtState`] via the unified adapter.
fn decode_rt(feed_id: &str, kind: RtKind, bytes: &[u8]) -> FeedRtState {
    let format = match kind {
        // Prefer auto so a mislabeled URL still works if payload is SIRI JSON
        RtKind::TripUpdates => RealtimeFormat::Auto,
        RtKind::VehiclePositions => RealtimeFormat::Auto,
        RtKind::ServiceAlerts => RealtimeFormat::Auto,
    };
    match ingest_realtime(feed_id, bytes, format) {
        Ok(delta) => {
            // If auto picked something but empty for this kind, fall back to GTFS-RT helpers
            let empty_for_kind = match kind {
                RtKind::TripUpdates => delta.state.trips.is_empty(),
                RtKind::VehiclePositions => delta.state.vehicles.is_empty(),
                RtKind::ServiceAlerts => delta.state.alerts.is_empty(),
            };
            if empty_for_kind && matches!(delta.format, RealtimeFormat::GtfsRt | RealtimeFormat::Auto)
            {
                let mut state = FeedRtState::new(feed_id);
                match kind {
                    RtKind::TripUpdates => apply_trip_updates(feed_id, bytes, &mut state),
                    RtKind::VehiclePositions => {
                        apply_vehicle_positions(feed_id, bytes, &mut state)
                    }
                    RtKind::ServiceAlerts => apply_service_alerts(feed_id, bytes, &mut state),
                }
                return state;
            }
            // Map adapter kind loosely — return full state from parser
            let _ = RealtimeKind::Mixed;
            delta.state
        }
        Err(_) => {
            let mut state = FeedRtState::new(feed_id);
            match kind {
                RtKind::TripUpdates => apply_trip_updates(feed_id, bytes, &mut state),
                RtKind::VehiclePositions => apply_vehicle_positions(feed_id, bytes, &mut state),
                RtKind::ServiceAlerts => apply_service_alerts(feed_id, bytes, &mut state),
            }
            state
        }
    }
}

/// Jitter in [0, max_ms] using a cheap LCG from the current time.
fn jitter_ms(max_ms: u64) -> u64 {
    if max_ms == 0 {
        return 0;
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    // simple mix
    let x = t.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17);
    x % (max_ms + 1)
}

pub async fn run_rt_poller(
    feed: FeedConfig,
    kind: RtKind,
    url: String,
    client: reqwest::Client,
    timeout: Duration,
    poll_secs: u64,
    outbound: Arc<Semaphore>,
    tx: mpsc::Sender<FeedEvent>,
    cancel: CancellationToken,
) {
    let base = Duration::from_secs(poll_secs.max(15));
    // Initial jitter so pollers don't thundering-herd
    let startup_jitter = Duration::from_millis(jitter_ms(base.as_millis() as u64 / 4));
    let mut backoff = base;
    let max_backoff = Duration::from_secs(300);

    info!(
        feed_id = %feed.id,
        rt_kind = kind.as_str(),
        ?kind,
        %url,
        poll_secs = poll_secs.max(15),
        startup_jitter_ms = startup_jitter.as_millis() as u64,
        "RT poller started"
    );

    tokio::select! {
        _ = cancel.cancelled() => {
            info!(feed_id = %feed.id, rt_kind = kind.as_str(), "RT poller stopped");
            return;
        }
        _ = tokio::time::sleep(startup_jitter) => {}
    }

    loop {
        // Interval with ±10% jitter each cycle
        let j = jitter_ms((base.as_millis() as u64) / 5);
        let half = (base.as_millis() as u64) / 10;
        let wait = base
            .saturating_sub(Duration::from_millis(half))
            + Duration::from_millis(j.min(half * 2));

        tokio::select! {
            _ = cancel.cancelled() => {
                info!(feed_id = %feed.id, rt_kind = kind.as_str(), "RT poller stopped");
                break;
            }
            _ = tokio::time::sleep(wait) => {
                let permit = match outbound.acquire().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let t0 = Instant::now();
                let headers: Vec<(String, String)> = feed
                    .auth_header_pair()
                    .into_iter()
                    .collect();
                let result = download_bytes_with_headers(
                    &client,
                    &url,
                    &feed.user_agent,
                    timeout,
                    &headers,
                )
                .await;
                drop(permit);

                match result {
                    Ok(bytes) => {
                        backoff = base;
                        let nbytes = bytes.len();
                        let feed_id = feed.id.clone();
                        let state = if nbytes > SPAWN_BLOCKING_THRESHOLD {
                            debug!(
                                feed_id = %feed.id,
                                rt_kind = kind.as_str(),
                                bytes = nbytes,
                                "RT decode on blocking pool"
                            );
                            match tokio::task::spawn_blocking(move || {
                                decode_rt(&feed_id, kind, &bytes)
                            })
                            .await
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    error!(
                                        feed_id = %feed.id,
                                        rt_kind = kind.as_str(),
                                        error = %e,
                                        "RT decode join failed"
                                    );
                                    continue;
                                }
                            }
                        } else {
                            decode_rt(&feed_id, kind, &bytes)
                        };

                        info!(
                            feed_id = %feed.id,
                            rt_kind = kind.as_str(),
                            ?kind,
                            bytes = nbytes,
                            duration_ms = t0.elapsed().as_millis() as u64,
                            trips = state.trip_update_count,
                            vehicles = state.vehicle_count,
                            alerts = state.alert_count,
                            rt_ok = 1u64,
                            "RT poll ok"
                        );
                        let _ = tx
                            .send(FeedEvent::RealtimeDelta {
                                feed_id: feed.id.clone(),
                                kind,
                                state,
                                // Full GTFS-RT (or full auto) snapshot replaces the kind
                                merge: false,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            feed_id = %feed.id,
                            rt_kind = kind.as_str(),
                            ?kind,
                            error = %e,
                            duration_ms = t0.elapsed().as_millis() as u64,
                            rt_fail = 1u64,
                            "RT poll failed"
                        );
                        error!(
                            feed_id = %feed.id,
                            rt_kind = kind.as_str(),
                            "backing off {:?}",
                            backoff
                        );
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(max_backoff);
                    }
                }
            }
        }
    }
}
