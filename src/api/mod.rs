pub mod graphql;
pub mod health;
pub mod metrics;

use crate::config::ServerConfig;
use crate::state::AppState;
use async_graphql::http::{GraphiQLSource, MultipartOptions, ALL_WEBSOCKET_PROTOCOLS};
use async_graphql::BatchRequest;
use async_graphql_axum::{GraphQLProtocol, GraphQLResponse, GraphQLWebSocket};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tracing::warn;

use self::graphql::schema::WsSubscriptionBudget;
use self::graphql::ServiceSchema;

/// How long a request waits for a global HTTP slot before `503`.
const HTTP_SLOT_WAIT: Duration = Duration::from_secs(1);

async fn graphql_handler(
    Extension(schema): Extension<ServiceSchema>,
    Extension(state): Extension<Arc<AppState>>,
    headers: HeaderMap,
    // `Bytes` honours `DefaultBodyLimit` (set per route below) — the stock
    // `GraphQLRequest` extractor streams the body without any size cap.
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let batch = match async_graphql::http::receive_batch_body(
        content_type,
        body.as_ref(),
        MultipartOptions::default().max_num_files(0),
    )
    .await
    {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    if let BatchRequest::Batch(ref reqs) = batch {
        let max = state.config.server.max_batch_size.max(1);
        if reqs.is_empty() || reqs.len() > max {
            return (
                StatusCode::BAD_REQUEST,
                format!("batch must contain between 1 and {max} operations"),
            )
                .into_response();
        }
    }
    let batch = batch.data(state);
    GraphQLResponse::from(schema.execute_batch(batch).await).into_response()
}

/// Same-origin check: does `Origin` (`scheme://host[:port]`) name the `Host` header?
fn origin_matches_host(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let authority = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
        .trim_end_matches('/');
    authority.eq_ignore_ascii_case(host.trim())
}

/// Browser `Origin` policy for the websocket endpoint (browsers do not apply
/// CORS to websockets, so cross-site pages could otherwise open `/ws`).
/// Non-browser clients send no `Origin` and are allowed.
pub fn ws_origin_allowed(server: &ServerConfig, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    server.origin_allowed(origin) || origin_matches_host(origin, host)
}

/// `/ws`: GraphQL subscriptions with a global connection cap, an `Origin`
/// allowlist, a max message size and a per-connection subscription budget.
async fn ws_handler(
    Extension(schema): Extension<ServiceSchema>,
    Extension(state): Extension<Arc<AppState>>,
    headers: HeaderMap,
    protocol: GraphQLProtocol,
    upgrade: WebSocketUpgrade,
) -> Response {
    let server = &state.config.server;
    if !ws_origin_allowed(server, &headers) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let Ok(permit) = state.ws_slots.clone().try_acquire_owned() else {
        warn!("rejecting websocket: max_ws_connections reached");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many websocket connections",
        )
            .into_response();
    };
    let mut data = async_graphql::Data::default();
    data.insert(WsSubscriptionBudget::new(
        server.max_subscriptions_per_connection,
    ));
    let max_msg = server.max_body_bytes.max(1024);
    upgrade
        .protocols(ALL_WEBSOCKET_PROTOCOLS)
        .max_message_size(max_msg)
        .max_frame_size(max_msg)
        .on_upgrade(move |stream| async move {
            // Held for the whole connection lifetime.
            let _permit = permit;
            GraphQLWebSocket::new(stream, schema, protocol)
                .with_data(data)
                .serve()
                .await
        })
}

/// Global guard for non-websocket routes: bounded concurrency (wait briefly,
/// then `503`) and a wall-clock timeout (`504`). Implemented as middleware
/// instead of tower layers so the `/ws` upgrade route stays untouched.
async fn http_guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let permit =
        match tokio::time::timeout(HTTP_SLOT_WAIT, state.http_slots.clone().acquire_owned()).await
        {
            Ok(Ok(p)) => p,
            _ => {
                return (StatusCode::SERVICE_UNAVAILABLE, "server busy, retry shortly")
                    .into_response()
            }
        };
    let timeout = state
        .config
        .server
        .effective_request_timeout(state.config.graphql.itinerary_timeout_secs);
    let resp = match tokio::time::timeout(timeout, next.run(req)).await {
        Ok(r) => r,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "request timed out").into_response(),
    };
    drop(permit);
    resp
}

/// CORS from `server.cors_allowed_origins` (`"*"` = permissive; otherwise an
/// exact allowlist; empty = no cross-origin browser access).
pub fn cors_layer(server: &ServerConfig) -> CorsLayer {
    if server.cors_permissive() {
        return CorsLayer::permissive();
    }
    let origins: Vec<HeaderValue> = server
        .cors_allowed_origins
        .iter()
        .filter_map(|o| {
            let o = o.trim().trim_end_matches('/');
            match HeaderValue::from_str(o) {
                Ok(v) if !o.is_empty() => Some(v),
                _ => {
                    warn!(origin = %o, "ignoring invalid cors_allowed_origins entry");
                    None
                }
            }
        })
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .max_age(Duration::from_secs(600))
}

async fn graphiql() -> impl IntoResponse {
    Html(
        GraphiQLSource::build()
            .endpoint("/graphql")
            .subscription_endpoint("/ws")
            .finish(),
    )
}

pub fn router(state: Arc<AppState>, schema: ServiceSchema) -> Router {
    let server = state.config.server.clone();

    let mut http = Router::new()
        .route("/health", get(health::health))
        .route(
            "/graphql",
            post(graphql_handler).layer(DefaultBodyLimit::max(server.max_body_bytes.max(1024))),
        );
    if server.metrics_enabled {
        http = http.route("/metrics", get(metrics::metrics));
    }
    if server.graphql_playground {
        http = http
            .route("/", get(graphiql))
            .route("/graphiql", get(graphiql));
    }
    let http = http.layer(middleware::from_fn_with_state(state.clone(), http_guard));

    let ws = Router::new().route("/ws", get(ws_handler));

    http.merge(ws)
        .layer(cors_layer(&server))
        .layer(Extension(schema))
        .layer(Extension(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn headers(origin: Option<&str>, host: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(o) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        if let Some(x) = host {
            h.insert(header::HOST, HeaderValue::from_str(x).unwrap());
        }
        h
    }

    #[test]
    fn ws_origin_policy() {
        let cfg = Config::default().server;
        // Non-browser client (no Origin) allowed.
        assert!(ws_origin_allowed(&cfg, &headers(None, Some("api:8080"))));
        // Default dev UI allowed.
        assert!(ws_origin_allowed(
            &cfg,
            &headers(Some("http://localhost:8088"), Some("127.0.0.1:8080"))
        ));
        // Same-origin (GraphiQL served by this server) allowed.
        assert!(ws_origin_allowed(
            &cfg,
            &headers(Some("http://transit.example:8080"), Some("transit.example:8080"))
        ));
        // Foreign site rejected.
        assert!(!ws_origin_allowed(
            &cfg,
            &headers(Some("https://evil.example"), Some("transit.example:8080"))
        ));
        let mut open = cfg.clone();
        open.cors_allowed_origins = vec!["*".into()];
        assert!(ws_origin_allowed(
            &open,
            &headers(Some("https://evil.example"), Some("transit.example:8080"))
        ));
    }
}
