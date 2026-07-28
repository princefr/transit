use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedStop {
    pub id: String,
    pub feed_id: String,
    pub name: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub is_station: bool,
    pub platform_code: Option<String>,
    pub code: Option<String>,
    pub description: Option<String>,
    pub wheelchair: i32,
    pub zone_id: Option<String>,
    pub url: Option<String>,
    pub timezone: Option<String>,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedStopConnection {
    pub nodes: Vec<CachedStop>,
    pub total_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedAddressSuggestion {
    pub id: String,
    pub label: String,
    pub number: Option<String>,
    pub rep: Option<String>,
    pub street: String,
    pub postcode: String,
    pub city: String,
    pub lat: f64,
    pub lon: f64,
    pub score: f64,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPlaceSuggestion {
    pub kind: String,
    pub id: String,
    pub label: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub stop_id: Option<String>,
    pub is_station: Option<bool>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub street: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedVehicle {
    pub trip_id: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub bearing: Option<f64>,
    pub speed: Option<f64>,
    pub updated_at: DateTime<Utc>,
    pub current_stop_id: Option<String>,
    pub label: Option<String>,
    pub vehicle_id: Option<String>,
    pub license_plate: Option<String>,
    pub occupancy: Option<String>,
    pub current_status: Option<String>,
    pub current_stop_sequence: Option<i32>,
    pub congestion: Option<String>,
    pub occupancy_percentage: Option<i32>,
    pub feed_id: Option<String>,
    pub route_short_name: Option<String>,
    pub route_long_name: Option<String>,
    pub trip_short_name: Option<String>,
    pub headsign: Option<String>,
    pub mode: Option<String>,
    pub route_color: Option<String>,
    pub delay_seconds: Option<i32>,
    pub canceled: bool,
    pub display_label: String,
    pub position_source: String,
    pub is_estimated: bool,
}
