//! SIRI Lite / SIRI JSON (PRIM) → [`FeedRtState`].
//!
//! Supports Île-de-France PRIM marketplace JSON shapes:
//! - Estimated Timetable → trip updates (expected times, status)
//! - Stop Monitoring → trip updates (+ vehicle at stop when resolvable later)
//! - General Message → service alerts
//!
//! Does **not** use Navitia APIs.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;

use crate::error::{Result, TransitError};
use crate::rt::overlay::{AlertRt, FeedRtState, StopTimeRt, TripRt, VehiclePos};

/// Parse ISO-8601 or partial timestamps to Unix seconds.
fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 2026-07-27T14:31:07.646Z
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    if let Ok(dt) = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%z") {
        return Some(dt.timestamp());
    }
    // without Z
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    None
}

fn ref_value(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    v.get("value")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

fn text_list_first(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = v.as_array() {
        for item in arr {
            if let Some(s) = item.as_str() {
                return Some(s.to_string());
            }
            if let Some(s) = item.get("value").and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
        }
    }
    v.get("value").and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Last meaningful segment of a SIRI LineRef / VehicleRef / journey ref for UI labels.
///
/// Examples: `STIF:Line::C01371:` → `C01371`, `RATP-SIV:VehicleJourney::42` → `42`.
fn humanize_siri_ref(raw: &str) -> Option<String> {
    const SKIP: &[&str] = &[
        "STIF",
        "RATP",
        "RATP-SIV",
        "IDFM",
        "SNCF",
        "Line",
        "StopPoint",
        "StopArea",
        "VehicleJourney",
        "DatedVehicleJourney",
        "Operator",
        "Q",
        "S",
    ];
    let parts: Vec<&str> = raw
        .split(|c| c == ':' || c == '/')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    for p in parts.iter().rev() {
        let upper = p.to_ascii_uppercase();
        if SKIP.iter().any(|s| s.eq_ignore_ascii_case(p)) {
            continue;
        }
        // Skip pure feed-like prefixes already listed; keep codes with digits or short names.
        if p.chars().any(|c| c.is_ascii_alphanumeric()) {
            // Prefer segments that contain a digit (line codes, journey numbers).
            if p.chars().any(|c| c.is_ascii_digit()) || p.len() <= 8 {
                return Some((*p).to_string());
            }
            if upper != *p && p.len() < 24 {
                return Some((*p).to_string());
            }
        }
    }
    parts.last().map(|s| (*s).to_string())
}

/// Build a human-readable vehicle label from MonitoredVehicleJourney / EstimatedVehicleJourney fields.
fn siri_vehicle_label(mvj: &Value, trip_raw: &str) -> Option<String> {
    let published = mvj.get("PublishedLineName").and_then(text_list_first);
    let dest = mvj
        .get("DestinationName")
        .and_then(text_list_first)
        .or_else(|| mvj.get("DirectionName").and_then(text_list_first));
    let vehicle_ref = mvj.get("VehicleRef").and_then(ref_value);
    let journey_num = mvj
        .get("VehicleJourneyName")
        .and_then(text_list_first)
        .or_else(|| {
            mvj.get("TrainNumbers")
                .and_then(|tn| tn.get("TrainNumberRef"))
                .and_then(ref_value)
        });

    if let Some(n) = journey_num.filter(|s| !s.is_empty()) {
        return match (published.as_ref(), dest.as_ref()) {
            (Some(p), Some(d)) => Some(format!("{n} {p} → {d}")),
            (Some(p), None) => Some(format!("{n} {p}")),
            (None, Some(d)) => Some(format!("{n} → {d}")),
            _ => Some(n),
        };
    }
    match (published.as_ref(), dest.as_ref()) {
        (Some(p), Some(d)) => Some(format!("{p} → {d}")),
        (None, Some(d)) => Some(d.clone()),
        (Some(p), None) => Some(p.clone()),
        _ => vehicle_ref
            .as_ref()
            .and_then(|r| humanize_siri_ref(r))
            .or_else(|| humanize_siri_ref(trip_raw))
            .or(vehicle_ref),
    }
}

fn siri_vehicle_id(feed_id: &str, mvj: &Value) -> Option<String> {
    mvj.get("VehicleRef")
        .and_then(ref_value)
        .map(|r| ns(feed_id, &r))
}

fn ns(feed_id: &str, raw: &str) -> String {
    if raw.contains(':') && raw.starts_with(feed_id) {
        return raw.to_string();
    }
    // Keep SIRI refs readable but namespaced
    format!("{feed_id}:{raw}")
}

fn delay_from_status(status: &str) -> Option<i32> {
    let s = status.to_ascii_lowercase();
    if s.is_empty() || s == "ontime" || s == "on_time" {
        return Some(0);
    }
    if s == "early" {
        return Some(-60);
    }
    if s.contains("cancel") {
        return None; // handled as canceled
    }
    if s.contains("delay") || s == "late" {
        return Some(120); // unknown magnitude; absolute times preferred
    }
    None
}

fn is_canceled_status(status: &str) -> bool {
    let s = status.to_ascii_lowercase();
    s.contains("cancel") || s == "cancelled" || s == "canceled"
}

fn as_array_or_one(v: &Value) -> Vec<&Value> {
    if let Some(a) = v.as_array() {
        a.iter().collect()
    } else if v.is_null() {
        vec![]
    } else {
        vec![v]
    }
}

fn service_delivery(root: &Value) -> &Value {
    root.get("Siri")
        .or_else(|| root.get("siri"))
        .and_then(|s| s.get("ServiceDelivery").or_else(|| s.get("serviceDelivery")))
        .unwrap_or(root)
}

/// SIRI Estimated Timetable → trip updates.
pub fn ingest_siri_estimated_timetable(feed_id: &str, bytes: &[u8]) -> Result<FeedRtState> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| TransitError::Parse(format!("siri ET json: {e}")))?;
    let mut state = FeedRtState::new(feed_id);
    state.trip_updates_fetched_at = Some(Utc::now());

    let sd = service_delivery(&root);
    let deliveries = sd
        .get("EstimatedTimetableDelivery")
        .or_else(|| sd.get("estimatedTimetableDelivery"))
        .map(as_array_or_one)
        .unwrap_or_default();

    let mut seq_counter: HashMap<String, u16> = HashMap::new();

    for del in deliveries {
        if del.get("ErrorCondition").is_some() {
            continue;
        }
        let frames = del
            .get("EstimatedJourneyVersionFrame")
            .or_else(|| del.get("estimatedJourneyVersionFrame"))
            .map(as_array_or_one)
            .unwrap_or_default();
        for frame in frames {
            let journeys = frame
                .get("EstimatedVehicleJourney")
                .or_else(|| frame.get("estimatedVehicleJourney"))
                .map(as_array_or_one)
                .unwrap_or_default();
            for j in journeys {
                let Some(trip_raw) = j
                    .get("DatedVehicleJourneyRef")
                    .and_then(ref_value)
                    .or_else(|| {
                        j.get("FramedVehicleJourneyRef")
                            .and_then(|f| f.get("DatedVehicleJourneyRef"))
                            .and_then(ref_value)
                    })
                else {
                    continue;
                };
                let trip_id = ns(feed_id, &trip_raw);
                let line = j
                    .get("LineRef")
                    .and_then(ref_value)
                    .map(|l| ns(feed_id, &l));
                // Destination / published line kept on vehicle stubs when Location present;
                // route_id (LineRef) is enough for synthetic display labels later.
                let _display_hint = siri_vehicle_label(j, &trip_raw);

                let calls_val = j
                    .get("EstimatedCalls")
                    .and_then(|c| c.get("EstimatedCall"))
                    .or_else(|| j.get("RecordedCalls").and_then(|c| c.get("RecordedCall")));
                let calls = calls_val.map(as_array_or_one).unwrap_or_default();

                let mut stop_updates = HashMap::new();
                let mut trip_delay: Option<i32> = None;
                let mut canceled = false;
                let mut seq = *seq_counter.get(&trip_id).unwrap_or(&1);

                for call in calls {
                    let stop_raw = call
                        .get("StopPointRef")
                        .and_then(ref_value)
                        .unwrap_or_default();
                    let stop_id = if stop_raw.is_empty() {
                        None
                    } else {
                        Some(ns(feed_id, &stop_raw))
                    };

                    let arr_status = call
                        .get("ArrivalStatus")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    let dep_status = call
                        .get("DepartureStatus")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    if is_canceled_status(arr_status) || is_canceled_status(dep_status) {
                        canceled = true;
                    }

                    let aimed_arr = call
                        .get("AimedArrivalTime")
                        .and_then(|x| x.as_str())
                        .and_then(parse_ts);
                    let aimed_dep = call
                        .get("AimedDepartureTime")
                        .and_then(|x| x.as_str())
                        .and_then(parse_ts);
                    let exp_arr = call
                        .get("ExpectedArrivalTime")
                        .and_then(|x| x.as_str())
                        .and_then(parse_ts);
                    let exp_dep = call
                        .get("ExpectedDepartureTime")
                        .and_then(|x| x.as_str())
                        .and_then(parse_ts);

                    let mut arrival_delay = match (aimed_arr, exp_arr) {
                        (Some(a), Some(e)) => Some((e - a) as i32),
                        _ => delay_from_status(arr_status),
                    };
                    let mut departure_delay = match (aimed_dep, exp_dep) {
                        (Some(a), Some(e)) => Some((e - a) as i32),
                        _ => delay_from_status(dep_status),
                    };
                    // Prefer expected absolute times
                    let arrival_time = exp_arr.or(aimed_arr);
                    let departure_time = exp_dep.or(aimed_dep);

                    if arrival_delay.is_none() && departure_delay.is_some() {
                        arrival_delay = departure_delay;
                    }
                    if departure_delay.is_none() && arrival_delay.is_some() {
                        departure_delay = arrival_delay;
                    }
                    if let Some(d) = departure_delay.or(arrival_delay) {
                        if trip_delay.is_none() {
                            trip_delay = Some(d);
                        }
                    }

                    stop_updates.insert(
                        seq,
                        StopTimeRt {
                            stop_sequence: seq,
                            stop_id,
                            arrival_delay,
                            departure_delay,
                            arrival_time,
                            departure_time,
                            skipped: false,
                        },
                    );
                    seq = seq.saturating_add(1);
                }
                seq_counter.insert(trip_id.clone(), seq);

                // Optional VehicleLocation on ET journey → real VP with identity.
                let loc = j.get("VehicleLocation");
                let lat = loc
                    .and_then(|l| l.get("Latitude").or_else(|| l.get("latitude")))
                    .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
                let lon = loc
                    .and_then(|l| l.get("Longitude").or_else(|| l.get("longitude")))
                    .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
                if let (Some(lat), Some(lon)) = (lat, lon) {
                    let label = _display_hint.clone().or_else(|| {
                        line.as_ref()
                            .and_then(|l| humanize_siri_ref(l))
                            .or_else(|| humanize_siri_ref(&trip_raw))
                    });
                    state.vehicles.insert(
                        trip_id.clone(),
                        VehiclePos {
                            trip_id: Some(trip_id.clone()),
                            lat,
                            lon,
                            bearing: None,
                            speed: None,
                            updated_at: Utc::now(),
                            current_stop_id: None,
                            label,
                            vehicle_id: siri_vehicle_id(feed_id, j),
                            license_plate: None,
                            occupancy: None,
                            occupancy_percentage: None,
                            current_status: Some("IN_TRANSIT_TO".into()),
                            current_stop_sequence: None,
                            congestion: None,
                        },
                    );
                }

                state.trips.insert(
                    trip_id,
                    TripRt {
                        delay: trip_delay,
                        canceled,
                        start_date: None,
                        route_id: line,
                        direction_id: None,
                        start_time: None,
                        stop_updates,
                    },
                );
            }
        }
    }

    state.trip_update_count = state.trips.len();
    state.vehicle_count = state.vehicles.len();
    if !state.vehicles.is_empty() {
        state.vehicles_fetched_at = state.trip_updates_fetched_at;
    }
    Ok(state)
}

