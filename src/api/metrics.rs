use crate::state::AppState;
use axum::Extension;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Prometheus text exposition for core process metrics.
pub async fn metrics(Extension(state): Extension<Arc<AppState>>) -> Response {
    let epoch = state.load_epoch();
    let rt = state.load_rt();

    let mut body = String::with_capacity(1024);
    body.push_str("# HELP transit_stop_count Number of stops in the current static epoch\n");
    body.push_str("# TYPE transit_stop_count gauge\n");
    body.push_str(&format!("transit_stop_count {}\n", epoch.stop_count()));

    body.push_str("# HELP transit_trip_count Number of trips in the current static epoch\n");
    body.push_str("# TYPE transit_trip_count gauge\n");
    body.push_str(&format!("transit_trip_count {}\n", epoch.trip_count()));

    body.push_str("# HELP transit_rt_version Realtime overlay version counter\n");
    body.push_str("# TYPE transit_rt_version counter\n");
    body.push_str(&format!("transit_rt_version {}\n", rt.version));

    body.push_str(
        "# HELP transit_feed_trip_updates_age_seconds Age of last trip-updates fetch per feed\n",
    );
    body.push_str("# TYPE transit_feed_trip_updates_age_seconds gauge\n");
    body.push_str(
        "# HELP transit_feed_vehicle_positions_age_seconds Age of last vehicle-positions fetch per feed\n",
    );
    body.push_str("# TYPE transit_feed_vehicle_positions_age_seconds gauge\n");
    body.push_str(
        "# HELP transit_feed_alerts_age_seconds Age of last service-alerts fetch per feed\n",
    );
    body.push_str("# TYPE transit_feed_alerts_age_seconds gauge\n");

    for f in state.config.enabled_feeds() {
        let fr = rt.feeds.get(&f.id);
        let id = sanitize_label(&f.id);
        if let Some(age) = fr.and_then(|x| x.trip_updates_age_secs()) {
            body.push_str(&format!(
                "transit_feed_trip_updates_age_seconds{{feed_id=\"{id}\"}} {age}\n"
            ));
        }
        if let Some(age) = fr.and_then(|x| x.vehicles_age_secs()) {
            body.push_str(&format!(
                "transit_feed_vehicle_positions_age_seconds{{feed_id=\"{id}\"}} {age}\n"
            ));
        }
        if let Some(age) = fr.and_then(|x| x.alerts_age_secs()) {
            body.push_str(&format!(
                "transit_feed_alerts_age_seconds{{feed_id=\"{id}\"}} {age}\n"
            ));
        }
    }

    let (gate_open, unauthorized_total, key_metrics) = crate::api::gate::api_gate_metrics();
    body.push_str(
        "# HELP transit_api_gate_open Whether the API gate is open (no auth/rate-limit required)\n",
    );
    body.push_str("# TYPE transit_api_gate_open gauge\n");
    body.push_str(&format!(
        "transit_api_gate_open {}\n",
        if gate_open { 1 } else { 0 }
    ));

    body.push_str(
        "# HELP transit_api_unauthorized_total Requests rejected for missing/unknown API keys\n",
    );
    body.push_str("# TYPE transit_api_unauthorized_total counter\n");
    body.push_str(&format!("transit_api_unauthorized_total {unauthorized_total}\n"));

    body.push_str("# HELP transit_api_key_requests_total Requests allowed per API key name\n");
    body.push_str("# TYPE transit_api_key_requests_total counter\n");
    for k in &key_metrics {
        body.push_str(&format!(
            "transit_api_key_requests_total{{key=\"{}\"}} {}\n",
            sanitize_label(&k.name),
            k.allowed
        ));
    }

    body.push_str("# HELP transit_api_key_rejected_total Requests rate-limited per API key name\n");
    body.push_str("# TYPE transit_api_key_rejected_total counter\n");
    for k in &key_metrics {
        body.push_str(&format!(
            "transit_api_key_rejected_total{{key=\"{}\"}} {}\n",
            sanitize_label(&k.name),
            k.rejected
        ));
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response()
}

fn sanitize_label(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
