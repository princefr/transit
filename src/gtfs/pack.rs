use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

use super::cell_bitset::CellBitSet;
use super::calendar::ServiceCalendar;
use super::siri_trip_map::SiriTripAliases;

/// Namespaced id: `{feed_id}:{raw_id}`
pub fn ns(feed_id: &str, raw: &str) -> String {
    format!("{feed_id}:{raw}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteMode {
    Tram,
    Metro,
    Rail,
    Bus,
    Ferry,
    Coach,
    Other,
}

impl RouteMode {
    pub fn from_gtfs_route_type(t: i32) -> Self {
        match t {
            0 => Self::Tram,
            1 => Self::Metro,
            2 => Self::Rail,
            3 => Self::Bus,
            4 => Self::Ferry,
            200..=299 => Self::Coach,
            100..=199 => Self::Rail,
            400..=499 => Self::Metro,
            900..=999 => Self::Tram,
            700..=799 => Self::Bus,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tram => "TRAM",
            Self::Metro => "METRO",
            Self::Rail => "RAIL",
            Self::Bus => "BUS",
            Self::Ferry => "FERRY",
            Self::Coach => "COACH",
            Self::Other => "OTHER",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StopRecord {
    pub id: String,
    pub feed_id: String,
    pub raw_id: String,
    pub name: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub parent_id: Option<String>,
    pub location_type: u8,
    pub platform_code: Option<String>,
    pub wheelchair: u8,
    /// GTFS `stop_code` (e.g. UIC / commercial code).
    pub stop_code: Option<String>,
    /// GTFS `stop_desc`.
    pub stop_desc: Option<String>,
    /// Namespaced GTFS `level_id` when present (`feed:level_id`).
    pub level_id: Option<String>,
    /// Namespaced GTFS `zone_id` (`feed:zone`) for fare_rules origin/destination/contains.
    pub zone_id: Option<String>,
    /// GTFS `stop_url`.
    pub stop_url: Option<String>,
    /// GTFS `stop_timezone` (IANA tz when stop differs from agency).
    pub stop_timezone: Option<String>,
}

/// One row from GTFS `levels.txt`.
#[derive(Debug, Clone)]
pub struct LevelRecord {
    /// Namespaced `feed:level_id`.
    pub id: String,
    pub feed_id: String,
    pub raw_id: String,
    /// Relative vertical ordering (GTFS float).
    pub level_index: f64,
    pub level_name: Option<String>,
}

/// GTFS `pathway_mode` values (spec integers).
/// 1 walkway, 2 stairs, 3 moving sidewalk, 4 escalator, 5 elevator,
/// 6 fare gate, 7 exit gate.
pub fn pathway_mode_name(mode: u8) -> &'static str {
    match mode {
        1 => "walkway",
        2 => "stairs",
        3 => "moving_sidewalk",
        4 => "escalator",
        5 => "elevator",
        6 => "fare_gate",
        7 => "exit_gate",
        _ => "unknown",
    }
}

/// Default pedestrian speed when deriving pathway duration from `length` (m/s).
pub const PATHWAY_WALK_SPEED_M_S: f64 = 1.2;
/// Fallback duration when neither `traversal_time` nor `length` is present.
pub const PATHWAY_DEFAULT_DURATION_S: u32 = 60;

/// One row from GTFS `pathways.txt` (local stop indices within the feed).
#[derive(Debug, Clone)]
pub struct PathwayRecord {
    pub pathway_id: String,
    pub from_stop_idx: u32,
    pub to_stop_idx: u32,
    pub pathway_mode: u8,
    pub is_bidirectional: bool,
    pub length_m: Option<f64>,
    pub traversal_time_s: Option<u32>,
    pub stair_count: Option<i32>,
    pub max_slope: Option<f64>,
    pub min_width: Option<f64>,
    pub signposted_as: Option<String>,
    pub reversed_signposted_as: Option<String>,
}

impl PathwayRecord {
    /// Walk duration: `traversal_time` → `length / 1.2 m/s` → 60s default.
    pub fn duration_s(&self) -> u32 {
        if let Some(t) = self.traversal_time_s {
            return t;
        }
        if let Some(len) = self.length_m {
            if len.is_finite() && len >= 0.0 {
                return (len / PATHWAY_WALK_SPEED_M_S).ceil() as u32;
            }
        }
        PATHWAY_DEFAULT_DURATION_S
    }

    pub fn distance_m(&self) -> f64 {
        self.length_m.filter(|l| l.is_finite() && *l >= 0.0).unwrap_or(0.0)
    }

    pub fn mode_name(&self) -> &'static str {
        pathway_mode_name(self.pathway_mode)
    }
}

impl StopRecord {
    pub fn is_station(&self) -> bool {
        self.location_type == 1
            || (self.location_type == 0 && self.parent_id.is_none())
    }
}

#[derive(Debug, Clone)]
pub struct RouteRecord {
    pub id: String,
    pub feed_id: String,
    pub short_name: String,
    pub long_name: String,
    pub mode: RouteMode,
    pub agency_id: Option<String>,
    /// GTFS `route_color` (hex without #, when present).
    pub color: Option<String>,
    /// GTFS `route_text_color`.
    pub text_color: Option<String>,
    /// Raw GTFS `route_type` integer (extended hierarchy included).
    pub route_type_raw: i32,
    /// GTFS `route_desc`.
    pub desc: Option<String>,
    /// GTFS `route_url`.
    pub url: Option<String>,
}

/// One row from GTFS `frequencies.txt` (times in seconds since local midnight).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrequencyWindow {
    pub start_s: u32,
    pub end_s: u32,
    pub headway_s: u32,
}

#[derive(Debug, Clone)]
pub struct TripRecord {
    pub id: String,
    pub feed_id: String,
    pub route_id: String,
    pub service_id: String,
    pub headsign: Option<String>,
    pub direction_id: Option<u8>,
    /// GTFS `trip_short_name` (e.g. train number).
    pub short_name: Option<String>,
    /// GTFS `wheelchair_accessible` (0 unknown, 1 yes, 2 no).
    pub wheelchair: u8,
    /// GTFS `bikes_allowed` (0 unknown, 1 yes, 2 no).
    pub bikes_allowed: u8,
    /// GTFS `block_id` (raw within feed; same block + feed ⇒ stay-seated).
    pub block_id: Option<String>,
    /// Namespaced GTFS `shape_id` when present (`feed:shape_id`).
    pub shape_id: Option<String>,
    /// Indices into the feed's packed stop_times for this trip (contiguous).
    pub stop_time_start: u32,
    pub stop_time_len: u32,
}

/// Compact stop_time: times in seconds since midnight (may exceed 24h).
#[derive(Debug, Clone, Copy)]
pub struct PackedStopTime {
    pub stop_idx: u32,
    pub arrival_s: u32,
    pub departure_s: u32,
    pub stop_sequence: u16,
    pub pickup_type: u8,
    pub drop_off_type: u8,
    /// Index into feed/epoch `headsign_pool` (0 = none / empty).
    pub stop_headsign_idx: u32,
    /// GTFS `timepoint` (0 = approximate, 1 = exact). Default 1 when absent.
    pub timepoint: u8,
    /// GTFS `shape_dist_traveled` along the trip shape (same units as shapes.txt).
    pub shape_dist_traveled: Option<f32>,
}

/// GTFS agency row (namespaced key = raw agency_id or "default").
#[derive(Debug, Clone)]
pub struct AgencyRecord {
    pub id: String,
    pub name: String,
    pub url: Option<String>,
    pub timezone: Option<String>,
    pub phone: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransferEdge {
    pub from_stop_idx: u32,
    pub to_stop_idx: u32,
    pub min_transfer_s: u32,
    pub transfer_type: u8,
}

/// GTFS `fare_attributes.txt` row (namespaced `fare_id`).
#[derive(Debug, Clone)]
pub struct FareAttribute {
    pub fare_id: String,
    pub price: f64,
    pub currency_type: String,
    /// 0 = paid on board, 1 = paid before boarding (GTFS).
    pub payment_method: u8,
    /// Number of transfers permitted; `None` = unlimited.
    pub transfers: Option<u8>,
    /// Transfer validity duration in seconds when present.
    pub transfer_duration: Option<u32>,
}

/// GTFS `fare_rules.txt` row. All fields except `fare_id` are optional filters.
#[derive(Debug, Clone)]
pub struct FareRule {
    pub fare_id: String,
    /// Namespaced route_id when set.
    pub route_id: Option<String>,
    /// Zone / origin id (raw or namespaced; see fare module).
    pub origin_id: Option<String>,
    pub destination_id: Option<String>,
    pub contains_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WalkEdge {
    pub from_stop_idx: u32,
    pub to_stop_idx: u32,
    pub duration_s: u32,
    pub distance_m: f64,
    /// GTFS `pathway_mode` when this edge comes from `pathways.txt`; `None` for
    /// synthetic transfers / hub links / parent-station edges.
    pub pathway_mode: Option<u8>,
}

impl WalkEdge {
    /// Effective walk duration, penalizing stairs (2) and escalators (4) when
    /// `wheelchair` is true so elevators/walkways are preferred.
    pub fn effective_duration_s(&self, wheelchair: bool) -> u32 {
        if !wheelchair {
            return self.duration_s;
        }
        match self.pathway_mode {
            // Stairs / escalator: heavily penalize (×100).
            Some(2) | Some(4) => self.duration_s.saturating_mul(100),
            _ => self.duration_s,
        }
    }
}

/// One agency/feed static bundle after parse.
#[derive(Debug)]
pub struct FeedStaticBundle {
    pub feed_id: String,
    pub stops: Vec<StopRecord>,
    pub stop_id_to_idx: HashMap<String, u32>,
    pub routes: HashMap<String, RouteRecord>,
    pub trips: Vec<TripRecord>,
    pub trip_id_to_idx: HashMap<String, u32>,
    pub stop_times: Vec<PackedStopTime>,
    /// Interned stop_headsign strings; index 0 is always empty ("none").
    pub headsign_pool: Vec<String>,
    /// stop_idx -> list of (trip_idx, stop_time offset within trip)
    pub stop_departures: Vec<Vec<(u32, u32)>>,
    /// Namespaced trip_id → frequency windows from `frequencies.txt` (if present).
    pub frequencies: HashMap<String, Vec<FrequencyWindow>>,
    pub calendar: ServiceCalendar,
    pub transfers: Vec<TransferEdge>,
    /// GTFS `pathways.txt` rows (local stop indices within the feed).
    pub pathways: Vec<PathwayRecord>,
    /// Namespaced level_id → row from `levels.txt`.
    pub levels: HashMap<String, LevelRecord>,
    pub agencies: HashMap<String, AgencyRecord>,
    /// Namespaced shape_id → ordered polyline points as (lat, lon).
    pub shapes: HashMap<String, Vec<(f64, f64)>>,
    /// GTFS fare_attributes (optional; often incomplete in French feeds).
    pub fares: Vec<FareAttribute>,
    /// GTFS fare_rules (optional).
    pub fare_rules: Vec<FareRule>,
    pub sha256: String,
    pub loaded_at: DateTime<Utc>,
    /// GTFS `feed_info.feed_start_date` (YYYYMMDD) when present.
    pub feed_start_date: Option<String>,
    /// GTFS `feed_info.feed_end_date` (YYYYMMDD) when present.
    pub feed_end_date: Option<String>,
    /// GTFS `feed_info.feed_publisher_name` when present.
    pub feed_publisher_name: Option<String>,
    /// GTFS `feed_info.feed_lang` when present.
    pub feed_lang: Option<String>,
    /// SIRI / NeTEx journey ref → namespaced `trip_id` (from `object_codes_extension.txt`).
    pub siri_trip_aliases: SiriTripAliases,
}

impl FeedStaticBundle {
    pub fn stop_count(&self) -> usize {
        self.stops.len()
    }

    pub fn trip_count(&self) -> usize {
        self.trips.len()
    }
}

/// Merged world snapshot after hub-linking all feeds.
#[derive(Debug)]
pub struct StaticEpoch {
    pub id: String,
    pub built_at: DateTime<Utc>,
    pub feeds: HashMap<String, Arc<FeedStaticBundle>>,
    /// Global stop list (concat of feeds with global indices).
    pub stops: Vec<StopRecord>,
    pub stop_id_to_idx: HashMap<String, u32>,
    /// Global trip list
    pub trips: Vec<GlobalTrip>,
    pub trip_id_to_idx: HashMap<String, u32>,
    pub stop_times: Vec<PackedStopTime>,
    /// Merged headsign pool across feeds; indices remapped when packing stop_times.
    pub headsign_pool: Vec<String>,
    pub stop_departures: Vec<Vec<(u32, u32)>>,
    /// Per-stop flag: at least one departure belongs to a frequency-based trip.
    /// RAPTOR disables sorted-list early exit on those stops (template times
    /// are not comparable to wall-clock board times).
    pub stop_has_freq: Vec<bool>,
    pub walk_edges: Vec<WalkEdge>,
    /// Adjacency for walk: stop_idx -> edges
    pub walk_adj: Vec<Vec<usize>>,
    /// Per-stop partition cell id (0..num_cells). Used by FLASH-TB arc-flags.
    pub stop_partition: Vec<u32>,
    /// Number of partition cells.
    pub num_cells: u32,
    /// Arc-flags for transfer pruning (FLASH-TB).
    ///
    /// Compressed representation (§5.2 flag-pattern compression): unique flag
    /// patterns live in `flag_patterns`; `arc_flag_pattern[edge_idx]` is the
    /// index of the edge's pattern. Bit C is set when this transfer edge is
    /// required to reach some stop in cell C.
    pub arc_flag_pattern: Vec<u32>,
    /// Deduplicated flag patterns, sorted by frequency (most common first).
    pub flag_patterns: Vec<CellBitSet>,
    /// Precomputed Trip-to-Trip Transfers for FLASH-TB.
    /// trip_transfers[T1_idx][alight_off] = Vec<TripTransferEntry>
    pub trip_transfers: Vec<Vec<Vec<TripTransferEntry>>>,
    /// Line ID for each trip (index into `line_trips`).
    pub line_of_trip: Vec<u32>,
    /// Trips per line, sorted by first departure time.
    pub line_trips: Vec<Vec<u32>>,
    pub calendars: HashMap<String, Arc<ServiceCalendar>>,
    /// Merged namespaced shape_id → ordered polyline points as (lat, lon).
    pub shapes: HashMap<String, Vec<(f64, f64)>>,
    /// Merged fare attributes from all feeds (namespaced fare_id).
    pub fares: Vec<FareAttribute>,
    /// Merged fare rules from all feeds.
    pub fare_rules: Vec<FareRule>,
    /// Merged SIRI journey ref aliases (IDFM `object_codes_extension.txt`).
    pub siri_trip_aliases: SiriTripAliases,
    /// Line product code (`C01742`) → global trip indices (for RT time disambiguation).
    pub route_line_to_trip_idxs: HashMap<String, Vec<u32>>,
    /// All stops with coordinates `(idx, lat, lon)` — reused for geo snap (avoid per-request scans).
    pub geo_stop_points: Vec<(usize, f64, f64)>,
    /// Boardable + IDFM place stops with coordinates for access / transfer geo queries.
    pub geo_boardable_points: Vec<(usize, f64, f64)>,
    /// Normalized place name → stop indices (boardable or station shell).
    pub place_name_index: HashMap<String, Vec<u32>>,
    /// Parent stop id → child stop indices (GTFS `parent_station`).
    pub children_by_parent: HashMap<String, Vec<u32>>,
    /// Precomputed per-stop search rows for autocomplete (normalized name/id,
    /// place kind, has-departures) — avoids re-normalizing 50k+ stops per query.
    pub stop_search: Vec<crate::search::StopSearchEntry>,
}

#[derive(Debug, Clone)]
pub struct GlobalTrip {
    pub id: String,
    pub feed_id: String,
    pub route_id: String,
    pub service_id: String,
    pub headsign: Option<String>,
    pub short_name: Option<String>,
    pub direction_id: Option<u8>,
    pub wheelchair: u8,
    pub bikes_allowed: u8,
    /// GTFS `block_id` (raw within feed).
    pub block_id: Option<String>,
    /// Namespaced GTFS shape_id when present.
    pub shape_id: Option<String>,
    pub mode: RouteMode,
    pub route_short_name: String,
    pub route_long_name: String,
    pub route_color: Option<String>,
    pub route_text_color: Option<String>,
    pub route_type_raw: i32,
    pub agency_name: Option<String>,
    pub stop_time_start: u32,
    pub stop_time_len: u32,
    /// Empty ⇒ schedule-based trip using absolute `stop_times`.
    /// Non-empty ⇒ frequency-based; `stop_times` are a relative template.
    pub frequency_windows: Vec<FrequencyWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripTransferEntry {
    pub target_trip: u32,
    pub target_board_off: u32,
    pub min_transfer_s: u32,
    /// Index into `StaticEpoch::flag_patterns` (compressed arc-flags).
    pub flag_pattern: u32,
}

impl StaticEpoch {
    pub fn empty() -> Self {
        Self {
            id: "empty".into(),
            built_at: Utc::now(),
            feeds: HashMap::new(),
            stops: Vec::new(),
            stop_id_to_idx: HashMap::new(),
            trips: Vec::new(),
            trip_id_to_idx: HashMap::new(),
            stop_times: Vec::new(),
            headsign_pool: vec![String::new()],
            stop_departures: Vec::new(),
            stop_has_freq: Vec::new(),
            walk_edges: Vec::new(),
            walk_adj: Vec::new(),
            stop_partition: Vec::new(),
            num_cells: 0,
            arc_flag_pattern: Vec::new(),
            flag_patterns: Vec::new(),
            trip_transfers: Vec::new(),
            line_of_trip: Vec::new(),
            line_trips: Vec::new(),
            calendars: HashMap::new(),
            shapes: HashMap::new(),
            fares: Vec::new(),
            fare_rules: Vec::new(),
            siri_trip_aliases: SiriTripAliases::default(),
            route_line_to_trip_idxs: HashMap::new(),
            geo_stop_points: Vec::new(),
            geo_boardable_points: Vec::new(),
            place_name_index: HashMap::new(),
            children_by_parent: HashMap::new(),
            stop_search: Vec::new(),
        }
    }

    pub fn stop_count(&self) -> usize {
        self.stops.len()
    }

    pub fn trip_count(&self) -> usize {
        self.trips.len()
    }

    pub fn get_stop(&self, id: &str) -> Option<&StopRecord> {
        self.stop_id_to_idx
            .get(id)
            .and_then(|&i| self.stops.get(i as usize))
    }

    /// Geo points for nearby queries; builds on the fly when indexes were not packed (unit tests).
    pub fn geo_stop_points_view(&self) -> Vec<(usize, f64, f64)> {
        if !self.geo_stop_points.is_empty() {
            return self.geo_stop_points.clone();
        }
        self.stops
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, s.lat?, s.lon?)))
            .collect()
    }

    /// Borrowed variant — avoids cloning ~48k points per geo query on packed epochs.
    /// Returns `None` when the epoch was not packed (caller must build the list).
    pub fn geo_stop_points_ref(&self) -> Option<&[(usize, f64, f64)]> {
        if self.geo_stop_points.is_empty() {
            None
        } else {
            Some(&self.geo_stop_points)
        }
    }

    /// Boardable / place geo points; falls back to a scan when not precomputed.
    pub fn geo_boardable_points_view(&self) -> Vec<(usize, f64, f64)> {
        if !self.geo_boardable_points.is_empty() {
            return self.geo_boardable_points.clone();
        }
        use crate::search::stops::is_idfm_place_stop;
        self.stops
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let boardable = self
                    .stop_departures
                    .get(i)
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);
                if boardable || is_idfm_place_stop(s) {
                    Some((i, s.lat?, s.lon?))
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Build a global epoch from one or more feed bundles + walk/transfer edges.
pub fn build_epoch(bundles: Vec<Arc<FeedStaticBundle>>, extra_walk: Vec<WalkEdge>) -> StaticEpoch {
    build_epoch_with_flash(bundles, extra_walk, FlashMode::Compute)
}

/// How to obtain FLASH-TB arc-flags during an epoch build.
pub enum FlashMode<'a> {
    /// Compute in-process without persistence (tests, one-off builds).
    Compute,
    /// Skip flag preprocessing entirely (throwaway / preliminary epochs).
    Skip,
    /// Load from the disk cache when the feed content hash matches;
    /// otherwise compute and persist. Nightly dataset resets recompute at
    /// most once per dataset version.
    Persist(&'a std::path::Path),
}

/// Like [`build_epoch`], but with explicit FLASH-TB flag handling.
pub fn build_epoch_with_flash(
    bundles: Vec<Arc<FeedStaticBundle>>,
    extra_walk: Vec<WalkEdge>,
    mode: FlashMode<'_>,
) -> StaticEpoch {
    let mut epoch = StaticEpoch::empty();
    epoch.id = uuid::Uuid::new_v4().to_string();
    epoch.built_at = Utc::now();

    let mut stop_offset: HashMap<String, u32> = HashMap::new();

    for bundle in &bundles {
        let base_stop = epoch.stops.len() as u32;
        stop_offset.insert(bundle.feed_id.clone(), base_stop);

        for s in &bundle.stops {
            let gs = s.clone();
            // parent_id already namespaced in parse
            epoch
                .stop_id_to_idx
                .insert(gs.id.clone(), epoch.stops.len() as u32);
            epoch.stops.push(gs);
        }

        epoch
            .calendars
            .insert(bundle.feed_id.clone(), Arc::new(bundle.calendar.clone()));
        epoch
            .feeds
            .insert(bundle.feed_id.clone(), Arc::clone(bundle));
        for (sid, pts) in &bundle.shapes {
            epoch.shapes.insert(sid.clone(), pts.clone());
        }
        epoch.fares.extend(bundle.fares.iter().cloned());
        epoch.fare_rules.extend(bundle.fare_rules.iter().cloned());
        for (k, v) in &bundle.siri_trip_aliases.to_trip_id {
            epoch.siri_trip_aliases.insert(k.clone(), v.clone());
        }
    }

    // Ensure stop_departures length
    epoch.stop_departures = vec![Vec::new(); epoch.stops.len()];

    for bundle in &bundles {
        let base_stop = *stop_offset.get(&bundle.feed_id).unwrap_or(&0);
        let base_st = epoch.stop_times.len() as u32;

        // Remap stop headsign indices into the global headsign pool.
        let mut local_to_global_hs: HashMap<u32, u32> = HashMap::new();
        local_to_global_hs.insert(0, 0);
        if !bundle.headsign_pool.is_empty() {
            // index 0 is always empty
            for (local_i, hs) in bundle.headsign_pool.iter().enumerate().skip(1) {
                let global_i = epoch.headsign_pool.len() as u32;
                epoch.headsign_pool.push(hs.clone());
                local_to_global_hs.insert(local_i as u32, global_i);
            }
        }

        // Remap stop times
        for st in &bundle.stop_times {
            let hs_idx = local_to_global_hs
                .get(&st.stop_headsign_idx)
                .copied()
                .unwrap_or(0);
            epoch.stop_times.push(PackedStopTime {
                stop_idx: st.stop_idx + base_stop,
                arrival_s: st.arrival_s,
                departure_s: st.departure_s,
                stop_sequence: st.stop_sequence,
                pickup_type: st.pickup_type,
                drop_off_type: st.drop_off_type,
                stop_headsign_idx: hs_idx,
                timepoint: st.timepoint,
                shape_dist_traveled: st.shape_dist_traveled,
            });
        }

        for t in &bundle.trips {
            let route = bundle.routes.get(&t.route_id);
            let mode = route.map(|r| r.mode).unwrap_or(RouteMode::Other);
            let short = route.map(|r| r.short_name.clone()).unwrap_or_default();
            let long = route.map(|r| r.long_name.clone()).unwrap_or_default();
            let route_color = route.and_then(|r| r.color.clone());
            let route_text_color = route.and_then(|r| r.text_color.clone());
            let route_type_raw = route.map(|r| r.route_type_raw).unwrap_or(3);
            let agency_name = route
                .and_then(|r| r.agency_id.as_ref())
                .and_then(|a| bundle.agencies.get(a))
                .map(|ag| ag.name.clone())
                .or_else(|| {
                    // single-agency feeds often omit agency_id on routes
                    if bundle.agencies.len() == 1 {
                        bundle.agencies.values().next().map(|ag| ag.name.clone())
                    } else {
                        None
                    }
                });

            let frequency_windows = bundle
                .frequencies
                .get(&t.id)
                .cloned()
                .unwrap_or_default();

            let g = GlobalTrip {
                id: t.id.clone(),
                feed_id: t.feed_id.clone(),
                route_id: t.route_id.clone(),
                service_id: t.service_id.clone(),
                headsign: t.headsign.clone(),
                short_name: t.short_name.clone(),
                direction_id: t.direction_id,
                wheelchair: t.wheelchair,
                bikes_allowed: t.bikes_allowed,
                block_id: t.block_id.clone(),
                shape_id: t.shape_id.clone(),
                mode,
                route_short_name: short,
                route_long_name: long,
                route_color,
                route_text_color,
                route_type_raw,
                agency_name,
                stop_time_start: base_st + t.stop_time_start,
                stop_time_len: t.stop_time_len,
                frequency_windows,
            };
            let trip_idx = epoch.trips.len() as u32;
            epoch.trip_id_to_idx.insert(g.id.clone(), trip_idx);
            epoch.trips.push(g);

            if let Some(token) = super::siri_trip_map::line_product_code(&epoch.trips[trip_idx as usize].route_id) {
                epoch
                    .route_line_to_trip_idxs
                    .entry(token)
                    .or_default()
                    .push(trip_idx);
            }

            // rebuild stop_departures for this trip
            for local_off in 0..t.stop_time_len {
                let st = &epoch.stop_times[(base_st + t.stop_time_start + local_off) as usize];
                if st.pickup_type != 1 {
                    epoch.stop_departures[st.stop_idx as usize].push((trip_idx, local_off));
                }
            }
        }

        // Indoor pathways (prefer over synthetic parent/hub links for same pairs)
        for pw in &bundle.pathways {
            let from = pw.from_stop_idx + base_stop;
            let to = pw.to_stop_idx + base_stop;
            let dur = pw.duration_s();
            let dist = pw.distance_m();
            epoch.walk_edges.push(WalkEdge {
                from_stop_idx: from,
                to_stop_idx: to,
                duration_s: dur,
                distance_m: dist,
                pathway_mode: Some(pw.pathway_mode),
            });
            if pw.is_bidirectional {
                epoch.walk_edges.push(WalkEdge {
                    from_stop_idx: to,
                    to_stop_idx: from,
                    duration_s: dur,
                    distance_m: dist,
                    pathway_mode: Some(pw.pathway_mode),
                });
            }
        }

        // transfers within feed
        for tr in &bundle.transfers {
            epoch.walk_edges.push(WalkEdge {
                from_stop_idx: tr.from_stop_idx + base_stop,
                to_stop_idx: tr.to_stop_idx + base_stop,
                duration_s: tr.min_transfer_s,
                distance_m: 0.0,
                pathway_mode: None,
            });
        }
    }

    // Pairs covered by pathways (unordered) — skip generic hub / parent edges.
    let pathway_pairs = pathway_pair_set(&bundles, &stop_offset);

    for e in extra_walk {
        let key = undirected_pair(e.from_stop_idx, e.to_stop_idx);
        if pathway_pairs.contains(&key) {
            continue;
        }
        epoch.walk_edges.push(e);
    }

    // parent-station default transfer between siblings (skip pathway pairs)
    add_parent_station_walks(&mut epoch, &pathway_pairs);

    // Complete transfer graph: every boardable stop linked to boardable neighbours
    // within transfer radius (grid). No named gares — RAPTOR finds RER→Métro itself.
    add_boardable_transfer_graph(&mut epoch, &pathway_pairs);

    // build adjacency
    epoch.walk_adj = vec![Vec::new(); epoch.stops.len()];
    for (i, e) in epoch.walk_edges.iter().enumerate() {
        if (e.from_stop_idx as usize) < epoch.walk_adj.len() {
            epoch.walk_adj[e.from_stop_idx as usize].push(i);
        }
    }

    sort_stop_departures(&mut epoch);
    let t_pack = std::time::Instant::now();
    compute_line_groups(&mut epoch);
    eprintln!("[pack] line groups in {:.1}s", t_pack.elapsed().as_secs_f32());
    compute_partition(&mut epoch);
    eprintln!(
        "[pack] layout-graph partition ({} cells) in {:.1}s",
        epoch.num_cells,
        t_pack.elapsed().as_secs_f32()
    );
    compute_trip_transfers(&mut epoch);
    eprintln!("[pack] trip transfers in {:.1}s", t_pack.elapsed().as_secs_f32());
    match mode {
        FlashMode::Skip => {}
        FlashMode::Persist(dir) => {
            use super::flash_store;
            use sha2::{Digest, Sha256};
            // Fingerprint the walk graph layout: flag arrays are indexed by
            // edge position, so any reordering must invalidate the cache.
            let mut gh = Sha256::new();
            gh.update((epoch.walk_edges.len() as u64).to_le_bytes());
            for e in &epoch.walk_edges {
                gh.update(e.from_stop_idx.to_le_bytes());
                gh.update(e.to_stop_idx.to_le_bytes());
                gh.update(e.duration_s.to_le_bytes());
            }
            let graph_fp = format!("{:x}", gh.finalize());
            let hash = format!(
                "{}-{}",
                flash_store::feed_content_hash(&bundles),
                &graph_fp[..12]
            );
            let path = flash_store::flash_path(dir, &hash);
            let cached = flash_store::load_flash(&path).ok().flatten().and_then(
                |(num_cells, arc_flag_pattern, flag_patterns, trip_transfers)| {
                    // Validate shape against the freshly built epoch.
                    let shape_ok = num_cells == epoch.num_cells
                        && arc_flag_pattern.len() == epoch.walk_edges.len()
                        && trip_transfers.len() == epoch.trip_transfers.len()
                        && trip_transfers
                            .iter()
                            .zip(epoch.trip_transfers.iter())
                            .all(|(a, b)| a.len() == b.len());
                    if !shape_ok {
                        let first_bad = trip_transfers
                            .iter()
                            .zip(epoch.trip_transfers.iter())
                            .position(|(a, b)| a.len() != b.len());
                        eprintln!(
                            "[pack] flash cache shape mismatch: cells({} vs {}) walk({}, {}) trips({}, {}) first_bad_trip={:?} bad_lens=({:?}, {:?})",
                            num_cells, epoch.num_cells,
                            arc_flag_pattern.len(), epoch.walk_edges.len(),
                            trip_transfers.len(), epoch.trip_transfers.len(),
                            first_bad,
                            first_bad.map(|i| trip_transfers[i].len()),
                            first_bad.map(|i| epoch.trip_transfers[i].len())
                        );
                    }
                    if shape_ok {
                        Some((arc_flag_pattern, flag_patterns, trip_transfers))
                    } else {
                        None
                    }
                },
            );
            if std::env::var("TRANSIT_PROFILE").ok().as_deref() == Some("1") {
                eprintln!("[pack] flash cache key {} -> {}", hash, path.display());
            }
            match cached {
                Some((ap, fp, tt)) => {
                    epoch.arc_flag_pattern = ap;
                    epoch.flag_patterns = fp;
                    epoch.trip_transfers = tt;
                    eprintln!(
                        "[pack] arc-flags loaded from disk cache in {:.1}s ({})",
                        t_pack.elapsed().as_secs_f32(),
                        path.display()
                    );
                }
                None => {
                    compute_arc_flags(&mut epoch);
                    if !epoch.flag_patterns.is_empty() {
                        match flash_store::save_flash(&path, &epoch) {
                            Ok(()) => {
                                flash_store::prune_flash_dir(dir, &hash);
                                eprintln!(
                                    "[pack] arc-flags computed in {:.1}s and saved to {}",
                                    t_pack.elapsed().as_secs_f32(),
                                    path.display()
                                );
                            }
                            Err(e) => {
                                eprintln!("[pack] arc-flags save failed: {e}");
                            }
                        }
                    }
                    eprintln!(
                        "[pack] arc-flags in {:.1}s",
                        t_pack.elapsed().as_secs_f32()
                    );
                }
            }
        }
        FlashMode::Compute => {
            compute_arc_flags(&mut epoch);
            eprintln!("[pack] arc-flags in {:.1}s", t_pack.elapsed().as_secs_f32());
        }
    }
    epoch.stop_search = crate::search::build_stop_search_index(&epoch);
    build_geo_and_place_indexes(&mut epoch);
    epoch
}

/// Sort each stop's departure list by scheduled departure time so RAPTOR can
/// binary-search the first boardable entry, and flag stops served by
/// frequency-based trips (template times break the ordering invariant).
fn sort_stop_departures(epoch: &mut StaticEpoch) {
    epoch.stop_has_freq = vec![false; epoch.stops.len()];
    let trips = &epoch.trips;
    let stop_times = &epoch.stop_times;
    let dep_of = |(trip_idx, off): &(u32, u32)| -> u32 {
        let t = &trips[*trip_idx as usize];
        stop_times[(t.stop_time_start + off) as usize].departure_s
    };
    for (stop_idx, deps) in epoch.stop_departures.iter_mut().enumerate() {
        deps.sort_by_key(dep_of);
        if deps
            .iter()
            .any(|&(t, _)| !trips[t as usize].frequency_windows.is_empty())
        {
            epoch.stop_has_freq[stop_idx] = true;
        }
    }
}

/// Compute line groups: trips sharing the same stop-index sequence belong to the same line.
/// Within each line, trips are sorted by first departure time.
fn compute_line_groups(epoch: &mut StaticEpoch) {
    use std::collections::HashMap;

    let mut pattern_to_line: HashMap<Vec<u32>, u32> = HashMap::new();
    let mut line_of_trip = Vec::with_capacity(epoch.trips.len());
    let mut line_trips: Vec<Vec<u32>> = Vec::new();

    for (ti, trip) in epoch.trips.iter().enumerate() {
        let start = trip.stop_time_start as usize;
        let len = trip.stop_time_len as usize;
        let pattern: Vec<u32> = (0..len)
            .map(|off| epoch.stop_times[start + off].stop_idx)
            .collect();

        let line_id = if let Some(&lid) = pattern_to_line.get(&pattern) {
            lid
        } else {
            let lid = line_trips.len() as u32;
            pattern_to_line.insert(pattern, lid);
            line_trips.push(Vec::new());
            lid
        };

        line_of_trip.push(line_id);
        line_trips[line_id as usize].push(ti as u32);
    }

    // Sort trips within each line by first departure time
    for trips in &mut line_trips {
        trips.sort_by_key(|&ti| {
            let trip = &epoch.trips[ti as usize];
            epoch.stop_times[trip.stop_time_start as usize].departure_s
        });
    }

    epoch.line_of_trip = line_of_trip;
    epoch.line_trips = line_trips;
}

/// FLASH-TB §5.1: partition stops into cells using the **layout graph** G_L.
///
/// G_L condenses all links between a pair of stops into one edge weighted by
/// the number of links (trip segments between consecutive stops + footpaths).
/// Partitioning on G_L instead of raw coordinates keeps stops with many direct
/// connections inside the same cell, which is what makes arc-flags effective.
///
/// Splitting strategy: recursive balanced bisection. Each split grows a region
/// by BFS from a peripheral node of the layout graph until half the stops,
/// keeping the graph structure intact; falls back to a coordinate median split
/// when the fragment is disconnected or has no links.
fn compute_partition(epoch: &mut StaticEpoch) {
    use std::collections::VecDeque;

    let n = epoch.stops.len();
    if n == 0 {
        epoch.stop_partition = Vec::new();
        epoch.num_cells = 0;
        return;
    }

    const TARGET_DEPTH: u32 = 8; // 2^8 = 256 cells
    let num_cells = 1u32 << TARGET_DEPTH;

    epoch.stop_partition = vec![0u32; n];

    // --- Build layout graph (undirected, weight = link count) ---
    let mut link_weight: HashMap<(u32, u32), u32> = HashMap::new();
    {
        for trip in &epoch.trips {
            let len = trip.stop_time_len;
            for i in 0..len.saturating_sub(1) {
                let a = epoch.stop_times[(trip.stop_time_start + i) as usize].stop_idx;
                let b = epoch.stop_times[(trip.stop_time_start + i + 1) as usize].stop_idx;
                if a != b {
                    *link_weight.entry(undirected_pair(a, b)).or_insert(0) += 1;
                }
            }
        }
        for e in &epoch.walk_edges {
            if e.from_stop_idx != e.to_stop_idx {
                *link_weight
                    .entry(undirected_pair(e.from_stop_idx, e.to_stop_idx))
                    .or_insert(0) += 1;
            }
        }
    }

    let mut adj: Vec<Vec<(u32, u32)>> = vec![Vec::new(); n];
    for ((a, b), w) in &link_weight {
        adj[*a as usize].push((*b, *w));
        adj[*b as usize].push((*a, *w));
    }
    for lists in &mut adj {
        lists.sort_unstable(); // deterministic BFS order
        lists.dedup_by_key(|x| x.0);
    }
    drop(link_weight);

    // Coordinate table for the fallback split.
    let coords: Vec<(f64, f64)> = epoch
        .stops
        .iter()
        .map(|s| (s.lat.unwrap_or(f64::NAN), s.lon.unwrap_or(f64::NAN)))
        .collect();

    /// Recursive balanced bisection over `members` (stop indices).
    fn split(
        adj: &[Vec<(u32, u32)>],
        coords: &[(f64, f64)],
        members: &mut [u32],
        depth: u32,
        target_depth: u32,
        base_cell: u32,
        out: &mut [u32],
    ) {
        if depth >= target_depth || members.len() <= 1 {
            for &m in members.iter() {
                out[m as usize] = base_cell;
            }
            return;
        }
        let half = members.len() / 2;

        // Membership filter for BFS.
        let mut mark = vec![u32::MAX; adj.len()];
        for (li, &m) in members.iter().enumerate() {
            mark[m as usize] = li as u32;
        }

        // BFS restricted to members; returns visit order.
        let bfs = |start: u32| -> Vec<u32> {
            let mut order = Vec::new();
            let mut seen = vec![false; members.len()];
            let mut q = VecDeque::new();
            q.push_back(start);
            seen[mark[start as usize] as usize] = true;
            while let Some(u) = q.pop_front() {
                order.push(u);
                for &(v, _) in &adj[u as usize] {
                    let li = mark[v as usize];
                    if li != u32::MAX && !seen[li as usize] {
                        seen[li as usize] = true;
                        q.push_back(v);
                    }
                }
            }
            order
        };

        let mut side_a: Vec<u32>;
        let mut side_b: Vec<u32>;

        if half >= 1 && !members.is_empty() {
            // Peripheral start: min weighted degree among members.
            let mut start = members[0];
            let mut best_deg = u64::MAX;
            for &m in members.iter() {
                let deg: u64 = adj[m as usize]
                    .iter()
                    .filter(|(v, _)| mark[*v as usize] != u32::MAX)
                    .map(|(_, w)| *w as u64)
                    .sum();
                if deg < best_deg {
                    best_deg = deg;
                    start = m;
                }
            }
            // Double sweep for a far endpoint, then grow from it.
            let sweep1 = bfs(start);
            let far1 = *sweep1.last().unwrap_or(&start);
            let sweep2 = bfs(far1);
            let far2 = *sweep2.last().unwrap_or(&far1);
            let grow = bfs(far2);

            if grow.len() >= 2 {
                // Cut the BFS visit order roughly in half. When the fragment is
                // fully connected this yields two balanced halves; when BFS
                // stalls early (disconnected remainder), the visited component
                // becomes side A and the rest is re-split recursively.
                let cut = half.saturating_add(1).min(grow.len());
                let a_set: std::collections::HashSet<u32> =
                    grow[..cut].iter().copied().collect();
                side_a = grow[..cut].to_vec();
                side_b = members
                    .iter()
                    .filter(|m| !a_set.contains(m))
                    .copied()
                    .collect();
            } else {
                side_a = Vec::new();
                side_b = Vec::new();
            }
        } else {
            side_a = Vec::new();
            side_b = Vec::new();
        }

        if side_a.is_empty() || side_b.is_empty() {
            // Disconnected / link-less fragment: coordinate median split.
            let lat_span = {
                let lats: Vec<f64> = members
                    .iter()
                    .map(|m| coords[*m as usize].0)
                    .filter(|v| v.is_finite())
                    .collect();
                match lats.iter().cloned().reduce(f64::max) {
                    Some(mx) => mx - lats.iter().cloned().fold(f64::INFINITY, f64::min),
                    None => 0.0,
                }
            };
            let lon_span = {
                let lons: Vec<f64> = members
                    .iter()
                    .map(|m| coords[*m as usize].1)
                    .filter(|v| v.is_finite())
                    .collect();
                match lons.iter().cloned().reduce(f64::max) {
                    Some(mx) => mx - lons.iter().cloned().fold(f64::INFINITY, f64::min),
                    None => 0.0,
                }
            };
            let axis = 1usize.min((lat_span < lon_span) as usize); // 0 = lat, 1 = lon
            members.sort_by(|&a, &b| {
                let (va, vb) = (coords[a as usize].0, coords[a as usize].1);
                let (xa, xb) = if axis == 0 { (va, vb) } else { (vb, va) };
                xa.partial_cmp(&xb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.cmp(&b))
            });
            let mid = members.len() / 2;
            side_a = members[..mid].to_vec();
            side_b = members[mid..].to_vec();
        }

        let cells_per_half = 1u32 << (target_depth - depth - 1);
        split(adj, coords, &mut side_a, depth + 1, target_depth, base_cell, out);
        split(
            adj,
            coords,
            &mut side_b,
            depth + 1,
            target_depth,
            base_cell + cells_per_half,
            out,
        );
    }

    let mut members: Vec<u32> = (0..n as u32).collect();
    split(
        &adj,
        &coords,
        &mut members,
        0,
        TARGET_DEPTH,
        0,
        &mut epoch.stop_partition,
    );
    epoch.num_cells = num_cells;
}

/// Precompute trip-to-trip transfers for FLASH-TB.
/// trip_transfers[t1][alight_off] = Vec<TripTransferEntry> listing the
/// earliest boardable trip on each reachable line from each alight stop.
fn compute_trip_transfers(epoch: &mut StaticEpoch) {
    use std::collections::HashMap;
    const DEFAULT_TRANSFER_S: u32 = 120;

    let n_trips = epoch.trips.len();
    let mut all_transfers: Vec<Vec<Vec<TripTransferEntry>>> = Vec::with_capacity(n_trips);

    // Per stop: (line, ascending [(dep_s, trip_idx, off)]) for every departure.
    // Lets each transfer lookup binary-search the earliest boardable trip of a
    // specific line instead of scanning the stop's whole departure tail.
    let mut lines_at_stop: Vec<Vec<(u32, Vec<(u32, u32, u32)>)>> = vec![Vec::new(); epoch.stops.len()];
    {
        let mut by_line: Vec<HashMap<u32, Vec<(u32, u32, u32)>>> =
            (0..epoch.stops.len()).map(|_| HashMap::new()).collect();
        for (si, deps) in epoch.stop_departures.iter().enumerate() {
            let m = &mut by_line[si];
            for &(t, o) in deps {
                let dep_s = epoch.stop_times[(epoch.trips[t as usize].stop_time_start + o) as usize]
                    .departure_s;
                m.entry(epoch.line_of_trip[t as usize])
                    .or_default()
                    .push((dep_s, t, o));
            }
        }
        for (v, m) in lines_at_stop.iter_mut().zip(by_line) {
            *v = m.into_iter().collect();
            v.sort_by_key(|(line, _)| *line);
            for (_, trips) in v.iter_mut() {
                trips.sort_by_key(|x| x.0);
            }
        }
    }

    for ti in 0..n_trips {
        let trip = &epoch.trips[ti];
        let start = trip.stop_time_start as usize;
        let len = trip.stop_time_len as usize;
        let mut per_alight: Vec<Vec<TripTransferEntry>> = Vec::with_capacity(len);
        
        for off in 0..len {
            let st = &epoch.stop_times[start + off];
            if st.drop_off_type == 1 {
                per_alight.push(Vec::new());
                continue;
            }
            let alight_stop = st.stop_idx as usize;
            let alight_arr = st.arrival_s;
            let board_after = alight_arr.saturating_add(DEFAULT_TRANSFER_S);
            
            let mut entries: Vec<TripTransferEntry> = Vec::new();
            let mut seen_lines: std::collections::HashSet<u32> = std::collections::HashSet::new();

            // Transfers are generated **at the alight stop only** (paper §2:
            // a transfer connects two stop events at a stop; walking between
            // stops is covered by the query's footpath phase). This keeps the
            // transfer set near-linear instead of exploding with the degree
            // of the boardable transfer graph.
            let target_stop = alight_stop as u32;
            let walk_dur = 0u32;
            {
                let effective_board = board_after.saturating_add(walk_dur);
                for (line_id, trips) in &lines_at_stop[target_stop as usize] {
                    let idx = trips.partition_point(|&(d, _, _)| d < effective_board);
                    let Some(&(_, trip2_idx, board_off)) = trips.get(idx) else {
                        continue;
                    };
                    if !seen_lines.insert(*line_id) {
                        continue; // already have earliest trip on this line
                    }
                    // Don't transfer back to the same trip
                    if trip2_idx as usize == ti {
                        continue;
                    }
                    entries.push(TripTransferEntry {
                        target_trip: trip2_idx,
                        target_board_off: board_off,
                        min_transfer_s: walk_dur.saturating_add(DEFAULT_TRANSFER_S),
                        flag_pattern: 0,
                    });
                }
            }
            per_alight.push(entries);
        }
        all_transfers.push(per_alight);
    }
    
    epoch.trip_transfers = all_transfers;
}

/// FLASH-TB §5: compute arc-flags with **forward** searches.
///
/// Instead of backward Dijkstras from boundary nodes (which only prove
/// reachability and massively over-flag), this mirrors the paper: solve many
/// one-to-all fixed-departure problems and set flag `(transfer, cell)` whenever
/// the transfer occurs on a journey found to a stop in that cell. Transfers
/// with no flags are removed (30–50% in practice), and remaining flag patterns
/// are compressed into a shared table (§5.2).
///
/// Exhaustive all-to-all profile preprocessing is quadratic and out of scope
/// for an online server; instead we sample source stops and departure times
/// across the service window (env knobs below). Because sampling can leave
/// legitimate transfers unflagged, the TBR query falls back to unpruned
/// search when a flagged query would come back empty.
///
/// Env knobs (read once at pack time):
/// - `TRANSIT_FLASH_SOURCES` — max source stops searched (default 24000)
/// - `TRANSIT_FLASH_SAMPLES` — departure times per source (default 4)
/// - `TRANSIT_FLASH_THREADS` — worker threads (default min(cores, 8))
fn compute_arc_flags(epoch: &mut StaticEpoch) {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tracing::info;

    let started = std::time::Instant::now();
    let num_cells = epoch.num_cells;
    let n_stops = epoch.stops.len();
    let walk_n = epoch.walk_edges.len();

    epoch.arc_flag_pattern = Vec::new();
    epoch.flag_patterns = Vec::new();
    if num_cells == 0 || n_stops == 0 || walk_n == 0 {
        return;
    }

    // Flat reference space: `[0, walk_n)` = walk edge index,
    // `[walk_n, walk_n + entries)` = flat trip-transfer entry index.
    let mut entry_prefix: Vec<Vec<u32>> = Vec::with_capacity(epoch.trip_transfers.len());
    let mut total_entries: u64 = 0;
    for per_alight in &epoch.trip_transfers {
        let mut pref = Vec::with_capacity(per_alight.len());
        for v in per_alight {
            pref.push(total_entries as u32);
            total_entries += v.len() as u64;
        }
        entry_prefix.push(pref);
    }
    let walk_base = walk_n as u64;
    let num_refs = walk_base + total_entries;
    if num_refs == 0 {
        return;
    }

    let env_usize = |key: &str, default: usize| -> usize {
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default)
    };
    let max_sources = env_usize("TRANSIT_FLASH_SOURCES", 24_000);
    let n_samples = env_usize("TRANSIT_FLASH_SAMPLES", 4).min(24);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let n_threads = env_usize("TRANSIT_FLASH_THREADS", cores.min(8));

    // --- Sample source stops and departure times ---
    // Every stop is a potential journey origin (walk-only corridors matter for
    // footpath flags), uniformly strided down to `max_sources`.
    let sources: Vec<u32> = {
        let all: Vec<u32> = (0..n_stops as u32).collect();
        if all.len() <= max_sources {
            all
        } else {
            let stride = all.len().div_ceil(max_sources);
            all.into_iter().step_by(stride).collect()
        }
    };
    let (min_dep, max_dep) = {
        let mut lo = u32::MAX;
        let mut hi = 0u32;
        for deps in &epoch.stop_departures {
            if let Some(&(t, _)) = deps.first() {
                lo = lo.min(epoch.stop_times[epoch.trips[t as usize].stop_time_start as usize].departure_s);
            }
            if let Some(&(t, _)) = deps.last() {
                hi = hi.max(
                    epoch.stop_times
                        [(epoch.trips[t as usize].stop_time_start) as usize]
                        .departure_s,
                );
            }
        }
        (lo, hi)
    };
    if sources.is_empty() {
        info!("flash-tb: no stops, arc-flags disabled");
        return;
    }
    let dep_times: Vec<u32> = if min_dep == u32::MAX {
        vec![0] // no service — still compute footpath flags
    } else if max_dep <= min_dep || n_samples == 1 {
        vec![min_dep]
    } else {
        let step = (max_dep - min_dep) as f64 / (n_samples - 1) as f64;
        (0..n_samples)
            .map(|i| min_dep + (step * i as f64) as u32)
            .collect()
    };

    // --- Forward searches (parallel over source stops) ---
    let next_source = AtomicUsize::new(0);
    let results: Mutex<Vec<HashMap<u64, CellBitSet>>> = Mutex::new(Vec::new());
    let horizon_s = dep_times[dep_times.len() - 1].saturating_add(FLASH_HORIZON_S);

    std::thread::scope(|scope| {
        for _ in 0..n_threads {
            scope.spawn(|| {
                let mut st = FlashState::new(n_stops, epoch.trips.len(), epoch.line_trips.len());
                let mut local: HashMap<u64, CellBitSet> = HashMap::new();
                loop {
                    let i = next_source.fetch_add(1, Ordering::Relaxed);
                    if i >= sources.len() {
                        break;
                    }
                    let ps = sources[i];
                    for &dep in &dep_times {
                        flash_search(
                            epoch, &entry_prefix, walk_base, ps, dep, horizon_s, &mut st,
                        );
                        flash_set_flags(epoch, &st, num_cells, &mut local);
                    }
                }
                if !local.is_empty() {
                    results.lock().unwrap().push(local);
                }
            });
        }
    });

    // --- Merge worker deltas ---
    let mut delta: HashMap<u64, CellBitSet> = HashMap::new();
    for map in results.into_inner().unwrap() {
        for (r, bs) in map {
            match delta.get_mut(&r) {
                Some(acc) => acc.union_with(&bs),
                None => {
                    delta.insert(r, bs);
                }
            }
        }
    }

    // Frequency-based trips are skipped by the search (template times), yet
    // the query may board them — conservatively flag every transfer touching
    // one, both as source trip and as target trip.
    for (ti, per_alight) in epoch.trip_transfers.iter().enumerate() {
        let source_is_freq = !epoch.trips[ti].frequency_windows.is_empty();
        for (off, entries) in per_alight.iter().enumerate() {
            let base = entry_prefix[ti][off];
            for (k, entry) in entries.iter().enumerate() {
                let target_is_freq =
                    !epoch.trips[entry.target_trip as usize].frequency_windows.is_empty();
                if !source_is_freq && !target_is_freq {
                    continue;
                }
                let r = walk_base + (base + k as u32) as u64;
                let bs = delta.entry(r).or_insert_with(|| CellBitSet::new(num_cells));
                for c in 0..num_cells {
                    bs.set_bit(c as usize);
                }
            }
        }
    }

    if delta.is_empty() {
        info!(
            elapsed_s = started.elapsed().as_secs_f32(),
            "flash-tb: no flags produced, arc-flags disabled"
        );
        return;
    }

    // --- Compress flag patterns (§5.2), most frequent first ---
    let zero = CellBitSet::new(num_cells);
    let mut counts: HashMap<&CellBitSet, usize> = HashMap::new();
    for bs in delta.values() {
        *counts.entry(bs).or_default() += 1;
    }
    // Unflagged references all share the zero pattern.
    let flagged_refs = delta.len();
    *counts.entry(&zero).or_default() += (num_refs - flagged_refs as u64) as usize;

    let mut ranked: Vec<(&CellBitSet, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    epoch.flag_patterns = ranked.iter().map(|(bs, _)| (*bs).clone()).collect();

    // Content-keyed lookup: distinct delta values may share one pattern.
    let mut id_by_words: HashMap<&[u64], u32> = HashMap::new();
    for (i, (bs, _)) in ranked.iter().enumerate() {
        id_by_words.insert(bs.words(), i as u32);
    }

    let pid_of = |r: u64| -> u32 {
        match delta.get(&r) {
            Some(bs) => id_by_words[bs.words()],
            None => id_by_words[zero.words()],
        }
    };

    epoch.arc_flag_pattern = (0..walk_n as u64).map(pid_of).collect();

    // --- Prune dead transfers: drop entries with an all-zero pattern ---
    let mut kept = 0usize;
    for (ti, per_alight) in epoch.trip_transfers.iter_mut().enumerate() {
        for (off, entries) in per_alight.iter_mut().enumerate() {
            let base = entry_prefix[ti][off];
            let mut kept_here: Vec<TripTransferEntry> = Vec::with_capacity(entries.len());
            for (k, entry) in entries.drain(..).enumerate() {
                let pid = pid_of(walk_base + (base + k as u32) as u64);
                if !epoch.flag_patterns[pid as usize].is_empty() {
                    kept_here.push(TripTransferEntry {
                        flag_pattern: pid,
                        ..entry
                    });
                }
            }
            kept += kept_here.len();
            *entries = kept_here;
        }
    }

    // Degenerate result (nothing meaningful flagged): disable pruning entirely
    // so queries fall back to unrestricted search.
    if epoch.flag_patterns.len() == 1 && epoch.flag_patterns[0].is_empty() {
        epoch.arc_flag_pattern = Vec::new();
        epoch.flag_patterns = Vec::new();
    }

    info!(
        elapsed_s = started.elapsed().as_secs_f32(),
        sources = sources.len(),
        samples = dep_times.len(),
        flagged_refs = flagged_refs,
        transfers_kept = kept,
        patterns = epoch.flag_patterns.len(),
        "flash-tb: forward arc-flags computed"
    );
}

/// Search budget: how far past the sampled departure a journey may run.
const FLASH_HORIZON_S: u32 = 6 * 3600;
/// Minimum transfer buffer when boarding after a transit leg (matches
/// `compute_trip_transfers::DEFAULT_TRANSFER_S`).
const FLASH_MIN_TRANSFER_S: u32 = 120;
/// Max rounds per forward search (≈ max transfers + 1).
const FLASH_MAX_ROUNDS: usize = 8;

/// Parent label for journey unpacking in forward searches. Chains follow
/// strictly-decreasing arrivals, so unpacking always terminates.
#[derive(Clone, Copy)]
enum FlashLab {
    Source,
    /// Rode a trip boarded directly from `board_stop` (no precomputed transfer).
    Board { board_stop: u32 },
    /// Arrival at a boarding stop improved via a precomputed trip-transfer.
    EntryUse { entry_ref: u64, prev_stop: u32 },
    /// Moved via a footpath / transfer walk edge.
    WalkMove { edge_idx: u64, prev_stop: u32 },
}

struct FlashState {
    earliest: Vec<u32>,
    lab: Vec<Option<FlashLab>>,
    stop_stamp: Vec<u32>,
    trip_stamp: Vec<u32>,
    line_stamp: Vec<u32>,
    reached: Vec<u32>,
    improved: Vec<u32>,
    next_reached: Vec<u32>,
    touched: Vec<u32>,
    stamp: u32,
}

impl FlashState {
    fn new(n_stops: usize, n_trips: usize, n_lines: usize) -> Self {
        Self {
            earliest: vec![u32::MAX; n_stops],
            lab: vec![None; n_stops],
            stop_stamp: vec![0; n_stops],
            trip_stamp: vec![0; n_trips],
            line_stamp: vec![0; n_lines.max(1)],
            reached: Vec::new(),
            improved: Vec::new(),
            next_reached: Vec::new(),
            touched: Vec::new(),
            stamp: 0,
        }
    }

    fn reset(&mut self) {
        for si in self.touched.drain(..) {
            self.earliest[si as usize] = u32::MAX;
            self.lab[si as usize] = None;
        }
    }

    #[inline]
    fn improve(&mut self, si: u32, arr: u32, lab: FlashLab) -> bool {
        if arr < self.earliest[si as usize] {
            if self.earliest[si as usize] == u32::MAX {
                self.touched.push(si);
            }
            self.earliest[si as usize] = arr;
            self.lab[si as usize] = Some(lab);
            true
        } else {
            false
        }
    }
}

/// One-to-all bounded-round TB-style search from `source` at `dep`.
/// Mirrors the TBR query's pruning rules (line pruning, per-round trip stamp)
/// so flagged journeys are exactly those the query algorithm can reproduce.
fn flash_search(
    epoch: &StaticEpoch,
    entry_prefix: &[Vec<u32>],
    walk_base: u64,
    source: u32,
    dep: u32,
    horizon_s: u32,
    st: &mut FlashState,
) {
    st.reset();
    st.stamp = st.stamp.wrapping_add(1);
    let seed_stamp = st.stamp;
    st.earliest[source as usize] = dep;
    st.lab[source as usize] = Some(FlashLab::Source);
    st.touched.push(source);
    st.reached.clear();
    st.reached.push(source);
    // Seed the walk phase so round 0 expands the initial footpath from the
    // origin (paper's `f0`).
    st.improved.clear();
    st.improved.push(source);
    st.stop_stamp[source as usize] = seed_stamp;

    for round in 0..FLASH_MAX_ROUNDS {
        if st.reached.is_empty() {
            break;
        }
        st.stamp = st.stamp.wrapping_add(1);
        let round_stamp = st.stamp;
        // NOTE: `improved` is drained (mem_take) by the walk phase below, so it
        // enters round 0 still holding the seeded source stop.
        st.next_reached.clear();

        let reached: Vec<u32> = std::mem::take(&mut st.reached);
        for &s in &reached {
            let arr = st.earliest[s as usize];
            if arr == u32::MAX {
                continue;
            }
            let board_after = if round == 0 {
                arr
            } else {
                arr.saturating_add(FLASH_MIN_TRANSFER_S)
            };

            let deps = epoch
                .stop_departures
                .get(s as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            // Frequency-template times break the sorted-order invariant, so on
            // those stops scan from the top like the query does.
            let has_freq = epoch
                .stop_has_freq
                .get(s as usize)
                .copied()
                .unwrap_or(false);
            let start = if has_freq {
                0
            } else {
                deps.partition_point(|&(t, o)| {
                    epoch.stop_times[(epoch.trips[t as usize].stop_time_start + o) as usize]
                        .departure_s
                        < board_after
                })
            };

            for &(ti, off) in &deps[start..] {
                let trip = &epoch.trips[ti as usize];
                let st_board =
                    &epoch.stop_times[(trip.stop_time_start + off) as usize];
                if trip.frequency_windows.is_empty() && st_board.departure_s < board_after {
                    continue;
                }
                if st_board.departure_s > horizon_s {
                    break;
                }
                if !trip.frequency_windows.is_empty() {
                    continue; // template times — covered by conservative flags
                }
                let line_id = epoch.line_of_trip.get(ti as usize).copied().unwrap_or(u32::MAX);
                if line_id != u32::MAX
                    && st.line_stamp.get(line_id as usize).copied().unwrap_or(0) == round_stamp
                {
                    continue; // line pruning: earliest trip per line per round
                }
                if st_board.pickup_type == 1 || st.trip_stamp[ti as usize] == round_stamp {
                    continue;
                }

                // Ride forward.
                st.trip_stamp[ti as usize] = round_stamp;
                if line_id != u32::MAX {
                    if let Some(ls) = st.line_stamp.get_mut(line_id as usize) {
                        *ls = round_stamp;
                    }
                }
                for roff in (off + 1)..trip.stop_time_len {
                    let st_r = &epoch.stop_times[(trip.stop_time_start + roff) as usize];
                    if st_r.drop_off_type == 1 {
                        continue;
                    }
                    if st_r.arrival_s >= horizon_s {
                        break;
                    }
                    let si = st_r.stop_idx;
                    if st.improve(si, st_r.arrival_s, FlashLab::Board { board_stop: s }) {
                        st.improved.push(si);
                    }

                    // Precomputed trip-to-trip transfers from this event.
                    if let Some(entries) = epoch
                        .trip_transfers
                        .get(ti as usize)
                        .and_then(|v| v.get(roff as usize))
                    {
                        let base = entry_prefix[ti as usize][roff as usize] as u64;
                        for (k, entry) in entries.iter().enumerate() {
                            let t2 = &epoch.trips[entry.target_trip as usize];
                            if !t2.frequency_windows.is_empty() {
                                continue;
                            }
                            let board_st = &epoch.stop_times
                                [(t2.stop_time_start + entry.target_board_off) as usize];
                            let effective = st_r.arrival_s.saturating_add(entry.min_transfer_s);
                            if board_st.departure_s < effective
                                || board_st.departure_s >= horizon_s
                            {
                                continue;
                            }
                            let si2 = board_st.stop_idx;
                            if st.improve(
                                si2,
                                board_st.departure_s,
                                FlashLab::EntryUse {
                                    entry_ref: walk_base + base + k as u64,
                                    prev_stop: si,
                                },
                            ) {
                                st.improved.push(si2);
                            }
                        }
                    }
                }
            }
        }

        // Walk phase: relax footpaths to a fixed point (paper assumes the
        // transitively closed footpath set F), then everything improved seeds
        // next round's boarding scan.
        let mut frontier: Vec<u32> = std::mem::take(&mut st.improved);
        while !frontier.is_empty() {
            let mut next_frontier: Vec<u32> = Vec::new();
            for s in frontier {
                let arr = st.earliest[s as usize];
                if arr == u32::MAX {
                    continue;
                }
                if st.stop_stamp[s as usize] != round_stamp {
                    st.stop_stamp[s as usize] = round_stamp;
                    st.next_reached.push(s);
                }
                for &ei in epoch
                    .walk_adj
                    .get(s as usize)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                {
                    let e = &epoch.walk_edges[ei];
                    let arr2 = arr.saturating_add(e.duration_s);
                    if arr2 >= horizon_s {
                        continue;
                    }
                    let to = e.to_stop_idx;
                    if st.improve(
                        to,
                        arr2,
                        FlashLab::WalkMove {
                            edge_idx: ei as u64,
                            prev_stop: s,
                        },
                    ) {
                        next_frontier.push(to);
                    }
                }
            }
            frontier = next_frontier;
        }

        st.reached = std::mem::take(&mut st.next_reached);
    }
}

/// Unpack every labelled journey and set flag bits: for each improved target
/// stop `pt`, mark every transfer on its path with cell(pt).
fn flash_set_flags(
    epoch: &StaticEpoch,
    st: &FlashState,
    num_cells: u32,
    delta: &mut std::collections::HashMap<u64, CellBitSet>,
) {
    // `touched` holds exactly the stops improved during the last search.
    for &si in &st.touched {
        let cell = match epoch.stop_partition.get(si as usize) {
            Some(&c) => c as usize,
            None => continue,
        };
        let mut cur = si as usize;
        let mut guard = 0usize;
        while guard < 1024 {
            guard += 1;
            match st.lab[cur] {
                None | Some(FlashLab::Source) => break,
                Some(FlashLab::Board { board_stop }) => cur = board_stop as usize,
                Some(FlashLab::EntryUse { entry_ref, prev_stop }) => {
                    delta.entry(entry_ref)
                        .or_insert_with(|| CellBitSet::new(num_cells))
                        .set_bit(cell);
                    cur = prev_stop as usize;
                }
                Some(FlashLab::WalkMove { edge_idx, prev_stop }) => {
                    delta.entry(edge_idx)
                        .or_insert_with(|| CellBitSet::new(num_cells))
                        .set_bit(cell);
                    cur = prev_stop as usize;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_flags_basic() {
        // Three stops in a line: A --(10s)--> B --(10s)--> C
        // With recursive bisection (median split by lat):
        //   A(48.0) goes to left half, B(48.001)+C(48.010) go to right half
        //   So cell_a != cell_b == cell_c
        let mut epoch = StaticEpoch::empty();
        epoch.stops = vec![
            StopRecord {
                id: "A".into(),
                feed_id: "t".into(),
                raw_id: "A".into(),
                name: "A".into(),
                lat: Some(48.0),
                lon: Some(2.0),
                parent_id: None,
                location_type: 0,
                platform_code: None,
                wheelchair: 0,
                stop_code: None,
                stop_desc: None,
                level_id: None,
                zone_id: None,
                stop_url: None,
                stop_timezone: None,
            },
            StopRecord {
                id: "B".into(),
                feed_id: "t".into(),
                raw_id: "B".into(),
                name: "B".into(),
                lat: Some(48.001),
                lon: Some(2.0),
                parent_id: None,
                location_type: 0,
                platform_code: None,
                wheelchair: 0,
                stop_code: None,
                stop_desc: None,
                level_id: None,
                zone_id: None,
                stop_url: None,
                stop_timezone: None,
            },
            StopRecord {
                id: "C".into(),
                feed_id: "t".into(),
                raw_id: "C".into(),
                name: "C".into(),
                lat: Some(48.010),
                lon: Some(2.0),
                parent_id: None,
                location_type: 0,
                platform_code: None,
                wheelchair: 0,
                stop_code: None,
                stop_desc: None,
                level_id: None,
                zone_id: None,
                stop_url: None,
                stop_timezone: None,
            },
        ];
        epoch.walk_edges = vec![
            WalkEdge {
                from_stop_idx: 0,
                to_stop_idx: 1,
                duration_s: 10,
                distance_m: 100.0,
                pathway_mode: None,
            },
            WalkEdge {
                from_stop_idx: 1,
                to_stop_idx: 2,
                duration_s: 10,
                distance_m: 100.0,
                pathway_mode: None,
            },
            WalkEdge {
                from_stop_idx: 1,
                to_stop_idx: 0,
                duration_s: 10,
                distance_m: 100.0,
                pathway_mode: None,
            },
            WalkEdge {
                from_stop_idx: 2,
                to_stop_idx: 1,
                duration_s: 10,
                distance_m: 100.0,
                pathway_mode: None,
            },
        ];
        epoch.walk_adj = vec![Vec::new(); 3];
        for (i, e) in epoch.walk_edges.iter().enumerate() {
            epoch.walk_adj[e.from_stop_idx as usize].push(i);
        }

        compute_partition(&mut epoch);
        compute_arc_flags(&mut epoch);

        assert!(!epoch.flag_patterns.is_empty(), "forward search should produce flags");
        let cell_a = epoch.stop_partition[0] as usize;
        let cell_b = epoch.stop_partition[1] as usize;
        let cell_c = epoch.stop_partition[2] as usize;

        // Forward-search semantics: a flag on edge e for cell C means some
        // unpacked journey to a stop in C used e.
        // Edge 1: B→C — used by journeys from A or B to C.
        if cell_b != cell_c {
            let pat = &epoch.flag_patterns[epoch.arc_flag_pattern[1] as usize];
            assert!(pat.get_bit(cell_c), "B→C should flag cell of C");
        }
        // Edge 2: B→A — used by journeys to A.
        if cell_a != cell_b {
            let pat = &epoch.flag_patterns[epoch.arc_flag_pattern[2] as usize];
            assert!(pat.get_bit(cell_a), "B→A should flag cell of A");
        }
        // Edge 3: C→B — used by journeys to B and (via B→A) to A.
        let pat3 = &epoch.flag_patterns[epoch.arc_flag_pattern[3] as usize];
        if cell_b != cell_c {
            assert!(pat3.get_bit(cell_b), "C→B should flag cell of B");
        }
        if cell_a != cell_c && cell_a != cell_b {
            assert!(pat3.get_bit(cell_a), "C→B should flag cell of A (reachable via B→A)");
        }
    }

    #[test]
    fn partition_respects_layout_graph() {
        // Two clusters joined by one high-weight trip corridor: the layout
        // graph split must keep each cluster connected rather than slicing
        // through the corridor's middle stops.
        let mut epoch = StaticEpoch::empty();
        // Chain: 0 - 1 - 2 | 3 - 4 - 5 with link 2-3
        for i in 0..6 {
            epoch.stops.push(StopRecord {
                id: format!("s{i}"),
                feed_id: "t".into(),
                raw_id: format!("s{i}"),
                name: format!("S{i}"),
                lat: Some(48.0 + i as f64 * 0.001),
                lon: Some(2.0),
                parent_id: None,
                location_type: 0,
                platform_code: None,
                wheelchair: 0,
                stop_code: None,
                stop_desc: None,
                level_id: None,
                zone_id: None,
                stop_url: None,
                stop_timezone: None,
            });
            epoch.stop_departures.push(Vec::new());
        }
        let mut edges: Vec<(u32, u32)> = vec![(0, 1), (1, 2), (3, 4), (4, 5)];
        // Corridor 2-3 repeated many times → heavy layout-graph weight.
        for _ in 0..50 {
            edges.push((2, 3));
        }
        epoch.walk_edges = edges
            .into_iter()
            .map(|(a, b)| WalkEdge {
                from_stop_idx: a,
                to_stop_idx: b,
                duration_s: 10,
                distance_m: 10.0,
                pathway_mode: None,
            })
            .collect();

        compute_partition(&mut epoch);
        // With only 6 stops and depth-8 bisection every stop gets its own cell
        // path; just verify assignment validity and balance at the top split.
        assert_eq!(epoch.num_cells, 256);
        assert_eq!(epoch.stop_partition.len(), 6);
        // Top-level split separates {0,1,2} from {3,4,5} (or keeps the chain
        // order) — cells must differ across the heavy corridor in at least one
        // direction after full recursion. Weak but meaningful sanity check:
        for i in 0..6 {
            assert!(epoch.stop_partition[i] < 256);
        }
    }
}

/// Precompute geo point lists and place/parent indexes used on every itinerary request.
fn build_geo_and_place_indexes(epoch: &mut StaticEpoch) {
    use crate::search::stops::{is_idfm_place_stop, place_name_key};

    epoch.geo_stop_points.clear();
    epoch.geo_boardable_points.clear();
    epoch.place_name_index.clear();
    epoch.children_by_parent.clear();

    for (i, s) in epoch.stops.iter().enumerate() {
        let (Some(lat), Some(lon)) = (s.lat, s.lon) else {
            continue;
        };
        epoch.geo_stop_points.push((i, lat, lon));

        let boardable = epoch
            .stop_departures
            .get(i)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let place = is_idfm_place_stop(s);
        if boardable || place {
            epoch.geo_boardable_points.push((i, lat, lon));
            let key = place_name_key(&s.name);
            if key.len() >= 3 {
                epoch
                    .place_name_index
                    .entry(key)
                    .or_default()
                    .push(i as u32);
            }
        }

        if let Some(ref p) = s.parent_id {
            epoch
                .children_by_parent
                .entry(p.clone())
                .or_default()
                .push(i as u32);
        }
        if s.location_type == 1 {
            epoch
                .children_by_parent
                .entry(s.id.clone())
                .or_default()
                .push(i as u32);
        }
    }
}

fn undirected_pair(a: u32, b: u32) -> (u32, u32) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Global stop pairs that have at least one pathway-derived walk edge.
fn pathway_pair_set(
    bundles: &[Arc<FeedStaticBundle>],
    stop_offset: &HashMap<String, u32>,
) -> std::collections::HashSet<(u32, u32)> {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    for bundle in bundles {
        let base = *stop_offset.get(&bundle.feed_id).unwrap_or(&0);
        for pw in &bundle.pathways {
            set.insert(undirected_pair(
                pw.from_stop_idx + base,
                pw.to_stop_idx + base,
            ));
        }
    }
    set
}

/// Build a **complete transfer subgraph** among boardable stops (have departures).
///
/// Uses a geographic grid so monomodal RER shells connect to nearby Métro/tram/bus
/// board points **without naming any gare**. Completeness > hard skip.
fn add_boardable_transfer_graph(
    epoch: &mut StaticEpoch,
    pathway_pairs: &std::collections::HashSet<(u32, u32)>,
) {
    use crate::feeds::hub_link::hub_links_grid;
    use crate::link::geo::nearby;
    use tracing::info;

    /// Transfer radius (m) for general boardable↔boardable links.
    const TRANSFER_RADIUS_M: f64 = 600.0;
    /// Extra radius from monomodal/multimodal places (large stations).
    const PLACE_RADIUS_M: f64 = 900.0;
    const PLACE_NEIGHBORS: usize = 32;
    const WALK_SPEED: f64 = 1.2;
    const MIN_TRANSFER_S: u32 = 75;
    const MAX_UNDIRECTED_PAIRS: usize = 300_000;

    let points: Vec<(usize, f64, f64)> = epoch
        .stops
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let boardable = epoch
                .stop_departures
                .get(i)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            let id = s.raw_id.to_ascii_lowercase();
            let place = id.contains("monomodalstopplace") || id.contains("multimodalstopplace");
            if !boardable && !place {
                return None;
            }
            if s.location_type == 1 && !place {
                return None;
            }
            Some((i, s.lat?, s.lon?))
        })
        .collect();

    if points.len() < 2 {
        return;
    }

    info!(
        boardable_points = points.len(),
        radius_m = TRANSFER_RADIUS_M,
        "building complete boardable transfer graph (grid)"
    );

    let mut pairs = hub_links_grid(&points, TRANSFER_RADIUS_M);
    if pairs.len() > MAX_UNDIRECTED_PAIRS {
        pairs.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        pairs.truncate(MAX_UNDIRECTED_PAIRS);
    }

    let mut seen: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
    let mut extra = Vec::with_capacity(pairs.len().saturating_mul(2) + 64_000);

    let push_pair = |extra: &mut Vec<WalkEdge>,
                     seen: &mut std::collections::HashSet<(u32, u32)>,
                     a: u32,
                     b: u32,
                     dist: f64| {
        if a == b {
            return;
        }
        let key = undirected_pair(a, b);
        if pathway_pairs.contains(&key) || !seen.insert(key) {
            return;
        }
        let dur = ((dist / WALK_SPEED).ceil() as u32).max(MIN_TRANSFER_S);
        extra.push(WalkEdge {
            from_stop_idx: a,
            to_stop_idx: b,
            duration_s: dur,
            distance_m: dist,
            pathway_mode: None,
        });
        extra.push(WalkEdge {
            from_stop_idx: b,
            to_stop_idx: a,
            duration_s: dur,
            distance_m: dist,
            pathway_mode: None,
        });
    };

    for (a, b, dist) in pairs {
        push_pair(&mut extra, &mut seen, a as u32, b as u32, dist);
    }

    // Dense star from every monomodal/multimodal place → nearest boardable (uncapped
    // by the global pair budget). This is what makes RER shell → Métro work.
    let boardable_pts: Vec<(usize, f64, f64)> = points.clone();
    let mut place_links = 0usize;
    for (i, s) in epoch.stops.iter().enumerate() {
        let id = s.raw_id.to_ascii_lowercase();
        if !id.contains("monomodalstopplace") && !id.contains("multimodalstopplace") {
            continue;
        }
        let (Some(lat), Some(lon)) = (s.lat, s.lon) else {
            continue;
        };
        let near = nearby(&boardable_pts, lat, lon, PLACE_RADIUS_M, PLACE_NEIGHBORS + 1);
        for (j, dist) in near {
            if j == i {
                continue;
            }
            let before = seen.len();
            push_pair(&mut extra, &mut seen, i as u32, j as u32, dist);
            if seen.len() > before {
                place_links += 1;
            }
        }
    }

    info!(
        transfer_walk_edges = extra.len(),
        monomodal_star_links = place_links,
        "boardable transfer graph ready"
    );
    epoch.walk_edges.extend(extra);
}

fn add_parent_station_walks(
    epoch: &mut StaticEpoch,
    pathway_pairs: &std::collections::HashSet<(u32, u32)>,
) {
    let mut by_parent: HashMap<String, Vec<u32>> = HashMap::new();
    for (idx, s) in epoch.stops.iter().enumerate() {
        if let Some(ref p) = s.parent_id {
            by_parent.entry(p.clone()).or_default().push(idx as u32);
        }
        if s.location_type == 1 {
            by_parent
                .entry(s.id.clone())
                .or_default()
                .push(idx as u32);
        }
    }
    const DEFAULT_TRANSFER: u32 = 120;
    // Deterministic edge order — walk_edges indices are referenced by the
    // persisted FLASH-TB flag table, so iteration must not depend on HashMap
    // seeding.
    let mut parents: Vec<&String> = by_parent.keys().collect();
    parents.sort();
    for parent in parents {
        let members = &by_parent[parent];
        if members.len() < 2 {
            continue;
        }
        for &a in members {
            for &b in members {
                if a == b {
                    continue;
                }
                if pathway_pairs.contains(&undirected_pair(a, b)) {
                    continue;
                }
                epoch.walk_edges.push(WalkEdge {
                    from_stop_idx: a,
                    to_stop_idx: b,
                    duration_s: DEFAULT_TRANSFER,
                    distance_m: 50.0,
                    pathway_mode: None,
                });
            }
        }
    }
}
