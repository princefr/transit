//! Transit map frontend (WASM).
//!
//! Fetches GraphQL (`stops`, `itineraries`, `departures`, `health`,
//! `tripRealtime`) and drives the Leaflet map via `window.TransitMap`
//! (see `map.js`). UI strings default to French.
//!
//! Geometry: prefer each leg's `geometry { lat lon }` from the API (GTFS shape
//! when available, else stop-chain / walk endpoints). Fallback rebuilds the
//! same chain client-side from stops. Colors: `routeColor` or mode defaults.
//!
//! Live vehicles (journey): when a journey is selected and "Temps réel" is on,
//! poll `tripRealtime(tripId)` every ~12s (GPS + shape-estimated positions).
//! Optionally, if a GraphQL WebSocket URL is available (`GRAPHQL_WS` / derived
//! `ws://…/ws`), subscribe to `watchTrips` and soft-fallback to poll on failure.
//! Journey markers stay visible during itinerary focus (GPS solid / Estimé dashed).
//!
//! Global live network (“Réseau en direct”): poll `vehicles(bbox|limit)` for the
//! map viewport every ~12s; pan also calls `warmLiveViewport` to prioritize PRIM
//! ET/SM interest. Global markers are hidden while an itinerary is focused.

use gloo_net::http::Request;
use js_sys::Function;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Document, Element, HtmlElement, HtmlInputElement, KeyboardEvent, WebSocket};

/// Poll interval for journey-scoped vehicle positions (ms).
const LIVE_POLL_MS: u32 = 12_000;

/// Poll interval for global live network vehicles (ms).
const GLOBAL_LIVE_POLL_MS: u32 = 12_000;

/// Max vehicles requested for the global network layer.
const GLOBAL_LIVE_LIMIT: i32 = 500;

const DEFAULT_GRAPHQL_URL: &str = "http://127.0.0.1:8080/graphql";

