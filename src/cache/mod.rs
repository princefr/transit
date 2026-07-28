//! Optional Redis cache for hot GraphQL paths and health.
//!
//! Disabled by default (`[redis] enabled = false`). When Redis is unreachable at
//! startup the server continues without caching.

mod dto;

use crate::config::RedisConfig;
use dto::{
    CachedAddressSuggestion, CachedPlaceSuggestion, CachedStop, CachedStopConnection,
    CachedVehicle,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::api::graphql::types::{
    AddressSuggestion, BBoxInput, GqlStop, ItineraryInput, Mode, NearInput, PlaceSuggestion,
    StopConnection, VehiclePosition,
};
use crate::routing::journey::ItineraryResult as RoutingItineraryResult;

const KEY_PREFIX: &str = "transit:v1";

#[derive(Clone)]
pub struct TransitCache {
    inner: Arc<TransitCacheInner>,
}

struct TransitCacheInner {
    conn: Option<ConnectionManager>,
    config: RedisConfig,
    connected: AtomicBool,
}

impl TransitCache {
    /// Connect when `[redis] enabled = true`. Returns a no-op cache on failure.
    pub async fn connect(config: RedisConfig) -> Self {
        if !config.enabled {
            info!("redis cache disabled (set redis.enabled = true to enable)");
            return Self::disabled(config);
        }

        let url = config.url.trim();
        if url.is_empty() {
            warn!("redis.enabled but redis.url is empty — cache disabled");
            return Self::disabled(config);
        }

        match redis::Client::open(url) {
            Ok(client) => match ConnectionManager::new(client).await {
                Ok(conn) => {
                    info!(url = %mask_redis_url(url), "redis cache connected");
                    Self {
                        inner: Arc::new(TransitCacheInner {
                            conn: Some(conn),
                            config,
                            connected: AtomicBool::new(true),
                        }),
                    }
                }
                Err(e) => {
                    warn!(error = %e, "redis connection failed — continuing without cache");
                    Self::disabled(config)
                }
            },
            Err(e) => {
                warn!(error = %e, "invalid redis url — continuing without cache");
                Self::disabled(config)
            }
        }
    }

    /// No-op cache for tests and when Redis is disabled.
    pub fn disabled(config: RedisConfig) -> Self {
        Self {
            inner: Arc::new(TransitCacheInner {
                conn: None,
                config,
                connected: AtomicBool::new(false),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.config.enabled
    }

    pub fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::Relaxed)
    }

    pub fn config(&self) -> &RedisConfig {
        &self.inner.config
    }

    // --- Search ---

    pub async fn get_stops(&self, key: &str) -> Option<StopConnection> {
        self.get_json(key).await.map(cached_stop_connection_into)
    }

    pub async fn set_stops(&self, key: &str, value: &StopConnection) {
        let cached = cached_stop_connection_from(value);
        self.set_json(key, &cached, self.inner.config.search_ttl_secs)
            .await;
    }

    pub fn stops_key(
        &self,
        epoch_id: &str,
        search: &str,
        near: Option<&NearInput>,
        limit: i32,
    ) -> String {
        let near_part = near.map(|n| {
            format!(
                "near:{:.5}:{:.5}:{:.0}",
                n.lat, n.lon, n.radius_meters
            )
        });
        let digest = hash_parts(&[
            search.trim().to_lowercase(),
            near_part.unwrap_or_default(),
            limit.to_string(),
        ]);
        format!("{KEY_PREFIX}:stops:{epoch_id}:{digest}")
    }

    pub async fn get_addresses(&self, key: &str) -> Option<Vec<AddressSuggestion>> {
        self.get_json::<Vec<CachedAddressSuggestion>>(key)
            .await
            .map(|rows| rows.into_iter().map(cached_address_into).collect())
    }

    pub async fn set_addresses(&self, key: &str, value: &[AddressSuggestion]) {
        let cached: Vec<_> = value.iter().map(cached_address_from).collect();
        self.set_json(key, &cached, self.inner.config.search_ttl_secs)
            .await;
    }

    pub fn addresses_key(&self, search: &str, limit: i32) -> String {
        let digest = hash_parts(&[search.trim().to_lowercase(), limit.to_string()]);
        format!("{KEY_PREFIX}:addr:{digest}")
    }

    pub async fn get_places(&self, key: &str) -> Option<Vec<PlaceSuggestion>> {
        self.get_json::<Vec<CachedPlaceSuggestion>>(key)
            .await
            .map(|rows| rows.into_iter().map(cached_place_into).collect())
    }

    pub async fn set_places(&self, key: &str, value: &[PlaceSuggestion]) {
        let cached: Vec<_> = value.iter().map(cached_place_from).collect();
        self.set_json(key, &cached, self.inner.config.search_ttl_secs)
            .await;
    }

    pub fn places_key(&self, epoch_id: &str, search: &str, limit: i32) -> String {
        let digest = hash_parts(&[search.trim().to_lowercase(), limit.to_string()]);
        format!("{KEY_PREFIX}:places:{epoch_id}:{digest}")
    }

    // --- Vehicles ---

    pub async fn get_vehicles(&self, key: &str) -> Option<Vec<VehiclePosition>> {
        self.get_json::<Vec<CachedVehicle>>(key)
            .await
            .map(|rows| rows.into_iter().map(cached_vehicle_into).collect())
    }

    pub async fn set_vehicles(&self, key: &str, value: &[VehiclePosition]) {
        let cached: Vec<_> = value.iter().map(cached_vehicle_from).collect();
        self.set_json(key, &cached, self.inner.config.vehicles_ttl_secs)
            .await;
    }

    pub fn vehicles_key(
        &self,
        epoch_id: &str,
        rt_version: u64,
        feed_id: Option<&str>,
        trip_id: Option<&str>,
        near: Option<&NearInput>,
        bbox: Option<&BBoxInput>,
        modes: Option<&[Mode]>,
        limit: i32,
    ) -> String {
        let mode_part = modes.map(|ms| {
            let mut names: Vec<String> = ms.iter().map(|m| format!("{m:?}")).collect();
            names.sort();
            names.join(",")
        });
        let near_part = near.map(|n| {
            format!(
                "near:{:.4}:{:.4}:{:.0}",
                n.lat, n.lon, n.radius_meters
            )
        });
        let bbox_part = bbox.map(|b| {
            format!(
                "bbox:{:.4}:{:.4}:{:.4}:{:.4}",
                b.min_lat, b.min_lon, b.max_lat, b.max_lon
            )
        });
        let digest = hash_parts(&[
            feed_id.unwrap_or("").into(),
            trip_id.unwrap_or("").into(),
            near_part.unwrap_or_default(),
            bbox_part.unwrap_or_default(),
            mode_part.unwrap_or_default(),
            limit.to_string(),
        ]);
        format!("{KEY_PREFIX}:veh:{epoch_id}:rt{rt_version}:{digest}")
    }

    // --- Itineraries (routing core result, before GraphQL mapping) ---

    pub async fn get_itinerary(&self, key: &str) -> Option<RoutingItineraryResult> {
        self.get_json(key).await
    }

    pub async fn set_itinerary(&self, key: &str, value: &RoutingItineraryResult) {
        self.set_json(key, value, self.inner.config.itinerary_ttl_secs)
            .await;
    }

    pub fn itinerary_key(
        &self,
        epoch_id: &str,
        rt_version: u64,
        input: &ItineraryInput,
        routing: &crate::config::RoutingConfig,
    ) -> String {
        let dep_bucket = input
            .departure_at
            .or(input.arrive_by)
            .map(|t| t.timestamp() / 60)
            .unwrap_or(0);
        let modes = input.modes.as_ref().map(|ms| {
            let mut names: Vec<String> = ms.iter().map(|m| format!("{m:?}")).collect();
            names.sort();
            names.join(",")
        });
        let digest = hash_parts(&[
            place_key_part(&input.from),
            place_key_part(&input.to),
            dep_bucket.to_string(),
            input.arrive_by.is_some().to_string(),
            input.max_transfers.to_string(),
            input.max_results.to_string(),
            modes.unwrap_or_default(),
            input.max_walk_meters.to_string(),
            input.wheelchair.unwrap_or(false).to_string(),
            input.bike_from.unwrap_or(false).to_string(),
            input.bike_to.unwrap_or(false).to_string(),
            routing.timezone.clone(),
            routing.max_transfers.to_string(),
            routing.raptor_max_rounds.to_string(),
            routing.osrm_url.trim().to_string(),
        ]);
        format!("{KEY_PREFIX}:itin:{epoch_id}:rt{rt_version}:{digest}")
    }

    // --- Health ---

    pub async fn get_health(&self, key: &str) -> Option<serde_json::Value> {
        self.get_json(key).await
    }

    pub async fn set_health(&self, key: &str, value: &serde_json::Value) {
        self.set_json(key, value, self.inner.config.health_ttl_secs)
            .await;
    }

    pub fn health_key(&self, epoch_id: &str, rt_version: u64) -> String {
        format!("{KEY_PREFIX}:health:{epoch_id}:rt{rt_version}")
    }

    // --- Invalidation ---

    /// Best-effort flush of keys for a replaced static epoch (optional hygiene).
    pub async fn invalidate_epoch(&self, epoch_id: &str) {
        let Some(conn) = &self.inner.conn else {
            return;
        };
        let pattern = format!("{KEY_PREFIX}:*:{epoch_id}:*");
        let mut conn = conn.clone();
        if let Err(e) = scan_delete_pattern(&mut conn, &pattern).await {
            debug!(error = %e, epoch_id, "redis epoch invalidation skipped");
        }
    }

    // --- Low-level ---

    async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let conn = self.inner.conn.as_ref()?;
        let mut conn = conn.clone();
        let raw: Option<String> = match conn.get(key).await {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, key, "redis get failed");
                self.inner.connected.store(false, Ordering::Relaxed);
                return None;
            }
        };
        self.inner.connected.store(true, Ordering::Relaxed);
        let Some(raw) = raw else {
            return None;
        };
        match serde_json::from_str(&raw) {
            Ok(v) => {
                debug!(key, "redis cache hit");
                Some(v)
            }
            Err(e) => {
                debug!(error = %e, key, "redis cache deserialize failed");
                None
            }
        }
    }

    async fn set_json<T: Serialize>(&self, key: &str, value: &T, ttl_secs: u64) {
        let Some(conn) = &self.inner.conn else {
            return;
        };
        let Ok(raw) = serde_json::to_string(value) else {
            return;
        };
        let ttl = ttl_secs.max(1);
        let mut conn = conn.clone();
        let res: redis::RedisResult<()> = conn.set_ex(key, raw, ttl).await;
        if let Err(e) = res {
            debug!(error = %e, key, "redis set failed");
            self.inner.connected.store(false, Ordering::Relaxed);
        } else {
            self.inner.connected.store(true, Ordering::Relaxed);
        }
    }
}

