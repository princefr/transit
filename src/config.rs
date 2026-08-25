use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Result, TransitError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub runtime: RuntimeConfig,
    pub routing: RoutingConfig,
    pub graphql: GraphqlConfig,
    pub feeds: Vec<FeedConfig>,
    /// PRIM Île-de-France Mobilités Navitia realtime (disruptions + vehicles).
    #[serde(default)]
    pub prim: PrimConfig,
    /// Base Adresse Nationale (local index for address autocomplete).
    #[serde(default)]
    pub ban: BanConfigSection,
    /// Optional Redis cache for hot GraphQL paths (disabled by default).
    #[serde(default)]
    pub redis: RedisConfig,
    /// GBFS bike/scooter-share feeds (disabled by default).
    #[serde(default)]
    pub gbfs: GbfsConfig,
    /// API keys + rate limiting (open access when disabled / no keys).
    #[serde(default)]
    pub api: crate::api::gate::ApiConfig,
}

/// GBFS auto-discovery manifest feeds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GbfsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_gbfs_interval")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub feeds: Vec<GbfsFeedConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GbfsFeedConfig {
    pub id: String,
    /// GBFS auto-discovery manifest URL (`gbfs.json`).
    pub url: String,
}

fn default_gbfs_interval() -> u64 {
    60
}

/// Local BAN index (`crates/ban-search`) — real street/address autocomplete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanConfigSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Directory with `index.bin` and optional `csv/` (default `{data_dir}/ban`).
    #[serde(default)]
    pub data_dir: PathBuf,
    /// Département codes; `["all"]` or empty list = whole of France (default).
    #[serde(default)]
    pub departments: Vec<String>,
    /// Download + build the index in the background at startup when
    /// `index.bin` is missing (default true).
    #[serde(default = "default_true")]
    pub auto_download: bool,
}

impl Default for BanConfigSection {
    fn default() -> Self {
        Self {
            enabled: true,
            data_dir: PathBuf::new(),
            departments: Vec::new(),
            auto_download: true,
        }
    }
}

impl BanConfigSection {
    /// Resolve data dir relative to runtime data_dir when empty.
    pub fn resolved_data_dir(&self, runtime_data_dir: &Path) -> PathBuf {
        if self.data_dir.as_os_str().is_empty() {
            runtime_data_dir.join("ban")
        } else {
            self.data_dir.clone()
        }
    }

    /// Department codes for ban-search, where an empty list means "all".
    /// `["all"]` (or case variants) is normalized to the empty list.
    pub fn resolved_departments(&self) -> Vec<String> {
        let all = self
            .departments
            .iter()
            .any(|d| d.eq_ignore_ascii_case("all"));
        if all || self.departments.is_empty() {
            Vec::new()
        } else {
            self.departments.clone()
        }
    }
}

