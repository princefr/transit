use crate::state::AppState;
use axum::extract::{ConnectInfo, Request};
use axum::Extension;
use std::net::SocketAddr;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// True when `/metrics` may answer this peer: always with
/// `server.metrics_public = true`, else only loopback peers. Without peer
/// info (server not started with connect-info) access is denied.
pub fn metrics_peer_allowed(public: bool, peer: Option<SocketAddr>) -> bool {
    public || peer.map(|p| p.ip().is_loopback()).unwrap_or(false)
}

/// Prometheus text exposition for core process metrics.
///
/// Mounted only when `server.metrics_enabled`; restricted to loopback peers
/// unless `server.metrics_public` (404 otherwise, to not advertise it).
pub async fn metrics(Extension(state): Extension<Arc<AppState>>, req: Request) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    if !metrics_peer_allowed(state.config.server.metrics_public, peer) {
        return StatusCode::NOT_FOUND.into_response();
    }
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

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_loopback_only_by_default() {
        let lo: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let lo6: SocketAddr = "[::1]:5000".parse().unwrap();
        let remote: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        assert!(metrics_peer_allowed(false, Some(lo)));
        assert!(metrics_peer_allowed(false, Some(lo6)));
        assert!(!metrics_peer_allowed(false, Some(remote)));
        assert!(!metrics_peer_allowed(false, None));
        assert!(metrics_peer_allowed(true, Some(remote)));
    }
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