/// SIRI Stop Monitoring → trip updates (next call) and optional vehicle stubs.
pub fn ingest_siri_stop_monitoring(feed_id: &str, bytes: &[u8]) -> Result<FeedRtState> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| TransitError::Parse(format!("siri SM json: {e}")))?;
    let mut state = FeedRtState::new(feed_id);
    state.trip_updates_fetched_at = Some(Utc::now());
    state.vehicles_fetched_at = Some(Utc::now());

    let sd = service_delivery(&root);
    let deliveries = sd
        .get("StopMonitoringDelivery")
        .or_else(|| sd.get("stopMonitoringDelivery"))
        .or_else(|| sd.get("VehicleMonitoringDelivery"))
        .map(as_array_or_one)
        .unwrap_or_default();

    let mut seq_by_trip: HashMap<String, u16> = HashMap::new();

    for del in deliveries {
        if del.get("ErrorCondition").is_some() {
            continue;
        }
        let visits = del
            .get("MonitoredStopVisit")
            .or_else(|| del.get("monitoredStopVisit"))
            .or_else(|| del.get("VehicleActivity"))
            .map(as_array_or_one)
            .unwrap_or_default();

        for visit in visits {
            let mon_ref = visit
                .get("MonitoringRef")
                .and_then(ref_value)
                .or_else(|| {
                    visit
                        .get("MonitoredVehicleJourney")
                        .and_then(|m| m.get("MonitoredCall"))
                        .and_then(|c| c.get("StopPointRef"))
                        .and_then(ref_value)
                });

            let mvj = visit
                .get("MonitoredVehicleJourney")
                .or_else(|| visit.get("monitoredVehicleJourney"))
                .unwrap_or(visit);

            let trip_raw = mvj
                .get("FramedVehicleJourneyRef")
                .and_then(|f| f.get("DatedVehicleJourneyRef"))
                .and_then(|v| {
                    if let Some(s) = v.as_str() {
                        Some(s.to_string())
                    } else {
                        ref_value(v)
                    }
                })
                .or_else(|| mvj.get("DatedVehicleJourneyRef").and_then(ref_value));

            let Some(trip_raw) = trip_raw else {
                continue;
            };
            let trip_id = ns(feed_id, &trip_raw);
            let line = mvj
                .get("LineRef")
                .and_then(ref_value)
                .map(|l| ns(feed_id, &l));

            let call = mvj
                .get("MonitoredCall")
                .or_else(|| mvj.get("monitoredCall"));

            let stop_raw = mon_ref.or_else(|| {
                call.and_then(|c| c.get("StopPointRef").and_then(ref_value))
            });
            let stop_id = stop_raw.as_ref().map(|s| ns(feed_id, s));

            let dep_status = call
                .and_then(|c| c.get("DepartureStatus"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let arr_status = call
                .and_then(|c| c.get("ArrivalStatus"))
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let canceled = is_canceled_status(dep_status) || is_canceled_status(arr_status);

            let exp_arr = call
                .and_then(|c| c.get("ExpectedArrivalTime"))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);
            let exp_dep = call
                .and_then(|c| c.get("ExpectedDepartureTime"))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);
            let aimed_arr = call
                .and_then(|c| c.get("AimedArrivalTime"))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);
            let aimed_dep = call
                .and_then(|c| c.get("AimedDepartureTime"))
                .and_then(|x| x.as_str())
                .and_then(parse_ts);

            let arrival_delay = match (aimed_arr, exp_arr) {
                (Some(a), Some(e)) => Some((e - a) as i32),
                _ => delay_from_status(arr_status),
            };
            let departure_delay = match (aimed_dep, exp_dep) {
                (Some(a), Some(e)) => Some((e - a) as i32),
                _ => delay_from_status(dep_status),
            };

            let seq = *seq_by_trip.entry(trip_id.clone()).or_insert(1);
            *seq_by_trip.get_mut(&trip_id).unwrap() = seq.saturating_add(1);

            let entry = state.trips.entry(trip_id.clone()).or_insert_with(|| TripRt {
                delay: departure_delay.or(arrival_delay),
                canceled,
                start_date: None,
                route_id: line.clone(),
                direction_id: None,
                start_time: None,
                stop_updates: HashMap::new(),
            });
            if canceled {
                entry.canceled = true;
            }
            if entry.delay.is_none() {
                entry.delay = departure_delay.or(arrival_delay);
            }
            if entry.route_id.is_none() {
                entry.route_id = line.clone();
            }
            entry.stop_updates.insert(
                seq,
                StopTimeRt {
                    stop_sequence: seq,
                    stop_id: stop_id.clone(),
                    arrival_delay,
                    departure_delay,
                    arrival_time: exp_arr.or(aimed_arr),
                    departure_time: exp_dep.or(aimed_dep),
                    skipped: false,
                },
            );

            // Vehicle at stop: lat/lon filled later by enricher; store placeholder only if VehicleAtStop
            let at_stop = call
                .and_then(|c| c.get("VehicleAtStop"))
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            // Optional Location in VehicleLocation
            let loc = mvj
                .get("VehicleLocation")
                .or_else(|| visit.get("VehicleLocation"));
            let lat = loc
                .and_then(|l| l.get("Latitude").or_else(|| l.get("latitude")))
                .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
            let lon = loc
                .and_then(|l| l.get("Longitude").or_else(|| l.get("longitude")))
                .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));

            if let (Some(lat), Some(lon)) = (lat, lon) {
                let label = siri_vehicle_label(mvj, &trip_raw).or_else(|| {
                    line.as_ref()
                        .and_then(|l| humanize_siri_ref(l))
                        .or_else(|| humanize_siri_ref(&trip_raw))
                });
                state.vehicles.insert(
                    trip_id.clone(),
                    VehiclePos {
                        trip_id: Some(trip_id.clone()),
                        lat,
                        lon,
                        bearing: None,
                        speed: None,
                        updated_at: Utc::now(),
                        current_stop_id: stop_id.clone(),
                        label,
                        vehicle_id: siri_vehicle_id(feed_id, mvj),
                        license_plate: None,
                        occupancy: None,
                        occupancy_percentage: None,
                        current_status: Some(if at_stop {
                            "STOPPED_AT".into()
                        } else {
                            "IN_TRANSIT_TO".into()
                        }),
                        current_stop_sequence: Some(seq as u32),
                        congestion: None,
                    },
                );
            }
        }
    }

    state.trip_update_count = state.trips.len();
    state.vehicle_count = state.vehicles.len();
    Ok(state)
}

