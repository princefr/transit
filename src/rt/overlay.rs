//! Realtime overlay state and query/apply helpers for itinerary enrichment.
//!
//! # Trip key strategy
//!
//! Decode inserts each trip under the namespaced key `feed:trip_id`. When the
//! GTFS-RT trip descriptor includes `start_date` (`YYYYMMDD`), the same
//! [`TripRt`] is also inserted under `feed:trip_id@YYYYMMDD`. Lookups try the
//! dated key first (if the caller passes one), then the undated key.
//!
//! # Delay resolution
//!
//! For a given stop, prefer stop-time update delays (arrival/departure), then
//! fall back to the trip-level delay. Absolute POSIX times on the stop update
//! override delay-based adjustment when present.

use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashMap;
use std::sync::Arc;

/// Result of applying realtime to a scheduled stop event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopRealtime {
    /// Adjusted arrival/departure time (same role as the scheduled input).
    pub realtime: DateTime<Utc>,
    /// Delay in seconds (positive = late). Zero when on time or absolute time used without delay field.
    pub delay_secs: i32,
    /// Stop is skipped in the trip update.
    pub skipped: bool,
    /// Whole trip is canceled.
    pub canceled: bool,
    /// Whether any RT data was found for this trip.
    pub has_rt: bool,
}

/// Diagnostic counters for a single feed or the whole overlay.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RtStats {
    pub feed_id: String,
    pub trip_update_count: usize,
    pub vehicle_count: usize,
    pub alert_count: usize,
    pub canceled_trip_count: usize,
    pub skipped_stop_count: usize,
    /// Trips that have at least one stop_time update.
    pub trips_with_stop_updates: usize,
    pub trip_updates_age_secs: Option<i64>,
    pub vehicles_age_secs: Option<i64>,
    pub alerts_age_secs: Option<i64>,
    pub header_timestamp: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct StopTimeRt {
    pub stop_sequence: u16,
    pub stop_id: Option<String>,
    pub arrival_delay: Option<i32>,
    pub departure_delay: Option<i32>,
    pub arrival_time: Option<i64>,
    pub departure_time: Option<i64>,
    pub skipped: bool,
}

#[derive(Debug, Clone, Default)]
pub struct TripRt {
    pub delay: Option<i32>,
    pub canceled: bool,
    pub start_date: Option<String>,
    /// Namespaced route id from `TripDescriptor.route_id` (`feed:route_id`).
    pub route_id: Option<String>,
    /// `TripDescriptor.direction_id` when present.
    pub direction_id: Option<u32>,
    /// `TripDescriptor.start_time` (HH:MM:SS), when present.
    pub start_time: Option<String>,
    pub stop_updates: HashMap<u16, StopTimeRt>,
}

impl TripRt {
    /// Find a stop update by sequence, or by namespaced / raw stop_id.
    pub fn stop_update_by_seq(&self, stop_sequence: u16) -> Option<&StopTimeRt> {
        self.stop_updates.get(&stop_sequence)
    }

    pub fn stop_update_by_stop_id(&self, stop_id: &str) -> Option<&StopTimeRt> {
        self.stop_updates.values().find(|s| {
            s.stop_id
                .as_deref()
                .is_some_and(|id| id == stop_id || id.ends_with(&format!(":{stop_id}")) || {
                    // raw id match if stored namespaced
                    id.rsplit_once(':').map(|(_, r)| r) == Some(stop_id)
                })
        })
    }

    /// Best-effort stop update: prefer sequence, then stop_id.
    pub fn find_stop_update(
        &self,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
    ) -> Option<&StopTimeRt> {
        if let Some(seq) = stop_sequence {
            if let Some(u) = self.stop_update_by_seq(seq) {
                return Some(u);
            }
        }
        if let Some(sid) = stop_id {
            return self.stop_update_by_stop_id(sid);
        }
        None
    }

    /// Delay seconds for arrival at a stop: stop arrival_delay → departure_delay → trip delay.
    pub fn arrival_delay_secs(
        &self,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
    ) -> Option<i32> {
        if let Some(u) = self.find_stop_update(stop_sequence, stop_id) {
            if let Some(d) = u.arrival_delay.or(u.departure_delay) {
                return Some(d);
            }
        }
        self.delay
    }

    /// Delay seconds for departure at a stop: stop departure_delay → arrival_delay → trip delay.
    pub fn departure_delay_secs(
        &self,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
    ) -> Option<i32> {
        if let Some(u) = self.find_stop_update(stop_sequence, stop_id) {
            if let Some(d) = u.departure_delay.or(u.arrival_delay) {
                return Some(d);
            }
        }
        self.delay
    }

