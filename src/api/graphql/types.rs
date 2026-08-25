use async_graphql::{ComplexObject, Context, Enum, Result, SimpleObject, Union, ID};
use chrono::{DateTime, Utc};

use crate::gtfs::pack::RouteMode as PackMode;
use crate::routing::journey::{Journey as CoreJourney, Leg as CoreLeg, Place as CorePlace};
use crate::rt::overlay::{AlertRt, RealtimeOverlay, StopTimeRt, TripRt, VehiclePos};
use crate::state::AppState;
use std::sync::Arc;

#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum Mode {
    Walk,
    Rail,
    Metro,
    Tram,
    Bus,
    Ferry,
    Coach,
    Other,
}

impl From<PackMode> for Mode {
    fn from(m: PackMode) -> Self {
        match m {
            PackMode::Tram => Mode::Tram,
            PackMode::Metro => Mode::Metro,
            PackMode::Rail => Mode::Rail,
            PackMode::Bus => Mode::Bus,
            PackMode::Ferry => Mode::Ferry,
            PackMode::Coach => Mode::Coach,
            PackMode::Other => Mode::Other,
        }
    }
}

impl Mode {
    pub fn to_pack(self) -> Option<PackMode> {
        Some(match self {
            Mode::Walk => return None,
            Mode::Rail => PackMode::Rail,
            Mode::Metro => PackMode::Metro,
            Mode::Tram => PackMode::Tram,
            Mode::Bus => PackMode::Bus,
            Mode::Ferry => PackMode::Ferry,
            Mode::Coach => PackMode::Coach,
            Mode::Other => PackMode::Other,
        })
    }
}

#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum RealtimeStatus {
    Scheduled,
    OnTime,
    Delayed,
    Early,
    Cancelled,
    /// Service alert / traffic disruption without trip cancel or measured delay.
    Disrupted,
    Partial,
    Unknown,
}

impl RealtimeStatus {
    pub fn parse(s: &str) -> Self {
        match s {
            "SCHEDULED" => Self::Scheduled,
            "ON_TIME" => Self::OnTime,
            "DELAYED" => Self::Delayed,
            "EARLY" => Self::Early,
            "CANCELLED" | "CANCELED" => Self::Cancelled,
            "DISRUPTED" => Self::Disrupted,
            "PARTIAL" => Self::Partial,
            _ => Self::Unknown,
        }
    }

    pub fn from_trip_rt(tr: Option<&TripRt>) -> Self {
        match tr {
            None => Self::Scheduled,
            Some(t) if t.canceled => Self::Cancelled,
            Some(t) => match t.delay {
                Some(d) if d > 0 => Self::Delayed,
                Some(d) if d < 0 => Self::Early,
                Some(_) => Self::OnTime,
                None if !t.stop_updates.is_empty() => Self::Partial,
                None => Self::Scheduled,
            },
        }
    }
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct GqlStop {
    pub id: ID,
    pub feed_id: String,
    pub name: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub is_station: bool,
    /// GTFS platform_code (keep existing field).
    pub platform_code: Option<String>,
    /// GTFS stop_code (UIC / commercial code). Empty until pack exposes it.
    pub code: Option<String>,
    /// GTFS stop_desc. Empty until pack exposes it.
    pub description: Option<String>,
    /// GTFS wheelchair_boarding (0 unknown, 1 accessible, 2 not).
    pub wheelchair: i32,
    /// GTFS `zone_id` when packed on StopRecord (null if not available).
    pub zone_id: Option<String>,
    /// GTFS `stop_url` when packed on StopRecord (null if not available).
    pub url: Option<String>,
    /// GTFS `stop_timezone` when packed on StopRecord (null if not available).
    pub timezone: Option<String>,
    /// Namespaced parent station id when present in GTFS.
    #[graphql(skip)]
    pub parent_id: Option<String>,
}

#[ComplexObject]
impl GqlStop {
    /// Parent station stop, if this platform/stop has a GTFS parent_station.
    async fn parent(&self, ctx: &Context<'_>) -> Result<Option<GqlStop>> {
        let Some(ref pid) = self.parent_id else {
            return Ok(None);
        };
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        Ok(epoch.get_stop(pid).map(stop_from_record))
    }

