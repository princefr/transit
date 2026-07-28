//! Realtime layer: GTFS-RT + SIRI adapters → overlay used by routing / GraphQL.
//!
//! ## Preferred ingest path
//!
//! Use [`adapter::ingest_realtime`] so both **GTFS-RT protobuf** and **SIRI JSON**
//! (Estimated Timetable, Stop Monitoring, General Message) map to the same
//! [`FeedRtState`]. Do **not** rely on Navitia JSON for core vehicle/delay data.
//!
//! ## Public surface for journey enrichment / GraphQL
//!
//! - [`adapter`] — unified GTFS-RT / SIRI → [`RealtimeDelta`]
//! - [`RealtimeOverlay`] — multi-feed snapshot; `get_trip`, `apply_arrival` /
//!   `apply_departure`, `is_trip_canceled`, `alerts_for_*`, `trips_canceled`
//! - [`FeedRtState`] — per-feed maps; dual-key trip insert
//! - [`decode`] — low-level GTFS-RT prost helpers
//! - [`estimate`] — geolocation from SIRI SM/ET + GTFS geometry when GPS missing

pub mod adapter;
pub mod alert_text;
pub mod decode;
pub mod estimate;
pub mod overlay;

pub use adapter::{
    detect_format, ingest_realtime, merge_delta_into, RealtimeDelta, RealtimeFormat, RealtimeKind,
};
pub use decode::{
    apply_service_alerts, apply_trip_updates, apply_vehicle_positions, decode_feed_message,
};
pub use estimate::{estimate_from_trip_updates, estimate_vehicle_pos, EstimatedVehicle};
pub use overlay::{
    AlertRt, FeedRtState, RealtimeOverlay, RtStats, StopRealtime, StopTimeRt, TripRt, VehiclePos,
};
