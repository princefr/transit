//! Unified realtime ingest: **GTFS-RT** or **SIRI** → [`FeedRtState`].
//!
//! # Formats
//!
//! | Source | Detection | Produces |
//! |--------|-----------|----------|
//! | GTFS-RT protobuf | binary / `decode_feed_message` | trips, vehicles, alerts |
//! | SIRI Estimated Timetable (JSON, PRIM) | `Siri.ServiceDelivery.EstimatedTimetableDelivery` | trips (expected times / delays) |
//! | SIRI Stop Monitoring (JSON) | `StopMonitoringDelivery` | trips + optional vehicle stubs |
//! | SIRI General Message (JSON) | `GeneralMessageDelivery` | alerts / disruptions |
//!
//! # Non-goals
//!
//! - **Navitia** JSON (`/v2/navitia/*`) — not handled here; use raw GTFS-RT/SIRI only.
//!
//! # Usage
//!
//! ```ignore
//! use transit::rt::adapter::{ingest_realtime, RealtimeFormat};
//! let delta = ingest_realtime("idfm", &bytes, RealtimeFormat::Auto)?;
//! // merge delta into overlay
//! ```

mod detect;
mod gtfs_rt;
mod siri;

pub use detect::{detect_format, RealtimeFormat};
pub use gtfs_rt::ingest_gtfs_rt;
pub use siri::{
    ingest_siri_estimated_timetable, ingest_siri_general_message, ingest_siri_stop_monitoring,
};

use crate::error::{Result, TransitError};
use crate::rt::overlay::FeedRtState;

/// Kind of realtime payload after ingest (for metrics / FeedEvent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeKind {
    TripUpdates,
    VehiclePositions,
    ServiceAlerts,
    /// Mixed SIRI or multi-entity GTFS-RT snapshot.
    Mixed,
}

/// Result of parsing one RT blob into our internal model.
#[derive(Debug, Clone)]
pub struct RealtimeDelta {
    pub feed_id: String,
    pub format: RealtimeFormat,
    pub kind: RealtimeKind,
    pub state: FeedRtState,
}

/// Ingest bytes as GTFS-RT or SIRI into a [`FeedRtState`].
///
/// - [`RealtimeFormat::Auto`]: sniff protobuf vs JSON SIRI.
/// - Explicit variants force one parser.
pub fn ingest_realtime(
    feed_id: &str,
    bytes: &[u8],
    format: RealtimeFormat,
) -> Result<RealtimeDelta> {
    let format = match format {
        RealtimeFormat::Auto => detect_format(bytes),
        other => other,
    };

    match format {
        RealtimeFormat::GtfsRt => {
            let (kind, state) = ingest_gtfs_rt(feed_id, bytes)?;
            Ok(RealtimeDelta {
                feed_id: feed_id.to_string(),
                format: RealtimeFormat::GtfsRt,
                kind,
                state,
            })
        }
        RealtimeFormat::SiriEstimatedTimetable => {
            let state = ingest_siri_estimated_timetable(feed_id, bytes)?;
            Ok(RealtimeDelta {
                feed_id: feed_id.to_string(),
                format: RealtimeFormat::SiriEstimatedTimetable,
                kind: RealtimeKind::TripUpdates,
                state,
            })
        }
        RealtimeFormat::SiriStopMonitoring => {
            let state = ingest_siri_stop_monitoring(feed_id, bytes)?;
            Ok(RealtimeDelta {
                feed_id: feed_id.to_string(),
                format: RealtimeFormat::SiriStopMonitoring,
                kind: RealtimeKind::Mixed,
                state,
            })
        }
        RealtimeFormat::SiriGeneralMessage => {
            let state = ingest_siri_general_message(feed_id, bytes)?;
            Ok(RealtimeDelta {
                feed_id: feed_id.to_string(),
                format: RealtimeFormat::SiriGeneralMessage,
                kind: RealtimeKind::ServiceAlerts,
                state,
            })
        }
        RealtimeFormat::Auto => Err(TransitError::Parse(
            "realtime format auto-detect failed (empty or unknown payload)".into(),
        )),
    }
}

/// Convenience: merge a delta into an existing feed state (full snapshot replace per kind).
pub fn merge_delta_into(target: &mut FeedRtState, delta: &RealtimeDelta) {
    match delta.kind {
        RealtimeKind::TripUpdates => {
            target.trips = delta.state.trips.clone();
            target.trip_updates_fetched_at = delta.state.trip_updates_fetched_at;
            target.trip_update_count = delta.state.trip_update_count;
            target.header_timestamp = delta.state.header_timestamp.or(target.header_timestamp);
        }
        RealtimeKind::VehiclePositions => {
            target.vehicles = delta.state.vehicles.clone();
            target.vehicles_fetched_at = delta.state.vehicles_fetched_at;
            target.vehicle_count = delta.state.vehicle_count;
        }
        RealtimeKind::ServiceAlerts => {
            target.alerts = delta.state.alerts.clone();
            target.alerts_fetched_at = delta.state.alerts_fetched_at;
            target.alert_count = delta.state.alert_count;
        }
        RealtimeKind::Mixed => {
            if !delta.state.trips.is_empty() {
                for (k, v) in &delta.state.trips {
                    target.trips.insert(k.clone(), v.clone());
                }
                target.trip_update_count = target.trips.len();
                target.trip_updates_fetched_at = delta
                    .state
                    .trip_updates_fetched_at
                    .or(target.trip_updates_fetched_at);
            }
            if !delta.state.vehicles.is_empty() {
                for (k, v) in &delta.state.vehicles {
                    target.vehicles.insert(k.clone(), v.clone());
                }
                target.vehicle_count = target.vehicles.len();
                target.vehicles_fetched_at = delta
                    .state
                    .vehicles_fetched_at
                    .or(target.vehicles_fetched_at);
            }
            if !delta.state.alerts.is_empty() {
                target.alerts = delta.state.alerts.clone();
                target.alert_count = target.alerts.len();
                target.alerts_fetched_at = delta
                    .state
                    .alerts_fetched_at
                    .or(target.alerts_fetched_at);
            }
        }
    }
    target.feed_id = delta.feed_id.clone();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_detect_empty() {
        assert_eq!(detect_format(b""), RealtimeFormat::Auto);
    }
}