    /// Child platforms / stops that list this stop as parent_station.
    async fn children(&self, ctx: &Context<'_>) -> Result<Vec<GqlStop>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let id = self.id.as_str();
        Ok(epoch
            .stops
            .iter()
            .filter(|s| s.parent_id.as_deref() == Some(id))
            .map(stop_from_record)
            .collect())
    }
}

pub fn stop_from_record(s: &crate::gtfs::pack::StopRecord) -> GqlStop {
    GqlStop {
        id: ID(s.id.clone()),
        feed_id: s.feed_id.clone(),
        name: s.name.clone(),
        lat: s.lat,
        lon: s.lon,
        is_station: s.is_station(),
        platform_code: s.platform_code.clone(),
        code: s.stop_code.clone(),
        description: s.stop_desc.clone(),
        wheelchair: s.wheelchair as i32,
        zone_id: s.zone_id.clone(),
        url: s.stop_url.clone(),
        timezone: s.stop_timezone.clone(),
        parent_id: s.parent_id.clone(),
    }
}

/// GTFS agency row for GraphQL (`feed:agencyId` namespaced id).
#[derive(SimpleObject, Clone)]
pub struct Agency {
    pub id: ID,
    pub feed_id: String,
    pub name: String,
    pub url: Option<String>,
    pub timezone: Option<String>,
    pub phone: Option<String>,
}

pub fn agency_from_record(feed_id: &str, a: &crate::gtfs::pack::AgencyRecord) -> Agency {
    let id = if a.id.contains(':') {
        a.id.clone()
    } else {
        format!("{feed_id}:{}", a.id)
    };
    Agency {
        id: ID(id),
        feed_id: feed_id.to_string(),
        name: a.name.clone(),
        url: a.url.clone(),
        timezone: a.timezone.clone(),
        phone: a.phone.clone(),
    }
}

#[derive(SimpleObject, Clone)]
pub struct StopConnection {
    pub nodes: Vec<GqlStop>,
    pub total_count: i32,
}

#[derive(SimpleObject, Clone)]
pub struct VehiclePosition {
    /// Namespaced trip id when the RT vehicle is linked to a trip (`feed:tripId`).
    pub trip_id: Option<ID>,
    pub lat: f64,
    pub lon: f64,
    pub bearing: Option<f64>,
    pub speed: Option<f64>,
    pub updated_at: DateTime<Utc>,
    pub current_stop_id: Option<String>,
    /// Vehicle descriptor label (train/bus number plate).
    pub label: Option<String>,
    /// Vehicle descriptor id.
    pub vehicle_id: Option<String>,
    /// License plate when present.
    pub license_plate: Option<String>,
    /// Occupancy status string when present (e.g. MANY_SEATS_AVAILABLE).
    pub occupancy: Option<String>,
    /// Vehicle current_status (IN_TRANSIT_TO, STOPPED_AT, …).
    /// Synthetic positions from TripUpdate use `ESTIMATED_FROM_TRIP_UPDATE`.
    pub current_status: Option<String>,
    /// Current stop sequence from vehicle position.
    pub current_stop_sequence: Option<i32>,
    /// GTFS-RT congestion level name when present.
    pub congestion: Option<String>,
    /// Passenger occupancy 0–100 when present on RT vehicle position.
    pub occupancy_percentage: Option<i32>,
    /// Feed id owning this vehicle (static epoch / RT overlay).
    pub feed_id: Option<String>,
    /// Route short name from static epoch when trip is known.
    pub route_short_name: Option<String>,
    /// Route long name from static epoch when trip is known.
    pub route_long_name: Option<String>,
    /// Trip short name (train number) from static epoch.
    pub trip_short_name: Option<String>,
    /// Trip headsign from static epoch.
    pub headsign: Option<String>,
    /// Mode from static route when known.
    pub mode: Option<Mode>,
    /// Route color hex (no `#`) from static epoch.
    pub route_color: Option<String>,
    /// Trip-level RT delay seconds when available.
    pub delay_seconds: Option<i32>,
    /// Whether the linked trip is canceled in RT.
    pub canceled: bool,
    /// Best human-readable identity for map icons / lists.
    ///
    /// Prefer `tripShortName` (train number), then raw `label` (vehicle plate / SIRI
    /// PublishedLineName+destination), then `routeShortName` + truncated `headsign`,
    /// then the last meaningful segment of `tripId` (e.g. LineRef / journey digits).
    pub display_label: String,
    /// `GPS` = SIRI SM/ET VehicleLocation or GTFS-RT VP; `ESTIMATED` = progress along GTFS.
    pub position_source: String,
    /// True when position is interpolated from schedule/RT times (not onboard GPS).
    pub is_estimated: bool,
}

/// Last meaningful segment of a SIRI/GTFS ref for UI when static GTFS is missing.
///
/// Examples: `idfm:STIF:Line::C01371:` → `C01371`, `sncf:TRAIN1` → `TRAIN1`.
pub fn humanize_rt_ref(raw: &str) -> Option<String> {
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
        .split(|c| c == ':' || c == '/' || c == '@')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    for p in parts.iter().rev() {
        if SKIP.iter().any(|s| s.eq_ignore_ascii_case(p)) {
            continue;
        }
        // Skip pure calendar dates often trailing SNCF trip ids (YYYYMMDD).
        if looks_like_service_date(p) {
            continue;
        }
        if p.chars().any(|c| c.is_ascii_alphanumeric()) {
            if p.chars().any(|c| c.is_ascii_digit()) || p.len() <= 12 {
                return Some((*p).to_string());
            }
            if p.len() < 24 {
                return Some((*p).to_string());
            }
        }
    }
    parts
        .iter()
        .rev()
        .find(|p| !looks_like_service_date(p))
        .map(|s| (*s).to_string())
}

fn looks_like_service_date(s: &str) -> bool {
    s.len() == 8 && s.chars().all(|c| c.is_ascii_digit())
}

/// Computed map/list label for a vehicle (see [`VehiclePosition::display_label`]).
pub fn compute_display_label(
    trip_short_name: Option<&str>,
    label: Option<&str>,
    route_short_name: Option<&str>,
    headsign: Option<&str>,
    trip_id: Option<&str>,
) -> String {
    let usable = |s: &str| {
        let t = s.trim();
        !t.is_empty() && !looks_like_service_date(t)
    };
    if let Some(t) = trip_short_name.map(str::trim).filter(|s| usable(s)) {
        return t.to_string();
    }
    if let Some(l) = label.map(str::trim).filter(|s| usable(s)) {
        return l.to_string();
    }
    let route = route_short_name.map(str::trim).filter(|s| usable(s));
    let head = headsign.map(str::trim).filter(|s| usable(s));
    match (route, head) {
        (Some(r), Some(h)) => {
            let h_trunc: String = h.chars().take(24).collect();
            format!("{r} → {h_trunc}")
        }
        (Some(r), None) => r.to_string(),
        (None, Some(h)) => h.chars().take(32).collect(),
        (None, None) => trip_id
            .and_then(humanize_rt_ref)
            .filter(|s| usable(s))
            .unwrap_or_else(|| "train".into()),
    }
}

/// Human-readable map marker label (line/product name, not cryptic trip refs).
pub fn compute_map_label(
    mode: Option<Mode>,
    trip_short_name: Option<&str>,
    label: Option<&str>,
    route_short_name: Option<&str>,
    route_long_name: Option<&str>,
    headsign: Option<&str>,
    trip_id: Option<&str>,
) -> String {
    let blob = format!(
        "{} {} {} {}",
        route_long_name.unwrap_or(""),
        label.unwrap_or(""),
        route_short_name.unwrap_or(""),
        trip_short_name.unwrap_or("")
    )
    .to_ascii_lowercase();

    if blob.contains("ouigo") {
        if let Some(t) = trip_short_name.map(str::trim).filter(|s| !s.is_empty()) {
            return format!("Ouigo {t}");
        }
        if let Some(r) = route_short_name.map(str::trim).filter(|s| !s.is_empty()) {
            return format!("Ouigo {r}");
        }
        return "Ouigo".into();
    }
    if blob.contains("inoui") || blob.contains("in oui") {
        if let Some(t) = trip_short_name.map(str::trim).filter(|s| !s.is_empty()) {
            return format!("TGV InOui {t}");
        }
        return "TGV InOui".into();
    }
    if blob.contains("tgv") {
        if let Some(t) = trip_short_name.map(str::trim).filter(|s| !s.is_empty()) {
            return format!("TGV {t}");
        }
    }

    let route = route_short_name.unwrap_or("").trim();
    let long = route_long_name.unwrap_or("").trim();
    let long_upper = long.to_ascii_uppercase();

    if long_upper.contains("RER ") || long_upper.starts_with("RER") {
        if let Some(rest) = long.strip_prefix("RER").or_else(|| long.strip_prefix("rer")) {
            let letter = rest.trim().chars().next().filter(|c| c.is_ascii_alphabetic());
            if let Some(l) = letter {
                return format!("RER {}", l.to_ascii_uppercase());
            }
        }
    }
    if route.len() == 1 {
        let c = route.chars().next().unwrap().to_ascii_uppercase();
        if matches!(c, 'A' | 'B' | 'C' | 'D' | 'E') && matches!(mode, Some(Mode::Rail) | None) {
            return format!("RER {c}");
        }
    }

    if long_upper.contains("TRANSILIEN") {
        for letter in ['H', 'J', 'K', 'L', 'N', 'P', 'R', 'U', 'V'] {
            if long_upper.contains(&format!("TRANSILIEN {letter}"))
                || long_upper.contains(&format!("LIGNE {letter}"))
            {
                return format!("Transilien {letter}");
            }
        }
        if route.len() == 1 {
            return format!("Transilien {}", route.to_ascii_uppercase());
        }
    }

    if matches!(mode, Some(Mode::Metro)) {
        if let Ok(n) = route.parse::<u8>() {
            if (1..=14).contains(&n) {
                return format!("Métro {n}");
            }
        }
        if let Some(n) = long
            .split_whitespace()
            .find_map(|w| w.parse::<u8>().ok().filter(|n| (1..=14).contains(n)))
        {
            return format!("Métro {n}");
        }
    }

    if matches!(mode, Some(Mode::Tram)) {
        let tram = if route.to_ascii_uppercase().starts_with('T') && route.len() >= 2 {
            route.to_string()
        } else if let Some(pos) = long.to_ascii_uppercase().find('T') {
            long[pos..]
                .split_whitespace()
                .next()
                .unwrap_or(route)
                .to_string()
        } else if !route.is_empty() {
            format!("T{route}")
        } else {
            String::new()
        };
        if !tram.is_empty() {
            return format!("Tram {tram}");
        }
    }

    if matches!(mode, Some(Mode::Bus) | Some(Mode::Coach)) && !route.is_empty() {
        if !route.starts_with('C') || route.len() > 6 {
            return format!("Bus {route}");
        }
    }

    compute_display_label(
        trip_short_name,
        label,
        route_short_name,
        headsign,
        trip_id,
    )
}

fn finish_display_label(out: &mut VehiclePosition) {
    out.display_label = compute_map_label(
        out.mode,
        out.trip_short_name.as_deref(),
        out.label.as_deref(),
        out.route_short_name.as_deref(),
        out.route_long_name.as_deref(),
        out.headsign.as_deref(),
        out.trip_id.as_ref().map(|id| id.as_str()),
    );
}

/// Best-effort label from TripRt when the vehicle descriptor has none.
fn label_from_trip_rt(trip_key: &str, trip: &TripRt) -> Option<String> {
    trip.route_id
        .as_deref()
        .and_then(humanize_rt_ref)
        .or_else(|| humanize_rt_ref(trip_key))
}

/// Map a raw overlay vehicle without static enrichment (subscription fallback).
#[allow(dead_code)]
pub fn map_vehicle(v: &VehiclePos) -> VehiclePosition {
    map_vehicle_enriched(v, None, None, None)
}

/// Map a vehicle and fill route/mode/delay fields from epoch + trip RT when present.
pub fn map_vehicle_enriched(
    v: &VehiclePos,
    feed_id: Option<&str>,
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
    trip_rt: Option<&TripRt>,
) -> VehiclePosition {
    let is_estimated = v
        .current_status
        .as_deref()
        .map(|s| s.to_ascii_uppercase().contains("ESTIMATED"))
        .unwrap_or(false);
    let mut out = VehiclePosition {
        trip_id: v.trip_id.as_ref().map(|t| ID(t.clone())),
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
        current_stop_sequence: v.current_stop_sequence.map(|s| s as i32),
        congestion: v.congestion.clone(),
        occupancy_percentage: v.occupancy_percentage.map(|p| p as i32),
        feed_id: feed_id.map(|s| s.to_string()),
        route_short_name: None,
        route_long_name: None,
        trip_short_name: None,
        headsign: None,
        mode: None,
        route_color: None,
        delay_seconds: trip_rt.and_then(|t| t.delay),
        canceled: trip_rt.map(|t| t.canceled).unwrap_or(false),
        display_label: String::new(),
        position_source: if is_estimated {
            "ESTIMATED".into()
        } else {
            "GPS".into()
        },
        is_estimated,
    };
    if let (Some(epoch), Some(tid)) = (epoch, v.trip_id.as_deref()) {
        apply_trip_enrichment(&mut out, epoch, tid, trip_rt);
    } else if out.label.is_none() {
        if let Some(t) = trip_rt {
            out.label = t.route_id.as_deref().and_then(humanize_rt_ref);
        }
        if out.label.is_none() {
            out.label = v.trip_id.as_deref().and_then(humanize_rt_ref);
        }
    }
    // SIRI LineRef often does not match GTFS trip id — still recover mode/line from route_id.
    if out.mode.is_none() {
        if let (Some(epoch), Some(t)) = (epoch, trip_rt) {
            if let Some(ref rid) = t.route_id {
                enrich_from_siri_line_ref(&mut out, epoch, rid);
            }
        }
    }
    finish_display_label(&mut out);
    out
}

/// Map `idfm:STIF:Line::C01371:` (or similar) to static route short_name + mode.
fn enrich_from_siri_line_ref(
    out: &mut VehiclePosition,
    epoch: &crate::gtfs::pack::StaticEpoch,
    route_ref: &str,
) {
    // Extract C01371-style token
    let token = humanize_rt_ref(route_ref).unwrap_or_default();
    if token.is_empty() {
        return;
    }
    for t in &epoch.trips {
        if t.route_id.contains(&token) || t.route_id.ends_with(&token) {
            if out.route_short_name.is_none() && !t.route_short_name.is_empty() {
                out.route_short_name = Some(t.route_short_name.clone());
            }
            if out.mode.is_none() {
                out.mode = Some(Mode::from(t.mode));
            }
            if out.route_color.is_none() {
                out.route_color = t.route_color.clone();
            }
            break;
        }
    }
    // Fallback known IDFM metro line codes (partial)
    if out.mode.is_none() {
        let tok = token.to_ascii_uppercase();
        if tok.starts_with('C') && tok.len() >= 5 {
            // Heuristic: many metro Line refs are C0137x — still better as Metro than unknown
            if tok.starts_with("C013") || tok.starts_with("C016") || tok.starts_with("C017") {
                out.mode = Some(if tok.starts_with("C017") {
                    Mode::Rail // RER / Transilien family often C017xx
                } else if tok.starts_with("C0138") || tok.starts_with("C0139") {
                    Mode::Tram
                } else {
                    Mode::Metro
                });
            }
        }
    }
    if out.route_short_name.is_none() {
        out.route_short_name = Some(token);
    }
}

/// Build an **estimated** vehicle position from TripUpdate / SIRI SM–ET stop times.
///
/// Uses time-based progress along the GTFS stop chain (and shape when packed):
/// - dwell at stop when `now` is between expected arrival and departure
/// - interpolate between calls (ET multi-stop) or prev→next (SM single call)
///
/// Status is always `ESTIMATED_*` — not live GPS. Real VP always wins in GraphQL.
pub fn synthetic_vehicle_from_trip_update(
    feed_id: &str,
    trip_key: &str,
    trip: &TripRt,
    epoch: &crate::gtfs::pack::StaticEpoch,
    updated_at: DateTime<Utc>,
) -> Option<VehiclePos> {
    // Prefer SIRI-aware geolocation estimate (SM / ET expected times + geometry).
    let synth_label = label_from_trip_rt(trip_key, trip);
    let now = Utc::now();
    if let Some(mut pos) =
        crate::rt::estimate::estimate_vehicle_pos(feed_id, trip_key, trip, epoch, now)
    {
        // RT overlay stamp vs position clock: interpolate with `now`, expose fetch time.
        pos.updated_at = updated_at;
        if pos.label.is_none() {
            pos.label = synth_label;
        }
        return Some(pos);
    }
    if trip.canceled || trip.stop_updates.is_empty() {
        return None;
    }
    // Fallback: pin at highest-signal stop (no absolute times)
    let mut best: Option<&StopTimeRt> = None;
    let mut best_score: i64 = i64::MIN;
    for u in trip.stop_updates.values() {
        let has_signal = u.arrival_delay.is_some()
            || u.departure_delay.is_some()
            || u.arrival_time.is_some()
            || u.departure_time.is_some();
        let score = (if has_signal { 1_000_000i64 } else { 0 }) + u.stop_sequence as i64;
        if score > best_score {
            best_score = score;
            best = Some(u);
        }
    }
    let su = best?;
    let (lat, lon, stop_id) = resolve_synthetic_stop(epoch, feed_id, trip_key, su)?;
    let base_key = trip_key.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_key);
    Some(VehiclePos {
        trip_id: Some(base_key.to_string()),
        lat,
        lon,
        bearing: None,
        speed: None,
        updated_at,
        current_stop_id: Some(stop_id),
        label: synth_label,
        vehicle_id: None,
        license_plate: None,
        occupancy: None,
        current_status: Some("ESTIMATED_FROM_TRIP_UPDATE".into()),
        current_stop_sequence: Some(su.stop_sequence as u32),
        congestion: None,
        occupancy_percentage: None,
    })
}