    /// Whether this stop is marked SKIPPED.
    pub fn is_stop_skipped(&self, stop_sequence: Option<u16>, stop_id: Option<&str>) -> bool {
        self.find_stop_update(stop_sequence, stop_id)
            .map(|u| u.skipped)
            .unwrap_or(false)
    }

    /// Apply RT to a scheduled arrival time.
    pub fn apply_arrival(
        &self,
        scheduled: DateTime<Utc>,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
    ) -> StopRealtime {
        self.apply_time(scheduled, stop_sequence, stop_id, true)
    }

    /// Apply RT to a scheduled departure time.
    pub fn apply_departure(
        &self,
        scheduled: DateTime<Utc>,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
    ) -> StopRealtime {
        self.apply_time(scheduled, stop_sequence, stop_id, false)
    }

    fn apply_time(
        &self,
        scheduled: DateTime<Utc>,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
        arrival: bool,
    ) -> StopRealtime {
        let skipped = self.is_stop_skipped(stop_sequence, stop_id);
        let canceled = self.canceled;

        if let Some(u) = self.find_stop_update(stop_sequence, stop_id) {
            let abs = if arrival {
                u.arrival_time.or(u.departure_time)
            } else {
                u.departure_time.or(u.arrival_time)
            };
            if let Some(ts) = abs {
                if let Some(rt) = Utc.timestamp_opt(ts, 0).single() {
                    let delay_secs = (rt - scheduled).num_seconds() as i32;
                    return StopRealtime {
                        realtime: rt,
                        delay_secs,
                        skipped,
                        canceled,
                        has_rt: true,
                    };
                }
            }
        }

        let delay = if arrival {
            self.arrival_delay_secs(stop_sequence, stop_id)
        } else {
            self.departure_delay_secs(stop_sequence, stop_id)
        };
        let delay_secs = delay.unwrap_or(0);
        StopRealtime {
            realtime: scheduled + chrono::Duration::seconds(delay_secs as i64),
            delay_secs,
            skipped,
            canceled,
            has_rt: delay.is_some() || skipped || canceled || self.find_stop_update(stop_sequence, stop_id).is_some(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VehiclePos {
    pub trip_id: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub bearing: Option<f64>,
    pub speed: Option<f64>,
    pub updated_at: DateTime<Utc>,
    pub current_stop_id: Option<String>,
    /// User-visible vehicle label (`VehicleDescriptor.label`).
    pub label: Option<String>,
    /// Internal vehicle id (`VehicleDescriptor.id`).
    pub vehicle_id: Option<String>,
    pub license_plate: Option<String>,
    /// GTFS-RT `OccupancyStatus` name, e.g. `EMPTY`, `MANY_SEATS_AVAILABLE`.
    pub occupancy: Option<String>,
    /// GTFS-RT `VehicleStopStatus` name: `INCOMING_AT`, `STOPPED_AT`, `IN_TRANSIT_TO`.
    pub current_status: Option<String>,
    pub current_stop_sequence: Option<u32>,
    /// GTFS-RT `CongestionLevel` name when present.
    pub congestion: Option<String>,
    /// Passenger occupancy 0–100 from `occupancy_percentage` or multi-carriage avg.
    pub occupancy_percentage: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct AlertRt {
    pub id: String,
    pub header: Option<String>,
    pub description: Option<String>,
    pub severity: String,
    pub informed_stop_ids: Vec<String>,
    pub informed_route_ids: Vec<String>,
    pub informed_trip_ids: Vec<String>,
    /// GTFS-RT `Alert.Cause` name, e.g. `CONSTRUCTION`.
    pub cause: Option<String>,
    /// GTFS-RT `Alert.Effect` name, e.g. `DETOUR`.
    pub effect: Option<String>,
    /// First translation of `Alert.url`.
    pub url: Option<String>,
    /// Start of first `active_period` (POSIX seconds), if any.
    pub active_start: Option<i64>,
    /// End of first `active_period` (POSIX seconds), if any.
    pub active_end: Option<i64>,
    /// All `active_period` ranges as `(start, end)` POSIX seconds.
    pub active_periods: Vec<(Option<i64>, Option<i64>)>,
    /// All `header_text` translations as `(language, text)`. Language may be empty.
    pub header_translations: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct AlertIndex {
    pub by_trip: HashMap<String, Vec<u16>>,
    pub by_route: HashMap<String, Vec<u16>>,
    pub by_stop: HashMap<String, Vec<u16>>,
}

#[derive(Debug, Clone, Default)]
pub struct FeedRtState {
    pub feed_id: String,
    /// Keys: `feed:trip_id` and optionally `feed:trip_id@YYYYMMDD`.
    pub trips: HashMap<String, TripRt>,
    pub vehicles: HashMap<String, VehiclePos>,
    pub alerts: Vec<AlertRt>,
    /// Inverted index rebuilt when alerts change (fast leg enrichment).
    pub alert_index: AlertIndex,
    pub trip_updates_fetched_at: Option<DateTime<Utc>>,
    pub vehicles_fetched_at: Option<DateTime<Utc>>,
    pub alerts_fetched_at: Option<DateTime<Utc>>,
    pub header_timestamp: Option<i64>,
    pub trip_update_count: usize,
    pub vehicle_count: usize,
    pub alert_count: usize,
}

impl FeedRtState {
    pub fn new(feed_id: impl Into<String>) -> Self {
        Self {
            feed_id: feed_id.into(),
            ..Default::default()
        }
    }

    pub fn trip_updates_age_secs(&self) -> Option<i64> {
        self.trip_updates_fetched_at
            .map(|t| (Utc::now() - t).num_seconds())
    }

    pub fn vehicles_age_secs(&self) -> Option<i64> {
        self.vehicles_fetched_at
            .map(|t| (Utc::now() - t).num_seconds())
    }

    pub fn alerts_age_secs(&self) -> Option<i64> {
        self.alerts_fetched_at
            .map(|t| (Utc::now() - t).num_seconds())
    }

    /// Insert trip under `feed:raw_trip_id` and, if start_date is set, also
    /// `feed:raw_trip_id@YYYYMMDD`.
    ///
    /// When `raw_trip_id` is empty (incomplete descriptor), inserts under a
    /// synthetic key derived from route_id / direction_id / start_date so the
    /// entity is still queryable via [`Self::find_trip_by_descriptor`].
    pub fn insert_trip(&mut self, feed_id: &str, raw_trip_id: &str, trip: TripRt) {
        if raw_trip_id.is_empty() {
            let key = incomplete_trip_key(feed_id, &trip);
            self.trips.insert(key, trip);
            return;
        }
        let base = format!("{feed_id}:{raw_trip_id}");
        if let Some(ref d) = trip.start_date {
            if !d.is_empty() {
                let dated = format!("{base}@{d}");
                self.trips.insert(dated, trip.clone());
            }
        }
        self.trips.insert(base, trip);
    }

    /// Look up by full key, or undated form if dated key misses.
    pub fn get_trip(&self, trip_key: &str) -> Option<&TripRt> {
        if let Some(t) = self.trips.get(trip_key) {
            return Some(t);
        }
        // If caller passed feed:trip@date, try without date.
        if let Some((base, _)) = trip_key.rsplit_once('@') {
            return self.trips.get(base);
        }
        None
    }

    /// Match incomplete RT trip updates that only published route / direction / date.
    ///
    /// `route_id` should be namespaced (`feed:route`). `direction_id` and
    /// `start_date` are matched when both the query and the stored trip have them.
    pub fn find_trip_by_descriptor(
        &self,
        route_id: &str,
        direction_id: Option<u32>,
        start_date: Option<&str>,
    ) -> Option<&TripRt> {
        self.trips.values().find(|t| {
            let rid_ok = t.route_id.as_ref().is_some_and(|r| {
                r == route_id
                    || r.ends_with(&format!(":{route_id}"))
                    || route_id.ends_with(&format!(":{r}"))
                    || r.rsplit_once(':').map(|(_, raw)| raw) == Some(route_id)
            });
            if !rid_ok {
                return false;
            }
            if let (Some(want), Some(got)) = (direction_id, t.direction_id) {
                if want != got {
                    return false;
                }
            } else if direction_id.is_some() && t.direction_id.is_none() {
                return false;
            }
            if let (Some(want), Some(got)) = (start_date, t.start_date.as_deref()) {
                if want != got {
                    return false;
                }
            } else if start_date.is_some() && t.start_date.is_none() {
                return false;
            }
            true
        })
    }

    /// Unique canceled trips (by undated key where possible).
    pub fn trips_canceled(&self) -> impl Iterator<Item = (&str, &TripRt)> {
        self.trips
            .iter()
            .filter(|(k, t)| t.canceled && !k.contains('@'))
            .map(|(k, t)| (k.as_str(), t))
    }

    pub fn alerts_for_stop<'a>(&'a self, stop_id: &str) -> Vec<&'a AlertRt> {
        self.alerts
            .iter()
            .filter(|a| {
                a.informed_stop_ids.iter().any(|s| {
                    s == stop_id
                        || s.ends_with(&format!(":{stop_id}"))
                        || stop_id.ends_with(&format!(":{s}"))
                        || s.rsplit_once(':').map(|(_, r)| r) == Some(stop_id)
                })
            })
            .collect()
    }

    pub fn alerts_for_route<'a>(&'a self, route_id: &str) -> Vec<&'a AlertRt> {
        self.alerts
            .iter()
            .filter(|a| {
                a.informed_route_ids.iter().any(|r| {
                    r == route_id
                        || r.ends_with(&format!(":{route_id}"))
                        || route_id.ends_with(&format!(":{r}"))
                        || r.rsplit_once(':').map(|(_, raw)| raw) == Some(route_id)
                })
            })
            .collect()
    }

    /// Rebuild inverted alert index after alerts are inserted/replaced.
    pub fn rebuild_alert_index(&mut self) {
        let mut idx = AlertIndex::default();
        for (i, a) in self.alerts.iter().enumerate() {
            let i = i as u16;
            for t in &a.informed_trip_ids {
                let undated = t.rsplit_once('@').map(|(b, _)| b).unwrap_or(t.as_str());
                idx.by_trip.entry(undated.to_string()).or_default().push(i);
                if undated != t.as_str() {
                    idx.by_trip.entry(t.clone()).or_default().push(i);
                }
            }
            for r in &a.informed_route_ids {
                idx.by_route.entry(r.clone()).or_default().push(i);
                if let Some((_, tail)) = r.rsplit_once(':') {
                    if !tail.is_empty() {
                        idx.by_route.entry(tail.to_string()).or_default().push(i);
                    }
                }
            }
            for s in &a.informed_stop_ids {
                idx.by_stop.entry(s.clone()).or_default().push(i);
                if let Some((_, tail)) = s.rsplit_once(':') {
                    if !tail.is_empty() {
                        idx.by_stop.entry(tail.to_string()).or_default().push(i);
                    }
                }
            }
        }
        self.alert_index = idx;
    }

    /// Candidate alerts for a transit leg (union of trip / route / stop keys).
    pub fn alert_candidates_for_leg(
        &self,
        trip_id: &str,
        route_id: &str,
        from_stop_id: &str,
        to_stop_id: &str,
    ) -> Vec<&AlertRt> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut push_idx = |i: u16| {
            if seen.insert(i) {
                if let Some(a) = self.alerts.get(i as usize) {
                    out.push(a);
                }
            }
        };
        let undated = trip_id.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_id);
        if let Some(idxs) = self.alert_index.by_trip.get(undated) {
            for &i in idxs {
                push_idx(i);
            }
        }
        if let Some(idxs) = self.alert_index.by_trip.get(trip_id) {
            for &i in idxs {
                push_idx(i);
            }
        }
        for key in [route_id, route_id.rsplit_once(':').map(|(_, t)| t).unwrap_or(route_id)]
        {
            if let Some(idxs) = self.alert_index.by_route.get(key) {
                for &i in idxs {
                    push_idx(i);
                }
            }
        }
        for sid in [from_stop_id, to_stop_id] {
            if let Some(idxs) = self.alert_index.by_stop.get(sid) {
                for &i in idxs {
                    push_idx(i);
                }
            }
            if let Some(tail) = sid.rsplit_once(':').map(|(_, t)| t) {
                if let Some(idxs) = self.alert_index.by_stop.get(tail) {
                    for &i in idxs {
                        push_idx(i);
                    }
                }
            }
        }
        out
    }