fn query_param(name: &str) -> Option<String> {
    let win = web_sys::window()?;
    let search = win.location().search().ok()?;
    for part in search.trim_start_matches('?').split('&') {
        if let Some(v) = part.strip_prefix(&format!("{name}=")) {
            let decoded = js_sys::decode_uri_component(v)
                .ok()
                .and_then(|s| s.as_string())
                .unwrap_or_else(|| v.to_string());
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

fn graphql_url() -> String {
    if let Some(win) = web_sys::window() {
        if let Ok(Some(storage)) = win.local_storage() {
            if let Ok(Some(v)) = storage.get_item("GRAPHQL_URL") {
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    if let Some(v) = query_param("graphql") {
        return v;
    }
    option_env!("GRAPHQL_URL")
        .unwrap_or(DEFAULT_GRAPHQL_URL)
        .to_string()
}

/// Optional GraphQL subscriptions URL (`ws://` or `wss://`).
///
/// Resolution order: `localStorage.GRAPHQL_WS`, `?graphql_ws=`, compile-time
/// `GRAPHQL_WS`, then derived from HTTP GraphQL URL (`/graphql` → `/ws`).
fn graphql_ws_url() -> Option<String> {
    if let Some(win) = web_sys::window() {
        if let Ok(Some(storage)) = win.local_storage() {
            if let Ok(Some(v)) = storage.get_item("GRAPHQL_WS") {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    if let Some(v) = query_param("graphql_ws") {
        return Some(v);
    }
    if let Some(v) = option_env!("GRAPHQL_WS") {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    // Soft derive: http(s)://host:port/graphql → ws(s)://host:port/ws
    let http = graphql_url();
    let ws = if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return None;
    };
    let derived = if ws.ends_with("/graphql") {
        format!("{}ws", ws.trim_end_matches("graphql"))
    } else if ws.contains("/graphql") {
        ws.replacen("/graphql", "/ws", 1)
    } else {
        // same origin path guess
        let base = ws.trim_end_matches('/');
        format!("{base}/ws")
    };
    Some(derived)
}

/// REST `/health` URL derived from the GraphQL endpoint.
fn health_url() -> String {
    let gql = graphql_url();
    if let Some(base) = gql.strip_suffix("/graphql") {
        format!("{base}/health")
    } else if gql.contains("/graphql") {
        gql.replacen("/graphql", "/health", 1)
    } else {
        let base = gql.trim_end_matches('/');
        format!("{base}/health")
    }
}

const HEALTH_POLL_LOADING_MS: u32 = 3_000;
const HEALTH_POLL_READY_MS: u32 = 15_000;

thread_local! {
    static HEALTH_POLL_HANDLE: RefCell<Option<gloo_timers::callback::Interval>> = RefCell::new(None);
    static STATIC_FEEDS_READY: RefCell<bool> = RefCell::new(false);
    static HEALTH_POLL_FAST: RefCell<Option<bool>> = RefCell::new(None);
}

// ─── Domain types ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct FeedHealth {
    id: String,
    static_loaded: bool,
    last_static_error: Option<String>,
    static_stops: Option<i32>,
    static_trips: Option<i32>,
}

#[derive(Clone, Debug)]
struct HealthSnapshot {
    status: String,
    _epoch_id: String,
    stops: i32,
    trips: i32,
    feeds: Vec<FeedHealth>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stop {
    id: String,
    name: String,
    lat: Option<f64>,
    lon: Option<f64>,
    #[serde(default)]
    is_station: bool,
    #[serde(default)]
    platform_code: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    wheelchair: Option<i32>,
}

#[derive(Clone, Debug)]
struct PlacePick {
    /// GTFS stop when kind is stop; synthetic for addresses.
    stop: Stop,
    /// `stop` or `address`.
    kind: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MapLeg {
    mode: String,
    route_color: Option<String>,
    dashed: bool,
    points: Vec<[f64; 2]>,
    label: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MapMarker {
    lat: f64,
    lon: f64,
    kind: String,
    title: String,
    mode: Option<String>,
    route_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearing: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct MapPayload {
    legs: Vec<MapLeg>,
    markers: Vec<MapMarker>,
}

/// Metadata for a transit leg we track for live VP.
#[derive(Clone, Debug)]
struct TrackedTrip {
    trip_id: String,
    mode: String,
    route: String,
    route_color: Option<String>,
    route_long_name: Option<String>,
    label: String,
}

/// Payload for `TransitMap.setLiveVehicles` / `setGlobalLiveVehicles`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveVehicle {
    id: String,
    lat: f64,
    lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearing: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// Server-computed best badge text (line / train number).
    #[serde(skip_serializing_if = "Option::is_none")]
    display_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trip_short_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vehicle_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    occupancy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headsign: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    feed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route_long_name: Option<String>,
    /// `GPS` or `ESTIMATED` from GraphQL `positionSource`.
    #[serde(skip_serializing_if = "Option::is_none")]
    position_source: Option<String>,
    /// True when position is shape/stop-chain estimate (not onboard GPS).
    #[serde(skip_serializing_if = "Option::is_none")]
    is_estimated: Option<bool>,
}

/// Map viewport bounds from Leaflet.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MapBounds {
    min_lat: f64,
    min_lon: f64,
    max_lat: f64,
    max_lon: f64,
}

// ─── GraphQL helpers ────────────────────────────────────────────────────────

async fn gql(query: &str, variables: Value) -> Result<Value, String> {
    let url = graphql_url();
    let body = json!({ "query": query, "variables": variables });
    let resp = Request::post(&url)
        .header("Content-Type", "application/json")
        .json(&body)
        .map_err(|e| format!("request build: {e}"))?
        .send()
        .await
        .map_err(|e| {
            format!(
                "network: {e} (API {url} — redémarrez le serveur, ou attendez la fin du \
                 chargement IDFM ~1 min, ou un calcul lourd peut saturer le process)"
            )
        })?;

    if !resp.ok() {
        return Err(format!("HTTP {} from {url}", resp.status()));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid JSON: {e}"))?;
    if let Some(errs) = v.get("errors").and_then(|e| e.as_array()) {
        if !errs.is_empty() {
            let msg = errs
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(if msg.is_empty() {
                "GraphQL error".into()
            } else {
                msg
            });
        }
    }
    v.get("data")
        .cloned()
        .ok_or_else(|| "missing data in GraphQL response".into())
}

async fn fetch_health_snapshot() -> Result<HealthSnapshot, String> {
    let url = health_url();
    let resp = Request::get(&url)
        .send()
        .await
        .map_err(|e| format!("health network: {e}"))?;
    if !resp.ok() {
        return Err(format!("health HTTP {}", resp.status()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("health JSON: {e}"))?;
    let feeds = body
        .get("feeds")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some(FeedHealth {
                        id: f.get("id")?.as_str()?.to_string(),
                        static_loaded: f.get("static_loaded").and_then(|v| v.as_bool())?,
                        last_static_error: f
                            .get("last_static_error")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        static_stops: f.get("static_stops").and_then(|v| v.as_u64()).map(|n| n as i32),
                        static_trips: f.get("static_trips").and_then(|v| v.as_u64()).map(|n| n as i32),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(HealthSnapshot {
        status: body
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string(),
        _epoch_id: body
            .get("epoch_id")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string(),
        stops: body.get("stops").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        trips: body.get("trips").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        feeds,
    })
}

async fn search_stops(search: &str, limit: i32) -> Result<Vec<Stop>, String> {
    let q = r#"
      query($search: String!, $limit: Int) {
        stops(search: $search, limit: $limit) {
          nodes { id name lat lon isStation platformCode code wheelchair }
          totalCount
        }
      }
    "#;
    let data = gql(q, json!({ "search": search, "limit": limit })).await?;
    let nodes = data
        .pointer("/stops/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for n in nodes {
        out.push(Stop {
            id: n
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            name: n
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            lat: n.get("lat").and_then(|x| x.as_f64()),
            lon: n.get("lon").and_then(|x| x.as_f64()),
            is_station: n
                .get("isStation")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            platform_code: n
                .get("platformCode")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            code: n
                .get("code")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            wheelchair: n.get("wheelchair").and_then(|x| x.as_i64()).map(|i| i as i32),
        });
    }
    Ok(out)
}

/// Unified place search (GTFS stops + BAN addresses). Falls back to stops-only.
async fn search_places(search: &str, limit: i32) -> Result<Vec<PlacePick>, String> {
    let q = r#"
      query($search: String!, $limit: Int) {
        places(search: $search, limit: $limit) {
          kind id label lat lon stopId isStation postcode city street
        }
      }
    "#;
    match gql(q, json!({ "search": search, "limit": limit })).await {
        Ok(data) => {
            let nodes = data
                .get("places")
                .and_then(|n| n.as_array())
                .cloned()
                .unwrap_or_default();
            let mut out = Vec::new();
            for n in nodes {
                let kind = n
                    .get("kind")
                    .and_then(|x| x.as_str())
                    .unwrap_or("stop")
                    .to_string();
                let label = n
                    .get("label")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = n
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let stop_id = n
                    .get("stopId")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| id.clone());
                out.push(PlacePick {
                    kind: kind.clone(),
                    stop: Stop {
                        id: stop_id,
                        name: label,
                        lat: n.get("lat").and_then(|x| x.as_f64()),
                        lon: n.get("lon").and_then(|x| x.as_f64()),
                        is_station: n
                            .get("isStation")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false),
                        platform_code: None,
                        code: {
                            let cp = n.get("postcode").and_then(|x| x.as_str()).unwrap_or("");
                            let city = n.get("city").and_then(|x| x.as_str()).unwrap_or("");
                            let s = format!("{cp} {city}").trim().to_string();
                            if s.is_empty() {
                                None
                            } else {
                                Some(s)
                            }
                        },
                        wheelchair: None,
                    },
                });
            }
            Ok(out)
        }
        Err(e) if e.contains("places") || e.contains("Unknown field") => {
            // Older API without places — stops only.
            let stops = search_stops(search, limit).await?;
            Ok(stops
                .into_iter()
                .map(|stop| PlacePick {
                    kind: "stop".into(),
                    stop,
                })
                .collect())
        }
        Err(e) => Err(e),
    }
}

async fn fetch_departures(stop_id: &str, limit: i32) -> Result<Value, String> {
    let q = r#"
      query($stopId: ID!, $limit: Int!) {
        departures(stopId: $stopId, limit: $limit, live: true) {
          tripId tripShortName routeShortName headsign mode routeColor
          scheduledDeparture realtimeDeparture delaySeconds canceled platform
          source status
        }
      }
    "#;
    let data = gql(q, json!({ "stopId": stop_id, "limit": limit })).await?;
    Ok(data
        .get("departures")
        .cloned()
        .unwrap_or(Value::Array(vec![])))
}

async fn fetch_traffic_messages(limit: i32) -> Result<Value, String> {
    // IDFM PRIM GeneralMessage + SNCF GTFS-RT; UI prefixes [ÎDF]/[SNCF] from id.
    // Prefer query with `lines` badges; fall back if API binary is older than schema.
    let q_full = r#"
      query($limit: Int!) {
        trafficMessages(limit: $limit) {
          id header description severity cause effect
          activeStart activeEnd
          informedRouteIds
          lines { routeId shortName color textColor mode }
        }
      }
    "#;
    let q_legacy = r#"
      query($limit: Int!) {
        trafficMessages(limit: $limit) {
          id header description severity cause effect
          activeStart activeEnd
        }
      }
    "#;
    match gql(q_full, json!({ "limit": limit })).await {
        Ok(data) => Ok(data
            .get("trafficMessages")
            .cloned()
            .unwrap_or(Value::Array(vec![]))),
        Err(e) if e.contains("informedRouteIds") || e.contains("lines") || e.contains("Unknown field") => {
            web_sys::console::warn_1(
                &"traffic: API without lines fields — restart server (make run); using legacy query"
                    .into(),
            );
            let data = gql(q_legacy, json!({ "limit": limit })).await?;
            Ok(data
                .get("trafficMessages")
                .cloned()
                .unwrap_or(Value::Array(vec![])))
        }
        Err(e) => Err(e),
    }
}

async fn plan_itineraries(
    from: &PlacePick,
    to: &PlacePick,
    time_at: Option<String>,
    arrive_by: bool,
    wheelchair: bool,
    bike_from: bool,
    bike_to: bool,
) -> Result<Value, String> {
    let place_json = |p: &PlacePick| -> Value {
        if p.kind == "address" {
            let mut o = json!({
                "name": p.stop.name,
            });
            if let Some(lat) = p.stop.lat {
                o["lat"] = json!(lat);
            }
            if let Some(lon) = p.stop.lon {
                o["lon"] = json!(lon);
            }
            o
        } else {
            json!({ "stopId": p.stop.id })
        }
    };
    // Full query with disruption payloads; fall back if API binary is older.
    let q = r#"
      query($input: ItineraryInput!) {
        itineraries(input: $input) {
          realtimeDegraded
          journeys {
            id departure arrival durationSeconds transfers realtimeStatus
            walkDistanceMeters alertHeaders
            fareAmount fareCurrency fareNote
            alerts {
              id header description severity cause effect
              lines { shortName color textColor }
            }
            legs {
              __typename
              ... on TransitLeg {
                mode routeShortName routeLongName tripShortName tripId
                stopHeadsign headsign
                routeColor routeTextColor wheelchair bikesAllowed
                sameVehicle
                scheduledDeparture realtimeDeparture delayDepartureSeconds
                scheduledArrival realtimeArrival delayArrivalSeconds
                canceled
                fromStop { id name lat lon }
                toStop { id name lat lon }
                intermediateStops {
                  stop { id name lat lon }
                  scheduledArrival scheduledDeparture
                  realtimeArrival realtimeDeparture
                }
                geometry { lat lon }
                vehicle { lat lon bearing label occupancy currentStatus updatedAt }
                alerts {
                  id header description severity cause effect
                  lines { shortName color textColor }
                }
              }
              ... on WalkLeg {
                mode
                durationSeconds distanceMeters
                fromName toName fromLat fromLon toLat toLon
                fromStopId toStopId
                geometry { lat lon }
              }
            }
          }
        }
      }
    "#;
    let mut input = json!({
        "from": place_json(from),
        "to": place_json(to),
        "maxResults": 4,
        "maxTransfers": 6,
        "maxWalkMeters": 2500,
    });
    if let Some(at) = time_at {
        if !at.is_empty() {
            if arrive_by {
                input["arriveBy"] = json!(at);
            } else {
                input["departureAt"] = json!(at);
            }
        }
    }
    if wheelchair {
        input["wheelchair"] = json!(true);
    }
    input["bikeFrom"] = json!(bike_from);
    input["bikeTo"] = json!(bike_to);
    let vars = json!({ "input": input });
    let data = match gql(q, vars.clone()).await {
        Ok(d) => d,
        Err(e)
            if e.contains("Unknown field")
                || e.contains("alerts")
                || e.contains("lines") =>
        {
            // Older API without journey.alerts — still get status/delays/headers.
            let q_legacy = r#"
              query($input: ItineraryInput!) {
                itineraries(input: $input) {
                  realtimeDegraded
                  journeys {
                    id departure arrival durationSeconds transfers realtimeStatus
                    walkDistanceMeters alertHeaders
                    fareAmount fareCurrency fareNote
                    legs {
                      __typename
                      ... on TransitLeg {
                        mode routeShortName routeLongName tripShortName tripId
                        stopHeadsign headsign
                        routeColor routeTextColor wheelchair bikesAllowed
                        sameVehicle
                        scheduledDeparture realtimeDeparture delayDepartureSeconds
                        scheduledArrival realtimeArrival delayArrivalSeconds
                        canceled
                        fromStop { id name lat lon }
                        toStop { id name lat lon }
                        intermediateStops {
                          stop { id name lat lon }
                          scheduledArrival scheduledDeparture
                          realtimeArrival realtimeDeparture
                        }
                        geometry { lat lon }
                        vehicle { lat lon bearing label occupancy currentStatus updatedAt }
                        alerts { header severity description }
                      }
                      ... on WalkLeg {
                        durationSeconds distanceMeters
                        fromName toName fromLat fromLon toLat toLon
                        fromStopId toStopId
                        geometry { lat lon }
                      }
                    }
                  }
                }
              }
            "#;
            gql(q_legacy, vars).await.map_err(|e2| format!("{e}; legacy: {e2}"))?
        }
        Err(e) => return Err(e),
    };
    Ok(data
        .get("itineraries")
        .cloned()
        .ok_or_else(|| "no itineraries field".to_string())?)
}

// ─── Map bridge ─────────────────────────────────────────────────────────────

fn call_map(method: &str, arg: Option<&str>) {
    let _ = call_map_value(method, arg);
}

fn call_map_value(method: &str, arg: Option<&str>) -> Option<JsValue> {
    let win = web_sys::window()?;
    let tm = js_sys::Reflect::get(&win, &JsValue::from_str("TransitMap")).ok()?;
    if tm.is_undefined() || tm.is_null() {
        return None;
    }
    let func = js_sys::Reflect::get(&tm, &JsValue::from_str(method)).ok()?;
    let func = func.dyn_into::<Function>().ok()?;
    match arg {
        Some(a) => func.call1(&tm, &JsValue::from_str(a)).ok(),
        None => func.call0(&tm).ok(),
    }
}

fn map_get_bounds() -> Option<MapBounds> {
    let v = call_map_value("getBounds", None)?;
    let s = js_sys::JSON::stringify(&v).ok()?.as_string()?;
    serde_json::from_str(&s).ok()
}

fn mode_str(v: &Value) -> String {
    v.as_str().unwrap_or("OTHER").to_uppercase()
}

fn stop_lat_lon(stop: &Value) -> Option<(f64, f64)> {
    let lat = stop.get("lat").and_then(|x| x.as_f64())?;
    let lon = stop.get("lon").and_then(|x| x.as_f64())?;
    Some((lat, lon))
}

/// Parse GraphQL `geometry: [{ lat, lon }, ...]` into map points.
fn geometry_points(leg: &Value) -> Vec<[f64; 2]> {
    let mut pts = Vec::new();
    if let Some(arr) = leg.get("geometry").and_then(|g| g.as_array()) {
        for p in arr {
            if let (Some(lat), Some(lon)) = (
                p.get("lat").and_then(|x| x.as_f64()),
                p.get("lon").and_then(|x| x.as_f64()),
            ) {
                pts.push([lat, lon]);
            }
        }
    }
    pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9);
    pts
}

/// Fallback polyline: stop chain or walk endpoints when `geometry` is empty.
fn fallback_points(leg: &Value, is_walk: bool) -> Vec<[f64; 2]> {
    let mut pts = Vec::new();
    if is_walk {
        if let (Some(lat), Some(lon)) = (
            leg.get("fromLat").and_then(|x| x.as_f64()),
            leg.get("fromLon").and_then(|x| x.as_f64()),
        ) {
            pts.push([lat, lon]);
        }
        if let (Some(lat), Some(lon)) = (
            leg.get("toLat").and_then(|x| x.as_f64()),
            leg.get("toLon").and_then(|x| x.as_f64()),
        ) {
            pts.push([lat, lon]);
        }
    } else {
        if let Some(from) = leg.get("fromStop") {
            if let Some((lat, lon)) = stop_lat_lon(from) {
                pts.push([lat, lon]);
            }
        }
        if let Some(inter) = leg.get("intermediateStops").and_then(|x| x.as_array()) {
            for is in inter {
                if let Some(s) = is.get("stop") {
                    if let Some((lat, lon)) = stop_lat_lon(s) {
                        pts.push([lat, lon]);
                    }
                }
            }
        }
        if let Some(to) = leg.get("toStop") {
            if let Some((lat, lon)) = stop_lat_lon(to) {
                pts.push([lat, lon]);
            }
        }
    }
    pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9);
    pts
}

/// Build map polylines/markers from a journey GraphQL object.
fn journey_to_map(journey: &Value) -> MapPayload {
    let mut legs_out = Vec::new();
    let mut markers = Vec::new();
    let legs = journey
        .get("legs")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();

    let mut first_pt: Option<(f64, f64, String)> = None;
    let mut last_pt: Option<(f64, f64, String)> = None;

    for (i, leg) in legs.iter().enumerate() {
        let ty = leg
            .get("__typename")
            .and_then(|t| t.as_str())
            .unwrap_or("");

        if ty == "WalkLeg" {
            let mut pts = geometry_points(leg);
            if pts.len() < 2 {
                pts = fallback_points(leg, true);
            }
            let from_n = leg
                .get("fromName")
                .and_then(|x| x.as_str())
                .unwrap_or("Walk");
            let to_n = leg.get("toName").and_then(|x| x.as_str()).unwrap_or("");
            let dur = leg
                .get("durationSeconds")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            let dist = leg
                .get("distanceMeters")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0);
            if let Some(p) = pts.first() {
                if first_pt.is_none() {
                    first_pt = Some((p[0], p[1], from_n.to_string()));
                }
            }
            if let Some(p) = pts.last() {
                last_pt = Some((p[0], p[1], to_n.to_string()));
            }
            if pts.len() >= 2 {
                let leg_mode = leg
                    .get("mode")
                    .and_then(|x| x.as_str())
                    .unwrap_or("WALK")
                    .to_ascii_uppercase();
                let is_bike = leg_mode == "BIKE";
                let label_mode = if is_bike { "Vélo" } else { "Marche" };
                legs_out.push(MapLeg {
                    mode: if is_bike {
                        "BIKE".into()
                    } else {
                        "WALK".into()
                    },
                    route_color: if is_bike {
                        Some("16a34a".into())
                    } else {
                        Some("64748b".into())
                    },
                    dashed: true,
                    points: pts,
                    label: format!(
                        "{label_mode} {from_n} → {to_n}<br/>{dist:.0} m · {dur}s"
                    ),
                });
            }
            continue;
        }

        // TransitLeg
        let mode = mode_str(leg.get("mode").unwrap_or(&Value::Null));
        let route_color = leg
            .get("routeColor")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().trim_start_matches('#').to_string())
            .filter(|s| s.len() == 6 && s.chars().all(|c| c.is_ascii_hexdigit()));
        let route = leg
            .get("routeShortName")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let trip = leg
            .get("tripShortName")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let head = leg
            .get("stopHeadsign")
            .or_else(|| leg.get("headsign"))
            .and_then(|x| x.as_str())
            .unwrap_or("");

        // Board/alight names for markers (even when geometry comes from shapes).
        if let Some(from) = leg.get("fromStop") {
            if let Some((lat, lon)) = stop_lat_lon(from) {
                let name = from
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("Origin");
                if first_pt.is_none() {
                    first_pt = Some((lat, lon, name.to_string()));
                }
                if i > 0 {
                    markers.push(MapMarker {
                        lat,
                        lon,
                        kind: "transfer".into(),
                        title: format!("Correspondance · {name}"),
                        mode: Some(mode.clone()),
                        route_color: route_color.clone(),
                        bearing: None,
                    });
                }
            }
        }
        if let Some(to) = leg.get("toStop") {
            if let Some((lat, lon)) = stop_lat_lon(to) {
                let name = to.get("name").and_then(|x| x.as_str()).unwrap_or("Dest");
                last_pt = Some((lat, lon, name.to_string()));
            }
        }

        let mut pts = geometry_points(leg);
        if pts.len() < 2 {
            pts = fallback_points(leg, false);
        }

        let label = format!("<b>{mode}</b> {route} {trip}<br/>{head}");
        if pts.len() >= 2 {
            legs_out.push(MapLeg {
                mode: mode.clone(),
                route_color: route_color.clone(),
                dashed: false,
                points: pts,
                label,
            });
        }

        if let Some(v) = leg.get("vehicle") {
            if let (Some(lat), Some(lon)) = (
                v.get("lat").and_then(|x| x.as_f64()),
                v.get("lon").and_then(|x| x.as_f64()),
            ) {
                let lbl = v
                    .get("label")
                    .and_then(|x| x.as_str())
                    .unwrap_or("Vehicle");
                let bearing = v.get("bearing").and_then(|x| x.as_f64());
                markers.push(MapMarker {
                    lat,
                    lon,
                    kind: "vehicle".into(),
                    title: format!("{lbl} ({mode})"),
                    mode: Some(mode),
                    route_color,
                    bearing,
                });
            }
        }
    }

    if let Some((lat, lon, title)) = first_pt {
        markers.insert(
            0,
            MapMarker {
                lat,
                lon,
                kind: "origin".into(),
                title: format!("Départ · {title}"),
                mode: None,
                route_color: None,
                bearing: None,
            },
        );
    }
    if let Some((lat, lon, title)) = last_pt {
        markers.push(MapMarker {
            lat,
            lon,
            kind: "destination".into(),
            title: format!("Arrivée · {title}"),
            mode: None,
            route_color: None,
            bearing: None,
        });
    }

    MapPayload {
        legs: legs_out,
        markers,
    }
}

// ─── DOM helpers ────────────────────────────────────────────────────────────

fn document() -> Document {
    web_sys::window()
        .expect("window")
        .document()
        .expect("document")
}

fn el(id: &str) -> Option<Element> {
    document().get_element_by_id(id)
}

fn set_html(id: &str, html: &str) {
    if let Some(e) = el(id) {
        e.set_inner_html(html);
    }
}

fn set_status(msg: &str, err: bool) {
    if let Some(e) = el("status") {
        e.set_inner_html(msg);
        let _ = e.class_list().toggle_with_force("error", err);
    }
}

fn set_field_loading(which: &str, loading: bool) {
    let id = if which == "origin" {
        "origin-loading"
    } else {
        "dest-loading"
    };
    if let Some(e) = el(id) {
        if loading {
            let _ = e.remove_attribute("hidden");
        } else {
            let _ = e.set_attribute("hidden", "true");
        }
    }
}

fn static_feeds_ready() -> bool {
    STATIC_FEEDS_READY.with(|r| *r.borrow())
}

fn set_plan_loading(loading: bool) {
    if let Some(btn) = el("plan-btn") {
        if loading {
            let _ = btn.set_attribute("disabled", "true");
            let _ = btn.class_list().add_1("is-loading");
        } else if static_feeds_ready() {
            let _ = btn.remove_attribute("disabled");
            let _ = btn.class_list().remove_1("is-loading");
        } else {
            let _ = btn.set_attribute("disabled", "true");
            let _ = btn.class_list().remove_1("is-loading");
        }
    }
    if let Some(sp) = el("plan-btn-spinner") {
        if loading {
            let _ = sp.remove_attribute("hidden");
        } else {
            let _ = sp.set_attribute("hidden", "true");
        }
    }
    if let Some(lab) = el("plan-btn-label") {
        lab.set_inner_html(if loading {
            "Recherche en cours…"
        } else {
            "Calculer l’itinéraire"
        });
    }
    if loading {
        set_html(
            "journeys",
            r#"<div class="journeys-loading">
              <span class="spinner spinner-lg"></span>
              <p>Recherche d’itinéraires…</p>
              <p class="hint">Cela peut prendre quelques secondes sur le réseau IDFM.</p>
            </div>"#,
        );
    }
}

fn show_suggest_loading(list_id: &str) {
    let Some(ul) = el(list_id) else {
        return;
    };
    ul.set_inner_html(
        r#"<li class="suggest-loading" aria-busy="true">
          <span class="spinner"></span>
          <span>Recherche…</span>
        </li>"#,
    );
    let _ = ul.remove_attribute("hidden");
}

fn input_value(id: &str) -> String {
    el(id)
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

fn checkbox_checked(id: &str) -> bool {
    el(id)
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.checked())
        .unwrap_or(false)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn mode_icon_path(mode: &str) -> String {
    let m = mode.to_lowercase();
    let name = match m.as_str() {
        "rail" | "train" => "train",
        "metro" => "metro",
        "tram" => "tram",
        "bus" | "coach" => "bus",
        "walk" => "walk",
        "bike" | "bicycle" | "cycling" | "velo" => "bike",
        "ferry" => "ferry",
        "wheelchair" => "wheelchair",
        "alert" => "alert",
        _ => "other",
    };
    format!("assets/icons/{name}.svg")
}

/// Format an API UTC/offset ISO timestamp as **local** wall-clock HH:MM.
///
/// GraphQL returns instants in UTC (e.g. `2026-07-27T17:50:00Z`). Slicing the
/// string showed UTC hours (17:50) while the user is in Europe/Paris (19:50 in
/// summer). Always convert via the browser timezone.
fn format_time(iso: &str) -> String {
    let d = js_sys::Date::new(&JsValue::from_str(iso));
    if d.get_time().is_nan() {
        // Fallback for unexpected shapes
        if iso.len() >= 16 && iso.as_bytes().get(10) == Some(&b'T') {
            return iso[11..16].to_string();
        }
        return iso.to_string();
    }
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

fn format_duration(secs: i64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 {
        format!("{h}h{m:02}")
    } else {
        format!("{m} min")
    }
}

/// Convert `<input type="datetime-local">` value to UTC ISO for GraphQL.
///
/// Wall clock is **browser local** (Europe/Paris on a French laptop). Previously
/// we appended `Z`, which treated 19:49 local as 19:49 UTC and skewed RAPTOR by
/// ~2h in summer (CEST).
fn datetime_local_to_iso(local: &str) -> Option<String> {
    let local = local.trim();
    if local.is_empty() {
        return None;
    }
    // Already an absolute instant
    if local.ends_with('Z') || local.contains('+') {
        return Some(local.to_string());
    }
    // "2026-07-27T14:30" or "2026-07-27T14:30:00"
    let (date, time) = local.split_once('T')?;
    let mut dp = date.split('-');
    let year: u32 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    let mut tp = time.split(':');
    let hour: u32 = tp.next()?.parse().ok()?;
    let minute: u32 = tp.next()?.parse().ok()?;
    let second: u32 = tp.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    // JS Date(y, m0, d, h, mi, s) = local timezone
    let d = js_sys::Date::new_with_year_month_day_hr_min_sec(
        year,
        (month as i32) - 1,
        day as i32,
        hour as i32,
        minute as i32,
        second as i32,
    );
    if d.get_time().is_nan() {
        return None;
    }
    d.to_iso_string().as_string()
}

// ─── App state ──────────────────────────────────────────────────────────────

struct App {
    origin: Option<PlacePick>,
    dest: Option<PlacePick>,
    journeys: Vec<Value>,
    selected: Option<usize>,
    /// Last plan used « Fauteuil roulant » — show wheelchair badges on results.
    plan_wheelchair: bool,
    origin_timer: Option<gloo_timers::callback::Timeout>,
    dest_timer: Option<gloo_timers::callback::Timeout>,
    /// Bumped whenever live tracking should cancel in-flight work.
    live_gen: u64,
    tracked_trips: Vec<TrackedTrip>,
    live_interval: Option<gloo_timers::callback::Interval>,
    /// Active GraphQL WS (graphql-transport-ws); closed on stop / restart.
    live_ws: Option<WebSocket>,
    /// Latest vehicle snapshot per trip id (WS path merges incremental updates).
    live_by_trip: HashMap<String, LiveVehicle>,
    /// True when WS subscription is driving updates (poll still used as soft fallback).
    live_ws_active: bool,
    /// Bumped whenever global live network polling should cancel in-flight work.
    global_live_gen: u64,
    global_live_interval: Option<gloo_timers::callback::Interval>,
    /// Debounce timer for map moveend → global live refresh.
    global_live_move_timer: Option<gloo_timers::callback::Timeout>,
}

impl App {
    fn new() -> Self {
        Self {
            origin: None,
            dest: None,
            journeys: Vec::new(),
            selected: None,
            plan_wheelchair: false,
            origin_timer: None,
            dest_timer: None,
            live_gen: 0,
            tracked_trips: Vec::new(),
            live_interval: None,
            live_ws: None,
            live_by_trip: HashMap::new(),
            live_ws_active: false,
            global_live_gen: 0,
            global_live_interval: None,
            global_live_move_timer: None,
        }
    }
}

/// Accessible journey badge (shown when « Fauteuil roulant » was used for planning).
fn wheelchair_badge_html() -> String {
    format!(
        r#"<span class="wheelchair-badge" title="Itinéraire calculé pour fauteuil roulant"><img src="{}" alt="" width="14" height="14"/><span>PMR</span></span>"#,
        mode_icon_path("wheelchair")
    )
}

/// Collect disruption messages for a journey (journey.alerts + leg.alerts + headers).
fn journey_disruption_messages(j: &Value) -> Vec<(String, String, String)> {
    // (header, description, severity)
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let push = |out: &mut Vec<(String, String, String)>,
                seen: &mut std::collections::HashSet<String>,
                h: &str,
                d: &str,
                sev: &str| {
        let key = format!("{h}|{d}");
        if h.is_empty() && d.is_empty() {
            return;
        }
        if !seen.insert(key) {
            return;
        }
        out.push((h.to_string(), d.to_string(), sev.to_string()));
    };
    if let Some(arr) = j.get("alerts").and_then(|a| a.as_array()) {
        for a in arr {
            let h = a.get("header").and_then(|x| x.as_str()).unwrap_or("");
            let d = a.get("description").and_then(|x| x.as_str()).unwrap_or("");
            let sev = a.get("severity").and_then(|x| x.as_str()).unwrap_or("");
            push(&mut out, &mut seen, h, d, sev);
        }
    }
    if let Some(legs) = j.get("legs").and_then(|l| l.as_array()) {
        for leg in legs {
            if let Some(arr) = leg.get("alerts").and_then(|a| a.as_array()) {
                for a in arr {
                    let h = a.get("header").and_then(|x| x.as_str()).unwrap_or("");
                    let d = a.get("description").and_then(|x| x.as_str()).unwrap_or("");
                    let sev = a.get("severity").and_then(|x| x.as_str()).unwrap_or("");
                    push(&mut out, &mut seen, h, d, sev);
                }
            }
        }
    }
    if out.is_empty() {
        if let Some(headers) = j.get("alertHeaders").and_then(|a| a.as_array()) {
            for h in headers {
                if let Some(s) = h.as_str() {
                    push(&mut out, &mut seen, s, "", "WARNING");
                }
            }
        }
    }
    out
}

/// Human RT impact for a journey card: status label + max delay + alert count.
fn journey_impact_html(j: &Value) -> String {
    let rt = j
        .get("realtimeStatus")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let mut max_delay = 0i64;
    let mut any_cancel = false;
    if let Some(legs) = j.get("legs").and_then(|l| l.as_array()) {
        for leg in legs {
            if leg.get("canceled").and_then(|x| x.as_bool()) == Some(true) {
                any_cancel = true;
            }
            if let Some(d) = leg.get("delayDepartureSeconds").and_then(|x| x.as_i64()) {
                if d.abs() > max_delay.abs() {
                    max_delay = d;
                }
            }
            if let Some(d) = leg.get("delayArrivalSeconds").and_then(|x| x.as_i64()) {
                if d.abs() > max_delay.abs() {
                    max_delay = d;
                }
            }
        }
    }
    let msgs = journey_disruption_messages(j);
    let mut parts = Vec::new();
    if any_cancel || rt == "CANCELLED" || rt == "CANCELED" {
        parts.push(
            r#"<span class="impact-sign impact-sign--cancel" title="Course(s) supprimée(s)">⛔ Supprimé</span>"#
                .to_string(),
        );
    } else if max_delay >= 60 {
        let mins = (max_delay + 59) / 60;
        parts.push(format!(
            r#"<span class="impact-sign impact-sign--delay" title="Retard temps réel">⏱ +{mins} min</span>"#
        ));
    } else if max_delay <= -60 {
        let mins = ((-max_delay) + 59) / 60;
        parts.push(format!(
            r#"<span class="impact-sign impact-sign--early" title="En avance">⏱ −{mins} min</span>"#
        ));
    } else if rt == "DELAYED" {
        parts.push(
            r#"<span class="impact-sign impact-sign--delay" title="Retard">⏱ Retard</span>"#
                .to_string(),
        );
    } else if rt == "ON_TIME" {
        parts.push(
            r#"<span class="impact-sign impact-sign--ok" title="À l’heure">✓ À l’heure</span>"#
                .to_string(),
        );
    }
    if !msgs.is_empty() {
        let n = msgs.len();
        let first = msgs[0].0.clone();
        let title = if first.is_empty() {
            "Perturbation — cliquer pour le détail".to_string()
        } else {
            first
        };
        parts.push(format!(
            r#"<span class="impact-sign impact-sign--alert" title="{t}">⚠ {label}</span>"#,
            t = escape_html(&title),
            label = if n == 1 {
                "Perturbé".to_string()
            } else {
                format!("{n} infos")
            }
        ));
    } else if rt == "DISRUPTED" {
        parts.push(
            r#"<span class="impact-sign impact-sign--alert" title="Perturbation">⚠ Perturbé</span>"#
                .to_string(),
        );
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(r#"<div class="journey-impact">{}</div>"#, parts.join(""))
}

fn format_delay_minutes(secs: i64) -> String {
    if secs == 0 {
        return String::new();
    }
    let mins = if secs > 0 {
        (secs + 59) / 60
    } else {
        -(((-secs) + 59) / 60)
    };
    if mins == 0 {
        format!("{secs:+}s")
    } else {
        format!("{mins:+} min")
    }
}

/// French label for journey `realtimeStatus`.
fn realtime_status_label(status: &str) -> String {
    match status {
        "ON_TIME" => "À l'heure".to_string(),
        "DELAYED" => "Retard".to_string(),
        "CANCELLED" | "CANCELED" => "Supprimé".to_string(),
        "DISRUPTED" => "Perturbé".to_string(),
        "SCHEDULED" => "Horaire".to_string(),
        _ => status.to_string(),
    }
}

/// Endpoint times from first/last transit leg (scheduled + optional RT).
fn journey_endpoint_times(j: &Value) -> (String, String, Option<String>, Option<String>) {
    let legs = j.get("legs").and_then(|l| l.as_array());
    let Some(legs) = legs else {
        let dep = j
            .get("departure")
            .and_then(|x| x.as_str())
            .map(format_time)
            .unwrap_or_else(|| "—".into());
        let arr = j
            .get("arrival")
            .and_then(|x| x.as_str())
            .map(format_time)
            .unwrap_or_else(|| "—".into());
        return (dep, arr, None, None);
    };
    let mut sched_dep = "—".to_string();
    let mut sched_arr = "—".to_string();
    let mut rt_dep: Option<String> = None;
    let mut rt_arr: Option<String> = None;
    for leg in legs {
        if leg.get("__typename").and_then(|t| t.as_str()) != Some("TransitLeg") {
            continue;
        }
        if sched_dep == "—" {
            if let Some(iso) = leg.get("scheduledDeparture").and_then(|x| x.as_str()) {
                sched_dep = format_time(iso);
            }
            if let Some(iso) = leg.get("realtimeDeparture").and_then(|x| x.as_str()) {
                rt_dep = Some(format_time(iso));
            }
        }
        if let Some(iso) = leg.get("scheduledArrival").and_then(|x| x.as_str()) {
            sched_arr = format_time(iso);
        }
        if let Some(iso) = leg.get("realtimeArrival").and_then(|x| x.as_str()) {
            rt_arr = Some(format_time(iso));
        }
    }
    (sched_dep, sched_arr, rt_dep, rt_arr)
}

/// Render HH:MM with optional struck scheduled time when RT differs.
fn format_transit_time_html(
    scheduled_iso: Option<&str>,
    realtime_iso: Option<&str>,
    delay_secs: Option<i64>,
    canceled: bool,
) -> String {
    let sched = scheduled_iso.map(format_time);
    let rt = realtime_iso.map(format_time);
    let time_cls = if canceled {
        "time time--canceled"
    } else if rt.is_some() {
        "time time--realtime"
    } else {
        "time"
    };
    let show = rt.clone().or(sched.clone()).unwrap_or_else(|| "—".into());
    let mut html = format!(r#"<span class="{time_cls}">{show}</span>"#);
    if let (Some(s), Some(r)) = (&sched, &rt) {
        if s != r && !canceled {
            html.push_str(&format!(
                r#" <span class="time-scheduled" title="Horaire théorique">{s}</span>"#
            ));
        }
    }
    if let Some(d) = delay_secs {
        if d != 0 && !canceled {
            html.push_str(&format!(
                r#" <span class="delay" title="Temps réel">{}</span>"#,
                format_delay_minutes(d)
            ));
        }
    }
    html
}

fn journey_times_html(j: &Value) -> String {
    let (sched_dep, sched_arr, _rt_dep, _rt_arr) = journey_endpoint_times(j);
    let legs = j.get("legs").and_then(|l| l.as_array());
    let mut dep_delay = None::<i64>;
    let mut arr_delay = None::<i64>;
    let mut canceled = false;
    if let Some(legs) = legs {
        for leg in legs {
            if leg.get("__typename").and_then(|t| t.as_str()) != Some("TransitLeg") {
                continue;
            }
            if dep_delay.is_none() {
                dep_delay = leg.get("delayDepartureSeconds").and_then(|x| x.as_i64());
            }
            arr_delay = leg.get("delayArrivalSeconds").and_then(|x| x.as_i64());
            if leg.get("canceled").and_then(|x| x.as_bool()) == Some(true) {
                canceled = true;
            }
        }
    }
    let dep_sched_iso = legs.and_then(|ls| {
        ls.iter()
            .find(|l| l.get("__typename").and_then(|t| t.as_str()) == Some("TransitLeg"))
            .and_then(|l| l.get("scheduledDeparture").and_then(|x| x.as_str()))
    });
    let arr_sched_iso = legs.and_then(|ls| {
        ls.iter()
            .rev()
            .find(|l| l.get("__typename").and_then(|t| t.as_str()) == Some("TransitLeg"))
            .and_then(|l| l.get("scheduledArrival").and_then(|x| x.as_str()))
    });
    let dep_rt_iso = legs.and_then(|ls| {
        ls.iter()
            .find(|l| l.get("__typename").and_then(|t| t.as_str()) == Some("TransitLeg"))
            .and_then(|l| l.get("realtimeDeparture").and_then(|x| x.as_str()))
    });
    let arr_rt_iso = legs.and_then(|ls| {
        ls.iter()
            .rev()
            .find(|l| l.get("__typename").and_then(|t| t.as_str()) == Some("TransitLeg"))
            .and_then(|l| l.get("realtimeArrival").and_then(|x| x.as_str()))
    });
    let dep_html = format_transit_time_html(dep_sched_iso, dep_rt_iso, dep_delay, canceled);
    let arr_html = format_transit_time_html(arr_sched_iso, arr_rt_iso, arr_delay, canceled);
    // Fallback when legs lack scheduled fields
    if dep_sched_iso.is_none() && _rt_dep.is_none() {
        return format!(r#"<span class="journey-dep-arr">{sched_dep} → {sched_arr}</span>"#);
    }
    format!(r#"<span class="journey-dep-arr">{dep_html} → {arr_html}</span>"#)
}

fn leg_times_compact_html(leg: &Value) -> String {
    let canceled = leg.get("canceled").and_then(|x| x.as_bool()).unwrap_or(false);
    let dep_delay = leg.get("delayDepartureSeconds").and_then(|x| x.as_i64());
    let arr_delay = leg.get("delayArrivalSeconds").and_then(|x| x.as_i64());
    let dep = format_transit_time_html(
        leg.get("scheduledDeparture").and_then(|x| x.as_str()),
        leg.get("realtimeDeparture").and_then(|x| x.as_str()),
        dep_delay,
        canceled,
    );
    let arr = format_transit_time_html(
        leg.get("scheduledArrival").and_then(|x| x.as_str()),
        leg.get("realtimeArrival").and_then(|x| x.as_str()),
        arr_delay,
        canceled,
    );
    format!(r#"<span class="leg-times">{dep} → {arr}</span>"#)
}

/// Expandable disruption block HTML (for trip detail and journey cards).
fn disruption_panel_html(msgs: &[(String, String, String)], open: bool) -> String {
    if msgs.is_empty() {
        return String::new();
    }
    let mut items = String::new();
    for (h, d, sev) in msgs.iter().take(12) {
        let title = if h.is_empty() {
            "Info trafic"
        } else {
            h.as_str()
        };
        let body = if !d.is_empty() && d != h {
            format!(
                r#"<div class="disruption-body">{}</div>"#,
                escape_html(d)
            )
        } else {
            String::new()
        };
        let sev_cls = match sev.to_ascii_uppercase().as_str() {
            "SEVERE" | "CRITICAL" => "disruption-item--severe",
            "INFO" => "disruption-item--info",
            _ => "disruption-item--warn",
        };
        items.push_str(&format!(
            r#"<div class="disruption-item {sev_cls}">
              <div class="disruption-head">⚠ {title}</div>
              {body}
            </div>"#,
            sev_cls = sev_cls,
            title = escape_html(title),
            body = body,
        ));
    }
    let open_attr = if open { " open" } else { "" };
    let n = msgs.len();
    format!(
        r#"<details class="disruption-panel"{open_attr}>
          <summary class="disruption-summary">
            <span class="disruption-summary-icon">⚠</span>
            <span>{n} info{plural} trafic — cliquer pour lire</span>
          </summary>
          <div class="disruption-list">{items}</div>
        </details>"#,
        open_attr = open_attr,
        n = n,
        plural = if n > 1 { "s" } else { "" },
        items = items,
    )
}

/// Extract transit trip ids + labels from a journey GraphQL object.
fn journey_tracked_trips(journey: &Value) -> Vec<TrackedTrip> {
    let mut out = Vec::new();
    let mut seen = HashMap::new();
    let legs = journey
        .get("legs")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();
    for leg in legs {
        let ty = leg
            .get("__typename")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if ty == "WalkLeg" {
            continue;
        }
        let Some(trip_id) = leg.get("tripId").and_then(|x| x.as_str()) else {
            continue;
        };
        if trip_id.is_empty() {
            continue;
        }
        if seen.contains_key(trip_id) {
            continue;
        }
        seen.insert(trip_id.to_string(), ());
        let mode = mode_str(leg.get("mode").unwrap_or(&Value::Null));
        let route = leg
            .get("routeShortName")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let trip_name = leg
            .get("tripShortName")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let route_color = leg
            .get("routeColor")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let route_long_name = leg
            .get("routeLongName")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        let label = if !trip_name.is_empty() {
            format!("{route} {trip_name}").trim().to_string()
        } else if !route.is_empty() {
            route.clone()
        } else {
            trip_id.to_string()
        };
        out.push(TrackedTrip {
            trip_id: trip_id.to_string(),
            mode,
            route,
            route_color,
            route_long_name,
            label,
        });
    }
    out
}

fn live_vehicles_enabled() -> bool {
    // Default on when the checkbox is missing (defensive).
    if el("live-vehicles").is_none() {
        return true;
    }
    checkbox_checked("live-vehicles")
}

fn set_live_empty(msg: Option<&str>) {
    if let Some(e) = el("live-vehicles-status") {
        match msg {
            Some(m) if !m.is_empty() => {
                e.set_inner_html(m);
                let _ = e.remove_attribute("hidden");
            }
            _ => {
                e.set_inner_html("");
                let _ = e.set_attribute("hidden", "true");
            }
        }
    }
}

fn set_live_badge(on: bool) {
    if let Some(e) = el("map-live-badge") {
        if on {
            let _ = e.remove_attribute("hidden");
        } else {
            let _ = e.set_attribute("hidden", "true");
        }
    }
}

fn close_live_ws() {
    APP.with(|app| {
        let mut a = app.borrow_mut();
        if let Some(ws) = a.live_ws.take() {
            let _ = ws.close();
        }
        a.live_ws_active = false;
        a.live_by_trip.clear();
    });
}

fn stop_live_tracking() {
    APP.with(|app| {
        let mut a = app.borrow_mut();
        a.live_gen = a.live_gen.wrapping_add(1);
        a.live_interval = None;
        a.tracked_trips.clear();
        if let Some(ws) = a.live_ws.take() {
            let _ = ws.close();
        }
        a.live_ws_active = false;
        a.live_by_trip.clear();
    });
    call_map("clearLiveVehicles", None);
    set_live_empty(None);
    set_live_badge(false);
}

/// Parse one `tripRealtime` / `watchTrips` object into a [`LiveVehicle`] using trip meta.
fn live_vehicle_from_rt(rt: &Value, meta: &TrackedTrip) -> Option<LiveVehicle> {
    let delay = rt.get("delaySeconds").and_then(|x| x.as_i64());
    let status = rt
        .get("status")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let v = rt.get("vehicle")?;
    if v.is_null() {
        return None;
    }
    let lat = v.get("lat").and_then(|x| x.as_f64())?;
    let lon = v.get("lon").and_then(|x| x.as_f64())?;
    let bearing = v.get("bearing").and_then(|x| x.as_f64());
    let label = v
        .get("label")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| Some(meta.label.clone()));
    let occupancy = v
        .get("occupancy")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let current_status = v
        .get("currentStatus")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let position_source = v
        .get("positionSource")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let is_estimated = v
        .get("isEstimated")
        .and_then(|x| x.as_bool())
        .or_else(|| {
            position_source
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("ESTIMATED"))
        })
        .or_else(|| {
            current_status
                .as_deref()
                .map(|s| s.to_ascii_uppercase().contains("ESTIMATED"))
        });
    let updated_at = v
        .get("updatedAt")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let color = meta.route_color.as_ref().map(|c| {
        let hex = c.trim_start_matches('#');
        format!("#{hex}")
    });
    let feed = meta.trip_id.split(':').next().map(|s| s.to_string());
    let display_from_api = v
        .get("displayLabel")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    Some(LiveVehicle {
        id: meta.trip_id.clone(),
        lat,
        lon,
        bearing,
        label: label.clone(),
        display_label: display_from_api.or_else(|| label.clone()).or_else(|| {
            if meta.route.is_empty() {
                None
            } else {
                Some(meta.route.clone())
            }
        }),
        trip_short_name: v
            .get("tripShortName")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        vehicle_id: v
            .get("vehicleId")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        occupancy,
        current_status,
        delay_seconds: delay,
        status,
        mode: Some(meta.mode.clone()),
        route: if meta.route.is_empty() {
            None
        } else {
            Some(meta.route.clone())
        },
        updated_at,
        color,
        headsign: v
            .get("headsign")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        feed,
        route_color: meta.route_color.clone(),
        route_long_name: v
            .get("routeLongName")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| meta.route_long_name.clone()),
        position_source,
        is_estimated,
    })
}

fn apply_live_snapshot(vehicles: &[LiveVehicle], trips_len: usize) {
    if vehicles.is_empty() {
        call_map("clearLiveVehicles", None);
        set_live_badge(false);
        if trips_len == 0 {
            set_live_empty(None);
        } else {
            set_live_empty(Some(
                "Aucune position temps réel pour ces courses",
            ));
        }
    } else {
        set_live_empty(None);
        set_live_badge(true);
        push_live_vehicles(vehicles);
    }
}

/// Try `graphql-transport-ws` subscription `watchTrips`. Returns false if URL missing or open fails.
fn try_start_live_ws(gen: u64, trips: &[TrackedTrip]) -> bool {
    let Some(url) = graphql_ws_url() else {
        return false;
    };
    if trips.is_empty() {
        return false;
    }

    let ws = match WebSocket::new_with_str(&url, "graphql-transport-ws") {
        Ok(ws) => ws,
        Err(e) => {
            web_sys::console::warn_1(&JsValue::from_str(&format!(
                "live WS open failed ({url}): {e:?}"
            )));
            return false;
        }
    };
    let _ = ws.set_binary_type(web_sys::BinaryType::Blob);

    let trip_ids: Vec<String> = trips.iter().map(|t| t.trip_id.clone()).collect();
    let meta_map: HashMap<String, TrackedTrip> = trips
        .iter()
        .map(|t| (t.trip_id.clone(), t.clone()))
        .collect();

    let on_open = Closure::wrap(Box::new(move |_: web_sys::Event| {
        let still = APP.with(|app| app.borrow().live_gen == gen);
        if !still {
            return;
        }
        let msg = json!({
            "type": "connection_init",
            "payload": {}
        });
        APP.with(|app| {
            if let Some(ws) = app.borrow().live_ws.as_ref() {
                let _ = ws.send_with_str(&msg.to_string());
            }
        });
    }) as Box<dyn FnMut(_)>);

    let on_message = Closure::wrap(Box::new(move |e: web_sys::MessageEvent| {
        let still = APP.with(|app| app.borrow().live_gen == gen);
        if !still {
            return;
        }
        let Some(text) = e.data().as_string() else {
            return;
        };
        let Ok(msg) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "connection_ack" => {
                let sub = json!({
                    "type": "subscribe",
                    "id": "watchTrips",
                    "payload": {
                        "query": r#"
                          subscription($ids: [ID!]!) {
                            watchTrips(tripIds: $ids) {
                              tripId delaySeconds canceled status
                              vehicle {
                                lat lon bearing speed updatedAt
                                label displayLabel occupancy currentStatus vehicleId
                                tripShortName headsign mode routeColor
                                positionSource isEstimated
                              }
                            }
                          }
                        "#,
                        "variables": { "ids": trip_ids }
                    }
                });
                APP.with(|app| {
                    let mut a = app.borrow_mut();
                    a.live_ws_active = true;
                    if let Some(ws) = a.live_ws.as_ref() {
                        let _ = ws.send_with_str(&sub.to_string());
                    }
                });
                web_sys::console::log_1(&JsValue::from_str(
                    "live WS: subscribed watchTrips (poll remains soft fallback)",
                ));
            }
            "ping" => {
                let pong = json!({ "type": "pong" });
                APP.with(|app| {
                    if let Some(ws) = app.borrow().live_ws.as_ref() {
                        let _ = ws.send_with_str(&pong.to_string());
                    }
                });
            }
            "next" => {
                let Some(payload) = msg.get("payload") else {
                    return;
                };
                // payload.data.watchTrips is a single TripRealtime per emission
                let rt = payload
                    .pointer("/data/watchTrips")
                    .cloned()
                    .or_else(|| payload.get("data").and_then(|d| d.get("watchTrips")).cloned());
                let Some(rt) = rt else {
                    return;
                };
                let tid = rt
                    .get("tripId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if tid.is_empty() {
                    return;
                }
                let Some(meta) = meta_map.get(&tid) else {
                    return;
                };
                if let Some(lv) = live_vehicle_from_rt(&rt, meta) {
                    let vehicles = APP.with(|app| {
                        let mut a = app.borrow_mut();
                        a.live_by_trip.insert(tid, lv);
                        a.live_by_trip.values().cloned().collect::<Vec<_>>()
                    });
                    apply_live_snapshot(&vehicles, meta_map.len());
                }
            }
            "error" | "complete" => {
                web_sys::console::warn_1(&JsValue::from_str(&format!(
                    "live WS {ty}: {text}"
                )));
                APP.with(|app| {
                    app.borrow_mut().live_ws_active = false;
                });
            }
            _ => {}
        }
    }) as Box<dyn FnMut(_)>);

    let on_error = Closure::wrap(Box::new(move |_: web_sys::Event| {
        web_sys::console::warn_1(&JsValue::from_str(
            "live WS error — soft fallback to poll",
        ));
        APP.with(|app| {
            app.borrow_mut().live_ws_active = false;
        });
    }) as Box<dyn FnMut(_)>);

    let on_close = Closure::wrap(Box::new(move |_: web_sys::CloseEvent| {
        APP.with(|app| {
            let mut a = app.borrow_mut();
            if a.live_gen == gen {
                a.live_ws_active = false;
            }
        });
    }) as Box<dyn FnMut(_)>);

    let _ = ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    let _ = ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    let _ = ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    let _ = ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_open.forget();
    on_message.forget();
    on_error.forget();
    on_close.forget();

    APP.with(|app| {
        let mut a = app.borrow_mut();
        a.live_ws = Some(ws);
        a.live_ws_active = false;
        a.live_by_trip.clear();
    });
    true
}

/// Build aliased GraphQL for multiple tripRealtime queries in one request.
async fn fetch_trip_vehicles(trips: &[TrackedTrip]) -> Result<Vec<LiveVehicle>, String> {
    if trips.is_empty() {
        return Ok(Vec::new());
    }
    // Cap concurrent tracked trips (mirrors server watchTrips limit spirit).
    let trips: Vec<&TrackedTrip> = trips.iter().take(16).collect();
    let mut query = String::from("query LiveVehicles(");
    let mut vars = serde_json::Map::new();
    let mut var_defs = Vec::new();
    let mut fields = Vec::new();
    for (i, t) in trips.iter().enumerate() {
        let vid = format!("id{i}");
        var_defs.push(format!("${vid}: ID!"));
        fields.push(format!(
            r#"t{i}: tripRealtime(tripId: ${vid}) {{
              tripId delaySeconds canceled status
              vehicle {{
                lat lon bearing speed updatedAt
                label displayLabel occupancy currentStatus vehicleId
                tripShortName headsign mode routeColor
                positionSource isEstimated
              }}
            }}"#
        ));
        vars.insert(vid, json!(t.trip_id));
    }
    query.push_str(&var_defs.join(", "));
    query.push_str(") {\n");
    query.push_str(&fields.join("\n"));
    query.push_str("\n}");

    let data = gql(&query, Value::Object(vars)).await?;
    let mut out = Vec::new();
    for (i, t) in trips.iter().enumerate() {
        let key = format!("t{i}");
        let Some(rt) = data.get(&key) else {
            continue;
        };
        if let Some(lv) = live_vehicle_from_rt(rt, t) {
            out.push(lv);
        }
    }
    Ok(out)
}

fn push_live_vehicles(vehicles: &[LiveVehicle]) {
    if let Ok(s) = serde_json::to_string(vehicles) {
        call_map("setLiveVehicles", Some(&s));
    }
}

fn poll_live_once(gen: u64, trips: Vec<TrackedTrip>) {
    wasm_bindgen_futures::spawn_local(async move {
        match fetch_trip_vehicles(&trips).await {
            Ok(vehicles) => {
                let still = APP.with(|app| {
                    let a = app.borrow();
                    a.live_gen == gen
                });
                if !still {
                    return;
                }
                // Prefer fresher WS merge when active and already showing positions.
                let ws_has = APP.with(|app| {
                    let a = app.borrow();
                    a.live_ws_active && !a.live_by_trip.is_empty()
                });
                if ws_has && vehicles.is_empty() {
                    return;
                }
                if !vehicles.is_empty() {
                    APP.with(|app| {
                        let mut a = app.borrow_mut();
                        for v in &vehicles {
                            a.live_by_trip.insert(v.id.clone(), v.clone());
                        }
                    });
                }
                let merged = APP.with(|app| {
                    let a = app.borrow();
                    if a.live_by_trip.is_empty() {
                        vehicles.clone()
                    } else {
                        a.live_by_trip.values().cloned().collect()
                    }
                });
                apply_live_snapshot(&merged, trips.len());
            }
            Err(e) => {
                let still = APP.with(|app| app.borrow().live_gen == gen);
                if still {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "live vehicles poll: {e}"
                    )));
                }
            }
        }
    });
}

/// Start / restart live VP: poll always; optional WS subscription when available.
fn restart_live_tracking() {
    let enabled = live_vehicles_enabled();
    let trips = APP.with(|app| {
        let a = app.borrow();
        match a.selected {
            Some(idx) if idx < a.journeys.len() => journey_tracked_trips(&a.journeys[idx]),
            _ => Vec::new(),
        }
    });

    if !enabled || trips.is_empty() {
        stop_live_tracking();
        if enabled {
            // Journey selected but no trip ids (walk-only) — keep quiet.
            if APP.with(|app| app.borrow().selected.is_some()) {
                set_live_empty(None);
            }
        }
        return;
    }

    close_live_ws();

    let gen = APP.with(|app| {
        let mut a = app.borrow_mut();
        a.live_gen = a.live_gen.wrapping_add(1);
        a.tracked_trips = trips.clone();
        a.live_interval = None;
        a.live_by_trip.clear();
        a.live_gen
    });

    // Optional WS path (soft): failures leave poll as sole source.
    let _ws = try_start_live_ws(gen, &trips);

    // Poll: immediate + interval (soft fallback / backup even if WS is up).
    poll_live_once(gen, trips.clone());

    let trips_for_interval = trips;
    let interval = gloo_timers::callback::Interval::new(LIVE_POLL_MS, move || {
        let (g, t) = APP.with(|app| {
            let a = app.borrow();
            (a.live_gen, a.tracked_trips.clone())
        });
        if g != gen {
            return;
        }
        if !live_vehicles_enabled() {
            return;
        }
        let list = if t.is_empty() {
            trips_for_interval.clone()
        } else {
            t
        };
        poll_live_once(g, list);
    });

    APP.with(|app| {
        app.borrow_mut().live_interval = Some(interval);
    });
}

// ─── Global live network (“Réseau en direct”) ────────────────────────────────

fn global_live_enabled() -> bool {
    if el("global-live").is_none() {
        return true;
    }
    checkbox_checked("global-live")
}

fn set_global_live_empty(msg: Option<&str>) {
    if let Some(e) = el("global-live-status") {
        match msg {
            Some(m) if !m.is_empty() => {
                e.set_inner_html(m);
                let _ = e.remove_attribute("hidden");
            }
            _ => {
                e.set_inner_html("");
                let _ = e.set_attribute("hidden", "true");
            }
        }
    }
}

fn set_global_live_badge(count: Option<usize>) {
    if let Some(e) = el("map-global-live-badge") {
        match count {
            Some(n) if n > 0 => {
                if let Some(span) = el("map-global-live-count") {
                    let label = if n == 1 {
                        "1 véhicule en direct".to_string()
                    } else {
                        format!("{n} véhicules en direct")
                    };
                    span.set_inner_html(&label);
                }
                let _ = e.remove_attribute("hidden");
            }
            _ => {
                let _ = e.set_attribute("hidden", "true");
            }
        }
    }
}

fn stop_global_live() {
    APP.with(|app| {
        let mut a = app.borrow_mut();
        a.global_live_gen = a.global_live_gen.wrapping_add(1);
        a.global_live_interval = None;
        a.global_live_move_timer = None;
    });
    call_map("clearGlobalLiveVehicles", None);
    set_global_live_empty(None);
    set_global_live_badge(None);
}

fn feed_from_trip_id(trip_id: &str) -> Option<String> {
    trip_id
        .split_once(':')
        .map(|(f, _)| f.to_string())
        .filter(|f| !f.is_empty())
}

fn live_vehicle_from_global(v: &Value) -> Option<LiveVehicle> {
    let lat = v.get("lat").and_then(|x| x.as_f64())?;
    let lon = v.get("lon").and_then(|x| x.as_f64())?;
    let trip_id = v
        .get("tripId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let vehicle_id = v
        .get("vehicleId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let trip_short = v
        .get("tripShortName")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let id = trip_id
        .clone()
        .or_else(|| vehicle_id.clone())
        .unwrap_or_else(|| format!("{lat},{lon}"));
    let display_label = v
        .get("displayLabel")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let label = v
        .get("label")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| trip_short.clone())
        .or_else(|| vehicle_id.clone());
    let feed = v
        .get("feedId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| trip_id.as_deref().and_then(feed_from_trip_id));
    let route_color = v
        .get("routeColor")
        .and_then(|x| x.as_str())
        .map(|s| s.trim_start_matches('#').to_string());
    let color = route_color.as_ref().map(|c| format!("#{c}"));
    let updated_at = v
        .get("updatedAt")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let status = if v.get("canceled").and_then(|x| x.as_bool()) == Some(true) {
        Some("CANCELED".to_string())
    } else {
        None
    };
    Some(LiveVehicle {
        id,
        lat,
        lon,
        bearing: v.get("bearing").and_then(|x| x.as_f64()),
        label: label.clone(),
        display_label: display_label.or_else(|| label.clone()).or(trip_short.clone()),
        trip_short_name: trip_short,
        vehicle_id,
        occupancy: v
            .get("occupancy")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        current_status: v
            .get("currentStatus")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        delay_seconds: v.get("delaySeconds").and_then(|x| x.as_i64()),
        status,
        mode: v
            .get("mode")
            .and_then(|x| x.as_str())
            .map(|s| s.to_uppercase()),
        route: v
            .get("routeShortName")
            .or_else(|| v.get("route"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        updated_at,
        color,
        headsign: v
            .get("headsign")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        feed,
        route_color,
        route_long_name: v
            .get("routeLongName")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty()),
        position_source: v
            .get("positionSource")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        is_estimated: v.get("isEstimated").and_then(|x| x.as_bool()).or_else(|| {
            v.get("positionSource")
                .and_then(|x| x.as_str())
                .map(|s| s.eq_ignore_ascii_case("ESTIMATED"))
        }),
    })
}

async fn fetch_global_vehicles(bbox: Option<&MapBounds>) -> Result<Vec<LiveVehicle>, String> {
    // Schema name is BboxInput (async-graphql), not BBoxInput.
    let q = r#"
      query GlobalVehicles($limit: Int, $bbox: BboxInput) {
        vehicles(limit: $limit, bbox: $bbox) {
          tripId lat lon bearing speed updatedAt
          label displayLabel vehicleId occupancy currentStatus congestion
          occupancyPercentage currentStopId feedId
          routeShortName routeLongName tripShortName headsign
          mode routeColor delaySeconds canceled
          positionSource isEstimated
        }
      }
    "#;
    let mut vars = json!({ "limit": GLOBAL_LIVE_LIMIT });
    if let Some(b) = bbox {
        vars["bbox"] = json!({
            "minLat": b.min_lat,
            "minLon": b.min_lon,
            "maxLat": b.max_lat,
            "maxLon": b.max_lon,
        });
    }
    let data = gql(q, vars).await?;
    let arr = data
        .get("vehicles")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        if let Some(lv) = live_vehicle_from_global(&v) {
            out.push(lv);
        }
    }
    Ok(out)
}

fn apply_global_live_snapshot(vehicles: &[LiveVehicle]) {
    if vehicles.is_empty() {
        call_map("clearGlobalLiveVehicles", None);
        set_global_live_badge(None);
        set_global_live_empty(Some(
            "Aucune position temps réel (beaucoup de flux SNCF n’ont pas de VehiclePositions ; l’API peut estimer via trip updates)",
        ));
    } else {
        set_global_live_empty(None);
        set_global_live_badge(Some(vehicles.len()));
        if let Ok(s) = serde_json::to_string(vehicles) {
            call_map("setGlobalLiveVehicles", Some(&s));
        }
    }
}

fn poll_global_live_once(gen: u64) {
    wasm_bindgen_futures::spawn_local(async move {
        let bounds = map_get_bounds();

        match fetch_global_vehicles(bounds.as_ref()).await {
            Ok(vehicles) => {
                let still = APP.with(|app| app.borrow().global_live_gen == gen);
                if !still || !global_live_enabled() {
                    return;
                }
                apply_global_live_snapshot(&vehicles);
            }
            Err(e) => {
                let still = APP.with(|app| app.borrow().global_live_gen == gen);
                if still && global_live_enabled() {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "global live vehicles poll: {e}"
                    )));
                }
            }
        }
    });
}

fn restart_global_live() {
    if !global_live_enabled() {
        stop_global_live();
        return;
    }

    let gen = APP.with(|app| {
        let mut a = app.borrow_mut();
        a.global_live_gen = a.global_live_gen.wrapping_add(1);
        a.global_live_interval = None;
        a.global_live_move_timer = None;
        a.global_live_gen
    });

    poll_global_live_once(gen);

    let interval = gloo_timers::callback::Interval::new(GLOBAL_LIVE_POLL_MS, move || {
        let g = APP.with(|app| app.borrow().global_live_gen);
        if g != gen {
            return;
        }
        if !global_live_enabled() {
            return;
        }
        poll_global_live_once(g);
    });

    APP.with(|app| {
        app.borrow_mut().global_live_interval = Some(interval);
    });
}

/// Soft-register viewport SM stops + ET LineRefs (no PRIM HTTP). Fire-and-forget.
fn warm_live_viewport_async(bounds: Option<MapBounds>) {
    let Some(b) = bounds else {
        return;
    };
    wasm_bindgen_futures::spawn_local(async move {
        let q = r#"
          query WarmLiveViewport($bbox: BboxInput!) {
            warmLiveViewport(bbox: $bbox)
          }
        "#;
        let vars = json!({
            "bbox": {
                "minLat": b.min_lat,
                "minLon": b.min_lon,
                "maxLat": b.max_lat,
                "maxLon": b.max_lon,
            }
        });
        if let Err(e) = gql(q, vars).await {
            web_sys::console::warn_1(&JsValue::from_str(&format!(
                "warmLiveViewport: {e}"
            )));
        }
    });
}

/// Debounced refresh after pan/zoom (does not restart the interval).
fn on_global_live_map_move() {
    let gen = APP.with(|app| app.borrow().global_live_gen);
    APP.with(|app| {
        let mut a = app.borrow_mut();
        a.global_live_move_timer = None;
    });
    let timeout = gloo_timers::callback::Timeout::new(400, move || {
        let bounds = map_get_bounds();
        // Always soft-warm PRIM poller interest for visible lines (even if global live off).
        warm_live_viewport_async(bounds.clone());
        let still = APP.with(|app| app.borrow().global_live_gen == gen);
        if still && global_live_enabled() {
            poll_global_live_once(gen);
        }
    });
    APP.with(|app| {
        app.borrow_mut().global_live_move_timer = Some(timeout);
    });
}

// ─── UI rendering ───────────────────────────────────────────────────────────

fn render_legend() {
    let modes = [
        ("train", "Train"),
        ("metro", "Métro"),
        ("tram", "Tram"),
        ("bus", "Bus"),
        ("walk", "Marche"),
        ("bike", "Vélo"),
        ("ferry", "Ferry"),
        ("wheelchair", "Fauteuil"),
        ("alert", "Alerte"),
    ];
    let mut html = String::new();
    for (icon, label) in modes {
        html.push_str(&format!(
            r#"<li><img src="assets/icons/{icon}.svg" alt="{label}"/><span>{label}</span></li>"#
        ));
    }
    set_html("mode-legend", &html);
}

fn format_fare(amount: f64, currency: Option<&str>) -> String {
    let cur = currency.unwrap_or("EUR");
    if (amount - amount.round()).abs() < 1e-9 {
        format!("{:.0} {cur}", amount)
    } else {
        format!("{amount:.2} {cur}")
    }
}

fn render_journeys(app: &App) {
    if app.journeys.is_empty() {
        set_html(
            "journeys",
            r#"<div class="empty">Aucun itinéraire. Recherchez une gare ou une ville, puis calculez.</div>"#,
        );
        return;
    }
    let mut html = String::new();
    for (i, j) in app.journeys.iter().enumerate() {
        let dep_arr_html = journey_times_html(j);
        let dur = j
            .get("durationSeconds")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let transfers = j
            .get("transfers")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let rt = j
            .get("realtimeStatus")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let rt_label = realtime_status_label(rt);
        let fare_html = match j.get("fareAmount").and_then(|x| x.as_f64()) {
            Some(amount) => {
                let cur = j.get("fareCurrency").and_then(|x| x.as_str());
                let note = j.get("fareNote").and_then(|x| x.as_str()).unwrap_or("");
                let title = if note.is_empty() {
                    String::new()
                } else {
                    format!(r#" title="{}""#, escape_html(note))
                };
                format!(
                    r#" · <span class="fare-badge"{title}>{}</span>"#,
                    escape_html(&format_fare(amount, cur)),
                    title = title
                )
            }
            None => String::new(),
        };
        let selected = app.selected == Some(i);
        let cls = if selected {
            "journey-card selected"
        } else {
            "journey-card"
        };
        let mut legs_html = String::from(r#"<div class="legs">"#);
        if let Some(legs) = j.get("legs").and_then(|l| l.as_array()) {
            for leg in legs {
                let ty = leg
                    .get("__typename")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if ty == "WalkLeg" {
                    let dist = leg
                        .get("distanceMeters")
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0);
                    let dur_s = leg
                        .get("durationSeconds")
                        .and_then(|x| x.as_i64())
                        .unwrap_or(0);
                    let leg_mode = leg
                        .get("mode")
                        .and_then(|x| x.as_str())
                        .unwrap_or("WALK")
                        .to_ascii_uppercase();
                    let is_bike = leg_mode == "BIKE";
                    let walk_icon = if is_bike {
                        mode_icon_path("bike")
                    } else if app.plan_wheelchair {
                        mode_icon_path("wheelchair")
                    } else {
                        mode_icon_path("walk")
                    };
                    let walk_label = if is_bike {
                        "Vélo"
                    } else if app.plan_wheelchair {
                        "Marche (PMR)"
                    } else {
                        "Marche"
                    };
                    legs_html.push_str(&format!(
                        r#"<div class="leg"><img class="mode-icon" src="{}" alt="{}" title="{}"/><div>{} · {:.0} m · {}</div></div>"#,
                        walk_icon,
                        if app.plan_wheelchair {
                            "fauteuil roulant"
                        } else {
                            "marche"
                        },
                        if app.plan_wheelchair {
                            "Correspondance à pied adaptée fauteuil roulant"
                        } else {
                            "Marche"
                        },
                        walk_label,
                        dist,
                        format_duration(dur_s)
                    ));
                } else {
                    let mode = mode_str(leg.get("mode").unwrap_or(&Value::Null));
                    let route = leg
                        .get("routeShortName")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    let trip = leg
                        .get("tripShortName")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    let from = leg
                        .pointer("/fromStop/name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    let to = leg
                        .pointer("/toStop/name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    let color = leg
                        .get("routeColor")
                        .and_then(|x| x.as_str())
                        .unwrap_or("334155");
                    let color = color.trim_start_matches('#');
                    let text = leg
                        .get("routeTextColor")
                        .and_then(|x| x.as_str())
                        .unwrap_or("FFFFFF");
                    let text = text.trim_start_matches('#');
                    let chip = if route.is_empty() {
                        String::new()
                    } else {
                        format!(
                            r#"<span class="route-chip" style="background:#{color};color:#{text}">{route}</span>"#
                        )
                    };
                    let canceled = leg
                        .get("canceled")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let delay = leg
                        .get("delayDepartureSeconds")
                        .and_then(|x| x.as_i64());
                    let same_vehicle = leg
                        .get("sameVehicle")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let leg_wc = leg.get("wheelchair").and_then(|x| x.as_i64());
                    let mut extra = String::new();
                    if same_vehicle {
                        extra.push_str(
                            r#" <span class="same-vehicle-badge" title="Même véhicule (sans descente)">Même véhicule</span>"#,
                        );
                    }
                    if app.plan_wheelchair {
                        match leg_wc {
                            Some(1) => {
                                extra.push_str(&format!(
                                    r#" <img class="leg-wc-icon" src="{}" alt="Accessible" title="Course accessible fauteuil roulant"/>"#,
                                    mode_icon_path("wheelchair")
                                ));
                            }
                            Some(2) => {
                                extra.push_str(
                                    r#" <span class="leg-wc-unknown" title="Non accessible selon GTFS">PMR non</span>"#,
                                );
                            }
                            _ => {
                                extra.push_str(&format!(
                                    r#" <img class="leg-wc-icon leg-wc-icon--soft" src="{}" alt="PMR" title="Accessibilité non renseignée"/>"#,
                                    mode_icon_path("wheelchair")
                                ));
                            }
                        }
                    }
                    if canceled {
                        extra.push_str(r#" <span class="badge-danger">supprimé</span>"#);
                    } else if let Some(d) = delay {
                        if d != 0 {
                            extra.push_str(&format!(
                                r#" <span class="delay">{}</span>"#,
                                format_delay_minutes(d)
                            ));
                        }
                    }
                    let leg_alerts = leg
                        .get("alerts")
                        .and_then(|a| a.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    if leg_alerts > 0 {
                        extra.push_str(&format!(
                            r#" <span class="leg-alert-sign" title="Perturbation sur cette course">⚠</span>"#
                        ));
                    }
                    legs_html.push_str(&format!(
                        r#"<div class="leg{leg_cls}"><img class="mode-icon" src="{icon}" alt="{mode_alt}"/><div>{chip}{mode} {trip} · {times} · {from} → {to}{extra}</div></div>"#,
                        icon = mode_icon_path(&mode),
                        mode_alt = escape_html(&mode),
                        chip = chip,
                        mode = escape_html(&mode),
                        trip = escape_html(trip),
                        times = leg_times_compact_html(leg),
                        from = escape_html(from),
                        to = escape_html(to),
                        extra = extra,
                        leg_cls = if canceled {
                            " leg--canceled"
                        } else if leg_alerts > 0 || delay.unwrap_or(0) != 0 {
                            " leg--affected"
                        } else {
                            ""
                        }
                    ));
                }
            }
        }
        legs_html.push_str("</div>");

        let transfer_label = if transfers == 0 {
            "direct".to_string()
        } else if transfers == 1 {
            "1 correspondance".to_string()
        } else {
            format!("{transfers} correspondances")
        };

        let wc_badge = if app.plan_wheelchair {
            wheelchair_badge_html()
        } else {
            String::new()
        };

        let legs_html = if app.plan_wheelchair {
            legs_html.replace(r#"class="leg"#, r#"class="leg leg--pmr"#)
        } else {
            legs_html
        };

        let impact = journey_impact_html(j);
        let disruptions = journey_disruption_messages(j);
        let disrupt_html = if !disruptions.is_empty() {
            disruption_panel_html(&disruptions, false)
        } else {
            String::new()
        };
        let affected_cls = if !impact.is_empty() || !disruptions.is_empty() {
            " journey-card--affected"
        } else {
            ""
        };

        let rank = i + 1;
        html.push_str(&format!(
            r#"<article class="{cls}{affected_cls}" data-journey-idx="{i}" role="button" tabindex="0" aria-pressed="{sel}">
              <div class="journey-card-top">
                <span class="journey-rank">{rank}</span>
                <div class="journey-times">
                  {dep_arr_html}
                  <span class="journey-dur badge">{dur}</span>
                </div>
                {wc_top}
              </div>
              {impact}
              <div class="meta">
                {wc_badge}
                <span class="badge badge-outline">{transfer_label}</span>
                <span class="rt-pill">{rt_label}</span>{fare_html}
              </div>
              {legs_html}
              {disrupt_html}
            </article>"#,
            cls = cls,
            affected_cls = affected_cls,
            i = i,
            rank = rank,
            sel = if selected { "true" } else { "false" },
            dep_arr_html = dep_arr_html,
            dur = format_duration(dur),
            wc_top = if app.plan_wheelchair {
                format!(
                    r#"<img class="journey-wc-icon" src="{}" alt="Fauteuil roulant" title="Accessible fauteuil roulant"/>"#,
                    mode_icon_path("wheelchair")
                )
            } else {
                String::new()
            },
            impact = impact,
            wc_badge = wc_badge,
            transfer_label = transfer_label,
            rt_label = escape_html(&rt_label),
            fare_html = fare_html,
            legs_html = legs_html,
            disrupt_html = disrupt_html,
        ));
    }
    // Header count for shadcn-style list
    let pmr_note = if app.plan_wheelchair {
        r#" <span class="journeys-pmr-note"><img src="assets/icons/wheelchair.svg" alt="" width="12" height="12"/> Fauteuil roulant</span>"#
    } else {
        ""
    };
    let header = format!(
        r#"<div class="journeys-header"><span class="journeys-count">{} proposition{}</span>{pmr_note}<span class="journeys-hint">Cliquer pour afficher sur la carte</span></div>"#,
        app.journeys.len(),
        if app.journeys.len() > 1 { "s" } else { "" },
        pmr_note = pmr_note
    );
    set_html("journeys", &format!("{header}{html}"));

    // Bind click handlers (do not steal clicks on expandable disruption <details>)
    if let Some(container) = el("journeys") {
        let cards = container.query_selector_all(".journey-card").ok();
        if let Some(list) = cards {
            for i in 0..list.length() {
                if let Some(node) = list.item(i) {
                    if let Ok(elem) = node.dyn_into::<Element>() {
                        let idx = elem
                            .get_attribute("data-journey-idx")
                            .and_then(|s| s.parse::<usize>().ok());
                        if let Some(idx) = idx {
                            let closure = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
                                if let Some(t) = e.target() {
                                    if let Ok(el) = t.dyn_into::<web_sys::Element>() {
                                        if el.closest("details.disruption-panel").ok().flatten().is_some()
                                            || el.closest("summary").ok().flatten().is_some()
                                        {
                                            e.stop_propagation();
                                            return;
                                        }
                                    }
                                }
                                select_journey(idx);
                            })
                                as Box<dyn FnMut(_)>);
                            let _ = elem.add_event_listener_with_callback(
                                "click",
                                closure.as_ref().unchecked_ref(),
                            );
                            closure.forget();
                        }
                    }
                }
            }
        }
    }
}

fn render_departures(deps: &Value) {
    let arr = deps.as_array();
    let Some(arr) = arr else {
        set_html("departures", r#"<div class="empty">—</div>"#);
        return;
    };
    if arr.is_empty() {
        set_html(
            "departures",
            r#"<div class="empty">Aucun départ dans la fenêtre.</div>"#,
        );
        return;
    }
    let mut html = String::new();
    for d in arr {
        let mode = mode_str(d.get("mode").unwrap_or(&Value::Null));
        let route = d
            .get("routeShortName")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let head = d.get("headsign").and_then(|x| x.as_str()).unwrap_or("");
        let trip = d
            .get("tripShortName")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let sched = d
            .get("scheduledDeparture")
            .and_then(|x| x.as_str())
            .map(format_time)
            .unwrap_or_else(|| "—".into());
        let rt = d
            .get("realtimeDeparture")
            .and_then(|x| x.as_str())
            .map(format_time);
        let delay = d.get("delaySeconds").and_then(|x| x.as_i64());
        let canceled = d.get("canceled").and_then(|x| x.as_bool()).unwrap_or(false);
        let platform = d
            .get("platform")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let source = d.get("source").and_then(|x| x.as_str()).unwrap_or("");
        let status = d.get("status").and_then(|x| x.as_str()).unwrap_or("");
        let live_badge = if source.contains("siri") {
            r#"<span class="live-badge">LIVE</span>"#
        } else {
            ""
        };
        let time_cls = if canceled { "time canceled" } else { "time" };
        let show_time = rt.unwrap_or(sched.clone());
        let mut delay_html = String::new();
        if let Some(dsec) = delay {
            if dsec != 0 && !canceled {
                delay_html = format!(r#"<span class="delay">{:+}s</span>"#, dsec);
            }
        }
        html.push_str(&format!(
            r#"<div class="dep-row">
              <span class="{time_cls}">{show_time} {live_badge}</span>
              <span><img class="mode-icon" src="{icon}" alt="" style="width:16px;height:16px;vertical-align:middle;margin-right:4px"/> <b>{route}</b> {trip} {head}</span>
              <span>{platform} {status} {delay_html}</span>
            </div>"#,
            time_cls = time_cls,
            show_time = show_time,
            live_badge = live_badge,
            icon = mode_icon_path(&mode),
            route = escape_html(route),
            trip = escape_html(trip),
            head = escape_html(head),
            platform = escape_html(platform),
            status = escape_html(status),
            delay_html = delay_html
        ));
    }
    set_html("departures", &html);
}

fn show_traffic_bar() {
    if let Some(b) = el("traffic-ticker-bar") {
        let _ = b.remove_attribute("hidden");
    }
}

/// Coloured line/train badge HTML for the traffic ticker (e.g. blue square « B »).
fn traffic_line_badge_html(line: &Value) -> Option<String> {
    let short = line
        .get("shortName")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    // Skip raw IDFM product codes if they ever leak through.
    if short.len() >= 5
        && short
            .chars()
            .next()
            .is_some_and(|c| c.eq_ignore_ascii_case(&'C'))
        && short.chars().skip(1).all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let bg = line
        .get("color")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().trim_start_matches('#').to_string())
        .filter(|s| s.len() == 6)
        .unwrap_or_else(|| "475569".into());
    let fg = line
        .get("textColor")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().trim_start_matches('#').to_string())
        .filter(|s| s.len() == 6)
        .unwrap_or_else(|| "FFFFFF".into());
    let label = escape_html(short);
    let title = escape_html(short);
    Some(format!(
        r#"<span class="traffic-line-badge" style="background-color:#{bg};color:#{fg}" title="Ligne {title}">{label}</span>"#
    ))
}

fn render_traffic_messages(msgs: &Value) {
    show_traffic_bar();
    let arr = msgs.as_array();
    let Some(arr) = arr else {
        set_html(
            "traffic-messages",
            r#"<span class="traffic-ticker-seg">Info trafic indisponible</span>"#,
        );
        return;
    };
    if arr.is_empty() {
        // Keep bar visible; RT alerts often arrive after first poll.
        set_html(
            "traffic-messages",
            r#"<span class="traffic-ticker-seg">Chargement des infos trafic…</span>"#,
        );
        return;
    }
    // Build one continuous line for the marquee (right → left), with line badges.
    let mut parts: Vec<String> = Vec::new();
    for m in arr.iter().take(40) {
        let id = m.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let src = if id.starts_with("idfm:") || id.contains(":idfm:") {
            "ÎDF"
        } else if id.starts_with("sncf:") {
            "SNCF"
        } else {
            ""
        };
        let header = m
            .get("header")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Info trafic");
        let desc = m
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim();
        let sev = m.get("severity").and_then(|x| x.as_str()).unwrap_or("");

        let mut badges = String::new();
        if let Some(lines) = m.get("lines").and_then(|x| x.as_array()) {
            for line in lines.iter().take(6) {
                if let Some(b) = traffic_line_badge_html(line) {
                    badges.push_str(&b);
                }
            }
        }

        let mut text = if src.is_empty() {
            header.to_string()
        } else {
            format!("[{src}] {header}")
        };
        if !desc.is_empty() && desc != header {
            let short = if desc.chars().count() > 140 {
                let t: String = desc.chars().take(137).collect();
                format!("{t}…")
            } else {
                desc.to_string()
            };
            text.push_str(" — ");
            text.push_str(&short);
        }
        if !sev.is_empty() {
            text.push_str(&format!(" [{sev}]"));
        }

        let item = if badges.is_empty() {
            format!(
                r#"<span class="traffic-ticker-item"><span class="traffic-ticker-text">{}</span></span>"#,
                escape_html(&text)
            )
        } else {
            format!(
                r#"<span class="traffic-ticker-item"><span class="traffic-line-badges">{badges}</span><span class="traffic-ticker-text">{}</span></span>"#,
                escape_html(&text)
            )
        };
        parts.push(item);
    }
    if parts.is_empty() {
        set_html(
            "traffic-messages",
            r#"<span class="traffic-ticker-seg">Aucune info trafic pour le moment</span>"#,
        );
        return;
    }
    let sep = r#"<span class="traffic-ticker-sep" aria-hidden="true"> · </span>"#;
    let line = parts.join(sep);
    // Duplicate for seamless loop
    let html = format!(
        r#"<span class="traffic-ticker-seg">{line}</span>{sep}<span class="traffic-ticker-seg">{line}</span>"#,
        line = line,
        sep = sep
    );
    set_html("traffic-messages", &html);
}

/// Fetch traffic once; safe to call repeatedly.
fn refresh_traffic_messages() {
    wasm_bindgen_futures::spawn_local(async {
        match fetch_traffic_messages(50).await {
            Ok(msgs) => render_traffic_messages(&msgs),
            Err(e) => {
                web_sys::console::warn_1(&format!("traffic messages: {e}").into());
                show_traffic_bar();
                set_html(
                    "traffic-messages",
                    &format!(
                        r#"<span class="traffic-ticker-seg">Info trafic: {}</span>"#,
                        escape_html(&e)
                    ),
                );
            }
        }
    });
}

// ─── Shared app cell ────────────────────────────────────────────────────────

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::new());
}

fn select_journey(idx: usize) {
    let journey = APP.with(|app| {
        let mut a = app.borrow_mut();
        if idx >= a.journeys.len() {
            return None;
        }
        a.selected = Some(idx);
        // Focus: hide network lines + global live; journey polylines remain.
        let payload = journey_to_map(&a.journeys[idx]);
        if let Ok(s) = serde_json::to_string(&payload) {
            call_map("drawJourney", Some(&s));
        }
        let j = a.journeys[idx].clone();
        render_journeys(&a);
        Some(j)
    });
    if let Some(j) = journey {
        show_trip_detail_card(&j);
    }
    // Global layer is suppressed while focused (map.js); keep poller warm if enabled.
    call_map("clearGlobalLiveVehicles", None);
    if live_vehicles_enabled() {
        restart_live_tracking();
    } else {
        stop_live_tracking();
        call_map("clearLiveVehicles", None);
    }
}

fn hide_trip_detail_card() {
    if let Some(card) = el("trip-detail-card") {
        let _ = card.set_attribute("hidden", "true");
    }
    if let Some(wrap) = el("map-wrap") {
        let _ = wrap.class_list().remove_1("trip-detail-open");
    }
}

fn show_trip_detail_card(journey: &Value) {
    let dep_arr_html = journey_times_html(journey);
    let dur = journey
        .get("durationSeconds")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let transfers = journey
        .get("transfers")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let walk_m = journey
        .get("walkDistanceMeters")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let rt = journey
        .get("realtimeStatus")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let xfer_label = match transfers {
        0 => "Direct".to_string(),
        1 => "1 correspondance".to_string(),
        n => format!("{n} correspondances"),
    };
    let fare = match journey.get("fareAmount").and_then(|x| x.as_f64()) {
        Some(a) => {
            let cur = journey
                .get("fareCurrency")
                .and_then(|x| x.as_str())
                .unwrap_or("EUR");
            format!(" · {}", format_fare(a, Some(cur)))
        }
        None => String::new(),
    };
    let walk_label = if walk_m >= 1.0 {
        format!(" · marche {:.0} m", walk_m)
    } else {
        String::new()
    };

    set_html(
        "trip-detail-title",
        &format!(
            r#"<span class="trip-time-wrap">{dep_arr}</span><span class="trip-dur">{dur}</span>"#,
            dep_arr = dep_arr_html,
            dur = escape_html(&format_duration(dur)),
        ),
    );
    let pmr = APP.with(|app| app.borrow().plan_wheelchair);
    let pmr_meta = if pmr {
        format!(
            r#" · <span class="wheelchair-badge wheelchair-badge--inline" title="Itinéraire calculé pour fauteuil roulant"><img src="{}" alt="" width="12" height="12"/> Fauteuil roulant</span>"#,
            mode_icon_path("wheelchair")
        )
    } else {
        String::new()
    };
    set_html(
        "trip-detail-meta",
        &format!(
            r#"{}{}{}{}{}"#,
            escape_html(&xfer_label),
            walk_label,
            fare,
            pmr_meta,
            if rt.is_empty() {
                String::new()
            } else {
                format!(
                    r#" · <span class="rt-pill">{}</span>"#,
                    escape_html(&realtime_status_label(rt))
                )
            }
        ),
    );

    let mut body = String::from(r#"<ol class="trip-timeline">"#);
    let mut leg_i = 0usize;
    if let Some(legs) = journey.get("legs").and_then(|l| l.as_array()) {
        for leg in legs {
            let ty = leg
                .get("__typename")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if ty == "WalkLeg" {
                let dist = leg
                    .get("distanceMeters")
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0);
                let dur_s = leg
                    .get("durationSeconds")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                let from = leg.get("fromName").and_then(|x| x.as_str()).unwrap_or("");
                let to = leg.get("toName").and_then(|x| x.as_str()).unwrap_or("");
                // Skip zero-length walks between same place
                if dist < 5.0 && from == to {
                    continue;
                }
                let pmr = APP.with(|app| app.borrow().plan_wheelchair);
                let leg_mode = leg
                    .get("mode")
                    .and_then(|x| x.as_str())
                    .unwrap_or("WALK")
                    .to_ascii_uppercase();
                let is_bike = leg_mode == "BIKE";
                let (icon, title) = if is_bike {
                    (mode_icon_path("bike"), "Vélo")
                } else if pmr {
                    (mode_icon_path("wheelchair"), "Marche (PMR)")
                } else {
                    (mode_icon_path("walk"), "Marche")
                };
                body.push_str(&format!(
                    r#"<li class="trip-step trip-step-walk{bike_cls}">
                      <div class="trip-step-icon"><img src="{icon}" alt="{title}"/></div>
                      <div class="trip-step-content">
                        <div class="trip-step-title">{title} · {dist:.0} m · {dur}</div>
                        <div class="trip-step-sub">{from} → {to}</div>
                      </div>
                    </li>"#,
                    bike_cls = if is_bike { " trip-step-bike" } else { "" },
                    icon = icon,
                    title = title,
                    dist = dist,
                    dur = escape_html(&format_duration(dur_s)),
                    from = escape_html(from),
                    to = escape_html(to),
                ));
            } else {
                leg_i += 1;
                let mode = mode_str(leg.get("mode").unwrap_or(&Value::Null));
                let route = leg
                    .get("routeShortName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let long = leg
                    .get("routeLongName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let head = leg
                    .get("stopHeadsign")
                    .or_else(|| leg.get("headsign"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let trip = leg
                    .get("tripShortName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let from_name = leg
                    .pointer("/fromStop/name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("—");
                let to_name = leg
                    .pointer("/toStop/name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("—");
                let canceled = leg
                    .get("canceled")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let dep_t = format_transit_time_html(
                    leg.get("scheduledDeparture").and_then(|x| x.as_str()),
                    leg.get("realtimeDeparture").and_then(|x| x.as_str()),
                    leg.get("delayDepartureSeconds").and_then(|x| x.as_i64()),
                    canceled,
                );
                let arr_t = format_transit_time_html(
                    leg.get("scheduledArrival").and_then(|x| x.as_str()),
                    leg.get("realtimeArrival").and_then(|x| x.as_str()),
                    leg.get("delayArrivalSeconds").and_then(|x| x.as_i64()),
                    canceled,
                );
                let color = leg
                    .get("routeColor")
                    .and_then(|x| x.as_str())
                    .unwrap_or("334155")
                    .trim_start_matches('#');
                let text = leg
                    .get("routeTextColor")
                    .and_then(|x| x.as_str())
                    .unwrap_or("FFFFFF")
                    .trim_start_matches('#');
                let delay = leg
                    .get("delayDepartureSeconds")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                let same_vehicle = leg
                    .get("sameVehicle")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);

                let chip = if route.is_empty() {
                    String::new()
                } else {
                    format!(
                        r#"<span class="route-chip" style="background:#{color};color:#{text}">{route}</span>"#,
                        color = escape_html(color),
                        text = escape_html(text),
                        route = escape_html(route),
                    )
                };
                let mode_label = match mode.as_str() {
                    "RAIL" | "TRAIN" => "Train / RER",
                    "METRO" | "SUBWAY" => "Métro",
                    "TRAM" => "Tram",
                    "BUS" | "COACH" => "Bus",
                    "FERRY" => "Ferry",
                    other => other,
                };
                let mut badges = String::new();
                if canceled {
                    badges.push_str(r#" <span class="badge-danger">supprimé</span>"#);
                } else if delay != 0 {
                    badges.push_str(&format!(
                        r#" <span class="delay">{}</span>"#,
                        format_delay_minutes(delay)
                    ));
                }
                if same_vehicle {
                    badges.push_str(
                        r#" <span class="same-vehicle-badge">même véhicule</span>"#,
                    );
                }
                if pmr {
                    match leg.get("wheelchair").and_then(|x| x.as_i64()) {
                        Some(1) => {
                            badges.push_str(&format!(
                                r#" <img class="leg-wc-icon" src="{}" alt="Accessible" title="Course accessible"/>"#,
                                mode_icon_path("wheelchair")
                            ));
                        }
                        Some(2) => {
                            badges.push_str(
                                r#" <span class="leg-wc-unknown">PMR non</span>"#,
                            );
                        }
                        _ => {
                            badges.push_str(&format!(
                                r#" <img class="leg-wc-icon leg-wc-icon--soft" src="{}" alt="PMR" title="Accessibilité non renseignée"/>"#,
                                mode_icon_path("wheelchair")
                            ));
                        }
                    }
                }

                // Intermediate stops
                let mut intermediates = String::new();
                if let Some(mids) = leg.get("intermediateStops").and_then(|a| a.as_array()) {
                    if !mids.is_empty() {
                        let n = mids.len();
                        let label = if n == 1 {
                            "1 arrêt intermédiaire".to_string()
                        } else {
                            format!("{n} arrêts intermédiaires")
                        };
                        intermediates.push_str(&format!(
                            r#"<details class="trip-stops"><summary>{}</summary><ul>"#,
                            escape_html(&label)
                        ));
                        for mid in mids {
                            let name = mid
                                .pointer("/stop/name")
                                .and_then(|x| x.as_str())
                                .unwrap_or("—");
                            let ta = format_transit_time_html(
                                mid.get("scheduledArrival")
                                    .or_else(|| mid.get("scheduledDeparture"))
                                    .and_then(|x| x.as_str()),
                                mid.get("realtimeArrival")
                                    .or_else(|| mid.get("realtimeDeparture"))
                                    .and_then(|x| x.as_str()),
                                None,
                                false,
                            );
                            intermediates.push_str(&format!(
                                r#"<li><span class="t">{}</span> {}</li>"#,
                                ta,
                                escape_html(name)
                            ));
                        }
                        intermediates.push_str("</ul></details>");
                    }
                }

                // Full disruption messages on this leg (expandable)
                let mut leg_msgs = Vec::new();
                if let Some(alerts) = leg.get("alerts").and_then(|a| a.as_array()) {
                    for al in alerts {
                        let h = al
                            .get("header")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let d = al
                            .get("description")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let sev = al
                            .get("severity")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        if !h.is_empty() || !d.is_empty() {
                            leg_msgs.push((h, d, sev));
                        }
                    }
                }
                let alerts_html = disruption_panel_html(&leg_msgs, !leg_msgs.is_empty());

                let head_line = if head.is_empty() {
                    String::new()
                } else {
                    format!(" · dir. {}", escape_html(head))
                };
                let trip_line = if trip.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", escape_html(trip))
                };
                let long_line = if long.is_empty() || long == route {
                    String::new()
                } else {
                    format!(" · {}", escape_html(long))
                };

                body.push_str(&format!(
                    r#"<li class="trip-step trip-step-transit">
                      <div class="trip-step-icon"><img src="{icon}" alt="{mode}"/></div>
                      <div class="trip-step-content">
                        <div class="trip-step-title">
                          {chip}<span class="mode-label">{mode_label}</span>{trip_line}{head_line}{badges}
                        </div>
                        <div class="trip-step-sub">{long_line}</div>
                        <div class="trip-step-od">
                          <div class="od-row"><span class="t dep">{dep_t}</span> <strong>{from}</strong></div>
                          <div class="od-row"><span class="t arr">{arr_t}</span> <strong>{to}</strong></div>
                        </div>
                        {intermediates}
                        {alerts}
                      </div>
                    </li>"#,
                    icon = mode_icon_path(&mode),
                    mode = escape_html(&mode),
                    chip = chip,
                    mode_label = escape_html(mode_label),
                    trip_line = trip_line,
                    head_line = head_line,
                    badges = badges,
                    long_line = long_line,
                    dep_t = dep_t,
                    arr_t = arr_t,
                    from = escape_html(from_name),
                    to = escape_html(to_name),
                    intermediates = intermediates,
                    alerts = alerts_html,
                ));
                let _ = leg_i;
            }
        }
    }

    // Journey-level disruptions (full text) at the top of the detail card.
    let journey_msgs = journey_disruption_messages(journey);
    let mut body_prefix = String::new();
    if !journey_msgs.is_empty() || !journey.get("realtimeStatus").and_then(|x| x.as_str()).unwrap_or("").is_empty() {
        let impact = journey_impact_html(journey);
        if !impact.is_empty() || !journey_msgs.is_empty() {
            body_prefix.push_str(r#"<div class="trip-detail-impact">"#);
            body_prefix.push_str(&impact);
            body_prefix.push_str(&disruption_panel_html(&journey_msgs, true));
            body_prefix.push_str("</div>");
        }
    }

    body.push_str("</ol>");
    set_html("trip-detail-body", &format!("{body_prefix}{body}"));

    if let Some(card) = el("trip-detail-card") {
        let _ = card.remove_attribute("hidden");
    }
    if let Some(wrap) = el("map-wrap") {
        let _ = wrap.class_list().add_1("trip-detail-open");
    }
}

fn show_place_suggest(list_id: &str, places: &[PlacePick], which: &'static str) {
    let Some(ul) = el(list_id) else {
        return;
    };
    if places.is_empty() {
        ul.set_inner_html("");
        let _ = ul.set_attribute("hidden", "true");
        return;
    }
    let mut html = String::new();
    for (i, p) in places.iter().enumerate() {
        let (kind, area) = if p.kind == "address" {
            let city = p.stop.code.as_deref().unwrap_or("");
            (
                "adresse",
                if city.is_empty() {
                    "BAN".to_string()
                } else {
                    format!("BAN · {city}")
                },
            )
        } else if p.stop.is_station {
            ("gare / pôle", "Île-de-France / SNCF".into())
        } else if p.stop.id.contains("monomodal") || p.stop.id.contains("StopPlace") {
            ("station", "transport".into())
        } else {
            ("arrêt", "transport".into())
        };
        html.push_str(&format!(
            r#"<li data-idx="{i}" data-which="{which}" class="suggest-{k}"><strong>{}</strong><span class="meta">{} · {}</span></li>"#,
            escape_html(&p.stop.name),
            kind,
            escape_html(&area),
            k = if p.kind == "address" { "addr" } else { "stop" },
        ));
    }
    ul.set_inner_html(&html);
    let _ = ul.remove_attribute("hidden");

    let items = ul.query_selector_all("li").ok();
    if let Some(list) = items {
        for i in 0..list.length() {
            if let Some(node) = list.item(i) {
                if let Ok(elem) = node.dyn_into::<Element>() {
                    let which = which;
                    let idx = elem
                        .get_attribute("data-idx")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let places_clone: Vec<PlacePick> = places.to_vec();
                    let list_id = list_id.to_string();
                    let closure = Closure::wrap(Box::new(move |_e: web_sys::MouseEvent| {
                        if let Some(pick) = places_clone.get(idx).cloned() {
                            on_place_picked(which, pick);
                        }
                        if let Some(ul) = el(&list_id) {
                            ul.set_inner_html("");
                            let _ = ul.set_attribute("hidden", "true");
                        }
                    })
                        as Box<dyn FnMut(_)>);
                    let _ = elem.add_event_listener_with_callback(
                        "click",
                        closure.as_ref().unchecked_ref(),
                    );
                    closure.forget();
                }
            }
        }
    }
}

fn show_suggest(list_id: &str, stops: &[Stop], which: &'static str) {
    let places: Vec<PlacePick> = stops
        .iter()
        .cloned()
        .map(|stop| PlacePick {
            kind: "stop".into(),
            stop,
        })
        .collect();
    show_place_suggest(list_id, &places, which);
}

fn on_stop_picked(which: &str, stop: Stop) {
    let input_id = if which == "origin" {
        "origin-input"
    } else {
        "dest-input"
    };
    if let Some(inp) = el(input_id).and_then(|e| e.dyn_into::<HtmlInputElement>().ok()) {
        inp.set_value(&stop.name);
    }
    if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
        // focusStop(lat, lon, title) via Reflect
        if let Some(win) = web_sys::window() {
            if let Ok(tm) = js_sys::Reflect::get(&win, &JsValue::from_str("TransitMap")) {
                if let Ok(func) = js_sys::Reflect::get(&tm, &JsValue::from_str("focusStop")) {
                    if let Ok(func) = func.dyn_into::<Function>() {
                        let _ = func.call3(
                            &tm,
                            &JsValue::from_f64(lat),
                            &JsValue::from_f64(lon),
                            &JsValue::from_str(&stop.name),
                        );
                    }
                }
            }
        }
    }

    on_place_picked(
        which,
        PlacePick {
            kind: "stop".into(),
            stop,
        },
    );
}

fn on_place_picked(which: &str, pick: PlacePick) {
    let input_id = if which == "origin" {
        "origin-input"
    } else {
        "dest-input"
    };
    if let Some(inp) = el(input_id).and_then(|e| e.dyn_into::<HtmlInputElement>().ok()) {
        inp.set_value(&pick.stop.name);
    }
    if let (Some(lat), Some(lon)) = (pick.stop.lat, pick.stop.lon) {
        if let Some(win) = web_sys::window() {
            if let Ok(tm) = js_sys::Reflect::get(&win, &JsValue::from_str("TransitMap")) {
                if let Ok(func) = js_sys::Reflect::get(&tm, &JsValue::from_str("focusStop")) {
                    if let Ok(func) = func.dyn_into::<Function>() {
                        let _ = func.call3(
                            &tm,
                            &JsValue::from_f64(lat),
                            &JsValue::from_f64(lon),
                            &JsValue::from_str(&pick.stop.name),
                        );
                    }
                }
            }
        }
    }
    let stop_id = pick.stop.id.clone();
    let kind = pick.kind.clone();
    APP.with(|app| {
        let mut a = app.borrow_mut();
        if which == "origin" {
            a.origin = Some(pick);
        } else {
            a.dest = Some(pick);
        }
    });
    if kind == "stop" && stop_id.contains(':') {
        wasm_bindgen_futures::spawn_local(async move {
            match fetch_departures(&stop_id, 12).await {
                Ok(deps) => render_departures(&deps),
                Err(e) => set_html(
                    "departures",
                    &format!(r#"<div class="empty">{}</div>"#, escape_html(&e)),
                ),
            }
        });
    }
}

fn schedule_search(which: &'static str, query: String) {
    APP.with(|app| {
        let mut a = app.borrow_mut();
        let list_id = if which == "origin" {
            "origin-suggest"
        } else {
            "dest-suggest"
        };
        // Clear previous spinner if user keeps typing before debounce fires
        set_field_loading(which, false);
        let which_load = which;
        let timeout = gloo_timers::callback::Timeout::new(280, move || {
            let q = query.trim().to_string();
            if q.chars().count() < 2 {
                set_field_loading(which_load, false);
                if let Some(ul) = el(list_id) {
                    ul.set_inner_html("");
                    let _ = ul.set_attribute("hidden", "true");
                }
                return;
            }
            set_field_loading(which_load, true);
            show_suggest_loading(list_id);
            wasm_bindgen_futures::spawn_local(async move {
                let result = search_places(&q, 14).await;
                set_field_loading(which_load, false);
                match result {
                    Ok(places) => {
                        if places.is_empty() {
                            if let Some(ul) = el(list_id) {
                                ul.set_inner_html(
                                    r#"<li class="suggest-empty">Aucun résultat</li>"#,
                                );
                                let _ = ul.remove_attribute("hidden");
                            }
                        } else {
                            show_place_suggest(list_id, &places, which_load);
                        }
                    }
                    Err(e) => {
                        if let Some(ul) = el(list_id) {
                            ul.set_inner_html(&format!(
                                r#"<li class="suggest-empty">{}</li>"#,
                                escape_html(&e)
                            ));
                            let _ = ul.remove_attribute("hidden");
                        }
                        if e.contains("timeout") || e.contains("TIMEOUT") {
                            set_status(
                                "Recherche trop longue. Réessayez avec un nom de gare plus court.",
                                true,
                            );
                        } else {
                            set_status(&e, true);
                        }
                    }
                }
            });
        });
        if which == "origin" {
            a.origin_timer = Some(timeout);
        } else {
            a.dest_timer = Some(timeout);
        }
    });
}

fn bind_input(id: &'static str, which: &'static str) {
    let Some(inp) = el(id) else {
        return;
    };
    let closure = Closure::wrap(Box::new(move |_e: web_sys::Event| {
        let q = input_value(id);
        schedule_search(which, q);
    }) as Box<dyn FnMut(_)>);
    let _ = inp.add_event_listener_with_callback("input", closure.as_ref().unchecked_ref());
    closure.forget();

    // Escape hides suggest
    let list_id = if which == "origin" {
        "origin-suggest"
    } else {
        "dest-suggest"
    };
    let key_closure = Closure::wrap(Box::new(move |e: KeyboardEvent| {
        if e.key() == "Escape" {
            if let Some(ul) = el(list_id) {
                ul.set_inner_html("");
                let _ = ul.set_attribute("hidden", "true");
            }
        }
    }) as Box<dyn FnMut(_)>);
    let _ = inp.add_event_listener_with_callback("keydown", key_closure.as_ref().unchecked_ref());
    key_closure.forget();
}

fn sync_time_field_label() {
    let arrive = checkbox_checked("arrive-by");
    if let Some(lab) = el("time-field-label") {
        lab.set_inner_html(if arrive {
            "Arriver avant (optionnel)"
        } else {
            "Heure de départ (optionnel)"
        });
    }
}

fn plan_clicked() {
    let (from, to) = APP.with(|app| {
        let a = app.borrow();
        (a.origin.clone(), a.dest.clone())
    });
    let Some(from) = from else {
        set_status(
            "Choisissez un départ (gare ou adresse) dans la liste.",
            true,
        );
        return;
    };
    let Some(to) = to else {
        set_status(
            "Choisissez une arrivée (gare ou adresse) dans la liste.",
            true,
        );
        return;
    };
    if from.kind == "stop"
        && to.kind == "stop"
        && from.stop.id == to.stop.id
    {
        set_status("Départ et arrivée doivent être différents.", true);
        return;
    }
    if from.kind == "address" && (from.stop.lat.is_none() || from.stop.lon.is_none()) {
        set_status("Adresse de départ sans coordonnées — resélectionnez.", true);
        return;
    }
    if to.kind == "address" && (to.stop.lat.is_none() || to.stop.lon.is_none()) {
        set_status("Adresse d’arrivée sans coordonnées — resélectionnez.", true);
        return;
    }

    let time_at = datetime_local_to_iso(&input_value("depart-at"));
    let arrive_by = checkbox_checked("arrive-by");
    let wheelchair = checkbox_checked("wheelchair");
    let bike_from = checkbox_checked("bike-from");
    let bike_to = checkbox_checked("bike-to");
    set_status("Recherche d’itinéraires…", false);
    set_plan_loading(true);

    wasm_bindgen_futures::spawn_local(async move {
        let result = plan_itineraries(
            &from,
            &to,
            time_at,
            arrive_by,
            wheelchair,
            bike_from,
            bike_to,
        )
        .await;
        set_plan_loading(false);
        match result {
            Ok(it) => {
                let degraded = it
                    .get("realtimeDegraded")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                let journeys = it
                    .get("journeys")
                    .and_then(|j| j.as_array())
                    .cloned()
                    .unwrap_or_default();
                let n = journeys.len();
                let first = APP.with(|app| {
                    let mut a = app.borrow_mut();
                    a.journeys = journeys;
                    a.plan_wheelchair = wheelchair;
                    a.selected = if n > 0 { Some(0) } else { None };
                    let first = if n > 0 {
                        Some(a.journeys[0].clone())
                    } else {
                        None
                    };
                    if let Some(0) = a.selected {
                        let payload = journey_to_map(&a.journeys[0]);
                        if let Ok(s) = serde_json::to_string(&payload) {
                            call_map("drawJourney", Some(&s));
                        }
                    } else {
                        call_map("clearJourneyAndFocus", None);
                    }
                    render_journeys(&a);
                    first
                });
                if n == 0 {
                    hide_trip_detail_card();
                    stop_live_tracking();
                    stop_global_live();
                    call_map("clearJourneyAndFocus", None);
                    set_status(
                        "Aucun itinéraire pour ce trajet. Essayez des gares ou villes \
                         (ex. Aulnay-sous-Bois → Gare du Nord). Attendez le chargement \
                         du réseau si la Santé indique peu de trips.",
                        true,
                    );
                } else {
                    if let Some(ref j) = first {
                        show_trip_detail_card(j);
                    }
                    // Focus suppresses global markers; journey live if "Temps réel" is on.
                    call_map("clearGlobalLiveVehicles", None);
                    if live_vehicles_enabled() {
                        restart_live_tracking();
                    } else {
                        stop_live_tracking();
                        call_map("clearLiveVehicles", None);
                    }
                    set_status(
                        &format!(
                            "{n} itinéraire(s) trouvé(s).{}",
                            if degraded {
                                " (temps réel dégradé)"
                            } else {
                                ""
                            }
                        ),
                        false,
                    );
                }
            }
            Err(e) => {
                hide_trip_detail_card();
                set_html(
                    "journeys",
                    r#"<div class="empty">Échec de la recherche. Réessayez.</div>"#,
                );
                if e.contains("timeout") || e.contains("TIMEOUT") {
                    set_status(
                        "Délai dépassé sur le calcul d’itinéraire. Réessayez, ou attendez \
                         la fin du chargement GTFS (Santé) si le réseau vient de démarrer.",
                        true,
                    );
                } else {
                    set_status(&e, true);
                }
            }
        }
    });
}

fn feed_display_name(id: &str) -> &str {
    match id {
        "idfm" => "Île-de-France Mobilités (IDFM)",
        "sncf" => "SNCF",
        _ => id,
    }
}

fn feed_loading_label(feed: &FeedHealth) -> String {
    let name = feed_display_name(&feed.id);
    if feed.static_loaded {
        let stats = match (feed.static_stops, feed.static_trips) {
            (Some(s), Some(t)) => format!(" — {s} arrêts · {t} courses"),
            _ => String::new(),
        };
        format!("{name} chargé{stats}")
    } else if let Some(err) = &feed.last_static_error {
        format!("Échec {name} — {err}")
    } else {
        format!("Chargement du réseau {name}…")
    }
}

fn apply_static_loading_ui(health: &HealthSnapshot) {
    let all_ready = if health.feeds.is_empty() {
        health.trips > 0
    } else {
        health.feeds.iter().all(|f| f.static_loaded)
    };
    let any_ready = health.trips > 0 || health.feeds.iter().any(|f| f.static_loaded);
    let pending: Vec<&FeedHealth> = health
        .feeds
        .iter()
        .filter(|f| !f.static_loaded)
        .collect();

    // Map overlay + sidebar banner
    if let Some(overlay) = el("static-loading-overlay") {
        if all_ready {
            let _ = overlay.set_attribute("hidden", "true");
            let _ = overlay.remove_attribute("aria-busy");
        } else {
            let _ = overlay.remove_attribute("hidden");
            let _ = overlay.set_attribute("aria-busy", "true");
        }
    }
    if let Some(banner) = el("static-loading-banner") {
        if all_ready {
            let _ = banner.set_attribute("hidden", "true");
        } else {
            let _ = banner.remove_attribute("hidden");
        }
    }

    if !all_ready {
        let primary = pending
            .iter()
            .find(|f| f.id == "idfm")
            .or_else(|| pending.first());
        let headline = primary
            .map(|f| feed_loading_label(f))
            .unwrap_or_else(|| "Chargement des horaires…".to_string());
        set_html("static-loading-message", &escape_html(&headline));
        if let Some(txt) = el("static-loading-banner-text") {
            txt.set_inner_html(&escape_html(&headline));
        }

        let mut feeds_html = String::new();
        for feed in &health.feeds {
            let (class, icon) = if feed.static_loaded {
                ("static-loading-feed--done", "✓")
            } else if feed.last_static_error.is_some() {
                ("static-loading-feed--error", "!")
            } else {
                ("static-loading-feed--pending", "")
            };
            let spinner = if feed.static_loaded || feed.last_static_error.is_some() {
                String::new()
            } else {
                r#"<span class="spinner" aria-hidden="true"></span>"#.to_string()
            };
            let icon_html = if icon.is_empty() {
                spinner
            } else {
                format!(r#"<span class="static-loading-feed-icon">{icon}</span>"#)
            };
            feeds_html.push_str(&format!(
                r#"<li class="static-loading-feed {class}">{icon_html}<span>{}</span></li>"#,
                escape_html(&feed_loading_label(feed))
            ));
        }
        set_html("static-loading-feeds", &feeds_html);
    }

    // Plan button: disabled until at least one feed contributes trips.
    if let Some(btn) = el("plan-btn") {
        if any_ready {
            let _ = btn.remove_attribute("disabled");
            let _ = btn.remove_attribute("title");
        } else {
            let _ = btn.set_attribute("disabled", "true");
            let _ = btn.set_attribute(
                "title",
                "En attente du chargement des horaires GTFS",
            );
        }
    }

    // Live features depend on static timetable data.
    for id in ["global-live", "live-vehicles"] {
        if let Some(cb) = el(id).and_then(|e| e.dyn_into::<HtmlInputElement>().ok()) {
            cb.set_disabled(!all_ready);
            if !all_ready {
                cb.set_title("Disponible une fois les horaires chargés");
            } else {
                let _ = cb.remove_attribute("title");
            }
        }
    }

    let was_ready = STATIC_FEEDS_READY.with(|r| *r.borrow());
    STATIC_FEEDS_READY.with(|r| *r.borrow_mut() = all_ready);

    if all_ready && !was_ready {
        if global_live_enabled() {
            restart_global_live();
        }
    } else if !all_ready && was_ready {
        stop_global_live();
        stop_live_tracking();
    } else if !all_ready {
        stop_global_live();
    }
}

fn apply_health_badge(health: &HealthSnapshot) {
    if let Some(b) = el("health-badge") {
        let pending: Vec<&str> = health
            .feeds
            .iter()
            .filter(|f| !f.static_loaded)
            .map(|f| feed_display_name(&f.id))
            .collect();
        let ok = health.status == "ok" && health.stops > 0 && pending.is_empty();
        let label = if pending.is_empty() {
            format!(
                "Prêt · {} arrêts · {} courses",
                health.stops, health.trips
            )
        } else if health.trips > 0 {
            format!(
                "Partiel · {} arrêts · {} courses · en attente : {}",
                health.stops,
                health.trips,
                pending.join(", ")
            )
        } else {
            format!("Chargement · {}", pending.join(", "))
        };
        b.set_inner_html(&escape_html(&label));
        let _ = b.set_attribute(
            "class",
            if ok {
                "badge badge-ok"
            } else if health.trips > 0 || health.status == "starting" {
                "badge badge-warn"
            } else {
                "badge badge-err"
            },
        );
    }
}

fn restart_health_poll(loading: bool) {
    let fast = loading;
    let changed = HEALTH_POLL_FAST.with(|cell| {
        let mut g = cell.borrow_mut();
        if g.map(|v| v == fast).unwrap_or(false) {
            false
        } else {
            *g = Some(fast);
            true
        }
    });
    if !changed {
        return;
    }
    let ms = if fast {
        HEALTH_POLL_LOADING_MS
    } else {
        HEALTH_POLL_READY_MS
    };
    HEALTH_POLL_HANDLE.with(|cell| {
        *cell.borrow_mut() = None;
        let handle = gloo_timers::callback::Interval::new(ms, || {
            refresh_health();
        });
        *cell.borrow_mut() = Some(handle);
    });
}

fn refresh_health() {
    wasm_bindgen_futures::spawn_local(async {
        match fetch_health_snapshot().await {
            Ok(health) => {
                let loading = if health.feeds.is_empty() {
                    health.trips == 0
                } else {
                    health.feeds.iter().any(|f| !f.static_loaded)
                };
                apply_static_loading_ui(&health);
                apply_health_badge(&health);
                restart_health_poll(loading);
            }
            Err(e) => {
                if let Some(b) = el("health-badge") {
                    b.set_inner_html(&format!("API hors ligne · {}", escape_html(&e)));
                    let _ = b.set_attribute("class", "badge badge-err");
                }
                if let Some(overlay) = el("static-loading-overlay") {
                    let _ = overlay.remove_attribute("hidden");
                }
                set_html(
                    "static-loading-message",
                    "Connexion au serveur impossible — vérifiez que l’API tourne.",
                );
            }
        }
    });
}

// ─── Entry ──────────────────────────────────────────────────────────────────

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();

    let url = graphql_url();
    set_html("graphql-url-label", &escape_html(&url));
    render_legend();
    call_map("init", None);

    // Traffic ticker: show immediately, retry (RT alerts often arrive after boot), then poll.
    show_traffic_bar();
    set_html(
        "traffic-messages",
        r#"<span class="traffic-ticker-seg">Chargement des infos trafic…</span>"#,
    );
    refresh_traffic_messages();
    // Quick retries while SNCF/IDFM RT warms up
    for delay_ms in [3_000_u32, 8_000, 20_000] {
        let _ = gloo_timers::callback::Timeout::new(delay_ms, || {
            refresh_traffic_messages();
        })
        .forget();
    }
    // Periodic refresh
    let traffic_interval = gloo_timers::callback::Interval::new(90_000, || {
        refresh_traffic_messages();
    });
    // Keep interval alive for the session
    traffic_interval.forget();

    bind_input("origin-input", "origin");
    bind_input("dest-input", "dest");

    if let Some(btn) = el("trip-detail-close") {
        let closure = Closure::wrap(Box::new(move |_e: web_sys::MouseEvent| {
            hide_trip_detail_card();
        }) as Box<dyn FnMut(_)>);
        let _ = btn.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref());
        closure.forget();
    }

    if let Some(btn) = el("plan-btn") {
        let closure = Closure::wrap(Box::new(move |_e: web_sys::MouseEvent| {
            plan_clicked();
        }) as Box<dyn FnMut(_)>);
        let _ = btn.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref());
        closure.forget();
    }

    if let Some(cb) = el("arrive-by") {
        let closure = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            sync_time_field_label();
        }) as Box<dyn FnMut(_)>);
        let _ = cb.add_event_listener_with_callback("change", closure.as_ref().unchecked_ref());
        closure.forget();
        sync_time_field_label();
    }

    if let Some(cb) = el("live-vehicles") {
        let closure = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if live_vehicles_enabled() {
                restart_live_tracking();
            } else {
                stop_live_tracking();
            }
        }) as Box<dyn FnMut(_)>);
        let _ = cb.add_event_listener_with_callback("change", closure.as_ref().unchecked_ref());
        closure.forget();
    }

    if let Some(cb) = el("global-live") {
        let closure = Closure::wrap(Box::new(move |_e: web_sys::Event| {
            if global_live_enabled() {
                restart_global_live();
            } else {
                stop_global_live();
            }
        }) as Box<dyn FnMut(_)>);
        let _ = cb.add_event_listener_with_callback("change", closure.as_ref().unchecked_ref());
        closure.forget();
    }

    // Map pan/zoom → refresh global live layer (debounced in handler).
    {
        let move_cb = Closure::wrap(Box::new(move || {
            on_global_live_map_move();
        }) as Box<dyn FnMut()>);
        if let Some(win) = web_sys::window() {
            let _ = js_sys::Reflect::set(
                &win,
                &JsValue::from_str("__transitGlobalLiveMoveEnd"),
                move_cb.as_ref().unchecked_ref(),
            );
        }
        move_cb.forget();
        call_map("onMoveEnd", Some("__transitGlobalLiveMoveEnd"));
    }

    // Global network map: default ON once static feeds are ready.
    if let Some(cb) = el("global-live").and_then(|e| e.dyn_into::<HtmlInputElement>().ok()) {
        cb.set_checked(true);
    }

    refresh_health();
    restart_health_poll(true);

    let ws_note = graphql_ws_url()
        .map(|u| format!(" · WS {u}"))
        .unwrap_or_default();
    web_sys::console::log_1(&JsValue::from_str(&format!(
        "transit-web prêt · GraphQL {url}{ws_note}"
    )));
}

// Silence unused import warning on HtmlElement in some toolchains
#[allow(dead_code)]
fn _touch(_: HtmlElement) {}