/// Resolve stop coords for a synthetic VP: stop_id first, then trip stop_sequence.
fn resolve_synthetic_stop(
    epoch: &crate::gtfs::pack::StaticEpoch,
    feed_id: &str,
    trip_key: &str,
    su: &StopTimeRt,
) -> Option<(f64, f64, String)> {
    if let Some(ref sid) = su.stop_id {
        let namespaced = if sid.contains(':') {
            sid.clone()
        } else {
            format!("{feed_id}:{sid}")
        };
        for c in [sid.as_str(), namespaced.as_str()] {
            if let Some(&idx) = epoch.stop_id_to_idx.get(c) {
                if let Some(stop) = epoch.stops.get(idx as usize) {
                    if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
                        return Some((lat, lon, stop.id.clone()));
                    }
                }
            }
        }
    }
    let base = trip_key.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_key);
    let &trip_idx = epoch.trip_id_to_idx.get(base)?;
    let trip = epoch.trips.get(trip_idx as usize)?;
    for off in 0..trip.stop_time_len {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        if st.stop_sequence == su.stop_sequence {
            let stop = epoch.stops.get(st.stop_idx as usize)?;
            if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
                return Some((lat, lon, stop.id.clone()));
            }
        }
    }
    for off in (0..trip.stop_time_len).rev() {
        let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
        let stop = match epoch.stops.get(st.stop_idx as usize) {
            Some(s) => s,
            None => continue,
        };
        if let (Some(lat), Some(lon)) = (stop.lat, stop.lon) {
            return Some((lat, lon, stop.id.clone()));
        }
    }
    None
}

/// Fill optional static + RT trip fields on a vehicle from namespaced trip id.
pub fn apply_trip_enrichment(
    out: &mut VehiclePosition,
    epoch: &crate::gtfs::pack::StaticEpoch,
    trip_id: &str,
    trip_rt: Option<&TripRt>,
) {
    // Drop optional `@YYYYMMDD` suffix for static lookup.
    let base = trip_id.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_id);
    let hint = trip_rt.map(|t| {
        let mut first_dep: Option<i64> = None;
        for u in t.stop_updates.values() {
            let ts = u.departure_time.or(u.arrival_time);
            first_dep = match (first_dep, ts) {
                (None, Some(x)) => Some(x),
                (Some(a), Some(x)) => Some(a.min(x)),
                (a, None) => a,
            };
        }
        crate::gtfs::TripResolveHint {
            route_id: t.route_id.clone(),
            first_departure_utc: first_dep,
        }
    });
    let trip = crate::gtfs::resolve_static_trip(epoch, base, hint.as_ref()).or_else(|| {
        crate::gtfs::resolve_static_trip(epoch, trip_id, hint.as_ref())
    });
    let Some(trip) = trip else {
        // No static trip match: still expose a human line/journey fragment.
        if out.feed_id.is_none() {
            if let Some((fid, _)) = base.split_once(':') {
                out.feed_id = Some(fid.to_string());
            }
        }
        if let Some(t) = trip_rt {
            out.delay_seconds = out.delay_seconds.or(t.delay);
            out.canceled = out.canceled || t.canceled;
            if out.route_short_name.is_none() {
                out.route_short_name = t.route_id.as_deref().and_then(humanize_rt_ref);
            }
            if out.label.is_none() {
                out.label = t
                    .route_id
                    .as_deref()
                    .and_then(humanize_rt_ref)
                    .or_else(|| humanize_rt_ref(trip_id));
            }
        } else if out.label.is_none() {
            out.label = humanize_rt_ref(trip_id);
        }
        return;
    };
    out.feed_id = Some(trip.feed_id.clone());
    out.route_short_name = Some(trip.route_short_name.clone()).filter(|s| !s.is_empty());
    out.route_long_name = Some(trip.route_long_name.clone()).filter(|s| !s.is_empty());
    out.trip_short_name = trip.short_name.clone();
    out.headsign = trip.headsign.clone();
    out.mode = Some(Mode::from(trip.mode));
    out.route_color = trip.route_color.clone();
    if out.label.is_none() {
        out.label = trip
            .short_name
            .clone()
            .or_else(|| trip.headsign.clone())
            .or_else(|| Some(trip.route_short_name.clone()).filter(|s| !s.is_empty()));
    }
    if let Some(t) = trip_rt {
        out.delay_seconds = out.delay_seconds.or(t.delay);
        out.canceled = out.canceled || t.canceled;
    }
}

#[cfg(test)]
mod display_label_tests {
    use super::{compute_display_label, compute_map_label, humanize_rt_ref, Mode};

    #[test]
    fn prefer_trip_short_name() {
        assert_eq!(
            compute_display_label(
                Some("6610"),
                Some("plate"),
                Some("TER"),
                Some("Paris"),
                Some("sncf:x"),
            ),
            "6610"
        );
    }

    #[test]
    fn map_label_metro_line() {
        assert_eq!(
            compute_map_label(
                Some(Mode::Metro),
                Some("99"),
                None,
                Some("1"),
                Some("Château de Vincennes"),
                None,
                None,
            ),
            "Métro 1"
        );
    }

    #[test]
    fn map_label_rer_b() {
        assert_eq!(
            compute_map_label(
                Some(Mode::Rail),
                None,
                None,
                Some("B"),
                Some("RER B"),
                Some("Saint-Rémy"),
                None,
            ),
            "RER B"
        );
    }

