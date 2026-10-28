//! SIRI `DatedVehicleJourneyRef` ↔ GTFS `trip_id` mapping for IDFM.
//!
//! IDFM ships cross-references in GTFS `object_codes_extension.txt` (`object_type=trip`,
//! `object_system=source`) e.g. `SNCF:ServiceJourney:{uuid}:LOC` → `IDFM:TN:SNCF:{uuid}`.
//! PRIM SIRI Lite uses operator-specific refs (`SNCF_MAGENTA_PRD:VehicleJourney::{uuid}:LOC`,
//! `RATP-SIV:VehicleJourney::…`, `stif:VehicleJourney:local-…`) that rarely equal GTFS `trip_id`.
//!
//! This module builds a lookup table at pack time and resolves static trips at runtime.

use std::collections::HashMap;

use chrono::Timelike;

use super::pack::{GlobalTrip, StaticEpoch};

/// Optional RT hints when a bare SIRI journey ref does not hit the alias table.
#[derive(Debug, Clone, Default)]
pub struct TripResolveHint {
    /// Namespaced LineRef / route id from the RT payload (`idfm:STIF:Line::C01742:`).
    pub route_id: Option<String>,
    /// First aimed/expected departure (Unix seconds UTC) from RT calls.
    pub first_departure_utc: Option<i64>,
}

/// Alias table: various SIRI / NeTEx ref forms → namespaced GTFS `trip_id`.
#[derive(Debug, Clone, Default)]
pub struct SiriTripAliases {
    pub to_trip_id: HashMap<String, String>,
}

impl SiriTripAliases {
    pub fn insert(&mut self, key: impl Into<String>, trip_id: impl Into<String>) {
        let key = key.into();
        if key.is_empty() {
            return;
        }
        self.to_trip_id
            .entry(key)
            .or_insert_with(|| trip_id.into());
    }

    pub fn lookup(&self, key: &str) -> Option<&str> {
        self.to_trip_id.get(key).map(|s| s.as_str())
    }

    /// SIRI / RT overlay keys that resolve to a namespaced GTFS `trip_id`.
    pub fn keys_for_trip<'a>(&'a self, trip_id: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.to_trip_id
            .iter()
            .filter_map(move |(k, v)| if v == trip_id { Some(k.as_str()) } else { None })
    }
}

/// Build aliases from IDFM `object_codes_extension.txt` (optional in other feeds).
///
/// Only trips present in `kept_raw_trip_ids` (post-horizon filter) are indexed.
pub fn build_from_object_codes(
    csv: &str,
    feed_id: &str,
    kept_raw_trip_ids: &std::collections::HashSet<String>,
) -> SiriTripAliases {
    let mut out = SiriTripAliases::default();
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(csv.as_bytes());
    for rec in rdr.deserialize::<HashMap<String, String>>() {
        let Ok(rec) = rec else { continue };
        if rec.get("object_type").map(|s| s.as_str()) != Some("trip") {
            continue;
        }
        let raw_trip = match rec.get("object_id") {
            Some(s) if !s.is_empty() => s.as_str(),
            _ => continue,
        };
        if !kept_raw_trip_ids.contains(raw_trip) {
            continue;
        }
        let namespaced = super::pack::ns(feed_id, raw_trip);
        let system = rec.get("object_system").map(|s| s.as_str()).unwrap_or("");
        let code = match rec.get("object_code") {
            Some(s) if !s.is_empty() => s.as_str(),
            _ => continue,
        };

        // Always index the GTFS trip id itself.
        out.insert(raw_trip, &namespaced);
        out.insert(&namespaced, &namespaced);

        if system == "source" || system == "external" {
            register_code_keys(&mut out, code, &namespaced);
        }
    }
    out
}