/// PRIM marketplace **SIRI Lite** continuous pollers (independent of static idfm GTFS).
///
/// Official quotas (shared per API key / contract):
/// - stop-monitoring (unitaire): **1_000_000 / day**
/// - estimated-timetable (globale): **1_000 / day**
/// - general-message: **20_000 / day**
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimConfig {
    /// Master switch; still requires a non-empty API key env to actually spawn.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_prim_base")]
    pub base_url: String,
    /// Env var name holding the `apikey` header value.
    #[serde(default = "default_prim_key_env")]
    pub api_key_env: String,
    /// Feed id written into the realtime overlay (`vehicles(feedId: …)`).
    #[serde(default = "default_prim_feed_id")]
    pub feed_id: String,
    /// Legacy alias → general-message interval when `gm_interval_secs` unset in old configs.
    #[serde(default = "default_prim_gm_interval")]
    pub disruptions_interval_secs: u64,
    /// Legacy alias → estimated-timetable interval.
    #[serde(default = "default_prim_et_interval")]
    pub vehicles_interval_secs: u64,
    /// Page size for disruptions list (unused for SIRI path; kept for config compat).
    #[serde(default = "default_prim_disruptions_count")]
    pub disruptions_page_count: u32,
    /// Page size for vehicle_positions (unused for SIRI path).
    #[serde(default = "default_prim_vehicles_count")]
    pub vehicles_page_count: u32,
    /// Max pagination pages for vehicle_positions per cycle (unused for SIRI path).
    #[serde(default = "default_prim_vehicles_max_pages")]
    pub vehicles_max_pages: u32,
    /// SIRI Estimated Timetable poll interval (seconds between LineRef samples).
    /// Budget: 1_000 calls/day → floor ~90s if polling 24/7 with one LineRef each time.
    #[serde(default = "default_prim_et_interval")]
    pub et_interval_secs: u64,
    /// Soft daily cap for ET (default 900, leave headroom under 1_000).
    #[serde(default = "default_prim_et_daily_budget")]
    pub et_daily_budget: u32,
    /// Max LineRefs fetched per ET interval (budget-paced; default 3).
    /// Actual count = min(this, remaining_budget / ticks_left_in_UTC_day).
    #[serde(default = "default_prim_et_lines_per_tick")]
    pub et_lines_per_tick: u32,
    /// SIRI General Message interval (seconds). Budget 20_000/day.
    #[serde(default = "default_prim_gm_interval")]
    pub gm_interval_secs: u64,
    #[serde(default = "default_prim_gm_daily_budget")]
    pub gm_daily_budget: u32,
    /// SIRI Stop Monitoring warm-poll interval (seconds between cycles).
    /// Budget 1_000_000/day — on-demand GraphQL also counts against this.
    /// Each cycle may hit up to `sm_max_stops_per_cycle` MonitoringRefs.
    #[serde(default = "default_prim_sm_interval")]
    pub sm_interval_secs: u64,
    #[serde(default = "default_prim_sm_daily_budget")]
    pub sm_daily_budget: u32,
    /// Max distinct MonitoringRefs warm-polled per SM cycle (budget-paced).
    #[serde(default = "default_prim_sm_max_stops")]
    pub sm_max_stops_per_cycle: u32,
    /// Optional LineRef list (STIF:Line::C…:). Empty = built-in metro/RER/tram/bus sample.
    #[serde(default)]
    pub line_refs: Vec<String>,
    /// Optional seed MonitoringRefs for continuous SM warm cache.
    #[serde(default)]
    pub seed_monitoring_refs: Vec<String>,
    #[serde(default = "default_ua")]
    pub user_agent: String,
}

fn default_true() -> bool {
    true
}
fn default_prim_base() -> String {
    "https://prim.iledefrance-mobilites.fr".into()
}
fn default_prim_key_env() -> String {
    "IDFM_PRIM_API_KEY".into()
}
fn default_prim_feed_id() -> String {
    "idfm".into()
}
fn default_prim_et_interval() -> u64 {
    120 // 1 LineRef/tick — PRIM short-term rate limit
}
fn default_prim_gm_interval() -> u64 {
    90
}
fn default_prim_sm_interval() -> u64 {
    60 // 1 MonitoringRef/min hard cap in poller
}
fn default_prim_et_daily_budget() -> u32 {
    700
}
fn default_prim_et_lines_per_tick() -> u32 {
    1
}
fn default_prim_gm_daily_budget() -> u32 {
    10_000
}
fn default_prim_sm_daily_budget() -> u32 {
    20_000 // soft; short-term 429 is the real limiter
}
fn default_prim_sm_max_stops() -> u32 {
    1
}
fn default_prim_disruptions_count() -> u32 {
    100
}
fn default_prim_vehicles_count() -> u32 {
    100
}
fn default_prim_vehicles_max_pages() -> u32 {
    3
}

impl Default for PrimConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: default_prim_base(),
            api_key_env: default_prim_key_env(),
            feed_id: default_prim_feed_id(),
            disruptions_interval_secs: default_prim_gm_interval(),
            vehicles_interval_secs: default_prim_et_interval(),
            disruptions_page_count: default_prim_disruptions_count(),
            vehicles_page_count: default_prim_vehicles_count(),
            vehicles_max_pages: default_prim_vehicles_max_pages(),
            et_interval_secs: default_prim_et_interval(),
            et_daily_budget: default_prim_et_daily_budget(),
            et_lines_per_tick: default_prim_et_lines_per_tick(),
            gm_interval_secs: default_prim_gm_interval(),
            gm_daily_budget: default_prim_gm_daily_budget(),
            sm_interval_secs: default_prim_sm_interval(),
            sm_daily_budget: default_prim_sm_daily_budget(),
            sm_max_stops_per_cycle: default_prim_sm_max_stops(),
            line_refs: Vec::new(),
            seed_monitoring_refs: Vec::new(),
            user_agent: default_ua(),
        }
    }
}

impl PrimConfig {
    /// True when enabled and the API key env var is non-empty.
    pub fn should_run(&self) -> bool {
        self.enabled
            && std::env::var(&self.api_key_env)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
    }