    #[test]
    fn map_label_ouigo_and_inoui() {
        assert_eq!(
            compute_map_label(
                Some(Mode::Rail),
                Some("7821"),
                None,
                None,
                Some("OUIGO Paris - Lyon"),
                None,
                None,
            ),
            "Ouigo 7821"
        );
        assert_eq!(
            compute_map_label(
                Some(Mode::Rail),
                Some("6610"),
                None,
                None,
                Some("TGV INOUI Paris - Marseille"),
                None,
                None,
            ),
            "TGV InOui 6610"
        );
    }

    #[test]
    fn map_label_tram() {
        assert_eq!(
            compute_map_label(
                Some(Mode::Tram),
                None,
                None,
                Some("T3a"),
                Some("Porte de Versailles"),
                None,
                None,
            ),
            "Tram T3a"
        );
    }

    #[test]
    fn prefer_label_then_route_headsign() {
        assert_eq!(
            compute_display_label(None, Some("12 → Châtelet"), Some("12"), Some("Châtelet"), None),
            "12 → Châtelet"
        );
        assert_eq!(
            compute_display_label(None, None, Some("RER A"), Some("Cergy-le-Haut terminus"), None),
            "RER A → Cergy-le-Haut terminus"
        );
        assert_eq!(
            compute_display_label(None, None, Some("T3"), None, None),
            "T3"
        );
    }

    #[test]
    fn fallback_humanize_trip_id() {
        assert_eq!(
            humanize_rt_ref("idfm:STIF:Line::C01371:").as_deref(),
            Some("C01371")
        );
        assert_eq!(
            compute_display_label(
                None,
                None,
                None,
                None,
                Some("idfm:RATP-SIV:VehicleJourney::99"),
            ),
            "99"
        );
        assert_eq!(
            compute_display_label(None, None, None, None, None),
            "train"
        );
    }
}

/// GTFS `levels.txt` row for indoor / multi-level stations (when packed on epoch).
#[derive(SimpleObject, Clone)]
pub struct Level {
    pub id: ID,
    /// Numeric level index (GTFS `level_index`); lower is typically below ground.
    pub index: f64,
    pub name: Option<String>,
}

/// GTFS `pathways.txt` edge between stops/locations (when packed on epoch).
#[derive(SimpleObject, Clone)]
pub struct Pathway {
    pub id: ID,
    pub from_stop_id: ID,
    pub to_stop_id: ID,
    /// GTFS pathway_mode name (WALKWAY, STAIRS, …) or raw int string.
    pub mode: String,
    pub duration_seconds: Option<i32>,
    pub bidirectional: bool,
}

/// Active window for a service alert (GTFS-RT `active_period` entry).
#[derive(SimpleObject, Clone)]
pub struct TimeRange {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

/// Line / train product affected by a traffic alert (for map ticker badges).
#[derive(SimpleObject, Clone, Debug)]
pub struct TrafficLine {
    /// Namespaced or SIRI route ref (`idfm:STIF:Line::C01743:` / `idfm:IDFM:C01743`).
    pub route_id: String,
    /// Display label (e.g. `B`, `14`, `T1`, `H`).
    pub short_name: String,
    /// Background hex without `#` when known (GTFS `route_color` or IDFM palette).
    pub color: Option<String>,
    /// Foreground hex without `#` when known.
    pub text_color: Option<String>,
    pub mode: Option<Mode>,
}

#[derive(SimpleObject, Clone)]
pub struct Alert {
    pub id: ID,
    pub header: Option<String>,
    pub description: Option<String>,
    pub severity: String,
    pub cause: Option<String>,
    pub effect: Option<String>,
    pub url: Option<String>,
    /// First active period start (legacy; prefer `activePeriods`).
    pub active_start: Option<DateTime<Utc>>,
    /// First active period end (legacy; prefer `activePeriods`).
    pub active_end: Option<DateTime<Utc>>,
    /// Active windows from RT overlay (currently first period only when present).
    pub active_periods: Vec<TimeRange>,
    /// Raw informed route / LineRef ids from RT (namespaced).
    pub informed_route_ids: Vec<String>,
    /// Resolved lines for UI badges (short name + colour), deduped by short name.
    pub lines: Vec<TrafficLine>,
}

pub fn map_alert(a: &AlertRt) -> Alert {
    map_alert_enriched(a, None)
}

/// Map an alert and resolve affected lines from the static epoch when available.
pub fn map_alert_enriched(
    a: &AlertRt,
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
) -> Alert {
    use chrono::TimeZone;
    let ts = |secs: Option<i64>| {
        secs.and_then(|s| Utc.timestamp_opt(s, 0).single())
    };
    let active_start = ts(a.active_start);
    let active_end = ts(a.active_end);
    let active_periods = if a.active_start.is_some() || a.active_end.is_some() {
        vec![TimeRange {
            start: active_start,
            end: active_end,
        }]
    } else {
        vec![]
    };
    // Re-pick language from stored translations when present; always strip HTML.
    let header = if !a.header_translations.is_empty() {
        crate::rt::alert_text::pick_translation(&a.header_translations)
    } else {
        a.header
            .as_ref()
            .map(|h| crate::rt::alert_text::clean_alert_text(h))
            .filter(|s| !s.is_empty())
    };
    let description = a
        .description
        .as_ref()
        .map(|d| crate::rt::alert_text::clean_alert_text(d))
        .filter(|s| !s.is_empty());
    let lines = resolve_traffic_lines(
        epoch,
        &a.informed_route_ids,
        header.as_deref(),
        description.as_deref(),
    );
    Alert {
        id: ID(a.id.clone()),
        header,
        description,
        severity: a.severity.clone(),
        cause: a.cause.clone(),
        effect: a.effect.clone(),
        url: a.url.clone(),
        active_start,
        active_end,
        active_periods,
        informed_route_ids: a.informed_route_ids.clone(),
        lines,
    }
}

/// Resolve display lines for an alert (GTFS short_name + colour, text fallback).
pub fn resolve_traffic_lines(
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
    informed_route_ids: &[String],
    header: Option<&str>,
    description: Option<&str>,
) -> Vec<TrafficLine> {
    let mut out: Vec<TrafficLine> = Vec::new();
    let mut seen_short: std::collections::HashSet<String> = std::collections::HashSet::new();

    let push = |out: &mut Vec<TrafficLine>,
                seen: &mut std::collections::HashSet<String>,
                line: TrafficLine| {
        let key = line.short_name.to_ascii_uppercase();
        if key.is_empty() || !seen.insert(key) {
            return;
        }
        out.push(line);
    };

    for rid in informed_route_ids {
        if let Some(line) = resolve_one_route_line(epoch, rid) {
            push(&mut out, &mut seen_short, line);
        }
    }

    // Always merge lines mentioned in free text. IDFM GeneralMessage is often
    // polled per LineRef (so informed_route_ids is only "4"), while the body
    // also cites "RER B et RER D" — badges should show every product.
    let blob = format!(
        "{} {}",
        header.unwrap_or(""),
        description.unwrap_or("")
    );
    for line in lines_from_alert_text(&blob) {
        push(&mut out, &mut seen_short, line);
    }

    out
}

fn resolve_one_route_line(
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
    route_ref: &str,
) -> Option<TrafficLine> {
    let token = humanize_rt_ref(route_ref).unwrap_or_default();
    if token.is_empty() && route_ref.is_empty() {
        return None;
    }

    if let Some(epoch) = epoch {
        // Prefer exact / suffix match on packed route ids (IDFM:C01743).
        for bundle in epoch.feeds.values() {
            for (id, r) in &bundle.routes {
                let matches = id == route_ref
                    || id.ends_with(route_ref)
                    || route_ref.ends_with(id)
                    || (!token.is_empty()
                        && (id.contains(&token)
                            || r.id.contains(&token)
                            || id.ends_with(&token)));
                if !matches {
                    continue;
                }
                let short = if !r.short_name.trim().is_empty() {
                    r.short_name.trim().to_string()
                } else if !token.is_empty() {
                    token.clone()
                } else {
                    humanize_rt_ref(id).unwrap_or_else(|| id.clone())
                };
                let (color, text_color) = line_colors_for(&short, r.color.clone(), r.text_color.clone());
                return Some(TrafficLine {
                    route_id: id.clone(),
                    short_name: short,
                    color,
                    text_color,
                    mode: Some(Mode::from(r.mode)),
                });
            }
        }
        // Trip table may have short_name when routes map is sparse.
        for t in &epoch.trips {
            if !token.is_empty()
                && (t.route_id.contains(&token) || t.route_id.ends_with(&token))
            {
                let short = if !t.route_short_name.is_empty() {
                    t.route_short_name.clone()
                } else {
                    token.clone()
                };
                let (color, text_color) =
                    line_colors_for(&short, t.route_color.clone(), None);
                return Some(TrafficLine {
                    route_id: t.route_id.clone(),
                    short_name: short,
                    color,
                    text_color,
                    mode: Some(Mode::from(t.mode)),
                });
            }
        }
    }

    // No static match: still show a badge from the LineRef token / short text.
    let short = if !token.is_empty() {
        // IDFM commercial codes C01743 are not user-facing — try palette by known map.
        idfm_code_to_short(&token).unwrap_or(token)
    } else {
        humanize_rt_ref(route_ref)?
    };
    let (color, text_color) = line_colors_for(&short, None, None);
    Some(TrafficLine {
        route_id: route_ref.to_string(),
        short_name: short,
        color,
        text_color,
        mode: None,
    })
}

/// Map known IDFM LineRef product codes to commercial short names (RER/metro/tram).
fn idfm_code_to_short(code: &str) -> Option<String> {
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
        "C01684" => "T4",
        "C01690" => "T5",
        "C01794" => "T6",
        "C01774" => "T7",
        "C01795" => "T8",
        "C01999" => "T9",
        "C02317" => "T10",
        "C01928" => "T11",
        "C01995" => "T12",
        "C02344" => "T13",
        _ => return None,
    };
    Some(name.into())
}

