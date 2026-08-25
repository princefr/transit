use crate::config::FeedConfig;
use crate::error::Result;
use crate::feeds::download::{
    download_conditional_with_headers, read_meta, write_current_zip, write_meta, FeedMeta,
};
use crate::feeds::FeedEvent;
use crate::gtfs::parse::load_gtfs_bytes_with_horizon;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub async fn run_static_watcher(
    feed: FeedConfig,
    data_dir: PathBuf,
    client: reqwest::Client,
    download_timeout: Duration,
    build_sem: Arc<Semaphore>,
    tx: mpsc::Sender<FeedEvent>,
    cancel: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(feed.static_check_interval_secs));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Initial load: try disk then network
    if let Err(e) =
        try_load_initial(&feed, &data_dir, &client, download_timeout, &build_sem, &tx).await
    {
        warn!(feed_id = %feed.id, error = %e, "initial static load failed; will retry");
        let _ = tx
            .send(FeedEvent::StaticFailed {
                feed_id: feed.id.clone(),
                error: e.to_string(),
            })
            .await;
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(feed_id = %feed.id, "static watcher stopped");
                break;
            }
            _ = interval.tick() => {
                if let Err(e) = check_and_update(
                    &feed, &data_dir, &client, download_timeout, &build_sem, &tx
                ).await {
                    error!(feed_id = %feed.id, error = %e, "static update failed");
                    let _ = tx.send(FeedEvent::StaticFailed {
                        feed_id: feed.id.clone(),
                        error: e.to_string(),
                    }).await;
                }
            }
        }
    }
}

async fn try_load_initial(
    feed: &FeedConfig,
    data_dir: &PathBuf,
    client: &reqwest::Client,
    timeout: Duration,
    build_sem: &Arc<Semaphore>,
    tx: &mpsc::Sender<FeedEvent>,
) -> Result<()> {
    let zip_path = crate::feeds::download::current_zip_path(data_dir, &feed.id);
    if zip_path.exists() {
        let t0 = Instant::now();
        let bytes = tokio::fs::read(&zip_path).await?;
        let nbytes = bytes.len();
        // Validate cached zip before parse; if corrupt, fall through to network
        if let Err(e) = crate::feeds::download::validate_gtfs_zip(&bytes) {
            warn!(
                feed_id = %feed.id,
                error = %e,
                "cached current.zip invalid; re-downloading"
            );
            return check_and_update(feed, data_dir, client, timeout, build_sem, tx).await;
        }
        let meta = read_meta(data_dir, &feed.id);
        let disk_sha = crate::feeds::download::sha256_hex(&bytes);
        if let Some(ref known) = meta.sha256 {
            if known != &disk_sha {
                warn!(
                    feed_id = %feed.id,
                    meta_sha = %known,
                    disk_sha = %disk_sha,
                    "cached zip sha256 mismatch meta; still loading disk"
                );
            }
        }
        info!(
            feed_id = %feed.id,
            path = %zip_path.display(),
            bytes = nbytes,
            sha256 = %disk_sha,
            "loading static GTFS from disk"
        );
        let feed_id = feed.id.clone();
        let horizon = feed.static_horizon_days;
        let _permit = build_sem
            .acquire()
            .await
            .map_err(|e| crate::error::TransitError::Other(e.into()))?;
        let parse_t0 = Instant::now();
        crate::gtfs::load_progress::set_phase(crate::gtfs::load_progress::PHASE_PARSE);
        let bundle = tokio::task::spawn_blocking(move || {
            load_gtfs_bytes_with_horizon(&feed_id, &bytes, horizon)
        })
        .await
        .map_err(|e| crate::error::TransitError::Other(e.into()))??;
        drop(_permit);
        info!(
            feed_id = %feed.id,
            bytes = nbytes,
            horizon_days = ?horizon,
            parse_ms = parse_t0.elapsed().as_millis() as u64,
            total_ms = t0.elapsed().as_millis() as u64,
            stops = bundle.stop_count(),
            trips = bundle.trip_count(),
            "static GTFS loaded from disk"
        );
        tx.send(FeedEvent::StaticReady {
            feed_id: feed.id.clone(),
            bundle: Arc::new(bundle),
            sha256: Some(disk_sha),
        })
        .await
        .ok();
        return Ok(());
    }
    check_and_update(feed, data_dir, client, timeout, build_sem, tx).await
}

