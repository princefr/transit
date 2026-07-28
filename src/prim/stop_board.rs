//! Live next departures via **SIRI Stop Monitoring** (not Navitia).
//!
//! PRIM: `GET /marketplace/stop-monitoring?MonitoringRef=STIF:StopPoint:Q:…:`

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::client::PrimClient;
use crate::error::{Result, TransitError};
use crate::gtfs::pack::{RouteMode, StopRecord};
use crate::rt::adapter::ingest_siri_stop_monitoring;
use crate::rt::overlay::FeedRtState;

/// One live departure row for GraphQL / UI.
#[derive(Debug, Clone)]
pub struct LiveDeparture {
    pub trip_id: String,
    pub route_short_name: Option<String>,
    pub route_long_name: Option<String>,
    pub trip_short_name: Option<String>,
    pub headsign: Option<String>,
    pub mode: RouteMode,
    pub scheduled_departure: DateTime<Utc>,
    pub realtime_departure: Option<DateTime<Utc>>,
    pub delay_seconds: Option<i32>,
    pub canceled: bool,
    pub platform: Option<String>,
    pub route_color: Option<String>,
    pub source: &'static str,
    pub line_ref: Option<String>,
    pub status: Option<String>,
}

/// Map a GTFS / app stop id to a PRIM SIRI `MonitoringRef`.
///
/// Examples that work:
/// - `STIF:StopPoint:Q:463041:` (already SIRI)
/// - `idfm:STIF:StopPoint:Q:463041:` → strip feed prefix
/// - raw digits `463041` → `STIF:StopPoint:Q:463041:`
///
/// **Does not** invent `StopPoint:Q:{n}` from monomodalStopPlace ids — that burned
/// PRIM quota with 429s (`monomodalStopPlace:43071` ≠ quay 43071).
pub fn monitoring_ref_from_stop_id(stop_id: &str) -> Option<String> {
    let raw = stop_id
        .strip_prefix("idfm:")
        .or_else(|| stop_id.strip_prefix("IDFM:"))
        .unwrap_or(stop_id)
        .trim();
    if raw.is_empty() {
        return None;
    }
    // Never invent StopPoint refs from place shells
    let lower = raw.to_ascii_lowercase();
    if lower.contains("monomodalstopplace")
        || lower.contains("multimodalstopplace")
        || lower.contains("stopplaceentrance")
    {
        return None;
    }
    if raw.contains("StopPoint") || raw.contains("StopArea") || raw.starts_with("STIF:") {
        let mut s = raw.to_string();
        if !s.ends_with(':') {
            s.push(':');
        }
        return Some(s);
    }
    // Bare numeric quay id only (not place ids)
    if raw.chars().all(|c| c.is_ascii_digit()) && raw.len() >= 4 {
        return Some(format!("STIF:StopPoint:Q:{raw}:"));
    }
    // IDFM:12345 style quay
    if let Some(rest) = raw.strip_prefix("IDFM:").or_else(|| raw.strip_prefix("idfm:")) {
        if rest.chars().all(|c| c.is_ascii_digit()) && rest.len() >= 4 {
            return Some(format!("STIF:StopPoint:Q:{rest}:"));
        }
    }
    None
}

fn mode_from_vehicle_mode(s: &str) -> RouteMode {
    match s.to_ascii_uppercase().as_str() {
        "METRO" | "SUBWAY" => RouteMode::Metro,
        "TRAM" | "TRAMWAY" => RouteMode::Tram,
        "BUS" | "COACH" => RouteMode::Bus,
        "RAIL" | "TRAIN" | "RER" => RouteMode::Rail,
        "FERRY" => RouteMode::Ferry,
        _ => RouteMode::Other,
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })
}

fn ref_val(v: &Value) -> Option<String> {
    v.as_str()
        .map(|s| s.to_string())
        .or_else(|| v.get("value").and_then(|x| x.as_str()).map(|s| s.to_string()))
}

