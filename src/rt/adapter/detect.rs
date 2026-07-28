//! Sniff payload format (GTFS-RT protobuf vs SIRI JSON).

/// Supported realtime wire formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeFormat {
    /// Sniff from bytes.
    Auto,
    /// GTFS-Realtime protobuf (`FeedMessage`).
    GtfsRt,
    /// SIRI Estimated Timetable (PRIM JSON or XML-like JSON).
    SiriEstimatedTimetable,
    /// SIRI Stop Monitoring.
    SiriStopMonitoring,
    /// SIRI General Message (service alerts / screen messages).
    SiriGeneralMessage,
}

/// Detect format from payload prefix.
///
/// - Protobuf GTFS-RT: not valid UTF-8 JSON, or fails JSON parse as SIRI.
/// - SIRI JSON: contains `Siri` and a delivery key.
pub fn detect_format(bytes: &[u8]) -> RealtimeFormat {
    if bytes.is_empty() {
        return RealtimeFormat::Auto;
    }
    // Skip UTF-8 BOM
    let b = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    };
    let trimmed = trim_start(b);
    if trimmed.first() == Some(&b'{') || trimmed.first() == Some(&b'[') {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(trimmed) {
            return detect_siri_json(&v);
        }
        // Invalid JSON that starts with `{` — fall through to GTFS-RT attempt by caller
        return RealtimeFormat::GtfsRt;
    }
    // XML SIRI (rare on PRIM; usually JSON)
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<Siri") || trimmed.starts_with(b"<siri")
    {
        let s = String::from_utf8_lossy(trimmed);
        if s.contains("EstimatedTimetable") {
            return RealtimeFormat::SiriEstimatedTimetable;
        }
        if s.contains("StopMonitoring") {
            return RealtimeFormat::SiriStopMonitoring;
        }
        if s.contains("GeneralMessage") {
            return RealtimeFormat::SiriGeneralMessage;
        }
    }
    RealtimeFormat::GtfsRt
}

fn trim_start(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    &b[i..]
}

fn detect_siri_json(v: &serde_json::Value) -> RealtimeFormat {
    let siri = v
        .get("Siri")
        .or_else(|| v.get("siri"))
        .unwrap_or(v);
    let sd = siri
        .get("ServiceDelivery")
        .or_else(|| siri.get("serviceDelivery"))
        .unwrap_or(siri);

    if has_key(sd, "EstimatedTimetableDelivery") || has_key(sd, "estimatedTimetableDelivery") {
        return RealtimeFormat::SiriEstimatedTimetable;
    }
    if has_key(sd, "StopMonitoringDelivery") || has_key(sd, "stopMonitoringDelivery") {
        return RealtimeFormat::SiriStopMonitoring;
    }
    if has_key(sd, "GeneralMessageDelivery") || has_key(sd, "generalMessageDelivery") {
        return RealtimeFormat::SiriGeneralMessage;
    }
    if has_key(sd, "VehicleMonitoringDelivery") || has_key(sd, "vehicleMonitoringDelivery") {
        // Treat VM as stop-monitoring-like for vehicle placement
        return RealtimeFormat::SiriStopMonitoring;
    }
    // Nested dump without delivery key but has EstimatedVehicleJourney
    let s = sd.to_string();
    if s.contains("EstimatedVehicleJourney") || s.contains("EstimatedCall") {
        return RealtimeFormat::SiriEstimatedTimetable;
    }
    if s.contains("MonitoredStopVisit") || s.contains("MonitoredVehicleJourney") {
        return RealtimeFormat::SiriStopMonitoring;
    }
    if s.contains("InfoMessage") || s.contains("GeneralMessage") {
        return RealtimeFormat::SiriGeneralMessage;
    }
    RealtimeFormat::GtfsRt
}

fn has_key(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_siri_et() {
        let j = br#"{"Siri":{"ServiceDelivery":{"EstimatedTimetableDelivery":[]}}}"#;
        assert_eq!(detect_format(j), RealtimeFormat::SiriEstimatedTimetable);
    }

    #[test]
    fn detect_siri_sm() {
        let j = br#"{"Siri":{"ServiceDelivery":{"StopMonitoringDelivery":[]}}}"#;
        assert_eq!(detect_format(j), RealtimeFormat::SiriStopMonitoring);
    }

    #[test]
    fn detect_siri_gm() {
        let j = br#"{"Siri":{"ServiceDelivery":{"GeneralMessageDelivery":[]}}}"#;
        assert_eq!(detect_format(j), RealtimeFormat::SiriGeneralMessage);
    }
}