async fn check_and_update(
    feed: &FeedConfig,
    data_dir: &PathBuf,
    client: &reqwest::Client,
    timeout: Duration,
    build_sem: &Arc<Semaphore>,
    tx: &mpsc::Sender<FeedEvent>,
) -> Result<()> {
    let t0 = Instant::now();
    crate::gtfs::load_progress::set_phase(crate::gtfs::load_progress::PHASE_DOWNLOAD);
    let meta = read_meta(data_dir, &feed.id);
    let headers: Vec<(String, String)> = feed.auth_header_pair().into_iter().collect();
    let dl = download_conditional_with_headers(
        client,
        &feed.static_url,
        &feed.user_agent,
        meta.etag.as_deref(),
        timeout,
        &headers,
    )
    .await?;

    if dl.not_modified {
        info!(
            feed_id = %feed.id,
            duration_ms = t0.elapsed().as_millis() as u64,
            "static GTFS not modified (304)"
        );
        return Ok(());
    }

    // Skip rebuild if content hash matches known meta (e.g. CDN dropped etag)
    if let Some(ref known) = meta.sha256 {
        if known == &dl.sha256 {
            // Refresh etag if server provided a new one without content change
            if dl.etag != meta.etag {
                let mut m = meta.clone();
                m.etag = dl.etag.clone();
                let _ = write_meta(data_dir, &feed.id, &m);
            }
            info!(
                feed_id = %feed.id,
                sha256 = %dl.sha256,
                bytes = dl.bytes.len(),
                duration_ms = t0.elapsed().as_millis() as u64,
                "static GTFS same sha256, skip rebuild"
            );
            return Ok(());
        }
    }

    let nbytes = dl.bytes.len();
    info!(
        feed_id = %feed.id,
        bytes = nbytes,
        sha256 = %dl.sha256,
        "validating and promoting static GTFS zip"
    );

    // validate + promote (write_current_zip validates stops.txt + stop_times.txt)
    write_current_zip(data_dir, &feed.id, &dl.bytes)?;
    write_meta(
        data_dir,
        &feed.id,
        &FeedMeta {
            etag: dl.etag.clone(),
            sha256: Some(dl.sha256.clone()),
            loaded_at: Some(chrono::Utc::now().to_rfc3339()),
            static_url: Some(feed.static_url.clone()),
        },
    )?;

    let feed_id = feed.id.clone();
    let bytes = dl.bytes;
    let sha = dl.sha256.clone();
    let horizon = feed.static_horizon_days;
    let _permit = build_sem
        .acquire()
        .await
        .map_err(|e| crate::error::TransitError::Other(e.into()))?;
    let parse_t0 = Instant::now();
    info!(
        feed_id = %feed.id,
        bytes = nbytes,
        horizon_days = ?horizon,
        "parsing static GTFS (blocking pool)"
    );
    crate::gtfs::load_progress::set_phase(crate::gtfs::load_progress::PHASE_PARSE);
    let bundle = tokio::task::spawn_blocking(move || {
        load_gtfs_bytes_with_horizon(&feed_id, &bytes, horizon)
    })
    .await
    .map_err(|e| crate::error::TransitError::Other(e.into()))??;
    drop(_permit);

    info!(
        feed_id = %feed.id,
        bytes = nbytes,
        sha256 = %sha,
        parse_ms = parse_t0.elapsed().as_millis() as u64,
        total_ms = t0.elapsed().as_millis() as u64,
        stops = bundle.stop_count(),
        trips = bundle.trip_count(),
        "static GTFS rebuild complete"
    );

    tx.send(FeedEvent::StaticReady {
        feed_id: feed.id.clone(),
        bundle: Arc::new(bundle),
        sha256: Some(sha),
    })
    .await
    .ok();
    Ok(())
}
