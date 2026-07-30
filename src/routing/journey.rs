use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use crate::gtfs::pack::{RouteMode, StaticEpoch};
use crate::rt::overlay::{RealtimeOverlay, TripRt};

/// Compact per-trip RT adjustments for RAPTOR (seconds).
///
/// Built from GTFS-RT trip updates so routing boards/alights on **predicted**
/// times, not only post-hoc display enrichment.
#[derive(Debug, Clone, Default)]
pub struct RtTripAdjust {
    /// Trip-level delay (s) when stop-level is absent.
    pub trip_delay_s: i32,
    /// stop_sequence → departure delay (s).
    pub dep_delay: HashMap<u16, i32>,
    /// stop_sequence → arrival delay (s).
    pub arr_delay: HashMap<u16, i32>,
    /// stop_sequence marked SKIPPED (no board / no alight).
    pub skipped: HashSet<u16>,
}

impl RtTripAdjust {
    pub fn from_trip_rt(t: &TripRt) -> Self {
        let mut a = Self {
            trip_delay_s: t.delay.unwrap_or(0),
            ..Default::default()
        };
        for (&seq, u) in &t.stop_updates {
            if u.skipped {
                a.skipped.insert(seq);
            }
            if let Some(d) = u.departure_delay.or(u.arrival_delay) {
                a.dep_delay.insert(seq, d);
            }
            if let Some(d) = u.arrival_delay.or(u.departure_delay) {
                a.arr_delay.insert(seq, d);
            }
        }
        a
    }

    pub fn dep_delay_for(&self, stop_sequence: u16) -> i32 {
        self.dep_delay
            .get(&stop_sequence)
            .copied()
            .unwrap_or(self.trip_delay_s)
    }

    pub fn arr_delay_for(&self, stop_sequence: u16) -> i32 {
        self.arr_delay
            .get(&stop_sequence)
            .copied()
            .unwrap_or(self.trip_delay_s)
    }

    pub fn is_skipped(&self, stop_sequence: u16) -> bool {
        self.skipped.contains(&stop_sequence)
    }
}

/// Build RAPTOR RT map from overlay (undated trip keys preferred).
pub fn rt_adjust_map_from_overlay(rt: &RealtimeOverlay) -> HashMap<String, RtTripAdjust> {
    if !rt.rt_adjust.is_empty() {
        return rt.rt_adjust.clone();
    }
    rt_adjust_map_from_feeds(&rt.feeds)
}

fn rt_adjust_map_from_feeds(
    feeds: &HashMap<String, crate::rt::overlay::FeedRtState>,
) -> HashMap<String, RtTripAdjust> {
    let mut out: HashMap<String, RtTripAdjust> = HashMap::new();
    for fr in feeds.values() {
        for (key, trip) in &fr.trips {
            if trip.canceled {
                continue;
            }
            let undated = key
                .rsplit_once('@')
                .map(|(b, _)| b.to_string())
                .unwrap_or_else(|| key.clone());
            let adj = RtTripAdjust::from_trip_rt(trip);
            match out.get(&undated) {
                Some(prev)
                    if prev.dep_delay.len() + prev.arr_delay.len()
                        >= adj.dep_delay.len() + adj.arr_delay.len()
                        && prev.trip_delay_s != 0 => {}
                _ => {
                    out.insert(undated, adj);
                }
            }
        }
    }
    out
}