fn register_code_keys(aliases: &mut SiriTripAliases, code: &str, namespaced_trip: &str) {
    let trimmed = code.trim_end_matches(':');
    aliases.insert(code, namespaced_trip);
    aliases.insert(trimmed, namespaced_trip);

    // NeTEx `:LOC` suffix
    if let Some(base) = trimmed.strip_suffix(":LOC") {
        aliases.insert(base, namespaced_trip);
    }

    // Journey id = last significant segment (uuid, local-…, COU_RATP_…)
    if let Some(jid) = journey_id_from_code(trimmed) {
        aliases.insert(&jid, namespaced_trip);
        if is_uuid(&jid) {
            aliases.insert(format!("IDFM:TN:SNCF:{jid}"), namespaced_trip);
            aliases.insert(super::pack::ns("idfm", &format!("IDFM:TN:SNCF:{jid}")), namespaced_trip);
            for op in ["SNCF_MAGENTA_PRD", "SNCF"] {
                aliases.insert(format!("{op}:VehicleJourney::{jid}:LOC"), namespaced_trip);
                aliases.insert(format!("{op}:VehicleJourney::{jid}"), namespaced_trip);
            }
        }
        if jid.starts_with("local-") {
            aliases.insert(format!("stif:VehicleJourney:{jid}:LOC"), namespaced_trip);
            aliases.insert(format!("IDFM:stif:{jid}"), namespaced_trip);
        }
    }

    // SIRI double-colon form from NeTEx single-colon ServiceJourney / VehicleJourney
    if let Some(siri) = netex_to_siri_vehicle_journey(trimmed) {
        aliases.insert(&siri, namespaced_trip);
        if let Some(no_loc) = siri.strip_suffix(":LOC") {
            aliases.insert(no_loc, namespaced_trip);
        }
    }
}

/// Parse `OPERATOR:ServiceJourney:ID:LOC` → `OPERATOR_SIRI:VehicleJourney::ID:LOC` variants.
fn netex_to_siri_vehicle_journey(code: &str) -> Option<String> {
    let parts: Vec<&str> = code.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let kind = parts[1];
    if !kind.eq_ignore_ascii_case("ServiceJourney") && !kind.eq_ignore_ascii_case("VehicleJourney")
    {
        return None;
    }
    let op = parts[0];
    let mut jid_parts: Vec<&str> = parts[2..].to_vec();
    if jid_parts.last().map(|s| s.eq_ignore_ascii_case("LOC")) == Some(true) {
        jid_parts.pop();
    }
    let jid = jid_parts.join(":");
    if jid.is_empty() {
        return None;
    }
    let siri_op = siri_operator_alias(op);
    Some(format!("{siri_op}:VehicleJourney::{jid}:LOC"))
}

fn siri_operator_alias(netex_op: &str) -> String {
    match netex_op.to_ascii_uppercase().as_str() {
        "SNCF" => "SNCF_MAGENTA_PRD".into(),
        "RATP" => "RATP-SIV".into(),
        "STIF" => "stif".into(),
        other => other.to_string(),
    }
}