    /// Effective ET interval: prefer `et_interval_secs`, fall back to legacy `vehicles_interval_secs`.
    pub fn et_interval(&self) -> Duration {
        let secs = if self.et_interval_secs > 0 {
            self.et_interval_secs
        } else {
            self.vehicles_interval_secs
        };
        Duration::from_secs(secs.max(60))
    }

    /// Effective GM interval.
    pub fn gm_interval(&self) -> Duration {
        let secs = if self.gm_interval_secs > 0 {
            self.gm_interval_secs
        } else {
            self.disruptions_interval_secs
        };
        Duration::from_secs(secs.max(30))
    }

    pub fn sm_interval(&self) -> Duration {
        Duration::from_secs(self.sm_interval_secs.max(15))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub graphql_playground: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub blocking_threads: usize,
    pub max_concurrent_static_builds: usize,
    pub max_outbound_rt_requests: usize,
    pub rt_request_timeout_secs: u64,
    pub static_download_timeout_secs: u64,
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub timezone: String,
    pub default_transfer_s: u32,
    pub max_transfers: u32,
    pub max_results: u32,
    pub raptor_max_rounds: u32,
    pub walk_speed_m_s: f64,
    pub max_walk_meters: u32,
    pub hub_link_radius_m: u32,
    /// Optional OSRM base URL for street-level walk/bike geometry + duration
    /// (e.g. `https://router.project-osrm.org`). Empty disables external routing
    /// and falls back to haversine straight lines. Env: `TRANSIT_OSRM_URL`.
    #[serde(default)]
    pub osrm_url: String,
    /// Bike access speed (m/s) when `accessMode: bike` (~15 km/h).
    #[serde(default = "default_bike_speed")]
    pub bike_speed_m_s: f64,
    /// Max bike distance for first/last mile (m).
    #[serde(default = "default_max_bike")]
    pub max_bike_meters: u32,
}

fn default_bike_speed() -> f64 {
    4.2
}
fn default_max_bike() -> u32 {
    5000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphqlConfig {
    pub max_complexity: usize,
    pub max_depth: usize,
    pub itinerary_timeout_secs: u64,
}

/// Optional Redis cache. When `enabled = false` (default) no connection is made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_redis_url")]
    pub url: String,
    /// Stop / address / place autocomplete (static per epoch).
    #[serde(default = "default_redis_search_ttl")]
    pub search_ttl_secs: u64,
    /// Map vehicle snapshots (bbox / near queries).
    #[serde(default = "default_redis_vehicles_ttl")]
    pub vehicles_ttl_secs: u64,
    /// RAPTOR itinerary core results (keyed by epoch + RT version).
    #[serde(default = "default_redis_itinerary_ttl")]
    pub itinerary_ttl_secs: u64,
    /// `/health` JSON for multi-instance load balancers.
    #[serde(default = "default_redis_health_ttl")]
    pub health_ttl_secs: u64,
}

fn default_redis_url() -> String {
    "redis://127.0.0.1:6379".into()
}
fn default_redis_search_ttl() -> u64 {
    300
}
fn default_redis_vehicles_ttl() -> u64 {
    10
}
fn default_redis_itinerary_ttl() -> u64 {
    45
}
fn default_redis_health_ttl() -> u64 {
    5
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: default_redis_url(),
            search_ttl_secs: default_redis_search_ttl(),
            vehicles_ttl_secs: default_redis_vehicles_ttl(),
            itinerary_ttl_secs: default_redis_itinerary_ttl(),
            health_ttl_secs: default_redis_health_ttl(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedConfig {
    pub id: String,
    pub enabled: bool,
    pub static_url: String,
    #[serde(default)]
    pub trip_updates_url: Option<String>,
    #[serde(default)]
    pub service_alerts_url: Option<String>,
    #[serde(default)]
    pub vehicle_positions_url: Option<String>,
    #[serde(default = "default_static_check")]
    pub static_check_interval_secs: u64,
    #[serde(default = "default_rt_poll")]
    pub rt_poll_interval_secs: u64,
    #[serde(default = "default_ua")]
    pub user_agent: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// HTTP header name for API auth (e.g. PRIM uses `apikey`).
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Env var holding the secret (e.g. `IDFM_PRIM_API_KEY`). Value is never logged.
    #[serde(default)]
    pub auth_env: Option<String>,
    /// When set, only keep trips whose service is active on at least one day in
    /// `[today, today + N]` (UTC date). Drops inactive stop_times to reduce RAM
    /// for huge feeds (e.g. IDFM). `None` keeps the full schedule.
    #[serde(default)]
    pub static_horizon_days: Option<u32>,
}

impl FeedConfig {
    /// Resolve optional auth header (name, value) from `auth_header` + `auth_env`.
    pub fn auth_header_pair(&self) -> Option<(String, String)> {
        let name = self.auth_header.as_ref()?.trim();
        let env_name = self.auth_env.as_ref()?.trim();
        if name.is_empty() || env_name.is_empty() {
            return None;
        }
        let value = std::env::var(env_name).ok()?.trim().to_string();
        if value.is_empty() {
            return None;
        }
        Some((name.to_string(), value))
    }
}

fn default_static_check() -> u64 {
    1800
}
fn default_rt_poll() -> u64 {
    60
}
fn default_ua() -> String {
    "transit-rs/0.1".into()
}
fn default_priority() -> i32 {
    100
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: "0.0.0.0:8080".into(),
                graphql_playground: true,
            },
            runtime: RuntimeConfig {
                blocking_threads: 4,
                max_concurrent_static_builds: 1,
                max_outbound_rt_requests: 8,
                rt_request_timeout_secs: 15,
                static_download_timeout_secs: 600,
                data_dir: PathBuf::from("./data"),
            },
            routing: RoutingConfig {
                timezone: "Europe/Paris".into(),
                default_transfer_s: 180,
                max_transfers: 6,
                max_results: 10,
                raptor_max_rounds: 8,
                walk_speed_m_s: 1.2,
                max_walk_meters: 2000,
                hub_link_radius_m: 250,
                osrm_url: String::new(),
                bike_speed_m_s: 4.2,
                max_bike_meters: 5000,
            },
            graphql: GraphqlConfig {
                max_complexity: 300,
                max_depth: 15,
                itinerary_timeout_secs: 90,
            },
            feeds: vec![FeedConfig {
                id: "sncf".into(),
                enabled: true,
                static_url: "https://eu.ftp.opendatasoft.com/sncf/plandata/Export_OpenData_SNCF_GTFS_NewTripId.zip".into(),
                trip_updates_url: Some(
                    "https://proxy.transport.data.gouv.fr/resource/sncf-gtfs-rt-trip-updates"
                        .into(),
                ),
                service_alerts_url: Some(
                    "https://proxy.transport.data.gouv.fr/resource/sncf-gtfs-rt-service-alerts"
                        .into(),
                ),
                vehicle_positions_url: None,
                static_check_interval_secs: 1800,
                rt_poll_interval_secs: 60,
                user_agent: "transit-rs/0.1 (France multimodal GTFS server)".into(),
                priority: 100,
                auth_header: None,
                auth_env: None,
                static_horizon_days: None,
            }],
            prim: PrimConfig::default(),
            ban: BanConfigSection::default(),
            redis: RedisConfig::default(),
            api: Default::default(),
            gbfs: GbfsConfig {
                enabled: false,
                poll_interval_secs: default_gbfs_interval(),
                feeds: Vec::new(),
            },
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut figment = Figment::new().merge(Serialized::defaults(Config::default()));

        let candidates = [
            Path::new("config/default.toml"),
            Path::new("/etc/transit/config.toml"),
        ];
        for path in candidates {
            if path.exists() {
                figment = figment.merge(Toml::file(path));
            }
        }
        if let Ok(extra) = std::env::var("TRANSIT_CONFIG") {
            figment = figment.merge(Toml::file(extra));
        }

        figment = figment.merge(Env::prefixed("TRANSIT_").split("__"));

        let mut cfg: Config = figment
            .extract()
            .map_err(|e| TransitError::Config(e.to_string()))?;

        // Convenience: `TRANSIT_OSRM_URL=https://router.project-osrm.org` (also
        // supports nested `TRANSIT_ROUTING__OSRM_URL` via figment above).
        if let Ok(url) = std::env::var("TRANSIT_OSRM_URL") {
            let url = url.trim().to_string();
            if !url.is_empty() {
                cfg.routing.osrm_url = url;
            }
        }

        Ok(cfg)
    }

    pub fn enabled_feeds(&self) -> impl Iterator<Item = &FeedConfig> {
        self.feeds.iter().filter(|f| f.enabled)
    }
}