fn text_first(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = v.as_array() {
        for it in arr {
            if let Some(s) = it.as_str().or_else(|| it.get("value").and_then(|x| x.as_str())) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Live board + optional RT snapshot (trips + **VehicleLocation GPS**).
pub struct LiveBoardResult {
    pub departures: Vec<LiveDeparture>,
    /// Parsed SM state for overlay merge (includes real GPS when PRIM sent it).
    pub rt_delta: Option<FeedRtState>,
}

/// Fetch live board from PRIM SIRI Stop Monitoring.
pub async fn fetch_live_departures(
    client: &PrimClient,
    monitoring_ref: &str,
    limit: usize,
) -> Result<Vec<LiveDeparture>> {
    Ok(fetch_live_board(client, monitoring_ref, limit)
        .await?
        .departures)
}

/// Like [`fetch_live_departures`] but also returns RT delta for map GPS merge.
pub async fn fetch_live_board(
    client: &PrimClient,
    monitoring_ref: &str,
    limit: usize,
) -> Result<LiveBoardResult> {
    let mut ref_q = monitoring_ref.to_string();
    if !ref_q.ends_with(':') {
        ref_q.push(':');
    }
    // Keep this stop in the continuous SM warm set
    super::register_sm_interest(&ref_q);
    if !super::sm_try_consume_quota() {
        return Err(TransitError::Http(
            "PRIM stop-monitoring daily soft budget exhausted".into(),
        ));
    }
    let path = format!(
        "/marketplace/stop-monitoring?MonitoringRef={}",
        crate_url_encode(&ref_q)
    );
    let bytes = client.get_bytes(&path).await?;

    // Parse once for overlay (trips + optional VehicleLocation → real GPS)
    let rt_delta = ingest_siri_stop_monitoring("idfm", &bytes).ok();

    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|e| TransitError::Parse(format!("stop-monitoring json: {e}")))?;
    let sd = root
        .get("Siri")
        .and_then(|s| s.get("ServiceDelivery"))
        .unwrap_or(&root);
    let deliveries = sd
        .get("StopMonitoringDelivery")
        .and_then(|d| d.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let mut out = Vec::new();
    for del in deliveries {
        if del.get("ErrorCondition").is_some() {
            let err = del
                .pointer("/ErrorCondition/ErrorInformation/ErrorText")
                .and_then(|x| x.as_str())
                .unwrap_or("SIRI error");
            return Err(TransitError::Http(format!("stop-monitoring: {err}")));
        }
        let visits = del
            .get("MonitoredStopVisit")
            .and_then(|v| v.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        for visit in visits {
            let mvj = match visit.get("MonitoredVehicleJourney") {
                Some(m) => m,
                None => continue,
            };
            let trip_raw = mvj
                .pointer("/FramedVehicleJourneyRef/DatedVehicleJourneyRef")
                .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| ref_val(v)))
                .or_else(|| mvj.get("DatedVehicleJourneyRef").and_then(ref_val))
                .unwrap_or_else(|| format!("siri-{}", out.len()));
            let line = mvj.get("LineRef").and_then(ref_val);
            let headsign = mvj
                .get("DestinationName")
                .and_then(text_first)
                .or_else(|| mvj.get("DestinationShortName").and_then(text_first))
                .or_else(|| mvj.get("DirectionName").and_then(text_first));
            let line_name = mvj.get("PublishedLineName").and_then(text_first);
            let mode = mvj
                .get("VehicleMode")
                .and_then(|m| {
                    if let Some(a) = m.as_array() {
                        a.first().and_then(|x| x.as_str())
                    } else {
                        m.as_str()
                    }
                })
                .map(mode_from_vehicle_mode)
                .unwrap_or(RouteMode::Other);

            let call = mvj.get("MonitoredCall");
            let exp = call
                .and_then(|c| c.get("ExpectedDepartureTime").or_else(|| c.get("ExpectedArrivalTime")))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);
            let aimed = call
                .and_then(|c| c.get("AimedDepartureTime").or_else(|| c.get("AimedArrivalTime")))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);
            let status = call
                .and_then(|c| c.get("DepartureStatus").or_else(|| c.get("ArrivalStatus")))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let canceled = status
                .as_deref()
                .map(|s| s.to_ascii_lowercase().contains("cancel"))
                .unwrap_or(false);
            let delay = match (aimed, exp) {
                (Some(a), Some(e)) => Some((e - a).num_seconds() as i32),
                _ => None,
            };
            let when = exp.or(aimed).unwrap_or_else(Utc::now);
            let platform = call
                .and_then(|c| c.get("DeparturePlatformName").or_else(|| c.get("ArrivalPlatformName")))
                .and_then(text_first);

            out.push(LiveDeparture {
                trip_id: format!("idfm:{trip_raw}"),
                route_short_name: line_name.clone(),
                route_long_name: line_name,
                trip_short_name: None,
                headsign,
                mode,
                scheduled_departure: aimed.unwrap_or(when),
                realtime_departure: exp.or(Some(when)),
                delay_seconds: delay,
                canceled,
                platform,
                route_color: None,
                source: "siri-stop-monitoring",
                line_ref: line,
                status,
            });
            if out.len() >= limit {
                break;
            }
        }
        if out.len() >= limit {
            break;
        }
    }

    out.sort_by_key(|d| d.realtime_departure.unwrap_or(d.scheduled_departure));
    out.truncate(limit);
    Ok(LiveBoardResult {
        departures: out,
        rt_delta,
    })
}

/// Optional stop record for GraphQL stop field (empty shell if unknown).
pub fn shell_stop(stop_id: &str, name: Option<String>) -> StopRecord {
    StopRecord {
        id: stop_id.to_string(),
        feed_id: "idfm".into(),
        raw_id: stop_id.to_string(),
        name: name.unwrap_or_else(|| stop_id.to_string()),
        lat: None,
        lon: None,
        parent_id: None,
        location_type: 0,
        platform_code: None,
        wheelchair: 0,
        stop_code: None,
        stop_desc: None,
        zone_id: None,
        stop_url: None,
        stop_timezone: None,
        level_id: None,
    }
}

fn crate_url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitoring_ref_from_digits() {
        assert_eq!(
            monitoring_ref_from_stop_id("463041").as_deref(),
            Some("STIF:StopPoint:Q:463041:")
        );
        assert_eq!(
            monitoring_ref_from_stop_id("idfm:STIF:StopPoint:Q:463041:").as_deref(),
            Some("STIF:StopPoint:Q:463041:")
        );
    }
}