fn journey_id_from_code(code: &str) -> Option<String> {
    // SIRI `OP:VehicleJourney::ID:LOC`
    if let Some((_, rest)) = code.split_once("::") {
        let id = rest.trim_end_matches(':').trim_end_matches(":LOC");
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    // NeTEx `OP:ServiceJourney:ID:LOC` or `OP:VehicleJourney:ID:LOC`
    let parts: Vec<&str> = code.split(':').collect();
    if parts.len() >= 3 {
        let kind = parts[1];
        if kind.eq_ignore_ascii_case("ServiceJourney")
            || kind.eq_ignore_ascii_case("VehicleJourney")
        {
            let mut tail: Vec<&str> = parts[2..].to_vec();
            if tail.last().map(|s| s.eq_ignore_ascii_case("LOC")) == Some(true) {
                tail.pop();
            }
            let jid = tail.join(":");
            if !jid.is_empty() {
                return Some(jid);
            }
        }
    }
    None
}

fn is_uuid(s: &str) -> bool {
    let s = s.trim();
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    bytes[8] == b'-' && bytes[13] == b'-' && bytes[18] == b'-' && bytes[23] == b'-'
}

/// Lookup keys to try for a namespaced or raw SIRI journey ref.
pub fn siri_ref_lookup_keys(base_key: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut push = |k: &str| {
        if !k.is_empty() && !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    };

    push(base_key);
    let raw = base_key
        .strip_prefix("idfm:")
        .or_else(|| base_key.strip_prefix("IDFM:"))
        .unwrap_or(base_key);
    push(raw);

    if let Some(stripped) = raw.strip_suffix(":LOC") {
        push(stripped);
    }

    if let Some(jid) = journey_id_from_code(raw) {
        push(&jid);
        if is_uuid(&jid) {
            push(&format!("IDFM:TN:SNCF:{jid}"));
            for op in ["SNCF_MAGENTA_PRD", "SNCF"] {
                push(&format!("{op}:VehicleJourney::{jid}:LOC"));
            }
        }
        if jid.starts_with("local-") {
            push(&format!("stif:VehicleJourney:{jid}:LOC"));
            push(&format!("IDFM:stif:{jid}"));
        }
    }

    keys
}

/// Resolve a static trip from a SIRI / RT trip key.
pub fn resolve_static_trip<'a>(
    epoch: &'a StaticEpoch,
    base_key: &str,
    hint: Option<&TripResolveHint>,
) -> Option<&'a GlobalTrip> {
    // 1) Exact GTFS trip id
    if let Some(&idx) = epoch.trip_id_to_idx.get(base_key) {
        return epoch.trips.get(idx as usize);
    }

    // 2) Alias table (object_codes_extension + derived SIRI forms)
    for key in siri_ref_lookup_keys(base_key) {
        if let Some(tid) = epoch.siri_trip_aliases.lookup(&key) {
            if let Some(&idx) = epoch.trip_id_to_idx.get(tid) {
                return epoch.trips.get(idx as usize);
            }
        }
    }

    // 3) SNCF UUID fast path: construct `feed:IDFM:TN:SNCF:{uuid}` from SIRI ref
    if let Some(jid) = journey_id_from_code(base_key) {
        if is_uuid(&jid) {
            let feed = base_key.split(':').next().unwrap_or("idfm");
            let constructed = format!("{feed}:IDFM:TN:SNCF:{jid}");
            if let Some(&idx) = epoch.trip_id_to_idx.get(&constructed) {
                return epoch.trips.get(idx as usize);
            }
        }
    }

    // 4) Time + route disambiguation (RER / Transilien when UUID missing from static)
    if let Some(h) = hint {
        if let Some(trip) = resolve_by_route_and_time(epoch, base_key, h) {
            return Some(trip);
        }
    }

    // 5) Legacy fuzzy suffix match — only on a distinctive tail, and only
    //    when exactly one static trip matches (HashMap order is random, so
    //    "first match" would be nondeterministic).
    fuzzy_unique_match(epoch, base_key)
}

/// Minimum length of the journey-ref tail used for fuzzy matching. Shorter
/// tails ("1", "", "LOC") match many unrelated trips.
const FUZZY_MIN_TAIL_LEN: usize = 6;

fn fuzzy_unique_match<'a>(epoch: &'a StaticEpoch, base_key: &str) -> Option<&'a GlobalTrip> {
    let trimmed = base_key.trim_end_matches(':');
    let trimmed = trimmed.strip_suffix(":LOC").unwrap_or(trimmed);
    let raw = trimmed.rsplit_once(':').map(|(_, r)| r).unwrap_or(trimmed);
    if raw.len() < FUZZY_MIN_TAIL_LEN {
        return None;
    }
    let mut found: Option<u32> = None;
    for (k, &idx) in &epoch.trip_id_to_idx {
        if k.ends_with(raw) || (raw.len() >= 8 && k.contains(raw)) {
            if found.is_some_and(|f| f != idx) {
                return None; // ambiguous
            }
            found = Some(idx);
        }
    }
    found.and_then(|idx| epoch.trips.get(idx as usize))
}