fn hash_parts(parts: &[String]) -> String {
    let joined = parts.join("|");
    let digest = Sha256::digest(joined.as_bytes());
    hex::encode(&digest[..16])
}

fn place_key_part(p: &crate::api::graphql::types::PlaceInput) -> String {
    format!(
        "s={}|lat={}|lon={}|n={}",
        p.stop_id.as_ref().map(|i| i.as_str()).unwrap_or(""),
        p.lat.map(|v| format!("{v:.5}")).unwrap_or_default(),
        p.lon.map(|v| format!("{v:.5}")).unwrap_or_default(),
        p.name.as_deref().unwrap_or("").trim().to_lowercase()
    )
}

fn mask_redis_url(url: &str) -> String {
    if let Some(at) = url.rfind('@') {
        if let Some(scheme_end) = url.find("://") {
            return format!("{}://***@{}", &url[..scheme_end], &url[at + 1..]);
        }
    }
    url.to_string()
}

async fn scan_delete_pattern(
    conn: &mut ConnectionManager,
    pattern: &str,
) -> redis::RedisResult<()> {
    let mut cursor: u64 = 0;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(200)
            .query_async(conn)
            .await?;
        if !keys.is_empty() {
            let _: () = conn.del(keys).await?;
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    Ok(())
}

// --- DTO conversions ---

fn cached_stop_from(s: &GqlStop) -> CachedStop {
    CachedStop {
        id: s.id.to_string(),
        feed_id: s.feed_id.clone(),
        name: s.name.clone(),
        lat: s.lat,
        lon: s.lon,
        is_station: s.is_station,
        platform_code: s.platform_code.clone(),
        code: s.code.clone(),
        description: s.description.clone(),
        wheelchair: s.wheelchair,
        zone_id: s.zone_id.clone(),
        url: s.url.clone(),
        timezone: s.timezone.clone(),
        parent_id: s.parent_id.clone(),
    }
}

fn cached_stop_into(s: CachedStop) -> GqlStop {
    GqlStop {
        id: async_graphql::ID(s.id),
        feed_id: s.feed_id,
        name: s.name,
        lat: s.lat,
        lon: s.lon,
        is_station: s.is_station,
        platform_code: s.platform_code,
        code: s.code,
        description: s.description,
        wheelchair: s.wheelchair,
        zone_id: s.zone_id,
        url: s.url,
        timezone: s.timezone,
        parent_id: s.parent_id,
    }
}

fn cached_stop_connection_from(c: &StopConnection) -> CachedStopConnection {
    CachedStopConnection {
        nodes: c.nodes.iter().map(cached_stop_from).collect(),
        total_count: c.total_count,
    }
}

fn cached_stop_connection_into(c: CachedStopConnection) -> StopConnection {
    StopConnection {
        nodes: c.nodes.into_iter().map(cached_stop_into).collect(),
        total_count: c.total_count,
    }
}

fn cached_address_from(a: &AddressSuggestion) -> CachedAddressSuggestion {
    CachedAddressSuggestion {
        id: a.id.clone(),
        label: a.label.clone(),
        number: a.number.clone(),
        rep: a.rep.clone(),
        street: a.street.clone(),
        postcode: a.postcode.clone(),
        city: a.city.clone(),
        lat: a.lat,
        lon: a.lon,
        score: a.score,
        kind: a.kind.clone(),
    }
}

fn cached_address_into(a: CachedAddressSuggestion) -> AddressSuggestion {
    AddressSuggestion {
        id: a.id,
        label: a.label,
        number: a.number,
        rep: a.rep,
        street: a.street,
        postcode: a.postcode,
        city: a.city,
        lat: a.lat,
        lon: a.lon,
        score: a.score,
        kind: a.kind,
    }
}

fn cached_place_from(p: &PlaceSuggestion) -> CachedPlaceSuggestion {
    CachedPlaceSuggestion {
        kind: p.kind.clone(),
        id: p.id.clone(),
        label: p.label.clone(),
        lat: p.lat,
        lon: p.lon,
        stop_id: p.stop_id.clone(),
        is_station: p.is_station,
        postcode: p.postcode.clone(),
        city: p.city.clone(),
        street: p.street.clone(),
    }
}

fn cached_place_into(p: CachedPlaceSuggestion) -> PlaceSuggestion {
    PlaceSuggestion {
        kind: p.kind,
        id: p.id,
        label: p.label,
        lat: p.lat,
        lon: p.lon,
        stop_id: p.stop_id,
        is_station: p.is_station,
        postcode: p.postcode,
        city: p.city,
        street: p.street,
    }
}

fn cached_vehicle_from(v: &VehiclePosition) -> CachedVehicle {
    CachedVehicle {
        trip_id: v.trip_id.as_ref().map(|t| t.to_string()),
        lat: v.lat,
        lon: v.lon,
        bearing: v.bearing,
        speed: v.speed,
        updated_at: v.updated_at,
        current_stop_id: v.current_stop_id.clone(),
        label: v.label.clone(),
        vehicle_id: v.vehicle_id.clone(),
        license_plate: v.license_plate.clone(),
        occupancy: v.occupancy.clone(),
        current_status: v.current_status.clone(),
        current_stop_sequence: v.current_stop_sequence,
        congestion: v.congestion.clone(),
        occupancy_percentage: v.occupancy_percentage,
        feed_id: v.feed_id.clone(),
        route_short_name: v.route_short_name.clone(),
        route_long_name: v.route_long_name.clone(),
        trip_short_name: v.trip_short_name.clone(),
        headsign: v.headsign.clone(),
        mode: v.mode.map(|m| format!("{m:?}")),
        route_color: v.route_color.clone(),
        delay_seconds: v.delay_seconds,
        canceled: v.canceled,
        display_label: v.display_label.clone(),
        position_source: v.position_source.clone(),
        is_estimated: v.is_estimated,
    }
}

fn cached_vehicle_into(v: CachedVehicle) -> VehiclePosition {
    VehiclePosition {
        trip_id: v.trip_id.map(async_graphql::ID),
        lat: v.lat,
        lon: v.lon,
        bearing: v.bearing,
        speed: v.speed,
        updated_at: v.updated_at,
        current_stop_id: v.current_stop_id,
        label: v.label,
        vehicle_id: v.vehicle_id,
        license_plate: v.license_plate,
        occupancy: v.occupancy,
        current_status: v.current_status,
        current_stop_sequence: v.current_stop_sequence,
        congestion: v.congestion,
        occupancy_percentage: v.occupancy_percentage,
        feed_id: v.feed_id,
        route_short_name: v.route_short_name,
        route_long_name: v.route_long_name,
        trip_short_name: v.trip_short_name,
        headsign: v.headsign,
        mode: v.mode.as_deref().map(mode_from_debug_str),
        route_color: v.route_color,
        delay_seconds: v.delay_seconds,
        canceled: v.canceled,
        display_label: v.display_label,
        position_source: v.position_source,
        is_estimated: v.is_estimated,
    }
}

fn mode_from_debug_str(s: &str) -> Mode {
    match s {
        "Walk" => Mode::Walk,
        "Rail" => Mode::Rail,
        "Metro" => Mode::Metro,
        "Tram" => Mode::Tram,
        "Bus" => Mode::Bus,
        "Ferry" => Mode::Ferry,
        "Coach" => Mode::Coach,
        _ => Mode::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        let a = hash_parts(&["paris".into(), "15".into()]);
        let b = hash_parts(&["paris".into(), "15".into()]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn itinerary_key_changes_with_epoch() {
        let cache = TransitCache::disabled(RedisConfig::default());
        let input = ItineraryInput {
            from: crate::api::graphql::types::PlaceInput {
                stop_id: Some(async_graphql::ID("idfm:a".into())),
                lat: None,
                lon: None,
                name: None,
            },
            to: crate::api::graphql::types::PlaceInput {
                stop_id: Some(async_graphql::ID("idfm:b".into())),
                lat: None,
                lon: None,
                name: None,
            },
            departure_at: None,
            arrive_by: None,
            max_transfers: 4,
            max_results: 3,
            modes: None,
            max_walk_meters: 2000,
            wheelchair: None,
            bike_from: None,
            bike_to: None,
        };
        let routing = crate::config::RoutingConfig {
            timezone: "Europe/Paris".into(),
            default_transfer_s: 180,
            max_transfers: 6,
            max_results: 10,
            raptor_max_rounds: 8,
            walk_speed_m_s: 1.2,
            max_walk_meters: 2000,
            hub_link_radius_m: 450,
            osrm_url: String::new(),
            bike_speed_m_s: 4.2,
            max_bike_meters: 5000,
        };
        let k1 = cache.itinerary_key("epoch-a", 1, &input, &routing);
        let k2 = cache.itinerary_key("epoch-b", 1, &input, &routing);
        assert_ne!(k1, k2);
    }
}
