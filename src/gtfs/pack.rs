use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;

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
    pub walk_edges: Vec<WalkEdge>,
    /// Adjacency for walk: stop_idx -> edges
    pub walk_adj: Vec<Vec<usize>>,
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
            walk_edges: Vec::new(),
            walk_adj: Vec::new(),
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

    build_geo_and_place_indexes(&mut epoch);

    epoch
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
    for (_parent, members) in by_parent {
        if members.len() < 2 {
            continue;
        }
        for &a in &members {
            for &b in &members {
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
