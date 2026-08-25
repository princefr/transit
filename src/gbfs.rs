//! GBFS (bike-share / scooter-share) ingestion.
//!
//! Polls one or more GBFS auto-discovery manifests, resolves
//! `station_information` + `station_status` URLs, and keeps a lightweight
//! snapshot of station availability for GraphQL + map display.
//!
//! MVP scope: data + API. Routing integration (rental as access/egress mode)
//! is a follow-up.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct GbfsStation {
    pub feed_id: String,
    pub station_id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Total docking capacity when reported.
    pub capacity: Option<u32>,
    pub bikes_available: Option<u32>,
    pub docks_available: Option<u32>,
    pub is_renting: bool,
}

#[derive(Debug, Clone, Default)]
pub struct GbfsSnapshot {
    pub stations: Vec<GbfsStation>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub type SharedGbfs = Arc<arc_swap::ArcSwap<GbfsSnapshot>>;

pub fn empty_snapshot() -> GbfsSnapshot {
    GbfsSnapshot::default()
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(rename = "data")]
    data: ManifestData,
}

#[derive(Debug, Deserialize)]
struct ManifestData {
    #[serde(default)]
    feeds: Vec<ManifestFeed>,
    // GBFS v3 nests per language
    #[serde(default)]
    fr: Option<LangFeeds>,
    #[serde(default)]
    en: Option<LangFeeds>,
}

#[derive(Debug, Deserialize)]
struct LangFeeds {
    #[serde(default)]
    feeds: Vec<ManifestFeed>,
}

#[derive(Debug, Deserialize)]
struct ManifestFeed {
    name: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct StationInfoFeed {
    #[serde(rename = "data")]
    data: StationData,
}

#[derive(Debug, Deserialize)]
struct StationData {
    #[serde(default)]
    stations: Vec<RawStation>,
}

#[derive(Debug, Deserialize)]
struct RawStation {
    station_id: String,
    #[serde(default)]
    name: Option<String>,
    lat: f64,
    lon: f64,
    #[serde(default)]
    capacity: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct StationStatusFeed {
    #[serde(rename = "data")]
    data: StatusData,
}

#[derive(Debug, Deserialize)]
struct StatusData {
    #[serde(default)]
    stations: Vec<RawStatus>,
}

#[derive(Debug, Deserialize)]
struct RawStatus {
    station_id: String,
    #[serde(default)]
    num_bikes_available: Option<u32>,
    #[serde(default)]
    num_docks_available: Option<u32>,
    /// May be bool (GBFS 2.x) or int (0/1) — accept both via Value.
    #[serde(default)]
    is_renting: Option<serde_json::Value>,
}

fn renting_flag(v: &Option<serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|x| x != 0).unwrap_or(true),
        _ => true,
    }
}

/// One poll cycle over a GBFS auto-discovery manifest.
pub async fn refresh_feed(
    client: &reqwest::Client,
    feed_id: &str,
    manifest_url: &str,
) -> Result<Vec<GbfsStation>, String> {
    let resp = client
        .get(manifest_url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("manifest fetch: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("manifest HTTP {}", resp.status()));
    }
    let manifest: Manifest = resp.json().await.map_err(|e| format!("manifest json: {e}"))?;

    let pick = |name_prefix: &str| -> Option<String> {
        let mut all: Vec<&ManifestFeed> = manifest.data.feeds.iter().collect();
        if let Some(fr) = &manifest.data.fr {
            all.extend(fr.feeds.iter());
        }
        if let Some(en) = &manifest.data.en {
            all.extend(en.feeds.iter());
        }
        all.into_iter()
            .find(|f| f.name.contains(name_prefix))
            .map(|f| f.url.clone())
    };
    let info_url = pick("station_information")
        .ok_or_else(|| "manifest missing station_information".to_string())?;
    let status_url = pick("station_status")
        .ok_or_else(|| "manifest missing station_status".to_string())?;

    let info: StationInfoFeed = get_json(client, &info_url).await?;
    let status_res: Result<StationStatusFeed, String> = get_json(client, &status_url).await;
    let status_map: HashMap<String, RawStatus> = match status_res {
        Ok(s) => s
            .data
            .stations
            .into_iter()
            .map(|s| (s.station_id.clone(), s))
            .collect(),
        Err(_) => HashMap::new(),
    };

    let now_renting_default = true;
    Ok(info
        .data
        .stations
        .into_iter()
        .map(|s| {
            let st = status_map.get(&s.station_id);
            let station_id = s.station_id;
            GbfsStation {
                feed_id: feed_id.to_string(),
                station_id: station_id.clone(),
                name: s.name.unwrap_or(station_id),
                lat: s.lat,
                lon: s.lon,
                capacity: s.capacity,
                bikes_available: st.and_then(|x| x.num_bikes_available),
                docks_available: st.and_then(|x| x.num_docks_available),
                is_renting: st
                    .map(|x| renting_flag(&x.is_renting))
                    .unwrap_or(now_renting_default),
            }
        })
        .collect())
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("fetch {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("json {url}: {e}"))
}

/// Background poller loop for all configured GBFS feeds.
pub fn spawn_gbfs_poller_self(
    config: &crate::config::Config,
    slot: SharedGbfs,
) {
    let feeds = config.gbfs.feeds.clone();
    if feeds.is_empty() || !config.gbfs.enabled {
        return;
    }
    let interval = Duration::from_secs(config.gbfs.poll_interval_secs.max(30));
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .user_agent("transit-rs/0.1")
            .build()
            .expect("gbfs client");
        loop {
            let mut all: Vec<GbfsStation> = Vec::new();
            for f in &feeds {
                match refresh_feed(&client, &f.id, &f.url).await {
                    Ok(stations) => all.extend(stations),
                    Err(e) => tracing::warn!(feed = %f.id, error = %e, "GBFS refresh failed"),
                }
            }
            if !all.is_empty() {
                let snap = GbfsSnapshot {
                    updated_at: Some(chrono::Utc::now()),
                    stations: all,
                };
                slot.store(Arc::new(snap));
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Track freshness without storing Instant in the snapshot.
#[derive(Default)]
pub struct LastRefresh(pub std::sync::Mutex<Option<Instant>>);