/// Prefer GTFS colours; else Île-de-France RER/métro/tram palette by short name.
fn line_colors_for(
    short: &str,
    gtfs_color: Option<String>,
    gtfs_text: Option<String>,
) -> (Option<String>, Option<String>) {
    if let Some(c) = gtfs_color.filter(|s| !s.is_empty()) {
        let t = gtfs_text
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| contrast_text_for_hex(&c));
        return (Some(c), Some(t));
    }
    let key = short.trim().to_ascii_uppercase();
    let (bg, fg) = match key.as_str() {
        // RER
        "A" => ("EB2132", "FFFFFF"),
        "B" => ("5091CB", "FFFFFF"),
        "C" => ("FFCC30", "000000"),
        "D" => ("008B5B", "FFFFFF"),
        "E" => ("B94E9A", "FFFFFF"),
        // Métro (classic colours)
        "1" => ("FFBE00", "000000"),
        "2" => ("0055C8", "FFFFFF"),
        "3" => ("6E6E00", "FFFFFF"),
        "3BIS" | "3B" => ("6EC4E8", "000000"),
        "4" => ("A0006E", "FFFFFF"),
        "5" => ("FF7E2E", "000000"),
        "6" => ("6ECA97", "000000"),
        "7" => ("FF82B4", "000000"),
        "7BIS" | "7B" => ("6ECA97", "000000"),
        "8" => ("D282BE", "000000"),
        "9" => ("B6BD00", "000000"),
        "10" => ("C9910D", "000000"),
        "11" => ("704B1C", "FFFFFF"),
        "12" => ("007852", "FFFFFF"),
        "13" => ("6EC4E8", "000000"),
        "14" => ("640082", "FFFFFF"),
        // Tram
        "T1" => ("0055C8", "FFFFFF"),
        "T2" => ("CF009E", "FFFFFF"),
        "T3A" => ("FF7E2E", "000000"),
        "T3B" => ("00AE41", "FFFFFF"),
        "T4" => ("F68F4B", "000000"),
        "T5" => ("6E6E00", "FFFFFF"),
        "T6" => ("E2231A", "FFFFFF"),
        "T7" => ("704B1C", "FFFFFF"),
        "T8" => ("C5A3CD", "000000"),
        "T9" => ("4CC4F2", "000000"),
        "T10" => ("6E6E00", "FFFFFF"),
        "T11" | "T11 EXPRESS" => ("F58400", "000000"),
        "T12" | "T12 EXPRESS" => ("A50034", "FFFFFF"),
        "T13" | "T13 EXPRESS" => ("8D5E2A", "FFFFFF"),
        // Transilien
        "H" => ("7B5840", "FFFFFF"),
        "J" => ("CDCD00", "000000"),
        "K" => ("C5A3CD", "000000"),
        "L" => ("7575C9", "FFFFFF"),
        "N" => ("00A092", "FFFFFF"),
        "P" => ("F0B600", "000000"),
        "R" => ("E4B4D0", "000000"),
        "U" => ("D3403B", "FFFFFF"),
        "V" => ("9F9825", "FFFFFF"),
        _ => return (None, None),
    };
    (Some(bg.into()), Some(fg.into()))
}

fn contrast_text_for_hex(hex: &str) -> String {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 {
        return "FFFFFF".into();
    }
    let ok = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    let (Some(r), Some(g), Some(b)) = (ok(0), ok(2), ok(4)) else {
        return "FFFFFF".into();
    };
    // Relative luminance threshold
    let lum = 0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b);
    if lum > 160.0 {
        "000000".into()
    } else {
        "FFFFFF".into()
    }
}

/// Fold French accents for crude line-name matching in alert free text.
fn fold_alert_text(text: &str) -> String {
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

/// Extract RER / métro / tram / Transilien labels from free text.
fn lines_from_alert_text(text: &str) -> Vec<TrafficLine> {
    let upper = fold_alert_text(text);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut add = |name: &str| {
        let short = name.trim();
        if short.is_empty() {
            return;
        }
        let key = short.to_ascii_uppercase();
        if !seen.insert(key.clone()) {
            return;
        }
        let (color, text_color) = line_colors_for(short, None, None);
        out.push(TrafficLine {
            route_id: format!("text:{key}"),
            short_name: short.to_string(),
            color,
            text_color,
            mode: None,
        });
    };

    // RER A–E
    for letter in ["A", "B", "C", "D", "E"] {
        let patterns = [
            format!("RER {letter}"),
            format!("RER{letter}"),
            format!("LIGNE {letter}"),
        ];
        if patterns.iter().any(|p| upper.contains(p)) {
            add(letter);
        }
    }
    // Transilien H,J,K,L,N,P,R,U,V
    for letter in ["H", "J", "K", "L", "N", "P", "R", "U", "V"] {
        if upper.contains(&format!("TRANSILIEN {letter}"))
            || upper.contains(&format!("LIGNE {letter} "))
            || upper.ends_with(&format!("LIGNE {letter}"))
        {
            add(letter);
        }
    }
    // Métro 1–14 (also "lignes 4 et 5", "la ligne 14")
    for n in 1..=14 {
        let s = n.to_string();
        if upper.contains(&format!("METRO {s}"))
            || upper.contains(&format!("METRO{s}"))
            || upper.contains(&format!("LIGNE {s} "))
            || upper.contains(&format!("LIGNE {s}."))
            || upper.contains(&format!("LIGNE {s},"))
            || upper.contains(&format!("LIGNES {s}"))
            || upper.contains(&format!("LIGNES {s} "))
            || upper.contains(&format!(" ET {s}."))
            || upper.contains(&format!(" ET {s},"))
            || upper.contains(&format!(" ET {s} "))
            || upper.ends_with(&format!("LIGNE {s}"))
            || upper.ends_with(&format!(" ET {s}"))
        {
            // Avoid treating Transilien-style single letters as numbers only.
            add(&s);
        }
    }
    // Tram T1–T13
    for n in 1..=13 {
        let t = format!("T{n}");
        if upper.contains(&t)
            || upper.contains(&format!("TRAM {n}"))
            || upper.contains(&format!("TRAMWAY {n}"))
        {
            // Avoid matching lone "T" in words — require T + digit already in `t`.
            if upper.contains(&format!(" {t}"))
                || upper.contains(&format!("TRAM {t}"))
                || upper.starts_with(&t)
                || upper.contains(&format!("TRAMWAY T{n}"))
            {
                add(&t);
            }
        }
    }
    // Specials
    if upper.contains("T3A") {
        add("T3a");
    }
    if upper.contains("T3B") {
        add("T3b");
    }

    out
}

/// Realtime snapshot for a single namespaced trip id (`feed:rawTripId`).
#[derive(SimpleObject, Clone)]
pub struct TripRealtime {
    pub trip_id: ID,
    pub delay_seconds: Option<i32>,
    pub canceled: bool,
    pub status: RealtimeStatus,
    pub vehicle: Option<VehiclePosition>,
    pub alerts: Vec<Alert>,
    /// Overlay version when this snapshot was taken.
    pub rt_version: i64,
}

/// Snapshot without estimated positions (no static epoch). Prefer
/// [`trip_realtime_for_with_epoch`] for map tracking.
#[allow(dead_code)]
pub fn trip_realtime_for(trip_id: &str, rt: &RealtimeOverlay) -> TripRealtime {
    trip_realtime_for_with_epoch(trip_id, rt, None)
}

/// Like [`trip_realtime_for`], but can synthesize an **estimated** map position
/// when no GPS `VehicleLocation` / GTFS-RT VP is available.
pub fn trip_realtime_for_with_epoch(
    trip_id: &str,
    rt: &RealtimeOverlay,
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
) -> TripRealtime {
    let tr = rt.get_trip(trip_id);
    let vehicle = rt
        .get_vehicle_for_trip(trip_id)
        .map(|v| {
            let feed = trip_id.split(':').next();
            map_vehicle_enriched(v, feed, epoch, tr)
        })
        .or_else(|| {
            // Estimated position for itinerary tracking when SM/ET GPS is absent.
            let feed = trip_id.split(':').next().unwrap_or("idfm");
            let trip = tr?;
            let epoch = epoch?;
            let updated = Utc::now();
            let pos = synthetic_vehicle_from_trip_update(feed, trip_id, trip, epoch, updated)?;
            Some(map_vehicle_enriched(&pos, Some(feed), Some(epoch), Some(trip)))
        });
    let alerts = rt
        .alerts_for_trip(trip_id)
        .into_iter()
        .map(map_alert)
        .collect();
    TripRealtime {
        trip_id: ID(trip_id.to_string()),
        delay_seconds: tr.and_then(|t| t.delay),
        canceled: tr.map(|t| t.canceled).unwrap_or(false),
        status: RealtimeStatus::from_trip_rt(tr),
        vehicle,
        alerts,
        rt_version: rt.version as i64,
    }
}

/// WGS84 point for map polylines.
#[derive(SimpleObject, Clone, Debug, PartialEq)]
pub struct LatLng {
    pub lat: f64,
    pub lon: f64,
}

impl LatLng {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }

    /// Build a point only when both coordinates are present.
    pub fn from_opt(lat: Option<f64>, lon: Option<f64>) -> Option<Self> {
        match (lat, lon) {
            (Some(lat), Some(lon)) => Some(Self { lat, lon }),
            _ => None,
        }
    }
}

