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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
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

#[derive(Default)]
struct KeyCounters {
    allowed: AtomicU64,
    rejected: AtomicU64,
}

struct Entry {
    cfg: ApiKeyConfig,
    bucket: Mutex<Bucket>,
    counters: KeyCounters,
}

pub struct ApiGate {
    /// key -> config + bucket + metering counters
    entries: HashMap<String, Entry>,
}

/// Global per-name metering registry (readable from /metrics without holding
/// the gate itself). Populated once by [`ApiGate::from_config`].
static KEY_COUNTERS: OnceLock<Mutex<HashMap<String, KeyCounters>>> = OnceLock::new();
/// Requests rejected for missing/unknown API keys.
static UNAUTHORIZED_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Whether the gate is open (no keys configured → auth/rate-limit bypassed).
static GATE_OPEN: AtomicBool = AtomicBool::new(true);

/// Snapshot of one named API key's metering counters.
pub struct ApiKeyMetric {
    pub name: String,
    pub allowed: u64,
    pub rejected: u64,
}

/// (gate_open, unauthorized_total, per-key metrics) for the metrics endpoint.
pub fn api_gate_metrics() -> (bool, u64, Vec<ApiKeyMetric>) {
    let open = GATE_OPEN.load(Ordering::Relaxed);
    let unauthorized = UNAUTHORIZED_TOTAL.load(Ordering::Relaxed);
    let mut keys = Vec::new();
    if let Some(reg) = KEY_COUNTERS.get() {
        if let Ok(map) = reg.lock() {
            for (name, c) in map.iter() {
                keys.push(ApiKeyMetric {
                    name: name.clone(),
                    allowed: c.allowed.load(Ordering::Relaxed),
                    rejected: c.rejected.load(Ordering::Relaxed),
                });
            }
        }
    }
    keys.sort_by(|a, b| a.name.cmp(&b.name));
    (open, unauthorized, keys)
}

fn registry_counter(name: &str, allowed: bool) {
    if let Some(reg) = KEY_COUNTERS.get() {
        if let Ok(map) = reg.lock() {
            if let Some(c) = map.get(name) {
                if allowed {
                    c.allowed.fetch_add(1, Ordering::Relaxed);
                } else {
                    c.rejected.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

impl ApiGate {
    pub fn from_config(cfg: &ApiConfig) -> Self {
        let entries: HashMap<String, Entry> = cfg
            .keys
            .iter()
            .map(|k| {
                (
                    k.key.clone(),
                    Entry {
                        cfg: k.clone(),
                        bucket: Mutex::new(Bucket {
                            tokens: k.burst as f64,
                            last: Instant::now(),
                        }),
                        counters: KeyCounters::default(),
                    },
                )
            })
            .collect();
        // Seed the global registry so /metrics exposes every configured key
        // from startup (even at zero requests).
        let reg = KEY_COUNTERS.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(mut map) = reg.lock() {
            for k in &cfg.keys {
                let name = if k.name.is_empty() { "unnamed" } else { &k.name };
                map.entry(name.to_string()).or_default();
            }
        }
        GATE_OPEN.store(entries.is_empty(), Ordering::Relaxed);
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
        UNAUTHORIZED_TOTAL.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::UNAUTHORIZED,
            "missing X-API-Key header",
        )
            .into_response();
    }
    let Some(entry) = gate.entries.get(key) else {
        UNAUTHORIZED_TOTAL.fetch_add(1, Ordering::Relaxed);
        return (StatusCode::UNAUTHORIZED, "invalid API key").into_response();
    };
    let (cfg, bucket) = (&entry.cfg, &entry.bucket);
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
        entry.counters.rejected.fetch_add(1, Ordering::Relaxed);
        registry_counter(
            if cfg.name.is_empty() { "unnamed" } else { &cfg.name },
            false,
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded",
        )
            .into_response();
    }
    entry.counters.allowed.fetch_add(1, Ordering::Relaxed);
    registry_counter(
        if cfg.name.is_empty() { "unnamed" } else { &cfg.name },
        true,
    );
    next.run(request).await
}