/// Rebuild alert indexes + routing caches after RT feeds change.
pub fn finalize_realtime_overlay(rt: &mut RealtimeOverlay) {
    for st in rt.feeds.values_mut() {
        st.rebuild_alert_index();
    }
    rt.canceled_trip_ids = rt
        .feeds
        .values()
        .flat_map(|f| {
            f.trips
                .iter()
                .filter(|(_, t)| t.canceled)
                .map(|(id, _)| {
                    id.rsplit_once('@')
                        .map(|(b, _)| b.to_string())
                        .unwrap_or_else(|| id.clone())
                })
        })
        .collect();
    rt.rt_adjust = rt_adjust_map_from_feeds(&rt.feeds);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Place {
    pub stop_id: Option<String>,
    pub name: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ItineraryQuery {
    pub from_stop_id: Option<String>,
    pub to_stop_id: Option<String>,
    pub from_lat: Option<f64>,
    pub from_lon: Option<f64>,
    pub to_lat: Option<f64>,
    pub to_lon: Option<f64>,
    /// Earliest departure (when `arrive_by` is false) or **arrival deadline** (when true).
    /// GraphQL: `departureAt` or `arriveBy` is copied here; see routing module docs.
    pub departure_at: DateTime<Utc>,
    /// When true, `departure_at` is an arrive-by deadline (not a depart-after time).
    pub arrive_by: bool,
    pub max_transfers: u32,
    pub max_results: u32,
    pub modes: Option<Vec<RouteMode>>,
    pub max_walk_meters: u32,
    pub walk_speed_m_s: f64,
    pub raptor_max_rounds: u32,
    pub default_transfer_s: u32,
    pub timezone: String,
    /// Optional OSRM base URL for street-level walk/bike geometry + duration.
    /// Empty / None → haversine only.
    pub osrm_url: Option<String>,
    /// First km (origin → first stop) by bike when true.
    pub bike_from: bool,
    /// Last km (last stop → destination) by bike when true.
    pub bike_to: bool,
    /// Bike access speed (m/s). Default ~4.2 (~15 km/h).
    pub bike_speed_m_s: f64,
    /// Max bike access distance (m) for first/last mile.
    pub max_bike_meters: u32,
    /// When true, use Trip-Based Public Transit Routing (TBR) instead of RAPTOR.
    /// TBR is a round-based alternative that operates on trips directly and can
    /// be more efficient when the number of trips is large but stops-per-trip
    /// is small.  Produces equivalent results to RAPTOR.
    pub use_tbr: bool,
    /// Trip ids (namespaced) to skip at boarding time, e.g. RT cancellations.
    /// Routing does not import the RT layer; callers pass the set.
    pub excluded_trip_ids: HashSet<String>,
    /// GTFS-RT delays / skipped stops keyed by namespaced trip id (`feed:trip`).
    /// Empty → pure schedule RAPTOR.
    pub rt_adjust: HashMap<String, RtTripAdjust>,
    /// When true, soft-filter trips: exclude `wheelchair == 2` (not allowed).
    /// GTFS: 0 = unknown (allowed), 1 = allowed, 2 = not allowed.
    pub wheelchair: bool,
}

impl ItineraryQuery {
    pub fn access_speed_from_m_s(&self) -> f64 {
        if self.bike_from {
            self.bike_speed_m_s.max(0.5)
        } else {
            self.walk_speed_m_s.max(0.1)
        }
    }

    pub fn access_speed_to_m_s(&self) -> f64 {
        if self.bike_to {
            self.bike_speed_m_s.max(0.5)
        } else {
            self.walk_speed_m_s.max(0.1)
        }
    }

    pub fn access_max_from_m(&self) -> f64 {
        if self.bike_from {
            self.max_bike_meters as f64
        } else {
            self.max_walk_meters as f64
        }
    }

    pub fn access_max_to_m(&self) -> f64 {
        if self.bike_to {
            self.max_bike_meters as f64
        } else {
            self.max_walk_meters as f64
        }
    }

    /// Builder-style helper for tests and callers that want empty exclusions.
    pub fn with_excluded_trips(mut self, ids: HashSet<String>) -> Self {
        self.excluded_trip_ids = ids;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopRef {
    pub stop_id: String,
    pub name: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub platform: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntermediateStop {
    pub stop: StopRef,
    pub scheduled_arrival: DateTime<Utc>,
    pub scheduled_departure: DateTime<Utc>,
    pub realtime_arrival: Option<DateTime<Utc>>,
    pub realtime_departure: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitLegData {
    pub mode: String,
    pub route_short_name: String,
    pub route_long_name: String,
    /// Namespaced GTFS route id (`feed:routeId`) for alert matching.
    pub route_id: String,
    pub agency_name: Option<String>,
    pub trip_id: String,
    /// GTFS trip_short_name (e.g. train number).
    pub trip_short_name: Option<String>,
    pub headsign: Option<String>,
    /// GTFS direction_id (0/1) when present.
    pub direction_id: Option<u8>,
    pub route_color: Option<String>,
    pub route_text_color: Option<String>,
    /// stop_times.stop_headsign at board, else trip headsign.
    pub stop_headsign: Option<String>,
    /// GTFS trips.wheelchair_accessible: 0 unknown, 1 allowed, 2 not.
    pub wheelchair: u8,
    /// GTFS trips.bikes_allowed: 0 unknown, 1 allowed, 2 not.
    pub bikes_allowed: u8,
    pub from: StopRef,
    pub to: StopRef,
    /// GTFS `stop_sequence` at board (for RT stop matching).
    pub from_stop_sequence: u16,
    /// GTFS `stop_sequence` at alight.
    pub to_stop_sequence: u16,
    pub scheduled_departure: DateTime<Utc>,
    pub scheduled_arrival: DateTime<Utc>,
    pub realtime_departure: Option<DateTime<Utc>>,
    pub realtime_arrival: Option<DateTime<Utc>>,
    pub delay_departure_s: Option<i32>,
    pub delay_arrival_s: Option<i32>,
    pub canceled: bool,
    pub vehicle_lat: Option<f64>,
    pub vehicle_lon: Option<f64>,
    pub vehicle_updated_at: Option<DateTime<Utc>>,
    /// Intermediate stops between board and alight (exclusive of endpoints).
    pub intermediate_stops: Vec<IntermediateStop>,
    /// Board→alight path as **`(lat, lon)`** pairs (Leaflet order, not GeoJSON).
    /// Filled from GTFS shapes when available, else stop-to-stop polyline.
    pub geometry: Vec<(f64, f64)>,
    /// True when this transit leg continues the previous leg on the same block/vehicle
    /// (GTFS `block_id` match within the same feed) — stay-seated transfer hint.
    pub same_vehicle: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkLegData {
    /// `WALK` (default) or `BIKE` for access/egress on street network.
    pub mode: String,
    pub from_name: String,
    pub to_name: String,
    pub from_stop_id: Option<String>,
    pub to_stop_id: Option<String>,
    pub distance_m: f64,
    pub duration_s: u32,
    pub from_lat: Option<f64>,
    pub from_lon: Option<f64>,
    pub to_lat: Option<f64>,
    pub to_lon: Option<f64>,
    /// Street path as **`(lat, lon)`** pairs (Leaflet order).
    /// Filled from OSRM when configured; otherwise endpoints-only polyline.
    pub geometry: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Leg {
    Transit(TransitLegData),
    Walk(WalkLegData),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journey {
    pub id: String,
    pub departure: DateTime<Utc>,
    pub arrival: DateTime<Utc>,
    pub duration_s: i64,
    pub transfers: u32,
    pub walk_distance_m: f64,
    pub realtime_status: String,
    pub legs: Vec<Leg>,
    pub alert_headers: Vec<String>,
    /// Best-effort GTFS fare amount when tables match; never invented.
    pub fare_amount: Option<f64>,
    pub fare_currency: Option<String>,
    /// Human-readable caveat (estimate only / incomplete data).
    pub fare_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItineraryResult {
    pub from: Place,
    pub to: Place,
    pub computed_at: DateTime<Utc>,
    pub static_epoch_id: String,
    pub realtime_age_s: Option<i64>,
    pub realtime_degraded: bool,
    pub journeys: Vec<Journey>,
}

pub fn service_date_in_tz(dt: DateTime<Utc>, tz_name: &str) -> NaiveDate {
    if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
        return dt.with_timezone(&tz).date_naive();
    }
    dt.date_naive()
}

/// Seconds since local midnight for departure (handles Europe/Paris).
pub fn local_seconds_since_midnight(dt: DateTime<Utc>, tz_name: &str) -> u32 {
    use chrono::Timelike;
    if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
        let local = dt.with_timezone(&tz);
        return local.num_seconds_from_midnight();
    }
    dt.time().num_seconds_from_midnight()
}

pub fn local_midnight_utc(date: NaiveDate, tz_name: &str) -> DateTime<Utc> {
    use chrono::TimeZone;
    if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
        return tz
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|| {
                DateTime::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap(), Utc)
            });
    }
    DateTime::from_naive_utc_and_offset(date.and_hms_opt(0, 0, 0).unwrap(), Utc)
}

/// Extract an IDFM / GTFS product code (e.g. `C01375`) from a route or LineRef.
fn product_code_token(raw: &str) -> Option<String> {
    // Prefer explicit C##### tokens used by IDFM (métro/RER/tram LineRef).
    for part in raw.split(|c: char| !c.is_ascii_alphanumeric()) {
        let p = part.trim();
        if p.len() >= 5
            && p.len() <= 8
            && p.as_bytes()[0].eq_ignore_ascii_case(&b'C')
            && p[1..].chars().all(|c| c.is_ascii_digit())
        {
            return Some(p.to_ascii_uppercase());
        }
    }
    None
}

/// True when two route refs denote the same product line (not a substring of "5").
fn route_refs_match(leg_route_id: &str, informed: &str) -> bool {
    if leg_route_id.is_empty() || informed.is_empty() {
        return false;
    }
    if leg_route_id == informed {
        return true;
    }
    // Same IDFM product code: IDFM:C01375 ↔ STIF:Line::C01375:
    if let (Some(a), Some(b)) = (
        product_code_token(leg_route_id),
        product_code_token(informed),
    ) {
        return a == b;
    }
    // Exact trailing raw id after last colon (no substring games).
    let leg_tail = leg_route_id
        .rsplit(|c| c == ':' || c == '/')
        .find(|p| !p.is_empty())
        .unwrap_or(leg_route_id);
    let inf_tail = informed
        .rsplit(|c| c == ':' || c == '/')
        .find(|p| !p.is_empty())
        .unwrap_or(informed);
    if !leg_tail.is_empty()
        && leg_tail.len() >= 3
        && leg_tail.eq_ignore_ascii_case(inf_tail)
    {
        return true;
    }
    false
}

/// Commercial short-name equality without matching "5" inside "C01375" or "14".
fn short_names_equal(leg_short: &str, other: &str) -> bool {
    let a = leg_short.trim();
    let b = other.trim();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a.eq_ignore_ascii_case(b)
}

/// Road / bus-stop traffic messages must not stick to rail / métro / tram legs.
fn is_road_traffic_alert(a: &crate::rt::overlay::AlertRt) -> bool {
    let blob = format!(
        "{} {} {}",
        a.header.as_deref().unwrap_or(""),
        a.description.as_deref().unwrap_or(""),
        a.cause.as_deref().unwrap_or("")
    )
    .to_ascii_uppercase();
    blob.contains("TRAFIC ROUTIER")
        || blob.contains("INFO TRAFIC ROUTIER")
        || (blob.contains("ARRET")
            && (blob.contains("DEPLACE") || blob.contains("DÉPLACÉ") || blob.contains("DEPLAC")))
            && (blob.contains("CIRCULATION") || blob.contains("SENS DE CIRCULATION"))
}

fn is_rail_like_mode(mode: &str) -> bool {
    matches!(
        mode.to_ascii_uppercase().as_str(),
        "RAIL" | "TRAIN" | "METRO" | "SUBWAY" | "TRAM" | "TRAMWAY" | "RER"
    )
}

/// Whether a service alert **directly** applies to this transit leg.
///
/// Matching is strict (trip / stop / LineRef product code). Free-text mentions
/// such as « privilégiez la ligne 14 » or « saturation sur les lignes 4 et 5 »
/// do **not** attach the alert to every cited line.
pub fn alert_applies_to_leg(
    a: &crate::rt::overlay::AlertRt,
    trip_id: &str,
    route_id: &str,
    route_short: &str,
    from_stop_id: &str,
    to_stop_id: &str,
) -> bool {
    alert_applies_to_leg_with_mode(a, trip_id, route_id, route_short, from_stop_id, to_stop_id, "")
}

/// Same as [`alert_applies_to_leg`] with mode (filters road traffic off rail modes).
pub fn alert_applies_to_leg_with_mode(
    a: &crate::rt::overlay::AlertRt,
    trip_id: &str,
    route_id: &str,
    route_short: &str,
    from_stop_id: &str,
    to_stop_id: &str,
    mode: &str,
) -> bool {
    if is_rail_like_mode(mode) && is_road_traffic_alert(a) {
        return false;
    }

    let tid_base = trip_id.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_id);
    let trip_hit = !tid_base.is_empty()
        && a.informed_trip_ids.iter().any(|t| {
            t == trip_id
                || t == tid_base
                || t.ends_with(&format!(":{tid_base}"))
                || t.rsplit_once(':').map(|(_, r)| r) == Some(tid_base)
        });
    if trip_hit {
        return true;
    }

    // Stops: require last significant id segment equality (no loose substring).
    let stop_tail = |sid: &str| -> String {
        sid.rsplit(|c| c == ':' || c == '/')
            .find(|p| !p.is_empty() && p.chars().any(|c| c.is_ascii_alphanumeric()))
            .unwrap_or(sid)
            .to_string()
    };
    let stop_hit = |sid: &str| {
        if sid.is_empty() {
            return false;
        }
        let want = stop_tail(sid);
        if want.is_empty() {
            return false;
        }
        a.informed_stop_ids
            .iter()
            .any(|s| s == sid || stop_tail(s).eq_ignore_ascii_case(&want))
    };
    if stop_hit(from_stop_id) || stop_hit(to_stop_id) {
        return true;
    }

    // Routes: structured LineRef / route_id only.
    let mut route_hit = false;
    if !a.informed_route_ids.is_empty() {
        if a.informed_route_ids
            .iter()
            .any(|r| route_refs_match(route_id, r))
        {
            route_hit = true;
        } else {
            let short = route_short.trim();
            if !short.is_empty() {
                for r in &a.informed_route_ids {
                    let tail = r
                        .rsplit(|c| c == ':' || c == '/')
                        .find(|p| !p.is_empty())
                        .unwrap_or(r.as_str());
                    // Informed ref is exactly the commercial name ("B", "14"), not a code.
                    if short_names_equal(short, tail)
                        && product_code_token(r).is_none()
                        && tail.len() <= 4
                    {
                        route_hit = true;
                        break;
                    }
                    if let Some(code) = product_code_token(r) {
                        if let Some(mapped) = idfm_product_to_short(&code) {
                            if short_names_equal(short, &mapped) {
                                // Same commercial line via product code table.
                                if product_code_token(route_id).as_deref() == Some(code.as_str())
                                    || product_code_token(route_id)
                                        .as_ref()
                                        .and_then(|c| idfm_product_to_short(c))
                                        .as_deref()
                                        == Some(mapped.as_str())
                                {
                                    route_hit = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if !route_hit {
            return false;
        }
        // LineRef match can be polluted when PRIM GM is polled per-line and
        // returns network-wide messages tagged with that LineRef. Require that
        // the free text does not primarily describe *other* lines only.
        if !alert_text_compatible_with_leg(a, route_short, mode) {
            return false;
        }
        return true;
    }

    // No structured selectors → do not attach from free text alone.
    false
}

/// Reject secondary / network-wide mentions that cite our line only as advice.
fn alert_text_compatible_with_leg(
    a: &crate::rt::overlay::AlertRt,
    route_short: &str,
    _mode: &str,
) -> bool {
    let short = route_short.trim();
    if short.is_empty() {
        return true;
    }
    let blob = fold_alert_blob(&format!(
        "{} {}",
        a.header.as_deref().unwrap_or(""),
        a.description.as_deref().unwrap_or("")
    ));
    if blob.is_empty() {
        return true;
    }

    let mentioned = commercial_lines_in_text(&blob);
    if mentioned.is_empty() {
        return true;
    }

    let su = short.to_ascii_uppercase();
    let we_are_mentioned = mentioned.iter().any(|m| m == &su);

    // LineRef says "metro 5" but the message is only about RER B/D → drop.
    if !we_are_mentioned {
        return false;
    }

    // Mentioned only as diversion / saturation target, while other products are
    // the actual subject (e.g. travaux RER B/D … saturation sur 4 et 5).
    if is_secondary_line_context(&blob, &su) {
        let others_primary = mentioned.iter().any(|m| m != &su && is_primary_context(&blob, m));
        if others_primary {
            return false;
        }
    }
    true
}

fn fold_alert_blob(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' | 'À' | 'Â' | 'Ä' | 'Á' => 'A',
            'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'E',
            'î' | 'ï' | 'í' | 'Î' | 'Ï' | 'Í' => 'I',
            'ô' | 'ö' | 'ó' | 'Ô' | 'Ö' | 'Ó' => 'O',
            'ù' | 'û' | 'ü' | 'ú' | 'Ù' | 'Û' | 'Ü' | 'Ú' => 'U',
            'ç' | 'Ç' => 'C',
            other => other,
        })
        .collect::<String>()
        .to_ascii_uppercase()
}

fn commercial_lines_in_text(blob: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |s: &str| {
        let u = s.to_ascii_uppercase();
        if !out.iter().any(|x| x == &u) {
            out.push(u);
        }
    };
    for letter in ["A", "B", "C", "D", "E"] {
        if blob.contains(&format!("RER {letter}")) || blob.contains(&format!("RER{letter}")) {
            push(letter);
        }
    }
    for n in 1..=14 {
        let s = n.to_string();
        if blob.contains(&format!("METRO {s}"))
            || blob.contains(&format!("METRO{s}"))
            || blob.contains(&format!("LIGNE {s}"))
            || blob.contains(&format!("LIGNES {s}"))
        {
            push(&s);
        }
        // "lignes 4 et 5" / "4 et 5"
        if blob.contains(&format!(" {s} "))
            || blob.contains(&format!(" {s}."))
            || blob.contains(&format!(" {s},"))
            || blob.ends_with(&format!(" {s}"))
            || blob.contains(&format!("ET {s}"))
        {
            // Only count digit lines when nearby "LIGNE" context exists.
            if blob.contains("LIGNE") || blob.contains("METRO") {
                push(&s);
            }
        }
    }
    for n in 1..=13 {
        let t = format!("T{n}");
        if blob.contains(&t) && (blob.contains("TRAM") || blob.contains(&format!(" {t}"))) {
            push(&t);
        }
    }
    out
}

fn is_secondary_line_context(blob: &str, short: &str) -> bool {
    // Windows around our short name that look like advice, not the incident subject.
    let markers = [
        "PRIVILEGIE",
        "PRIVILEGIEZ",
        "SATURATION",
        "REPORTEZ",
        "REPORTER",
        "UTILISEZ",
        "EMPRUNTEZ",
        "EN DIRECTION DE LA LIGNE",
        "VERS LA LIGNE",
        "CORRESPONDANCE AVEC",
    ];
    // Find occurrences of our short as a line token.
    let needles = [
        format!("LIGNE {short}"),
        format!("LIGNES {short}"),
        format!("METRO {short}"),
        format!(" ET {short}"),
        format!(" {short} "),
        format!(" {short}."),
    ];
    for n in &needles {
        if let Some(idx) = blob.find(n.as_str()) {
            let start = idx.saturating_sub(48);
            let end = (idx + n.len() + 24).min(blob.len());
            let window = &blob[start..end];
            if markers.iter().any(|m| window.contains(m)) {
                return true;
            }
        }
    }
    // Global secondary phrasing with our line listed among several.
    if markers.iter().any(|m| blob.contains(m)) && blob.contains(short) {
        // If RER / TRAVAUX lead the message, digit lines are usually secondary.
        if blob.contains("RER ") || blob.contains("TRAVAUX SUR") {
            return true;
        }
    }
    false
}

fn is_primary_context(blob: &str, short: &str) -> bool {
    let patterns = [
        format!("RER {short}"),
        format!("RER{short}"),
        format!("METRO {short}"),
        format!("TRAVAUX SUR LES RER {short}"),
        format!("TRAVAUX SUR LE RER {short}"),
        format!("INCIDENT LIGNE {short}"),
        format!("LIGNE {short} :"),
        format!("LIGNE {short} -"),
        format!("LIGNE {short} INTERRUPT"),
        format!("LIGNE {short} TRAFIC"),
    ];
    patterns.iter().any(|p| blob.contains(p.as_str()))
        || blob.starts_with(&format!("LIGNE {short}"))
        || blob.starts_with(&format!("METRO {short}"))
        || blob.starts_with(&format!("RER {short}"))
}

/// IDFM LineRef product codes → commercial short names (métro / RER / tram).
fn idfm_product_to_short(code: &str) -> Option<String> {
    let c = code.to_ascii_uppercase();
    let name = match c.as_str() {
        "C01742" => "A",
        "C01743" => "B",
        "C01727" => "C",
        "C01728" => "D",
        "C01729" => "E",
        "C01371" => "1",
        "C01372" => "2",
        "C01373" => "3",
        "C01374" => "4",
        "C01375" => "5",
        "C01376" => "6",
        "C01377" => "7",
        "C01378" => "8",
        "C01379" => "9",
        "C01380" => "10",
        "C01381" => "11",
        "C01382" => "12",
        "C01383" => "13",
        "C01384" => "14",
        "C01389" => "T1",
        "C01390" => "T2",
        "C01391" => "T3a",
        "C01679" => "T3b",
        _ => return None,
    };
    Some(name.into())
}

fn apply_stop_realtime(
    rt: &crate::rt::overlay::RealtimeOverlay,
    trip_rt: Option<&TripRt>,
    trip_id: &str,
    scheduled: DateTime<Utc>,
    stop_sequence: Option<u16>,
    stop_id: Option<&str>,
    arrival: bool,
) -> crate::rt::overlay::StopRealtime {
    if let Some(tr) = trip_rt {
        if arrival {
            tr.apply_arrival(scheduled, stop_sequence, stop_id)
        } else {
            tr.apply_departure(scheduled, stop_sequence, stop_id)
        }
    } else if arrival {
        rt.apply_arrival(trip_id, scheduled, stop_sequence, stop_id)
    } else {
        rt.apply_departure(trip_id, scheduled, stop_sequence, stop_id)
    }
}

pub fn enrich_with_realtime(
    mut journey: Journey,
    rt: &crate::rt::overlay::RealtimeOverlay,
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
) -> Journey {
    let mut status = "SCHEDULED".to_string();
    let mut any_delay = false;
    let mut any_cancel = false;
    let mut any_rt = false;
    let mut any_alert = false;
    let mut alerts = Vec::new();

    for leg in &mut journey.legs {
        if let Leg::Transit(t) = leg {
            let trip_rt = rt.resolve_trip_rt(
                epoch,
                &t.trip_id,
                &t.route_id,
                t.direction_id,
                t.scheduled_departure,
            );
            let trip_canceled = trip_rt.map(|tr| tr.canceled).unwrap_or(false)
                || rt.is_trip_canceled(&t.trip_id);

            let dep = apply_stop_realtime(
                rt,
                trip_rt,
                &t.trip_id,
                t.scheduled_departure,
                Some(t.from_stop_sequence),
                Some(t.from.stop_id.as_str()),
                false,
            );
            let arr = apply_stop_realtime(
                rt,
                trip_rt,
                &t.trip_id,
                t.scheduled_arrival,
                Some(t.to_stop_sequence),
                Some(t.to.stop_id.as_str()),
                true,
            );

            if dep.canceled || arr.canceled || trip_canceled {
                t.canceled = true;
                any_cancel = true;
            }
            if dep.has_rt || arr.has_rt {
                any_rt = true;
                t.realtime_departure = Some(dep.realtime);
                t.realtime_arrival = Some(arr.realtime);
                t.delay_departure_s = Some(dep.delay_secs);
                t.delay_arrival_s = Some(arr.delay_secs);
                if dep.delay_secs != 0 || arr.delay_secs != 0 {
                    any_delay = true;
                }
            }

            // Intermediate stops: keep scheduled; attach RT when overlay matches.
            for mid in &mut t.intermediate_stops {
                let m_arr = apply_stop_realtime(
                    rt,
                    trip_rt,
                    &t.trip_id,
                    mid.scheduled_arrival,
                    None,
                    Some(mid.stop.stop_id.as_str()),
                    true,
                );
                if m_arr.has_rt {
                    mid.realtime_arrival = Some(m_arr.realtime);
                }
                let m_dep = apply_stop_realtime(
                    rt,
                    trip_rt,
                    &t.trip_id,
                    mid.scheduled_departure,
                    None,
                    Some(mid.stop.stop_id.as_str()),
                    false,
                );
                if m_dep.has_rt {
                    mid.realtime_departure = Some(m_dep.realtime);
                }
            }

            let vehicle_trip_id = trip_rt
                .and_then(|_| rt.overlay_trip_key(epoch, &t.trip_id))
                .unwrap_or_else(|| t.trip_id.clone());
            if let Some(v) = rt.get_vehicle_for_trip(&vehicle_trip_id) {
                t.vehicle_lat = Some(v.lat);
                t.vehicle_lon = Some(v.lon);
                t.vehicle_updated_at = Some(v.updated_at);
            }

            // TripUpdates/SM cancel or delay already applied; attach GM/GTFS-RT alerts.
            let feed_id = crate::rt::overlay::RealtimeOverlay::feed_of(&t.trip_id);
            let candidates = feed_id
                .and_then(|fid| rt.feeds.get(fid))
                .map(|st| {
                    st.alert_candidates_for_leg(
                        &t.trip_id,
                        &t.route_id,
                        &t.from.stop_id,
                        &t.to.stop_id,
                    )
                })
                .unwrap_or_default();
            for a in candidates {
                if alert_applies_to_leg_with_mode(
                    a,
                    &t.trip_id,
                    &t.route_id,
                    &t.route_short_name,
                    &t.from.stop_id,
                    &t.to.stop_id,
                    &t.mode,
                ) {
                    any_alert = true;
                    if let Some(ref h) = a.header {
                        if !h.is_empty() {
                            alerts.push(h.clone());
                        }
                    } else if let Some(ref d) = a.description {
                        let short: String = d.chars().take(120).collect();
                        if !short.is_empty() {
                            alerts.push(short);
                        }
                    }
                }
            }
        }
    }

    // Adjust journey times from first/last transit realtime if present
    if let Some(Leg::Transit(first)) = journey.legs.iter().find(|l| matches!(l, Leg::Transit(_))) {
        if let Some(rd) = first.realtime_departure {
            journey.departure = rd;
        }
    }
    if let Some(Leg::Transit(last)) = journey.legs.iter().rev().find(|l| matches!(l, Leg::Transit(_)))
    {
        if let Some(ra) = last.realtime_arrival {
            journey.arrival = ra;
        }
    }
    journey.duration_s = (journey.arrival - journey.departure).num_seconds();

    if any_cancel {
        status = "CANCELLED".into();
    } else if any_delay {
        status = "DELAYED".into();
    } else if any_alert {
        status = "DISRUPTED".into();
    } else if any_rt {
        status = "ON_TIME".into();
    }
    journey.realtime_status = status;
    alerts.sort();
    alerts.dedup();
    journey.alert_headers = alerts;
    journey
}

pub fn new_journey_id() -> String {
    Uuid::new_v4().to_string()
}

pub fn stop_ref(epoch: &StaticEpoch, idx: u32) -> StopRef {
    let s = &epoch.stops[idx as usize];
    StopRef {
        stop_id: s.id.clone(),
        name: s.name.clone(),
        lat: s.lat,
        lon: s.lon,
        platform: s.platform_code.clone(),
    }
}

#[cfg(test)]
mod enrich_rt_tests {
    use super::*;
    use crate::rt::overlay::{FeedRtState, RealtimeOverlay, StopTimeRt, TripRt};
    use chrono::TimeZone;
    use std::collections::HashMap;

    fn sched_hms(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).single().unwrap()
    }

    fn sample_leg(trip_id: &str, dep: DateTime<Utc>, arr: DateTime<Utc>) -> Leg {
        Leg::Transit(TransitLegData {
            mode: "METRO".into(),
            route_short_name: "1".into(),
            route_long_name: "Ligne 1".into(),
            route_id: "test:r1".into(),
            agency_name: None,
            trip_id: trip_id.into(),
            trip_short_name: None,
            headsign: None,
            direction_id: Some(0),
            route_color: None,
            route_text_color: None,
            stop_headsign: None,
            wheelchair: 0,
            bikes_allowed: 0,
            from: StopRef {
                stop_id: "test:A".into(),
                name: "A".into(),
                lat: None,
                lon: None,
                platform: None,
            },
            to: StopRef {
                stop_id: "test:B".into(),
                name: "B".into(),
                lat: None,
                lon: None,
                platform: None,
            },
            from_stop_sequence: 1,
            to_stop_sequence: 2,
            scheduled_departure: dep,
            scheduled_arrival: arr,
            realtime_departure: None,
            realtime_arrival: None,
            delay_departure_s: None,
            delay_arrival_s: None,
            canceled: false,
            vehicle_lat: None,
            vehicle_lon: None,
            vehicle_updated_at: None,
            intermediate_stops: vec![],
            geometry: vec![],
            same_vehicle: false,
        })
    }

    #[test]
    fn enrich_applies_rt_without_double_delay() {
        let dep = sched_hms(2026, 7, 28, 8, 0);
        let arr = sched_hms(2026, 7, 28, 8, 30);
        let mut journey = Journey {
            id: "j1".into(),
            departure: dep,
            arrival: arr,
            duration_s: 1800,
            transfers: 0,
            walk_distance_m: 0.0,
            realtime_status: "SCHEDULED".into(),
            legs: vec![sample_leg("test:t1", dep, arr)],
            alert_headers: vec![],
            fare_amount: None,
            fare_currency: None,
            fare_note: None,
        };

        let mut stop_updates = HashMap::new();
        stop_updates.insert(
            1,
            StopTimeRt {
                stop_sequence: 1,
                stop_id: Some("test:A".into()),
                arrival_delay: None,
                departure_delay: Some(300),
                arrival_time: None,
                departure_time: None,
                skipped: false,
            },
        );
        stop_updates.insert(
            2,
            StopTimeRt {
                stop_sequence: 2,
                stop_id: Some("test:B".into()),
                arrival_delay: Some(300),
                departure_delay: None,
                arrival_time: None,
                departure_time: None,
                skipped: false,
            },
        );
        let mut feed = FeedRtState::new("test");
        feed.insert_trip(
            "test",
            "t1",
            TripRt {
                delay: Some(300),
                canceled: false,
                start_date: None,
                route_id: Some("test:r1".into()),
                direction_id: Some(0),
                start_time: None,
                stop_updates,
            },
        );
        let mut rt = RealtimeOverlay::default();
        rt.feeds.insert("test".into(), feed);

        journey = enrich_with_realtime(journey, &rt, None);
        let Leg::Transit(t) = &journey.legs[0] else {
            panic!("expected transit leg");
        };
        assert_eq!(t.scheduled_departure, dep);
        assert_eq!(t.scheduled_arrival, arr);
        assert_eq!(
            t.realtime_departure,
            Some(dep + chrono::Duration::seconds(300))
        );
        assert_eq!(
            t.realtime_arrival,
            Some(arr + chrono::Duration::seconds(300))
        );
        assert_eq!(t.delay_departure_s, Some(300));
        assert_eq!(journey.departure, dep + chrono::Duration::seconds(300));
        assert_eq!(journey.realtime_status, "DELAYED");
    }
}

#[cfg(test)]
mod alert_match_tests {
    use super::*;
    use crate::rt::overlay::AlertRt;

    fn alert(header: &str, desc: &str, routes: &[&str], stops: &[&str]) -> AlertRt {
        AlertRt {
            id: "idfm:test".into(),
            header: Some(header.into()),
            description: Some(desc.into()),
            severity: "WARNING".into(),
            informed_stop_ids: stops.iter().map(|s| (*s).to_string()).collect(),
            informed_route_ids: routes.iter().map(|s| (*s).to_string()).collect(),
            informed_trip_ids: vec![],
            cause: None,
            effect: None,
            url: None,
            active_start: None,
            active_end: None,
            active_periods: vec![],
            header_translations: vec![],
        }
    }

    #[test]
    fn rejects_road_traffic_on_metro() {
        let a = alert(
            "Info Trafic Routier - Arrêt « Mâcon »",
            "L'arrêt Mâcon est déplacé dans les deux sens de circulation.",
            &["idfm:STIF:Line::C01375:"],
            &[],
        );
        assert!(!alert_applies_to_leg_with_mode(
            &a,
            "idfm:trip1",
            "idfm:IDFM:C01375",
            "5",
            "idfm:stopA",
            "idfm:stopB",
            "METRO",
        ));
    }

    #[test]
    fn rejects_rer_works_secondary_mention_on_metro_5() {
        let a = alert(
            "⚠️ Importants travaux sur les RER B et RER D, privilégiez la ligne 14 et le RER E. Risque de saturation sur les lignes 4 et 5.",
            "",
            &["idfm:STIF:Line::C01375:"], // polled LineRef noise
            &[],
        );
        assert!(!alert_applies_to_leg_with_mode(
            &a,
            "idfm:trip1",
            "idfm:IDFM:C01375",
            "5",
            "idfm:stopA",
            "idfm:stopB",
            "METRO",
        ));
    }

    #[test]
    fn accepts_direct_metro_5_alert() {
        let a = alert(
            "Métro 5 : trafic ralenti",
            "Retards entre Place d'Italie et Gare de l'Est.",
            &["idfm:STIF:Line::C01375:"],
            &[],
        );
        assert!(alert_applies_to_leg_with_mode(
            &a,
            "idfm:trip1",
            "idfm:IDFM:C01375",
            "5",
            "idfm:stopA",
            "idfm:stopB",
            "METRO",
        ));
    }

    #[test]
    fn accepts_rer_b_on_rer_b_leg() {
        let a = alert(
            "Importants travaux sur les RER B et RER D",
            "Trafic perturbé sur le RER B.",
            &["idfm:STIF:Line::C01743:"],
            &[],
        );
        assert!(alert_applies_to_leg_with_mode(
            &a,
            "idfm:tripB",
            "idfm:IDFM:C01743",
            "B",
            "idfm:s1",
            "idfm:s2",
            "RAIL",
        ));
    }
}
