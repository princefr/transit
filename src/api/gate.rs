//! API-key authentication + per-key token-bucket rate limiting.
//!
//! Config:
//! ```toml
//! [api]
//! enabled = true
//! [[api.keys]]
//! key = "secret-abc"
//! name = "mobile-app"
//! rate_limit_rps = 20
//! burst = 40
//! ```
//!
//! When `enabled = false` (default) or no keys are configured the API is
//! open — matching previous behavior. Keys are passed as `X-API-Key`.

use axum::{
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiKeyConfig {
    pub key: String,
    /// Human label for logs/metrics.
    #[serde(default)]
    pub name: String,
    /// Sustained requests/second allowed for this key.
    #[serde(default = "default_rps")]
    pub rate_limit_rps: f64,
    /// Short-burst allowance on top of the sustained rate.
    #[serde(default = "default_burst")]
    pub burst: u32,
}

fn default_rps() -> f64 {
    10.0
}
fn default_burst() -> u32 {
    20
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct ApiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub keys: Vec<ApiKeyConfig>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct ApiGate {
    /// key -> (config, bucket)
    entries: HashMap<String, (ApiKeyConfig, Mutex<Bucket>)>,
}

impl ApiGate {
    pub fn from_config(cfg: &ApiConfig) -> Self {
        let entries = cfg
            .keys
            .iter()
            .map(|k| {
                (
                    k.key.clone(),
                    (
                        k.clone(),
                        Mutex::new(Bucket {
                            tokens: k.burst as f64,
                            last: Instant::now(),
                        }),
                    ),
                )
            })
            .collect();
        Self { entries }
    }

    pub fn is_open(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Extract-and-verify middleware. Returns early with 401/429 on failure.
pub async fn api_gate_middleware(
    gate: std::sync::Arc<ApiGate>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if gate.is_open() {
        return next.run(request).await;
    }
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if key.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            "missing X-API-Key header",
        )
            .into_response();
    }
    let Some((cfg, bucket)) = gate.entries.get(key) else {
        return (StatusCode::UNAUTHORIZED, "invalid API key").into_response();
    };
    let now = Instant::now();
    let ok = {
        let mut b = bucket.lock().unwrap();
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.last = now;
        b.tokens = (b.tokens + elapsed * cfg.rate_limit_rps).min(cfg.burst as f64);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    };
    if !ok {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded",
        )
            .into_response();
    }
    next.run(request).await
}