    pub fn stats(&self) -> RtStats {
        let mut canceled_trip_count = 0usize;
        let mut skipped_stop_count = 0usize;
        let mut trips_with_stop_updates = 0usize;
        for (k, t) in &self.trips {
            // Count undated keys only to avoid double-counting dual inserts.
            if k.contains('@') {
                continue;
            }
            if t.canceled {
                canceled_trip_count += 1;
            }
            if !t.stop_updates.is_empty() {
                trips_with_stop_updates += 1;
            }
            skipped_stop_count += t.stop_updates.values().filter(|s| s.skipped).count();
        }
        RtStats {
            feed_id: self.feed_id.clone(),
            trip_update_count: self.trip_update_count,
            vehicle_count: self.vehicle_count,
            alert_count: self.alert_count,
            canceled_trip_count,
            skipped_stop_count,
            trips_with_stop_updates,
            trip_updates_age_secs: self.trip_updates_age_secs(),
            vehicles_age_secs: self.vehicles_age_secs(),
            alerts_age_secs: self.alerts_age_secs(),
            header_timestamp: self.header_timestamp,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RealtimeOverlay {
    pub version: u64,
    pub feeds: HashMap<String, FeedRtState>,
    /// Namespaced undated trip ids canceled in RT (rebuilt when overlay is finalized).
    pub canceled_trip_ids: std::collections::HashSet<String>,
    /// Per-trip RT adjustments for RAPTOR (rebuilt when overlay is finalized).
    pub rt_adjust: HashMap<String, crate::routing::journey::RtTripAdjust>,
}

impl RealtimeOverlay {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Merge trip updates + vehicle positions from a partial feed snapshot (e.g. on-demand SM).
    ///
    /// Used so SIRI Stop Monitoring `VehicleLocation` (true GPS) reaches the map
    /// without waiting for the warm poller.
    pub fn merge_feed_delta(&mut self, feed_id: &str, delta: FeedRtState) {
        let entry = self
            .feeds
            .entry(feed_id.to_string())
            .or_insert_with(|| FeedRtState::new(feed_id));
        for (k, v) in delta.trips {
            entry.trips.insert(k, v);
        }
        for (k, v) in delta.vehicles {
            entry.vehicles.insert(k, v);
        }
        entry.trip_update_count = entry.trips.len();
        entry.vehicle_count = entry.vehicles.len();
        if delta.trip_updates_fetched_at.is_some() {
            entry.trip_updates_fetched_at = delta.trip_updates_fetched_at;
        }
        if delta.vehicles_fetched_at.is_some() {
            entry.vehicles_fetched_at = delta.vehicles_fetched_at;
        }
        // Drop stale vehicles (> 15 min) so SM/ET GPS does not linger forever.
        let cutoff = Utc::now() - chrono::Duration::minutes(15);
        entry.vehicles.retain(|_, v| v.updated_at >= cutoff);
        entry.vehicle_count = entry.vehicles.len();
        self.version = self.version.wrapping_add(1);
    }

    /// Resolve feed id from a namespaced trip key `feed:...`.
    pub fn feed_of(trip_id: &str) -> Option<&str> {
        trip_id.split(':').next()
    }

    pub fn get_trip(&self, trip_id: &str) -> Option<&TripRt> {
        let feed = Self::feed_of(trip_id)?;
        self.feeds.get(feed)?.get_trip(trip_id)
    }

    /// Prefer exact `trip_id` key; if missing (or `trip_id` is `None`), match
    /// incomplete RT entities by namespaced `route_id` + optional direction/date.
    pub fn get_trip_matching(
        &self,
        trip_id: Option<&str>,
        route_id: Option<&str>,
        direction_id: Option<u32>,
        start_date: Option<&str>,
    ) -> Option<&TripRt> {
        if let Some(tid) = trip_id.filter(|s| !s.is_empty()) {
            if let Some(t) = self.get_trip(tid) {
                return Some(t);
            }
        }
        let route_id = route_id.filter(|s| !s.is_empty())?;
        let feed = Self::feed_of(route_id)?;
        self.feeds
            .get(feed)?
            .find_trip_by_descriptor(route_id, direction_id, start_date)
    }

    /// Resolve RT overlay trip for a static GTFS namespaced `trip_id`.
    ///
    /// Tries exact key, SIRI alias reverse lookup, then route/direction/date descriptor match.
    pub fn resolve_trip_rt<'a>(
        &'a self,
        epoch: Option<&crate::gtfs::pack::StaticEpoch>,
        trip_id: &str,
        route_id: &str,
        direction_id: Option<u8>,
        scheduled_departure: DateTime<Utc>,
    ) -> Option<&'a TripRt> {
        if let Some(t) = self.get_trip(trip_id) {
            return Some(t);
        }
        let feed = Self::feed_of(trip_id).or_else(|| Self::feed_of(route_id))?;

        if let Some(epoch) = epoch {
            for alias_key in epoch.siri_trip_aliases.keys_for_trip(trip_id) {
                let candidates = [
                    alias_key.to_string(),
                    format!("{feed}:{alias_key}"),
                ];
                for key in candidates {
                    if let Some(t) = self.get_trip(&key) {
                        return Some(t);
                    }
                }
            }
        }

        let start_date = Self::service_date_yyyymmdd(scheduled_departure);
        self.get_trip_matching(
            Some(trip_id),
            Some(route_id),
            direction_id.map(|d| d as u32),
            Some(&start_date),
        )
    }

    /// Overlay trip key used for vehicle lookup (SIRI ref when aliased).
    pub fn overlay_trip_key(
        &self,
        epoch: Option<&crate::gtfs::pack::StaticEpoch>,
        static_trip_id: &str,
    ) -> Option<String> {
        if self.get_trip(static_trip_id).is_some() {
            return Some(static_trip_id.to_string());
        }
        let feed = Self::feed_of(static_trip_id)?;
        if let Some(epoch) = epoch {
            for alias_key in epoch.siri_trip_aliases.keys_for_trip(static_trip_id) {
                let candidates = [
                    alias_key.to_string(),
                    format!("{feed}:{alias_key}"),
                ];
                for key in candidates {
                    if self.get_trip(&key).is_some() {
                        return Some(key);
                    }
                }
            }
        }
        None
    }

    fn service_date_yyyymmdd(dt: DateTime<Utc>) -> String {
        dt.with_timezone(&chrono_tz::Europe::Paris)
            .format("%Y%m%d")
            .to_string()
    }

    /// Whether the trip is canceled (false if no RT).
    pub fn is_trip_canceled(&self, trip_id: &str) -> bool {
        self.get_trip(trip_id).map(|t| t.canceled).unwrap_or(false)
    }

    /// Apply RT to scheduled arrival; if no RT, returns scheduled with has_rt=false.
    pub fn apply_arrival(
        &self,
        trip_id: &str,
        scheduled: DateTime<Utc>,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
    ) -> StopRealtime {
        match self.get_trip(trip_id) {
            Some(t) => t.apply_arrival(scheduled, stop_sequence, stop_id),
            None => StopRealtime {
                realtime: scheduled,
                delay_secs: 0,
                skipped: false,
                canceled: false,
                has_rt: false,
            },
        }
    }

    pub fn apply_departure(
        &self,
        trip_id: &str,
        scheduled: DateTime<Utc>,
        stop_sequence: Option<u16>,
        stop_id: Option<&str>,
    ) -> StopRealtime {
        match self.get_trip(trip_id) {
            Some(t) => t.apply_departure(scheduled, stop_sequence, stop_id),
            None => StopRealtime {
                realtime: scheduled,
                delay_secs: 0,
                skipped: false,
                canceled: false,
                has_rt: false,
            },
        }
    }

    pub fn get_vehicle_for_trip(&self, trip_id: &str) -> Option<&VehiclePos> {
        let feed = Self::feed_of(trip_id)?;
        let st = self.feeds.get(feed)?;
        st.vehicles
            .values()
            .find(|v| v.trip_id.as_deref() == Some(trip_id))
            .or_else(|| st.vehicles.get(trip_id))
    }

    /// Whether any real vehicle position is linked to this trip key (or undated base).
    pub fn has_vehicle_for_trip(&self, trip_id: &str) -> bool {
        if self.get_vehicle_for_trip(trip_id).is_some() {
            return true;
        }
        if let Some((base, _)) = trip_id.rsplit_once('@') {
            return self.get_vehicle_for_trip(base).is_some();
        }
        false
    }

    pub fn alerts_for_trip(&self, trip_id: &str) -> Vec<&AlertRt> {
        let feed = match Self::feed_of(trip_id) {
            Some(f) => f,
            None => return vec![],
        };
        let Some(st) = self.feeds.get(feed) else {
            return vec![];
        };
        // Match full key or strip @date for informed_trip_ids which are undated.
        let undated = trip_id.rsplit_once('@').map(|(b, _)| b).unwrap_or(trip_id);
        st.alerts
            .iter()
            .filter(|a| {
                a.informed_trip_ids
                    .iter()
                    .any(|t| t == trip_id || t == undated)
            })
            .collect()
    }

    pub fn alerts_for_stop(&self, stop_id: &str) -> Vec<&AlertRt> {
        let feed = match Self::feed_of(stop_id) {
            Some(f) => f,
            None => {
                // Un-namespaced: search all feeds
                return self
                    .feeds
                    .values()
                    .flat_map(|st| st.alerts_for_stop(stop_id))
                    .collect();
            }
        };
        match self.feeds.get(feed) {
            Some(st) => st.alerts_for_stop(stop_id),
            None => vec![],
        }
    }

    pub fn alerts_for_route(&self, route_id: &str) -> Vec<&AlertRt> {
        let feed = match Self::feed_of(route_id) {
            Some(f) => f,
            None => {
                return self
                    .feeds
                    .values()
                    .flat_map(|st| st.alerts_for_route(route_id))
                    .collect();
            }
        };
        match self.feeds.get(feed) {
            Some(st) => st.alerts_for_route(route_id),
            None => vec![],
        }
    }

    /// Canceled trip keys across all feeds (undated keys only).
    pub fn trips_canceled(&self) -> Vec<(&str, &TripRt)> {
        self.feeds
            .values()
            .flat_map(|st| st.trips_canceled())
            .collect()
    }

    pub fn max_trip_updates_age_secs(&self) -> Option<i64> {
        self.feeds
            .values()
            .filter_map(|f| f.trip_updates_age_secs())
            .max()
    }

    pub fn stats(&self) -> Vec<RtStats> {
        self.feeds.values().map(|f| f.stats()).collect()
    }

    /// Aggregate stats across all feeds (feed_id empty).
    pub fn stats_total(&self) -> RtStats {
        let mut total = RtStats::default();
        for s in self.stats() {
            total.trip_update_count += s.trip_update_count;
            total.vehicle_count += s.vehicle_count;
            total.alert_count += s.alert_count;
            total.canceled_trip_count += s.canceled_trip_count;
            total.skipped_stop_count += s.skipped_stop_count;
            total.trips_with_stop_updates += s.trips_with_stop_updates;
            total.trip_updates_age_secs =
                max_opt(total.trip_updates_age_secs, s.trip_updates_age_secs);
            total.vehicles_age_secs = max_opt(total.vehicles_age_secs, s.vehicles_age_secs);
            total.alerts_age_secs = max_opt(total.alerts_age_secs, s.alerts_age_secs);
        }
        total
    }
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// Synthetic map key for trip updates without `trip_id`.
fn incomplete_trip_key(feed_id: &str, trip: &TripRt) -> String {
    let route = trip.route_id.as_deref().unwrap_or("");
    let dir = trip
        .direction_id
        .map(|d| d.to_string())
        .unwrap_or_default();
    let sd = trip.start_date.as_deref().unwrap_or("");
    let st = trip.start_time.as_deref().unwrap_or("");
    format!("{feed_id}:#r={route}&d={dir}&sd={sd}&st={st}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sched(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    fn sample_trip() -> TripRt {
        let mut stop_updates = HashMap::new();
        stop_updates.insert(
            1,
            StopTimeRt {
                stop_sequence: 1,
                stop_id: Some("sncf:STOP_A".into()),
                arrival_delay: Some(120),
                departure_delay: Some(90),
                arrival_time: None,
                departure_time: None,
                skipped: false,
            },
        );
        stop_updates.insert(
            2,
            StopTimeRt {
                stop_sequence: 2,
                stop_id: Some("sncf:STOP_B".into()),
                arrival_delay: None,
                departure_delay: None,
                arrival_time: None,
                departure_time: None,
                skipped: true,
            },
        );
        stop_updates.insert(
            3,
            StopTimeRt {
                stop_sequence: 3,
                stop_id: Some("sncf:STOP_C".into()),
                arrival_delay: None,
                departure_delay: None,
                arrival_time: Some(1_700_000_300),
                departure_time: Some(1_700_000_310),
                skipped: false,
            },
        );
        TripRt {
            delay: Some(60),
            canceled: false,
            start_date: Some("20240115".into()),
            route_id: Some("sncf:R1".into()),
            direction_id: Some(0),
            start_time: Some("08:00:00".into()),
            stop_updates,
        }
    }

    #[test]
    fn stop_delay_prefers_stop_over_trip() {
        let t = sample_trip();
        assert_eq!(t.arrival_delay_secs(Some(1), None), Some(120));
        assert_eq!(t.departure_delay_secs(Some(1), None), Some(90));
        // No stop-level delay → trip delay
        assert_eq!(t.arrival_delay_secs(Some(99), None), Some(60));
        // By stop_id
        assert_eq!(
            t.arrival_delay_secs(None, Some("sncf:STOP_A")),
            Some(120)
        );
        assert_eq!(t.arrival_delay_secs(None, Some("STOP_A")), Some(120));
    }

    #[test]
    fn apply_arrival_adds_delay() {
        let t = sample_trip();
        let s = sched(1_700_000_000);
        let r = t.apply_arrival(s, Some(1), None);
        assert_eq!(r.delay_secs, 120);
        assert_eq!(r.realtime, sched(1_700_000_120));
        assert!(!r.skipped);
        assert!(!r.canceled);
        assert!(r.has_rt);
    }

    #[test]
    fn apply_uses_absolute_time() {
        let t = sample_trip();
        let s = sched(1_700_000_000);
        let r = t.apply_arrival(s, Some(3), None);
        assert_eq!(r.realtime, sched(1_700_000_300));
        assert_eq!(r.delay_secs, 300);
        assert!(r.has_rt);
    }

    #[test]
    fn skipped_and_canceled() {
        let mut t = sample_trip();
        assert!(t.is_stop_skipped(Some(2), None));
        let r = t.apply_departure(sched(1_700_000_000), Some(2), None);
        assert!(r.skipped);
        assert!(r.has_rt);

        t.canceled = true;
        let r = t.apply_arrival(sched(1_700_000_000), Some(1), None);
        assert!(r.canceled);
    }

    #[test]
    fn dual_key_insert_and_lookup() {
        let mut feed = FeedRtState::new("sncf");
        feed.insert_trip("sncf", "T1", sample_trip());
        assert!(feed.trips.contains_key("sncf:T1"));
        assert!(feed.trips.contains_key("sncf:T1@20240115"));
        assert!(feed.get_trip("sncf:T1").is_some());
        assert!(feed.get_trip("sncf:T1@20240115").is_some());
        // Dated miss falls back? same data under both; undated exists
        assert!(feed.get_trip("sncf:T1@20990101").is_some()); // falls back to base
    }

    #[test]
    fn trips_canceled_and_alerts() {
        let mut feed = FeedRtState::new("sncf");
        let mut t = sample_trip();
        t.canceled = true;
        feed.insert_trip("sncf", "CX", t);
        let canceled: Vec<_> = feed.trips_canceled().map(|(k, _)| k).collect();
        assert_eq!(canceled, vec!["sncf:CX"]);

        feed.alerts.push(AlertRt {
            id: "sncf:a1".into(),
            header: Some("Works".into()),
            description: None,
            severity: "WARNING".into(),
            informed_stop_ids: vec!["sncf:STOP_A".into()],
            informed_route_ids: vec!["sncf:R1".into()],
            informed_trip_ids: vec!["sncf:CX".into()],
            cause: Some("CONSTRUCTION".into()),
            effect: Some("SIGNIFICANT_DELAYS".into()),
            url: None,
            active_start: None,
            active_end: None,
            active_periods: vec![],
            header_translations: vec![("".into(), "Works".into())],
        });
        assert_eq!(feed.alerts_for_stop("sncf:STOP_A").len(), 1);
        assert_eq!(feed.alerts_for_stop("STOP_A").len(), 1);
        assert_eq!(feed.alerts_for_route("sncf:R1").len(), 1);
        assert_eq!(feed.alerts_for_route("R1").len(), 1);

        let st = feed.stats();
        assert_eq!(st.canceled_trip_count, 1);
        assert_eq!(st.skipped_stop_count, 1);
    }

    #[test]
    fn overlay_apply_without_rt() {
        let ov = RealtimeOverlay::default();
        let s = sched(1_700_000_000);
        let r = ov.apply_arrival("sncf:missing", s, Some(1), None);
        assert!(!r.has_rt);
        assert_eq!(r.realtime, s);
        assert_eq!(r.delay_secs, 0);
    }

    #[test]
    fn overlay_get_trip_and_cancel() {
        let mut ov = RealtimeOverlay::default();
        let mut feed = FeedRtState::new("sncf");
        let mut t = sample_trip();
        t.canceled = true;
        feed.insert_trip("sncf", "T9", t);
        feed.trip_update_count = 1;
        ov.feeds.insert("sncf".into(), feed);

        assert!(ov.is_trip_canceled("sncf:T9"));
        assert!(ov.is_trip_canceled("sncf:T9@20240115"));
        assert!(!ov.is_trip_canceled("sncf:other"));

        let r = ov.apply_arrival("sncf:T9", sched(1_700_000_000), Some(1), None);
        assert!(r.canceled);
        assert_eq!(r.delay_secs, 120);

        assert_eq!(ov.trips_canceled().len(), 1);
        let total = ov.stats_total();
        assert_eq!(total.canceled_trip_count, 1);
    }
}