/// Straight-line path from endpoint coordinates (walk or missing shape).
pub fn geometry_from_endpoints(
    from_lat: Option<f64>,
    from_lon: Option<f64>,
    to_lat: Option<f64>,
    to_lon: Option<f64>,
) -> Vec<LatLng> {
    let mut out = Vec::with_capacity(2);
    if let Some(p) = LatLng::from_opt(from_lat, from_lon) {
        out.push(p);
    }
    if let Some(p) = LatLng::from_opt(to_lat, to_lon) {
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    out
}

/// Push a point if both lat/lon exist; skip consecutive duplicates.
fn push_stop_point(out: &mut Vec<LatLng>, lat: Option<f64>, lon: Option<f64>) {
    if let Some(p) = LatLng::from_opt(lat, lon) {
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
}

/// Transit path: board → intermediate stops → alight (stop chain when no GTFS shape).
pub fn geometry_transit_stop_chain(
    from: &GqlStop,
    intermediate: &[IntermediateStopGql],
    to: &GqlStop,
) -> Vec<LatLng> {
    let mut out = Vec::with_capacity(intermediate.len() + 2);
    push_stop_point(&mut out, from.lat, from.lon);
    for mid in intermediate {
        push_stop_point(&mut out, mid.stop.lat, mid.stop.lon);
    }
    push_stop_point(&mut out, to.lat, to.lon);
    out
}

/// Flatten leg geometries into a single journey polyline.
pub fn geometry_concat_legs(legs: &[Leg]) -> Vec<LatLng> {
    let mut out = Vec::new();
    for leg in legs {
        let pts = match leg {
            Leg::TransitLeg(t) => &t.geometry,
            Leg::WalkLeg(w) => &w.geometry,
        };
        for p in pts {
            if out.last() != Some(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

/// Stop-chain polyline for a full trip (ordered stop times).
pub fn geometry_from_trip_stops(stops: &[TripStopTime]) -> Vec<LatLng> {
    let mut out = Vec::with_capacity(stops.len());
    for st in stops {
        push_stop_point(&mut out, st.stop.lat, st.stop.lon);
    }
    out
}

#[derive(SimpleObject, Clone)]
pub struct IntermediateStopGql {
    pub stop: GqlStop,
    pub scheduled_arrival: DateTime<Utc>,
    pub scheduled_departure: DateTime<Utc>,
    pub realtime_arrival: Option<DateTime<Utc>>,
    pub realtime_departure: Option<DateTime<Utc>>,
}

#[derive(SimpleObject, Clone)]
pub struct TransitLeg {
    pub mode: Mode,
    pub route_short_name: Option<String>,
    pub route_long_name: Option<String>,
    pub agency_name: Option<String>,
    pub trip_id: Option<ID>,
    pub headsign: Option<String>,
    /// GTFS trip_short_name (e.g. train number).
    pub trip_short_name: Option<String>,
    /// GTFS direction_id (0/1).
    pub direction_id: Option<i32>,
    /// Route color hex without `#` when available.
    pub route_color: Option<String>,
    pub route_text_color: Option<String>,
    /// stop_times.stop_headsign at board stop when available.
    pub stop_headsign: Option<String>,
    /// Trip wheelchair_accessible (0/1/2).
    pub wheelchair: Option<i32>,
    /// Trip bikes_allowed (0/1/2).
    pub bikes_allowed: Option<i32>,
    pub from_stop: GqlStop,
    pub to_stop: GqlStop,
    pub scheduled_departure: DateTime<Utc>,
    pub scheduled_arrival: DateTime<Utc>,
    pub realtime_departure: Option<DateTime<Utc>>,
    pub realtime_arrival: Option<DateTime<Utc>>,
    pub delay_departure_seconds: Option<i32>,
    pub delay_arrival_seconds: Option<i32>,
    pub canceled: bool,
    pub vehicle: Option<VehiclePosition>,
    pub alerts: Vec<Alert>,
    pub intermediate_stops: Vec<IntermediateStopGql>,
    /// Map path for this leg (GTFS shape when available; else stop-chain endpoints).
    pub geometry: Vec<LatLng>,
    /// Stay-seated transfer: same GTFS block_id as previous transit leg in same feed.
    pub same_vehicle: bool,
}

#[derive(SimpleObject, Clone)]
pub struct WalkLeg {
    /// `WALK` or `BIKE` (street path for access/egress).
    pub mode: String,
    pub from_name: String,
    pub to_name: String,
    pub from_stop_id: Option<String>,
    pub to_stop_id: Option<String>,
    pub distance_meters: f64,
    pub duration_seconds: i32,
    pub from_lat: Option<f64>,
    pub from_lon: Option<f64>,
    pub to_lat: Option<f64>,
    pub to_lon: Option<f64>,
    /// Street path (OSRM when configured) or endpoints for map rendering.
    pub geometry: Vec<LatLng>,
}

#[derive(Union, Clone)]
pub enum Leg {
    TransitLeg(TransitLeg),
    WalkLeg(WalkLeg),
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Journey {
    pub id: ID,
    pub departure: DateTime<Utc>,
    pub arrival: DateTime<Utc>,
    pub duration_seconds: i32,
    pub transfers: i32,
    pub walk_distance_meters: f64,
    pub realtime_status: RealtimeStatus,
    pub legs: Vec<Leg>,
    pub alert_headers: Vec<String>,
    /// Full disruption messages for this journey (trip/stop/line matched).
    pub alerts: Vec<Alert>,
    /// Estimated fare amount when routing provides it (null if unknown / not modeled).
    pub fare_amount: Option<f64>,
    /// ISO 4217 currency code for `fareAmount` (e.g. EUR).
    pub fare_currency: Option<String>,
    /// Free-text fare note (e.g. "Navigo day pass", "approx").
    pub fare_note: Option<String>,
}

#[ComplexObject]
impl Journey {
    /// Concatenation of leg geometries for a single map polyline.
    async fn geometry(&self) -> Vec<LatLng> {
        geometry_concat_legs(&self.legs)
    }
}

#[derive(SimpleObject, Clone)]
pub struct Place {
    pub stop_id: Option<String>,
    pub name: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

#[derive(SimpleObject, Clone)]
pub struct ItineraryResult {
    pub from: Place,
    pub to: Place,
    pub computed_at: DateTime<Utc>,
    pub static_epoch_id: String,
    pub realtime_age_seconds: Option<i32>,
    pub realtime_degraded: bool,
    pub journeys: Vec<Journey>,
}

/// One departure from a stop board.
#[derive(SimpleObject, Clone)]
pub struct Departure {
    pub trip_id: ID,
    pub route_short_name: Option<String>,
    pub route_long_name: Option<String>,
    pub trip_short_name: Option<String>,
    pub headsign: Option<String>,
    pub mode: Mode,
    pub scheduled_departure: DateTime<Utc>,
    pub realtime_departure: Option<DateTime<Utc>>,
    pub delay_seconds: Option<i32>,
    pub canceled: bool,
    pub platform: Option<String>,
    pub route_color: Option<String>,
    pub stop: GqlStop,
    /// `gtfs-static` | `siri-stop-monitoring` | `gtfs-rt`
    pub source: Option<String>,
    /// SIRI departure/arrival status when live (onTime, delayed, …)
    pub status: Option<String>,
}

/// Full trip stop list with optional realtime.
#[derive(SimpleObject, Clone)]
pub struct TripDetail {
    pub id: ID,
    pub route_short_name: Option<String>,
    pub long_name: Option<String>,
    pub short_name: Option<String>,
    pub headsign: Option<String>,
    pub mode: Mode,
    pub direction_id: Option<i32>,
    pub color: Option<String>,
    pub stops: Vec<TripStopTime>,
    /// Full trip shape when packed; otherwise stop-chain coordinates (may be empty).
    pub geometry: Option<Vec<LatLng>>,
}

#[derive(SimpleObject, Clone)]
pub struct TripStopTime {
    pub stop: GqlStop,
    pub scheduled_arrival: DateTime<Utc>,
    pub scheduled_departure: DateTime<Utc>,
    pub realtime_arrival: Option<DateTime<Utc>>,
    pub realtime_departure: Option<DateTime<Utc>>,
    pub delay_seconds: Option<i32>,
    pub skipped: bool,
    pub platform: Option<String>,
}

#[derive(SimpleObject, Clone)]
pub struct FeedStatus {
    pub id: ID,
    pub static_loaded: bool,
    pub trip_updates_age_seconds: Option<i32>,
    pub vehicle_positions_age_seconds: Option<i32>,
    pub alerts_age_seconds: Option<i32>,
    pub trip_update_count: i32,
    pub vehicle_count: i32,
    pub alert_count: i32,
    pub ok: bool,
    /// GTFS `feed_info.feed_start_date` (YYYYMMDD) when packed.
    pub feed_start_date: Option<String>,
    /// GTFS `feed_info.feed_end_date` (YYYYMMDD) when packed.
    pub feed_end_date: Option<String>,
    /// GTFS `feed_info.feed_publisher_name` when packed (null if not available).
    pub publisher: Option<String>,
}

#[derive(SimpleObject, Clone)]
pub struct SystemHealth {
    pub status: String,
    pub feeds: Vec<FeedStatus>,
    pub stop_count: i32,
    pub trip_count: i32,
    pub epoch_id: String,
}

#[derive(async_graphql::InputObject, Debug, Clone)]
pub struct PlaceInput {
    pub stop_id: Option<ID>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub name: Option<String>,
}

/// BAN address autocomplete hit.
#[derive(SimpleObject, Clone)]
pub struct AddressSuggestion {
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
    /// `address` or `street`.
    pub kind: String,
}

/// Unified origin/destination suggestion (stop or BAN address).
#[derive(SimpleObject, Clone)]
pub struct PlaceSuggestion {
    /// `stop` or `address`.
    pub kind: String,
    pub id: String,
    pub label: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Namespaced stop id when `kind == stop`.
    pub stop_id: Option<String>,
    pub is_station: Option<bool>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub street: Option<String>,
}

#[derive(async_graphql::InputObject)]
pub struct NearInput {
    pub lat: f64,
    pub lon: f64,
    /// Search radius in meters (default 800, max 5000).
    #[graphql(default = 800.0)]
    pub radius_meters: f64,
}

/// Geographic bounding box for map viewport vehicle queries.
///
/// Exposed as GraphQL `BboxInput` (async-graphql name) — clients must use that spelling.
#[derive(async_graphql::InputObject, Clone, Debug)]
#[graphql(name = "BboxInput")]
pub struct BBoxInput {
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

impl BBoxInput {
    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.min_lat && lat <= self.max_lat && lon >= self.min_lon && lon <= self.max_lon
    }

    pub fn is_valid(&self) -> bool {
        (-90.0..=90.0).contains(&self.min_lat)
            && (-90.0..=90.0).contains(&self.max_lat)
            && (-180.0..=180.0).contains(&self.min_lon)
            && (-180.0..=180.0).contains(&self.max_lon)
            && self.min_lat <= self.max_lat
            && self.min_lon <= self.max_lon
    }
}

#[derive(async_graphql::InputObject)]
/// One GBFS bike/scooter-share station with live availability.
#[derive(SimpleObject, Clone)]
pub struct VehicleRentalStation {
    pub id: ID,
    pub feed_id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub capacity: Option<i32>,
    pub bikes_available: Option<i32>,
    pub docks_available: Option<i32>,
    pub is_renting: bool,
}

/// Itinerary planning input.
#[derive(async_graphql::InputObject, Debug, Clone)]
pub struct ItineraryInput {
    pub from: PlaceInput,
    pub to: PlaceInput,
    pub departure_at: Option<DateTime<Utc>>,
    pub arrive_by: Option<DateTime<Utc>>,
    #[graphql(default = 6)]
    pub max_transfers: i32,
    #[graphql(default = 5)]
    pub max_results: i32,
    pub modes: Option<Vec<Mode>>,
    /// Access / transfer walk budget (m). IDFM monomodal hubs need ≥2 km for Métro links.
    #[graphql(default = 2500)]
    pub max_walk_meters: i32,
    /// Prefer wheelchair-accessible trips/stops when routing supports it.
    pub wheelchair: Option<bool>,
    /// First km by bike (origin → first stop).
    pub bike_from: Option<bool>,
    /// Last km by bike (last stop → destination).
    pub bike_to: Option<bool>,
    /// Use Trip-Based Public Transit Routing (FLASH-TB) instead of RAPTOR.
    /// Defaults to true — FLASH-TB is the primary router.
    pub use_tbr: Option<bool>,
    /// Scenario planning: namespaced route/line ids to exclude from routing
    /// entirely ("what if line X is down?"). Trips on these lines are never
    /// boarded and alternative journeys are returned.
    #[graphql(default)]
    pub excluded_lines: Option<Vec<String>>,
}

/// Input for the `isochrone` query — "where can I travel to within N minutes?"
#[derive(async_graphql::InputObject, Debug, Clone)]
pub struct IsochroneInput {
    /// Origin (stop id or coordinates).
    pub origin: PlaceInput,
    /// Departure time; defaults to now. (Arrive-by isochrones are not supported.)
    pub departure_at: Option<DateTime<Utc>>,
    /// Travel-time budget in minutes (clamped 5–120).
    pub max_minutes: i32,
    /// Modes allowed for transit legs.
    pub modes: Option<Vec<Mode>>,
    /// Max walk/bike distance from origin to first stop (m).
    #[graphql(default = 1000)]
    pub max_access_meters: i32,
    /// Wheelchair-only trips and step-free preference.
    #[graphql(default)]
    pub wheelchair: bool,
    /// Scenario planning: namespaced route/line ids to exclude.
    #[graphql(default)]
    pub excluded_lines: Option<Vec<String>>,
}

/// One reachable stop with its earliest arrival inside the time budget.
#[derive(SimpleObject, Clone)]
pub struct IsochroneStop {
    pub stop: GqlStop,
    /// Arrival time at this stop.
    pub arrival_at: DateTime<Utc>,
    /// Total travel seconds from origin (including initial access walk).
    pub travel_seconds: i32,
    /// Number of transit legs used (0 = reached on foot only).
    pub legs: i32,
}

/// Build a GraphQL error with a machine-readable `code` extension.
pub fn gql_err(message: impl Into<String>, code: &str) -> async_graphql::Error {
    let mut err = async_graphql::Error::new(message);
    err.extensions
        .get_or_insert_with(Default::default)
        .set("code", code);
    err
}

pub fn mode_from_str(s: &str) -> Mode {
    match s {
        "RAIL" => Mode::Rail,
        "METRO" => Mode::Metro,
        "TRAM" => Mode::Tram,
        "BUS" => Mode::Bus,
        "FERRY" => Mode::Ferry,
        "COACH" => Mode::Coach,
        "WALK" => Mode::Walk,
        _ => Mode::Other,
    }
}

pub fn map_place(p: &CorePlace) -> Place {
    Place {
        stop_id: p.stop_id.clone(),
        name: p.name.clone(),
        lat: p.lat,
        lon: p.lon,
    }
}

fn stop_ref_to_gql(sr: &crate::routing::journey::StopRef) -> GqlStop {
    let feed_id = sr
        .stop_id
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();
    GqlStop {
        id: ID(sr.stop_id.clone()),
        feed_id,
        name: sr.name.clone(),
        lat: sr.lat,
        lon: sr.lon,
        is_station: true,
        platform_code: sr.platform.clone(),
        code: None,
        description: None,
        wheelchair: 0,
        zone_id: None,
        url: None,
        timezone: None,
        parent_id: None,
    }
}

/// Match GM / GTFS-RT alerts to a transit leg (trip, stops, line).
fn collect_alerts_for_leg(
    rt: &RealtimeOverlay,
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
    t: &crate::routing::journey::TransitLegData,
) -> Vec<Alert> {
    use crate::routing::journey::alert_applies_to_leg_with_mode;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for a in rt.feeds.values().flat_map(|f| f.alerts.iter()) {
        if alert_applies_to_leg_with_mode(
            a,
            &t.trip_id,
            &t.route_id,
            &t.route_short_name,
            &t.from.stop_id,
            &t.to.stop_id,
            &t.mode,
        ) {
            let mapped = map_alert_enriched(a, epoch);
            let key = crate::rt::alert_text::alert_dedupe_key(
                mapped.header.as_deref(),
                mapped.description.as_deref(),
            );
            if seen.insert(key) {
                out.push(mapped);
            }
        }
    }
    out
}

pub fn map_journey(
    j: &CoreJourney,
    rt: &RealtimeOverlay,
    epoch: Option<&crate::gtfs::pack::StaticEpoch>,
) -> Journey {
    let legs: Vec<Leg> = j
        .legs
        .iter()
        .map(|leg| match leg {
            CoreLeg::Transit(t) => {
                let leg_alerts = collect_alerts_for_leg(rt, epoch, t);
                let intermediate_stops: Vec<IntermediateStopGql> = t
                    .intermediate_stops
                    .iter()
                    .map(|is| IntermediateStopGql {
                        stop: stop_ref_to_gql(&is.stop),
                        scheduled_arrival: is.scheduled_arrival,
                        scheduled_departure: is.scheduled_departure,
                        realtime_arrival: is.realtime_arrival,
                        realtime_departure: is.realtime_departure,
                    })
                    .collect();
                let from_stop = stop_ref_to_gql(&t.from);
                let to_stop = stop_ref_to_gql(&t.to);
                // Prefer RAPTOR-filled shape/stop polyline (`(lat, lon)`); else stop chain.
                let geometry = if !t.geometry.is_empty() {
                    t.geometry
                        .iter()
                        .map(|&(lat, lon)| LatLng { lat, lon })
                        .collect()
                } else {
                    geometry_transit_stop_chain(&from_stop, &intermediate_stops, &to_stop)
                };
                Leg::TransitLeg(TransitLeg {
                    mode: mode_from_str(&t.mode),
                    route_short_name: Some(t.route_short_name.clone()),
                    route_long_name: Some(t.route_long_name.clone()),
                    agency_name: t.agency_name.clone(),
                    trip_id: Some(ID(t.trip_id.clone())),
                    headsign: t.headsign.clone(),
                    trip_short_name: t.trip_short_name.clone(),
                    direction_id: t.direction_id.map(|d| d as i32),
                    route_color: t.route_color.clone(),
                    route_text_color: t.route_text_color.clone(),
                    stop_headsign: t.stop_headsign.clone(),
                    wheelchair: Some(t.wheelchair as i32),
                    bikes_allowed: Some(t.bikes_allowed as i32),
                    from_stop,
                    to_stop,
                    scheduled_departure: t.scheduled_departure,
                    scheduled_arrival: t.scheduled_arrival,
                    realtime_departure: t.realtime_departure,
                    realtime_arrival: t.realtime_arrival,
                    delay_departure_seconds: t.delay_departure_s,
                    delay_arrival_seconds: t.delay_arrival_s,
                    canceled: t.canceled,
                    vehicle: match (t.vehicle_lat, t.vehicle_lon, t.vehicle_updated_at) {
                        (Some(lat), Some(lon), Some(updated_at)) => {
                            let v = rt.get_vehicle_for_trip(&t.trip_id);
                            let trip_rt = rt.get_trip(&t.trip_id);
                            let pos = VehiclePos {
                                trip_id: Some(t.trip_id.clone()),
                                lat,
                                lon,
                                bearing: v.and_then(|x| x.bearing),
                                speed: v.and_then(|x| x.speed),
                                updated_at,
                                current_stop_id: v.and_then(|x| x.current_stop_id.clone()),
                                label: v.and_then(|x| x.label.clone()),
                                vehicle_id: v.and_then(|x| x.vehicle_id.clone()),
                                license_plate: v.and_then(|x| x.license_plate.clone()),
                                occupancy: v.and_then(|x| x.occupancy.clone()),
                                current_status: v.and_then(|x| x.current_status.clone()),
                                current_stop_sequence: v.and_then(|x| x.current_stop_sequence),
                                congestion: v.and_then(|x| x.congestion.clone()),
                                occupancy_percentage: v.and_then(|x| x.occupancy_percentage),
                            };
                            let feed = t.trip_id.split_once(':').map(|(f, _)| f);
                            Some(map_vehicle_enriched(&pos, feed, epoch, trip_rt))
                        }
                        _ => None,
                    },
                    alerts: leg_alerts,
                    intermediate_stops,
                    geometry,
                    same_vehicle: t.same_vehicle,
                })
            }
            CoreLeg::Walk(w) => Leg::WalkLeg(WalkLeg {
                mode: if w.mode.is_empty() {
                    "WALK".into()
                } else {
                    w.mode.clone()
                },
                from_name: w.from_name.clone(),
                to_name: w.to_name.clone(),
                from_stop_id: w.from_stop_id.clone(),
                to_stop_id: w.to_stop_id.clone(),
                distance_meters: w.distance_m,
                duration_seconds: w.duration_s as i32,
                from_lat: w.from_lat,
                from_lon: w.from_lon,
                to_lat: w.to_lat,
                to_lon: w.to_lon,
                geometry: if w.geometry.len() >= 2 {
                    w.geometry
                        .iter()
                        .map(|&(lat, lon)| LatLng::new(lat, lon))
                        .collect()
                } else {
                    geometry_from_endpoints(w.from_lat, w.from_lon, w.to_lat, w.to_lon)
                },
            }),
        })
        .collect();

    // Deduped full alerts across all legs (for journey-level panel).
    let mut journey_alerts: Vec<Alert> = Vec::new();
    let mut seen_alert = std::collections::HashSet::new();
    for leg in legs.iter() {
        if let Leg::TransitLeg(tl) = leg {
            for a in &tl.alerts {
                let key = crate::rt::alert_text::alert_dedupe_key(
                    a.header.as_deref(),
                    a.description.as_deref(),
                );
                if seen_alert.insert(key) {
                    journey_alerts.push(a.clone());
                }
            }
        }
    }

    Journey {
        id: ID(j.id.clone()),
        departure: j.departure,
        arrival: j.arrival,
        duration_seconds: j.duration_s as i32,
        transfers: j.transfers as i32,
        walk_distance_meters: j.walk_distance_m,
        realtime_status: RealtimeStatus::parse(&j.realtime_status),
        legs,
        alert_headers: j.alert_headers.clone(),
        alerts: journey_alerts,
        fare_amount: j.fare_amount,
        fare_currency: j.fare_currency.clone(),
        fare_note: j.fare_note.clone(),
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn walk_geometry_endpoints() {
        let g = geometry_from_endpoints(Some(48.8), Some(2.3), Some(48.9), Some(2.4));
        assert_eq!(g.len(), 2);
        assert_eq!(g[0], LatLng::new(48.8, 2.3));
        assert_eq!(g[1], LatLng::new(48.9, 2.4));
    }

    #[test]
    fn walk_geometry_partial_missing() {
        let g = geometry_from_endpoints(Some(1.0), Some(2.0), None, Some(3.0));
        assert_eq!(g, vec![LatLng::new(1.0, 2.0)]);
    }

    #[test]
    fn transit_geometry_stop_chain() {
        let t0 = chrono::Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap();
        let from = GqlStop {
            id: ID("a".into()),
            feed_id: "f".into(),
            name: "A".into(),
            lat: Some(1.0),
            lon: Some(2.0),
            is_station: true,
            platform_code: None,
            code: None,
            description: None,
            wheelchair: 0,
            zone_id: None,
            url: None,
            timezone: None,
            parent_id: None,
        };
        let mid = IntermediateStopGql {
            stop: GqlStop {
                id: ID("b".into()),
                feed_id: "f".into(),
                name: "B".into(),
                lat: Some(1.5),
                lon: Some(2.5),
                is_station: true,
                platform_code: None,
                code: None,
                description: None,
                wheelchair: 0,
                zone_id: None,
                url: None,
                timezone: None,
                parent_id: None,
            },
            scheduled_arrival: t0,
            scheduled_departure: t0,
            realtime_arrival: None,
            realtime_departure: None,
        };
        let to = GqlStop {
            id: ID("c".into()),
            feed_id: "f".into(),
            name: "C".into(),
            lat: Some(2.0),
            lon: Some(3.0),
            is_station: true,
            platform_code: None,
            code: None,
            description: None,
            wheelchair: 0,
            zone_id: None,
            url: None,
            timezone: None,
            parent_id: None,
        };
        let g = geometry_transit_stop_chain(&from, &[mid], &to);
        assert_eq!(
            g,
            vec![
                LatLng::new(1.0, 2.0),
                LatLng::new(1.5, 2.5),
                LatLng::new(2.0, 3.0)
            ]
        );
    }

    #[test]
    fn journey_geometry_flattens_legs() {
        let walk = Leg::WalkLeg(WalkLeg {
            mode: "WALK".into(),
            from_name: "a".into(),
            to_name: "b".into(),
            from_stop_id: None,
            to_stop_id: None,
            distance_meters: 100.0,
            duration_seconds: 60,
            from_lat: Some(0.0),
            from_lon: Some(0.0),
            to_lat: Some(1.0),
            to_lon: Some(1.0),
            geometry: vec![LatLng::new(0.0, 0.0), LatLng::new(1.0, 1.0)],
        });
        let walk2 = Leg::WalkLeg(WalkLeg {
            mode: "WALK".into(),
            from_name: "b".into(),
            to_name: "c".into(),
            from_stop_id: None,
            to_stop_id: None,
            distance_meters: 100.0,
            duration_seconds: 60,
            from_lat: Some(1.0),
            from_lon: Some(1.0),
            to_lat: Some(2.0),
            to_lon: Some(2.0),
            geometry: vec![LatLng::new(1.0, 1.0), LatLng::new(2.0, 2.0)],
        });
        let flat = geometry_concat_legs(&[walk, walk2]);
        assert_eq!(
            flat,
            vec![
                LatLng::new(0.0, 0.0),
                LatLng::new(1.0, 1.0),
                LatLng::new(2.0, 2.0)
            ]
        );
    }
}

#[cfg(test)]
mod traffic_line_tests {
    use super::*;

    #[test]
    fn rer_b_from_line_ref_code() {
        let lines = resolve_traffic_lines(
            None,
            &["idfm:STIF:Line::C01743:".into()],
            Some("Perturbation"),
            Some("Trafic perturbé"),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].short_name, "B");
        assert_eq!(lines[0].color.as_deref(), Some("5091CB"));
    }

    #[test]
    fn rer_b_from_alert_text() {
        let lines = resolve_traffic_lines(
            None,
            &[],
            Some("Incident RER B"),
            Some("Retards entre Aulnay et Châtelet"),
        );
        assert!(
            lines.iter().any(|l| l.short_name == "B"),
            "expected B badge from text, got {:?}",
            lines
        );
        let b = lines.iter().find(|l| l.short_name == "B").unwrap();
        assert_eq!(b.color.as_deref(), Some("5091CB"));
    }

    #[test]
    fn metro_14_palette() {
        let lines = resolve_traffic_lines(None, &[], Some("Métro 14 ralenti"), None);
        assert!(lines.iter().any(|l| l.short_name == "14"));
    }
}
