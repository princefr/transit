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
        // One process-wide reqwest client (connection pool + TLS config) shared by
        // every `PrimClient`, so per-request `from_env` calls are cheap and reuse
        // keep-alive connections instead of re-handshaking with PRIM.
        let http = shared_http()?;
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
        pace_prim_request(None).await;
        self.get_bytes_unpaced(path).await
    }

    /// Like [`get_bytes`](Self::get_bytes) but gives up (without calling PRIM and
    /// without reserving a pacer slot) when the shared pacer queue would make
    /// this request wait longer than `max_wait`. Used by on-demand GraphQL paths
    /// so user traffic never piles up behind the background pollers.
    pub async fn get_bytes_bounded(&self, path: &str, max_wait: Duration) -> Result<Vec<u8>> {
        if !pace_prim_request(Some(max_wait)).await {
            return Err(TransitError::Http(format!(
                "PRIM pacer busy (wait > {}ms); skipped",
                max_wait.as_millis()
            )));
        }
        self.get_bytes_unpaced(path).await
    }

    async fn get_bytes_unpaced(&self, path: &str) -> Result<Vec<u8>> {
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

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static SHARED_HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn shared_http() -> Result<reqwest::Client> {
    if let Some(c) = SHARED_HTTP.get() {
        return Ok(c.clone());
    }
    let built = reqwest::Client::builder()
        .gzip(true)
        .pool_max_idle_per_host(4)
        .build()
        .map_err(|e| TransitError::Config(format!("prim reqwest: {e}")))?;
    // Another thread may have won the race; either client is equivalent.
    Ok(SHARED_HTTP.get_or_init(|| built).clone())
}

/// Time slot (start instant) reserved by the most recent PRIM request.
static LAST_PRIM_AT: Mutex<Option<Instant>> = Mutex::new(None);
const PRIM_MIN_GAP_MS: u64 = 700;

/// Compute the next free pacer slot given the last reserved one.
fn next_prim_slot(prev: Option<Instant>, now: Instant) -> Instant {
    match prev {
        Some(p) => (p + Duration::from_millis(PRIM_MIN_GAP_MS)).max(now),
        None => now,
    }
}

/// Reserve the next pacer slot (≥ 700ms after the previously reserved one) and
/// sleep until it. With `max_wait`, returns `false` without reserving when the
/// slot is further away than that.
async fn pace_prim_request(max_wait: Option<Duration>) -> bool {
    let wait = {
        let mut guard = LAST_PRIM_AT.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let slot = next_prim_slot(*guard, now);
        let wait = slot.saturating_duration_since(now);
        if let Some(max) = max_wait {
            if wait > max {
                return false;
            }
        }
        *guard = Some(slot);
        wait
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
    true
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

    #[test]
    fn pacer_slots_stack_instead_of_collapsing() {
        let now = Instant::now();
        let gap = Duration::from_millis(PRIM_MIN_GAP_MS);
        assert_eq!(next_prim_slot(None, now), now);
        // Previous slot already reserved in the future → next one is a full gap later.
        let s1 = now + gap;
        assert_eq!(next_prim_slot(Some(s1), now), s1 + gap);
        // Old slot long past → now.
        let old = now.checked_sub(Duration::from_secs(10)).unwrap_or(now);
        assert!(next_prim_slot(Some(old), now) >= now);
    }
}