/// Seconds in a GTFS service day (used to map wall-clock times onto the
/// previous service day's > 24:00 times).
const DAY_S: i64 = 86_400;

fn resolve_by_route_and_time<'a>(
    epoch: &'a StaticEpoch,
    base_key: &str,
    hint: &TripResolveHint,
) -> Option<&'a GlobalTrip> {
    let dep_utc = hint.first_departure_utc?;
    let line_token = hint
        .route_id
        .as_deref()
        .and_then(line_product_code)
        .or_else(|| line_token_from_journey_ref(base_key))?;

    let tz = chrono_tz::Europe::Paris;
    let dt = chrono::DateTime::from_timestamp(dep_utc, 0)?.with_timezone(&tz);
    let dep_s = dt.num_seconds_from_midnight() as i64;
    let today = dt.date_naive();
    let yesterday = today.pred_opt();

    let candidates = epoch.route_line_to_trip_idxs.get(&line_token)?;
    // Service is unknown when the feed has no calendar (synthetic epochs):
    // don't filter in that case.
    let runs_on = |trip: &GlobalTrip, date: chrono::NaiveDate| -> bool {
        epoch
            .calendars
            .get(&trip.feed_id)
            .map(|cal| cal.is_active(&trip.service_id, date))
            .unwrap_or(true)
    };

    // (|delta| s, scheduled departure relative to `today` midnight, trip_idx)
    let mut matches: Vec<(i64, i64, u32)> = Vec::new();
    for &trip_idx in candidates {
        let Some(trip) = epoch.trips.get(trip_idx as usize) else {
            continue;
        };
        if !trip.frequency_windows.is_empty() {
            continue;
        }
        let Some(st) = epoch.stop_times.get(trip.stop_time_start as usize) else {
            continue;
        };
        let sched = st.departure_s as i64;
        let mut best_for_trip: Option<(i64, i64)> = None;
        // Trip of today's service day: schedule time == wall time.
        if runs_on(trip, today) {
            best_for_trip = Some(((sched - dep_s).abs(), sched));
        }
        // Trip of yesterday's service day: schedule time − 24h == wall time
        // (e.g. a 00:30 RT departure ↔ 24:30 in GTFS).
        if let Some(y) = yesterday {
            if sched >= DAY_S / 2 && runs_on(trip, y) {
                let rel = sched - DAY_S;
                let d = (rel - dep_s).abs();
                if best_for_trip.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best_for_trip = Some((d, rel));
                }
            }
        }
        if let Some((d, rel)) = best_for_trip {
            if d <= 900 + 120 {
                matches.push((d, rel, trip_idx));
            }
        }
    }
    let &(delta, best_rel, trip_idx) = matches.iter().min_by_key(|m| (m.0, m.2))?;
    if delta > 900 {
        return None;
    }
    // Reject ambiguous matches (two trips within 2 min of schedule)
    let close = matches
        .iter()
        .filter(|m| (m.1 - best_rel).abs() <= 120)
        .count();
    if close <= 1 || delta <= 60 {
        return epoch.trips.get(trip_idx as usize);
    }
    None
}

fn line_token_from_journey_ref(base_key: &str) -> Option<String> {
    let jid = journey_id_from_code(base_key)?;
    // RATP-SIV dated: 20260727.361.A.C01371
    let part = jid.rsplit('.').next()?;
    if part.len() >= 5 && part.starts_with('C') {
        return Some(part.to_ascii_uppercase());
    }
    None
}

