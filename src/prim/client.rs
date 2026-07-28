//! HTTP client for PRIM (Île-de-France Mobilités) — **raw SIRI / GTFS**, not Navitia.
//!
//! Header: `apikey: <IDFM_PRIM_API_KEY>`

use crate::error::{Result, TransitError};
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://prim.iledefrance-mobilites.fr";

#[derive(Clone)]
pub struct PrimClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    user_agent: String,
    timeout: Duration,
}

impl PrimClient {
    pub fn key_present(env_name: &str) -> bool {
        std::env::var(env_name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    }

    pub fn from_env(
        base_url: &str,
        api_key_env: &str,
        user_agent: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let api_key = std::env::var(api_key_env)
            .map_err(|_| {
                TransitError::Config(format!("missing env {api_key_env} for PRIM apikey"))
            })?
            .trim()
            .to_string();
        if api_key.is_empty() {
            return Err(TransitError::Config(format!(
                "empty env {api_key_env} for PRIM apikey"
            )));
        }
        let http = reqwest::Client::builder()
            .gzip(true)
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| TransitError::Config(format!("prim reqwest: {e}")))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            user_agent: user_agent.to_string(),
            timeout,
        })
    }

    pub fn absolute_url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_string();
        }
        format!(
            "{}{}",
            self.base_url,
            if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            }
        )
    }

    /// GET with PRIM `apikey` header.
    ///
    /// Global min gap between PRIM calls (~700ms) so SM+ET+GM never stampede
    /// the short-term rate limit (HTTP 429).
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        pace_prim_request().await;
        let url = self.absolute_url(path);
        let resp = self
            .http
            .get(&url)
            .header("apikey", &self.api_key)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json, application/xml, */*")
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| TransitError::Http(format!("PRIM GET {url}: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| TransitError::Http(format!("PRIM body {url}: {e}")))?
            .to_vec();
        if !status.is_success() {
            let hint = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]);
            return Err(TransitError::Http(format!(
                "PRIM GET {url} -> {status}: {hint}"
            )));
        }
        Ok(bytes)
    }
}

use std::sync::Mutex;
use std::time::Instant;

static LAST_PRIM_AT: Mutex<Option<Instant>> = Mutex::new(None);
const PRIM_MIN_GAP_MS: u64 = 700;

async fn pace_prim_request() {
    let wait_ms = {
        let mut guard = LAST_PRIM_AT.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let wait = if let Some(prev) = *guard {
            let elapsed = now.duration_since(prev).as_millis() as u64;
            PRIM_MIN_GAP_MS.saturating_sub(elapsed)
        } else {
            0
        };
        *guard = Some(now + Duration::from_millis(wait));
        wait
    };
    if wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_url_joins() {
        let c = PrimClient {
            http: reqwest::Client::new(),
            base_url: "https://prim.example".into(),
            api_key: "k".into(),
            user_agent: "ua".into(),
            timeout: Duration::from_secs(1),
        };
        assert_eq!(
            c.absolute_url("/marketplace/estimated-timetable"),
            "https://prim.example/marketplace/estimated-timetable"
        );
    }
}
