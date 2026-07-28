//! GTFS-RT protobuf → [`FeedRtState`] via existing prost decode.

use super::RealtimeKind;
use crate::error::{Result, TransitError};
use crate::rt::decode::{
    apply_service_alerts, apply_trip_updates, apply_vehicle_positions, decode_feed_message,
};
use crate::rt::overlay::FeedRtState;

/// Ingest a GTFS-RT `FeedMessage` blob.
///
/// Detects entity types present and returns the primary [`RealtimeKind`].
pub fn ingest_gtfs_rt(feed_id: &str, bytes: &[u8]) -> Result<(RealtimeKind, FeedRtState)> {
    let msg = decode_feed_message(bytes)
        .map_err(|e| TransitError::Parse(format!("gtfs-rt decode: {e}")))?;

    let mut has_tu = false;
    let mut has_vp = false;
    let mut has_sa = false;
    for e in &msg.entity {
        if e.trip_update.is_some() {
            has_tu = true;
        }
        if e.vehicle.is_some() {
            has_vp = true;
        }
        if e.alert.is_some() {
            has_sa = true;
        }
    }

    let mut state = FeedRtState::new(feed_id);
    // Each apply_* only mutates its own maps (FULL_DATASET clears that map only).
    if has_tu || (!has_vp && !has_sa) {
        apply_trip_updates(feed_id, bytes, &mut state);
    }
    if has_vp {
        apply_vehicle_positions(feed_id, bytes, &mut state);
    }
    if has_sa {
        apply_service_alerts(feed_id, bytes, &mut state);
    }

    let kind = match (has_tu, has_vp, has_sa) {
        (true, false, false) => RealtimeKind::TripUpdates,
        (false, true, false) => RealtimeKind::VehiclePositions,
        (false, false, true) => RealtimeKind::ServiceAlerts,
        _ => RealtimeKind::Mixed,
    };
    Ok((kind, state))
}