pub fn line_product_code(route_or_line: &str) -> Option<String> {
    let upper = route_or_line.to_ascii_uppercase();
    if let Some(i) = upper.find("LINE::") {
        let rest = &upper[i + 6..];
        let tok: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if tok.len() >= 4 {
            return Some(tok);
        }
    }
    for part in route_or_line.split(|c: char| !c.is_ascii_alphanumeric()) {
        let p = part.to_ascii_uppercase();
        if p.len() >= 5 && p.starts_with('C') && p.chars().skip(1).all(|c| c.is_ascii_digit()) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::pack::{GlobalTrip, RouteMode, StaticEpoch};

    #[test]
    fn object_codes_maps_sncf_uuid() {
        let csv = "\
object_type,object_id,object_system,object_code
trip,IDFM:TN:SNCF:9e1f3cec-e963-4c83-878e-b924db62121d,source,SNCF:ServiceJourney:9e1f3cec-e963-4c83-878e-b924db62121d:LOC
";
        let mut kept = std::collections::HashSet::new();
        kept.insert("IDFM:TN:SNCF:9e1f3cec-e963-4c83-878e-b924db62121d".into());
        let aliases = build_from_object_codes(csv, "idfm", &kept);
        let siri = "SNCF_MAGENTA_PRD:VehicleJourney::9e1f3cec-e963-4c83-878e-b924db62121d:LOC";
        assert_eq!(
            aliases.lookup(siri),
            Some("idfm:IDFM:TN:SNCF:9e1f3cec-e963-4c83-878e-b924db62121d")
        );
    }

    #[test]
    fn resolve_sncf_from_siri_ref() {
        let uuid = "9e1f3cec-e963-4c83-878e-b924db62121d";
        let trip_id = format!("idfm:IDFM:TN:SNCF:{uuid}");
        let mut epoch = StaticEpoch::empty();
        epoch.trips.push(GlobalTrip {
            id: trip_id.clone(),
            feed_id: "idfm".into(),
            route_id: "idfm:IDFM:C01742".into(),
            service_id: "svc".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Rail,
            route_short_name: "A".into(),
            route_long_name: "RER A".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 2,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len: 0,
            frequency_windows: vec![],
        });
        epoch.trip_id_to_idx.insert(trip_id, 0);
        let siri = format!("idfm:SNCF_MAGENTA_PRD:VehicleJourney::{uuid}:LOC");
        let trip = resolve_static_trip(&epoch, &siri, None).expect("resolve SNCF");
        assert!(trip.id.contains(uuid));
    }

    #[test]
    fn journey_id_extracts_siri_double_colon() {
        let id = journey_id_from_code("RATP-SIV:VehicleJourney::20260727.361.A.C01371:LOC");
        assert_eq!(id.as_deref(), Some("20260727.361.A.C01371"));
    }

    fn push_trip(epoch: &mut StaticEpoch, id: &str, service_id: &str, first_dep_s: u32) -> u32 {
        use crate::gtfs::pack::PackedStopTime;
        let start = epoch.stop_times.len() as u32;
        epoch.stop_times.push(PackedStopTime {
            stop_idx: 0,
            arrival_s: first_dep_s,
            departure_s: first_dep_s,
            stop_sequence: 1,
            pickup_type: 0,
            drop_off_type: 0,
            stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
        });
        let idx = epoch.trips.len() as u32;
        epoch.trips.push(GlobalTrip {
            id: id.into(),
            feed_id: "idfm".into(),
            route_id: "idfm:IDFM:C01742".into(),
            service_id: service_id.into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Rail,
            route_short_name: "A".into(),
            route_long_name: "RER A".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 2,
            agency_name: None,
            stop_time_start: start,
            stop_time_len: 1,
            frequency_windows: vec![],
        });
        epoch.trip_id_to_idx.insert(id.into(), idx);
        epoch
            .route_line_to_trip_idxs
            .entry("C01742".into())
            .or_default()
            .push(idx);
        idx
    }

    #[test]
    fn fuzzy_match_requires_distinctive_unique_tail() {
        let mut epoch = StaticEpoch::empty();
        push_trip(&mut epoch, "idfm:IDFM:TN:XYZ123456", "S", 3600);
        push_trip(&mut epoch, "idfm:IDFM:RATP:line1", "S", 3600);
        push_trip(&mut epoch, "idfm:IDFM:RATP:other1", "S", 3600);

        // Empty tail (ref ends with ':') used to match every trip.
        assert!(resolve_static_trip(&epoch, "idfm:OP:VehicleJourney::", None).is_none());
        assert!(resolve_static_trip(&epoch, "idfm:OP:VehicleJourney:::", None).is_none());
        // Short tail "1" matches several trips.
        assert!(resolve_static_trip(&epoch, "idfm:OP:VehicleJourney::1", None).is_none());
        // `:LOC` suffix is not a usable tail on its own.
        assert!(resolve_static_trip(&epoch, "idfm:OP:LOC", None).is_none());

        // Distinctive unique tail resolves (also with a :LOC suffix).
        let t = resolve_static_trip(&epoch, "idfm:OP:VehicleJourney::XYZ123456:LOC", None)
            .expect("unique tail");
        assert_eq!(t.id, "idfm:IDFM:TN:XYZ123456");

        // Same tail on two trips → ambiguous → no match.
        push_trip(&mut epoch, "idfm:IDFM:TN:OTHER:XYZ123456", "S", 7200);
        assert!(resolve_static_trip(&epoch, "idfm:OP:VehicleJourney::XYZ123456", None).is_none());
    }

    #[test]
    fn route_time_resolution_respects_calendar_and_after_midnight() {
        use crate::gtfs::calendar::ServiceCalendar;
        use chrono::{NaiveDate, TimeZone};

        let mut epoch = StaticEpoch::empty();
        let mut cal = ServiceCalendar::new();
        let from = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        cal.add_regular("WD".into(), from, to, true, true, true, true, true, false, false);
        cal.add_regular("WE".into(), from, to, false, false, false, false, false, true, true);
        epoch
            .calendars
            .insert("idfm".into(), std::sync::Arc::new(cal));

        // Weekend trip at 08:00 and weekday trip at 08:03; Monday RT at 08:00.
        push_trip(&mut epoch, "idfm:we-0800", "WE", 8 * 3600);
        push_trip(&mut epoch, "idfm:wd-0803", "WD", 8 * 3600 + 180);
        // Weekday service-day trip at 24:30 (i.e. Tuesday 00:30 wall clock).
        push_trip(&mut epoch, "idfm:wd-2430", "WD", 24 * 3600 + 1800);

        let paris = chrono_tz::Europe::Paris;
        let hint = |y, m, d, hh, mm| TripResolveHint {
            route_id: Some("idfm:STIF:Line::C01742:".into()),
            first_departure_utc: Some(
                paris
                    .with_ymd_and_hms(y, m, d, hh, mm, 0)
                    .single()
                    .unwrap()
                    .timestamp(),
            ),
        };
        let key = "idfm:SNCF:VehicleJourney::nomatch";

        // Monday 2026-07-27 08:00 → the weekday trip, not the weekend one.
        let t = resolve_static_trip(&epoch, key, Some(&hint(2026, 7, 27, 8, 0))).unwrap();
        assert_eq!(t.id, "idfm:wd-0803");
        // Saturday 2026-08-01 08:00 → the weekend trip.
        let t = resolve_static_trip(&epoch, key, Some(&hint(2026, 8, 1, 8, 0))).unwrap();
        assert_eq!(t.id, "idfm:we-0800");
        // Tuesday 00:30 → Monday's 24:30 trip.
        let t = resolve_static_trip(&epoch, key, Some(&hint(2026, 7, 28, 0, 30))).unwrap();
        assert_eq!(t.id, "idfm:wd-2430");
        // Sunday 00:30: Saturday's service day has no 24:30 trip.
        assert!(resolve_static_trip(&epoch, key, Some(&hint(2026, 8, 2, 0, 30))).is_none());
    }
}
