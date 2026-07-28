pub mod graphql;
pub mod health;
pub mod metrics;

use crate::state::AppState;
use async_graphql::http::GraphiQLSource;
use async_graphql_axum::{GraphQLRequest, GraphQLResponse, GraphQLSubscription};
use axum::response::{Html, IntoResponse};
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
    let playground = state.config.server.graphql_playground;

    let mut r = Router::new()
        .route("/health", get(health::health))
        .route("/metrics", get(metrics::metrics))
        .route("/graphql", post(graphql_handler))
        .route_service("/ws", GraphQLSubscription::new(schema.clone()));

    if playground {
        r = r.route("/", get(graphiql)).route("/graphiql", get(graphiql));
    }

    r.layer(Extension(schema))
        .layer(Extension(state))
}
