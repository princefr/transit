use crate::state::AppState;
use axum::Extension;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn health(Extension(state): Extension<Arc<AppState>>) -> Json<Value> {
    let epoch = state.load_epoch();
    let rt = state.load_rt();
    let cache_key = state
        .cache
        .health_key(&epoch.id, rt.version);

    if let Some(cached) = state.cache.get_health(&cache_key).await {
        return Json(cached);
    }

    let body = build_health_json(&state, &epoch, &rt);
    state.cache.set_health(&cache_key, &body).await;
    Json(body)
}

fn build_health_json(
    state: &AppState,
    epoch: &crate::gtfs::pack::StaticEpoch,
    rt: &crate::rt::overlay::RealtimeOverlay,
) -> Value {
    let runtime = crate::feeds::runtime_status();
    let mut feeds = Vec::new();
    for f in state.config.enabled_feeds() {
        let fr = rt.feeds.get(&f.id);
        let rs = runtime.get(&f.id);
        feeds.push(json!({
            "id": f.id,
            "static_loaded": epoch.feeds.contains_key(&f.id),
            "trip_updates_age_s": fr.and_then(|x| x.trip_updates_age_secs()),
            "vehicles_age_s": fr.and_then(|x| x.vehicles_age_secs()),
            "alerts_age_s": fr.and_then(|x| x.alerts_age_secs()),
            "trip_update_count": fr.map(|x| x.trip_update_count).unwrap_or(0),
            "vehicle_count": fr.map(|x| x.vehicle_count).unwrap_or(0),
            "alert_count": fr.map(|x| x.alert_count).unwrap_or(0),
            "last_static_error": rs.and_then(|s| s.last_static_error.clone()),
            "last_static_ok_at": rs.and_then(|s| s.last_static_ok_at.map(|t| t.to_rfc3339())),
            "static_sha256": rs.and_then(|s| s.last_static_sha256.clone()),
            "static_stops": rs.and_then(|s| s.last_static_stops),
            "static_trips": rs.and_then(|s| s.last_static_trips),
        }));
    }
    let rt_stats = rt.stats_total();
    let status = if epoch.trip_count() > 0 {
        "ok"
    } else {
        "starting"
    };
    json!({
        "status": status,
        "epoch_id": epoch.id,
        "stops": epoch.stop_count(),
        "trips": epoch.trip_count(),
        "rt_version": rt.version,
        "rt_stats": {
            "canceled_trips": rt_stats.canceled_trip_count,
            "trip_updates": rt_stats.trip_update_count,
            "vehicles": rt_stats.vehicle_count,
            "alerts": rt_stats.alert_count,
        },
        "feeds": feeds,
        "dataset_loading": crate::gtfs::load_progress::snapshot_json(),
        "cache": {
            "redis_enabled": state.cache.enabled(),
            "redis_connected": state.cache.connected(),
        },
    })
}
