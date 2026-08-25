pub mod gate;
pub mod graphql;
pub mod health;
pub mod metrics;

use crate::state::AppState;
use async_graphql::http::GraphiQLSource;
use async_graphql_axum::{GraphQLRequest, GraphQLResponse, GraphQLSubscription};
use axum::response::{Html, IntoResponse};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Extension, Router};
use std::sync::Arc;

use self::graphql::ServiceSchema;

async fn graphql_handler(
    schema: Extension<ServiceSchema>,
    state: Extension<Arc<AppState>>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let mut req = req.into_inner();
    req = req.data(state.0.clone());
    schema.execute(req).await.into()
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
    use axum::middleware;

    let playground = state.config.server.graphql_playground;

    let gate: std::sync::Arc<gate::ApiGate> =
        std::sync::Arc::new(gate::ApiGate::from_config(&state.config.api));

    let mut r = Router::new()
        .route("/health", get(health::health))
        .route("/metrics", get(metrics::metrics))
        .route("/graphql", post(graphql_handler))
        .route_service("/ws", GraphQLSubscription::new(schema.clone()))
        // Health/metrics stay unauthenticated; everything above passes
        // through the key + rate-limit gate (open when no keys configured).
        .layer(middleware::from_fn(
            move |headers: HeaderMap, req: axum::extract::Request, next: middleware::Next| {
                let gate = gate.clone();
                async move { gate::api_gate_middleware(gate, headers, req, next).await }
            },
        ));

    if playground {
        r = r.route("/", get(graphiql)).route("/graphiql", get(graphiql));
    }

    r.layer(Extension(schema))
        .layer(Extension(state))
}
