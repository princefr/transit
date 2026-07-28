use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransitError {
    #[error("configuration: {0}")]
    Config(String),
    #[error("feed not found: {0}")]
    FeedNotFound(String),
    #[error("stop not found: {0}")]
    StopNotFound(String),
    #[error("static timetable not loaded")]
    NoEpoch,
    #[error("routing: {0}")]
    Routing(String),
    #[error("download: {0}")]
    Download(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, TransitError>;

impl IntoResponse for TransitError {
    fn into_response(self) -> Response {
        let status = match &self {
            TransitError::StopNotFound(_) | TransitError::FeedNotFound(_) => StatusCode::NOT_FOUND,
            TransitError::NoEpoch => StatusCode::SERVICE_UNAVAILABLE,
            TransitError::Config(_) | TransitError::Routing(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(json!({
            "error": self.to_string(),
            "code": format!("{:?}", self).split('(').next().unwrap_or("Error"),
        }));
        (status, body).into_response()
    }
}