fn harvest_message_text(
    t: &Value,
    prefer_short: bool,
    short_msg: &mut Option<String>,
    long_msg: &mut Option<String>,
    translations: &mut Vec<(String, String)>,
) {
    let text = t
        .get("value")
        .and_then(|x| x.as_str())
        .or_else(|| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    let lang = t
        .get("lang")
        .or_else(|| t.get("Lang"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    translations.push((lang, text.clone()));
    if prefer_short {
        if short_msg.is_none() {
            *short_msg = Some(text);
        }
    } else if long_msg.is_none() {
        *long_msg = Some(text);
    }
}

/// SIRI General Message → alerts.
pub fn ingest_siri_general_message(feed_id: &str, bytes: &[u8]) -> Result<FeedRtState> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| TransitError::Parse(format!("siri GM json: {e}")))?;
    let mut state = FeedRtState::new(feed_id);
    state.alerts_fetched_at = Some(Utc::now());

    let sd = service_delivery(&root);
    let deliveries = sd
        .get("GeneralMessageDelivery")
        .or_else(|| sd.get("generalMessageDelivery"))
        .map(as_array_or_one)
        .unwrap_or_default();

    for del in deliveries {
        if del.get("ErrorCondition").is_some() {
            // still try InfoMessage if any
        }
        let messages = del
            .get("InfoMessage")
            .or_else(|| del.get("infoMessage"))
            .or_else(|| del.get("GeneralMessage"))
            .map(as_array_or_one)
            .unwrap_or_default();

        for (i, msg) in messages.iter().enumerate() {
            let id = msg
                .get("InfoMessageIdentifier")
                .and_then(ref_value)
                .or_else(|| msg.get("ItemIdentifier").and_then(ref_value))
                .unwrap_or_else(|| format!("{feed_id}:gm:{i}"));

            // PRIM IDFM: Content = { LineRef: [...], Message: [{ MessageType, MessageText }] }
            // Other SIRI: Content.MessageText / MessageContent / MessageType
            let content = msg.get("Content").or_else(|| msg.get("content"));

            let mut header: Option<String> = None;
            let mut description: Option<String> = None;
            let mut translations: Vec<(String, String)> = Vec::new();
            let mut short_msg: Option<String> = None;
            let mut long_msg: Option<String> = None;

            // IDFM: Content.Message[] with MessageType SHORT_MESSAGE / TEXT_ONLY / ...
            if let Some(msgs) = content.and_then(|c| c.get("Message")).or_else(|| msg.get("Message"))
            {
                for m in as_array_or_one(msgs) {
                    let mtype = m
                        .get("MessageType")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_ascii_uppercase();
                    let prefer_short = mtype.contains("SHORT") || mtype == "TITLE";
                    if let Some(mt) = m.get("MessageText").or_else(|| m.get("messageText")) {
                        for t in as_array_or_one(mt) {
                            harvest_message_text(
                                t,
                                prefer_short,
                                &mut short_msg,
                                &mut long_msg,
                                &mut translations,
                            );
                        }
                    }
                }
            }

            // Classic: Content.MessageText or msg.MessageText
            if short_msg.is_none() && long_msg.is_none() {
                if let Some(texts) = content
                    .and_then(|c| c.get("MessageText"))
                    .or_else(|| msg.get("MessageText"))
                {
                    for t in as_array_or_one(texts) {
                        harvest_message_text(
                            t,
                            false,
                            &mut short_msg,
                            &mut long_msg,
                            &mut translations,
                        );
                    }
                }
            }

            if description.is_none() {
                description = long_msg
                    .clone()
                    .or_else(|| short_msg.clone())
                    .or_else(|| {
                        content
                            .and_then(|c| c.get("MessageContent"))
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                    });
            }
            header = short_msg
                .clone()
                .or_else(|| {
                    description
                        .as_ref()
                        .map(|d| d.chars().take(100).collect::<String>())
                })
                .or(header);

            // Channel → severity (Perturbation, Information, Commercial, …)
            let channel = msg
                .get("InfoChannelRef")
                .and_then(ref_value)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let severity = if channel.contains("perturb") || channel.contains("incident") {
                "SEVERE"
            } else if channel.contains("travaux") || channel.contains("work") {
                "WARNING"
            } else if channel.contains("info") || channel.contains("commercial") {
                "INFO"
            } else {
                "WARNING"
            };

            let mut informed_line = Vec::new();
            let mut informed_stop = Vec::new();
            // LineRef on message or inside Content (IDFM)
            for lr_src in [
                msg.get("LineRef"),
                content.and_then(|c| c.get("LineRef")),
                msg.get("LineRefs"),
                content.and_then(|c| c.get("LineRefs")),
            ]
            .into_iter()
            .flatten()
            {
                for lr in as_array_or_one(lr_src) {
                    if let Some(s) = ref_value(lr) {
                        let n = ns(feed_id, &s);
                        if !informed_line.contains(&n) {
                            informed_line.push(n);
                        }
                    }
                }
            }
            if let Some(sr) = msg.get("StopPointRef").and_then(ref_value) {
                informed_stop.push(ns(feed_id, &sr));
            }
            if let Some(arr) = content.and_then(|c| c.get("StopPointRef")) {
                for sr in as_array_or_one(arr) {
                    if let Some(s) = ref_value(sr) {
                        informed_stop.push(ns(feed_id, &s));
                    }
                }
            }

            let mut periods = Vec::new();
            if let Some(vp) = msg.get("ValididyPeriod").or_else(|| msg.get("ValidityPeriod")) {
                for p in as_array_or_one(vp) {
                    let start = p
                        .get("StartTime")
                        .and_then(|x| x.as_str())
                        .and_then(parse_ts);
                    let end = p.get("EndTime").and_then(|x| x.as_str()).and_then(parse_ts);
                    periods.push((start, end));
                }
            }
            // IDFM: ValidUntilTime at message root (no start)
            if periods.is_empty() {
                if let Some(end) = msg
                    .get("ValidUntilTime")
                    .and_then(|x| x.as_str())
                    .and_then(parse_ts)
                {
                    let start = msg
                        .get("RecordedAtTime")
                        .and_then(|x| x.as_str())
                        .and_then(parse_ts);
                    periods.push((start, Some(end)));
                }
            }
            let (active_start, active_end) = periods.first().copied().unwrap_or((None, None));

            if header.is_none() && description.is_none() {
                continue;
            }

            // Prefer FR translation for header/description when available
            if let Some(fr) = translations
                .iter()
                .find(|(l, _)| l.eq_ignore_ascii_case("fr"))
                .map(|(_, t)| t.clone())
            {
                if description.as_ref().map(|d| d != &fr).unwrap_or(true) {
                    // keep longer text as description when we already have it
                    if description.as_ref().map(|d| d.len()).unwrap_or(0) < fr.len() {
                        description = Some(fr.clone());
                    }
                }
                if header.as_ref().map(|h| h.len() > 100).unwrap_or(true) {
                    header = Some(fr.chars().take(100).collect());
                }
            }

            state.alerts.push(AlertRt {
                id: ns(feed_id, &id),
                header,
                description,
                severity: severity.into(),
                header_translations: translations,
                informed_stop_ids: informed_stop,
                informed_route_ids: informed_line,
                informed_trip_ids: vec![],
                cause: if channel.is_empty() {
                    None
                } else {
                    Some(channel)
                },
                effect: None,
                url: None,
                active_start,
                active_end,
                active_periods: periods,
            });
        }
    }

    state.alert_count = state.alerts.len();
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_et_sample() {
        let j = r#"{
          "Siri": {
            "ServiceDelivery": {
              "EstimatedTimetableDelivery": [{
                "EstimatedJourneyVersionFrame": [{
                  "EstimatedVehicleJourney": [{
                    "DatedVehicleJourneyRef": {"value": "VJ1"},
                    "LineRef": {"value": "STIF:Line::C01371:"},
                    "EstimatedCalls": {
                      "EstimatedCall": [{
                        "StopPointRef": {"value": "STIF:StopPoint:Q:1:"},
                        "AimedDepartureTime": "2026-07-27T14:00:00Z",
                        "ExpectedDepartureTime": "2026-07-27T14:02:00Z",
                        "DepartureStatus": "delayed"
                      }]
                    }
                  }]
                }]
              }]
            }
          }
        }"#;
        let state = ingest_siri_estimated_timetable("idfm", j.as_bytes()).unwrap();
        assert_eq!(state.trips.len(), 1);
        let t = state.trips.values().next().unwrap();
        assert_eq!(t.delay, Some(120));
        assert!(!t.canceled);
        let su = t.stop_updates.values().next().unwrap();
        assert_eq!(su.departure_delay, Some(120));
    }

    #[test]
    fn parse_gm_idfm_content_message_array() {
        // Real PRIM IDFM shape (RER A sample).
        let j = r#"{
          "Siri": {
            "ServiceDelivery": {
              "GeneralMessageDelivery": [{
                "Status": "true",
                "InfoMessage": [{
                  "ItemIdentifier": "RATP-SIV:Item::MSG.1:LOC",
                  "InfoMessageIdentifier": {"value": "RATP-SIV:InfoMessage::MSG.1:LOC"},
                  "InfoChannelRef": {"value": "Perturbation"},
                  "ValidUntilTime": "2026-08-08T02:45:00.000Z",
                  "RecordedAtTime": "2026-06-15T13:15:23.672Z",
                  "Content": {
                    "LineRef": [{"value": "STIF:Line::C01742:"}],
                    "Message": [
                      {
                        "MessageType": "SHORT_MESSAGE",
                        "MessageText": {
                          "value": "Du 8 août au 23 août, trafic interrompu Vincennes–Noisy.",
                          "lang": "fr"
                        }
                      },
                      {
                        "MessageType": "TEXT_ONLY",
                        "MessageText": {
                          "value": "Du 8 août au 23 août inclus, le trafic sera interrompu entre Vincennes et Noisy-le-Grand – Mont d'Est (travaux). Bus de remplacement.",
                          "lang": "fr"
                        }
                      }
                    ]
                  }
                }]
              }]
            }
          }
        }"#;
        let state = ingest_siri_general_message("idfm", j.as_bytes()).unwrap();
        assert_eq!(state.alerts.len(), 1, "alerts={:?}", state.alerts);
        let a = &state.alerts[0];
        assert!(
            a.header.as_deref().unwrap_or("").contains("août")
                || a.description.as_deref().unwrap_or("").contains("Vincennes"),
            "header={:?} desc={:?}",
            a.header,
            a.description
        );
        assert_eq!(a.severity, "SEVERE");
        assert!(
            a.informed_route_ids.iter().any(|r| r.contains("C01742")),
            "routes={:?}",
            a.informed_route_ids
        );
        assert!(a.active_end.is_some());
    }

    #[test]
    fn parse_sm_sample() {
        let j = r#"{
          "Siri": {
            "ServiceDelivery": {
              "StopMonitoringDelivery": [{
                "MonitoredStopVisit": [{
                  "MonitoringRef": {"value": "STIF:StopPoint:Q:463041:"},
                  "MonitoredVehicleJourney": {
                    "LineRef": {"value": "STIF:Line::C01371:"},
                    "FramedVehicleJourneyRef": {
                      "DatedVehicleJourneyRef": "RATP-SIV:VehicleJourney::1"
                    },
                    "MonitoredCall": {
                      "ExpectedArrivalTime": "2026-07-27T14:29:52.470Z",
                      "ExpectedDepartureTime": "2026-07-27T14:29:52.470Z",
                      "DepartureStatus": "onTime",
                      "VehicleAtStop": false
                    }
                  }
                }]
              }]
            }
          }
        }"#;
        let state = ingest_siri_stop_monitoring("idfm", j.as_bytes()).unwrap();
        assert_eq!(state.trips.len(), 1);
        let t = state.trips.values().next().unwrap();
        assert!(!t.stop_updates.is_empty());
        assert!(t.route_id.as_deref().unwrap_or("").contains("C01371"));
    }

    #[test]
    fn parse_sm_vehicle_identity() {
        let j = r#"{
          "Siri": {
            "ServiceDelivery": {
              "StopMonitoringDelivery": [{
                "MonitoredStopVisit": [{
                  "MonitoringRef": {"value": "STIF:StopPoint:Q:463041:"},
                  "MonitoredVehicleJourney": {
                    "LineRef": {"value": "STIF:Line::C01371:"},
                    "PublishedLineName": [{"value": "38"}],
                    "DestinationName": [{"value": "Porte d'Orléans"}],
                    "VehicleRef": {"value": "BUS-1234"},
                    "FramedVehicleJourneyRef": {
                      "DatedVehicleJourneyRef": "RATP-SIV:VehicleJourney::99"
                    },
                    "VehicleLocation": {"Latitude": 48.85, "Longitude": 2.35},
                    "MonitoredCall": {
                      "ExpectedArrivalTime": "2026-07-27T14:29:52.470Z",
                      "ExpectedDepartureTime": "2026-07-27T14:29:52.470Z",
                      "DepartureStatus": "onTime",
                      "VehicleAtStop": true
                    }
                  }
                }]
              }]
            }
          }
        }"#;
        let state = ingest_siri_stop_monitoring("idfm", j.as_bytes()).unwrap();
        assert_eq!(state.vehicles.len(), 1);
        let v = state.vehicles.values().next().unwrap();
        assert!(
            v.label.as_deref().unwrap_or("").contains("38"),
            "label={:?}",
            v.label
        );
        assert!(
            v.label.as_deref().unwrap_or("").contains("Orléans"),
            "label={:?}",
            v.label
        );
        assert_eq!(v.vehicle_id.as_deref(), Some("idfm:BUS-1234"));
        assert_eq!(v.current_status.as_deref(), Some("STOPPED_AT"));
    }

    #[test]
    fn humanize_line_and_journey_refs() {
        assert_eq!(humanize_siri_ref("STIF:Line::C01371:").as_deref(), Some("C01371"));
        assert_eq!(
            humanize_siri_ref("RATP-SIV:VehicleJourney::42").as_deref(),
            Some("42")
        );
    }

    #[test]
    fn parse_gm_sample() {
        let j = r#"{
          "Siri": {
            "ServiceDelivery": {
              "GeneralMessageDelivery": [{
                "InfoMessage": [{
                  "InfoMessageIdentifier": {"value": "MSG1"},
                  "LineRef": {"value": "STIF:Line::C01371:"},
                  "Content": {
                    "MessageText": [{"value": "Trafic perturbé", "lang": "FR"}]
                  }
                }]
              }]
            }
          }
        }"#;
        let state = ingest_siri_general_message("idfm", j.as_bytes()).unwrap();
        assert_eq!(state.alerts.len(), 1);
        assert!(state.alerts[0]
            .description
            .as_deref()
            .unwrap_or("")
            .contains("perturb"));
    }
}
