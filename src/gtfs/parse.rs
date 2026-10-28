use chrono::{NaiveDate, Utc};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use tracing::{info, warn};
use zip::ZipArchive;

use super::calendar::ServiceCalendar;
use super::pack::{
    ns, AgencyRecord, FareAttribute, FareRule, FeedStaticBundle, FrequencyWindow, LevelRecord,
    PackedStopTime, PathwayRecord, RouteMode, RouteRecord, StopRecord, TransferEdge, TripRecord,
};
use super::siri_trip_map::{build_from_object_codes, SiriTripAliases};
use crate::error::{Result, TransitError};

/// Hours above this in a GTFS time are treated as invalid (GTFS allows > 24h
/// for after-midnight service, but nothing legitimately runs 3 days past
/// the service date).
const MAX_GTFS_TIME_HOURS: u32 = 72;

/// Max decompressed size accepted for `stop_times.txt` (zip-bomb guard).
pub const MAX_STOP_TIMES_ENTRY_BYTES: u64 = 4 << 30;
/// Max decompressed size accepted for any other GTFS zip entry.
pub const MAX_ZIP_ENTRY_BYTES: u64 = 512 << 20;

/// Default timezone when the feed's `agency_timezone` is absent / invalid.
const DEFAULT_FEED_TZ: chrono_tz::Tz = chrono_tz::Europe::Paris;

/// Parse a GTFS `HH:MM:SS` time (hours may exceed 24) into seconds since
/// service-day noon-minus-12h. Rejects non-digit input, minutes/seconds
/// ≥ 60 and hours > [`MAX_GTFS_TIME_HOURS`].
fn parse_time_to_seconds(s: &str) -> Option<u32> {
    let s = s.trim();
    let mut parts = s.split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let num = |p: &str, max_len: usize| -> Option<u32> {
        if p.is_empty() || p.len() > max_len || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let h = num(h, 3)?;
    let m = num(m, 2)?;
    let sec = num(sec, 2)?;
    if h > MAX_GTFS_TIME_HOURS || m >= 60 || sec >= 60 {
        return None;
    }
    h.checked_mul(3600)?
        .checked_add(m.checked_mul(60)?)?
        .checked_add(sec)
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

/// Feed timezone from the first valid `agency_timezone` in agency.txt,
/// falling back to Europe/Paris.
fn feed_timezone(agency_csv: Option<&str>) -> chrono_tz::Tz {
    let Some(csv) = agency_csv else {
        return DEFAULT_FEED_TZ;
    };
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(csv.as_bytes());
    for rec in rdr.deserialize::<HashMap<String, String>>() {
        let Ok(rec) = rec else { continue };
        if let Some(tz) = rec
            .get("agency_timezone")
            .and_then(|s| s.trim().parse::<chrono_tz::Tz>().ok())
        {
            return tz;
        }
    }
    DEFAULT_FEED_TZ
}

fn zip_entry_cap(name: &str) -> u64 {
    if name.eq_ignore_ascii_case("stop_times.txt") {
        MAX_STOP_TIMES_ENTRY_BYTES
    } else {
        MAX_ZIP_ENTRY_BYTES
    }
}

fn find_zip_entry(archive: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Option<String> {
    // case-insensitive search on the basename
    archive
        .file_names()
        .filter_map(|n| n.ok())
        .find(|n| {
            let n: &str = n;
            let base = Path::new(n)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(n);
            base.eq_ignore_ascii_case(name)
        })
        .map(|s| s.to_string())
}

fn read_named_zip_entry(
    archive: &mut ZipArchive<Cursor<Vec<u8>>>,
    entry_name: &str,
    cap: u64,
) -> Result<String> {
    let file = archive
        .by_name(entry_name)
        .map_err(|e| TransitError::Parse(e.to_string()))?;
    // Declared size is attacker-controlled but cheap to check first; the
    // `take` below enforces the cap on the actual decompressed stream.
    if file.size() > cap {
        return Err(TransitError::Parse(format!(
            "{entry_name}: declared size {} exceeds cap {cap} bytes",
            file.size()
        )));
    }
    let mut bytes = Vec::with_capacity(file.size().min(cap) as usize);
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| TransitError::Parse(format!("{entry_name}: {e}")))?;
    if bytes.len() as u64 > cap {
        return Err(TransitError::Parse(format!(
            "{entry_name}: decompressed size exceeds cap {cap} bytes"
        )));
    }
    let mut buf =
        String::from_utf8(bytes).map_err(|e| TransitError::Parse(format!("{entry_name}: {e}")))?;
    // strip UTF-8 BOM
    if buf.starts_with('\u{feff}') {
        buf.drain(..'\u{feff}'.len_utf8());
    }
    Ok(buf)
}

fn read_zip_entry(archive: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Result<String> {
    let Some(entry_name) = find_zip_entry(archive, name) else {
        return Err(TransitError::Parse(format!("missing {name} in GTFS zip")));
    };
    read_named_zip_entry(archive, &entry_name, zip_entry_cap(name))
}

fn optional_zip_entry(archive: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Option<String> {
    let entry_name = find_zip_entry(archive, name)?;
    match read_named_zip_entry(archive, &entry_name, zip_entry_cap(name)) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(entry = name, error = %e, "skipping unreadable optional GTFS file");
            None
        }
    }
}

/// Stop time as parsed, before blank-time interpolation.
struct RawStopTime {
    /// Arrival (falls back to departure when only that is given).
    arr: Option<u32>,
    /// Departure (falls back to arrival when only that is given).
    dep: Option<u32>,
    /// Packed row; `arrival_s` / `departure_s` are filled by [`fill_missing_times`].
    st: PackedStopTime,
}

fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let dlat = lat2 - lat1;
    let dlon = lon2 - lon1;
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

/// Fill blank arrival/departure times of a trip already sorted by
/// `stop_sequence`, interpolating linearly between the surrounding timed
/// stops. The interpolation weight is, in order of preference:
/// `shape_dist_traveled` (when present and non-decreasing over the gap),
/// straight-line distance between consecutive stops (when all have
/// coordinates), else stop index. Interpolated rows get `timepoint = 0`.
///
/// Returns the number of interpolated rows, or `None` when the first or last
/// stop has no time (the trip cannot be anchored and must be dropped).
fn fill_missing_times(list: &mut [RawStopTime], stops: &[StopRecord]) -> Option<usize> {
    let n = list.len();
    if n == 0 {
        return Some(0);
    }
    if list[0].dep.is_none() || list[n - 1].arr.is_none() {
        return None;
    }
    let mut interpolated = 0usize;
    let mut a = 0usize; // last timed index
    while a < n - 1 {
        let b = (a + 1..n).find(|&i| list[i].dep.is_some()).unwrap_or(n - 1);
        if b > a + 1 {
            let t_a = list[a].dep.unwrap_or(0);
            let t_b = list[b].arr.unwrap_or(t_a).max(t_a);
            // Cumulative weights over a..=b (relative to a).
            let mut w = vec![0.0f64; b - a + 1];
            let shape_ok = (a..=b).all(|i| list[i].st.shape_dist_traveled.is_some())
                && (a..b).all(|i| {
                    list[i + 1].st.shape_dist_traveled.unwrap_or(0.0)
                        >= list[i].st.shape_dist_traveled.unwrap_or(0.0)
                })
                && list[b].st.shape_dist_traveled > list[a].st.shape_dist_traveled;
            let coord = |i: usize| -> Option<(f64, f64)> {
                let s = stops.get(list[i].st.stop_idx as usize)?;
                Some((s.lat?, s.lon?))
            };
            let mut filled = false;
            if shape_ok {
                let base = list[a].st.shape_dist_traveled.unwrap_or(0.0) as f64;
                for (k, i) in (a..=b).enumerate() {
                    w[k] = list[i].st.shape_dist_traveled.unwrap_or(0.0) as f64 - base;
                }
                filled = true;
            } else if (a..=b).all(|i| coord(i).is_some()) {
                let mut acc = 0.0;
                for (k, i) in (a + 1..=b).enumerate() {
                    acc += haversine_m(coord(i - 1).unwrap(), coord(i).unwrap());
                    w[k + 1] = acc;
                }
                filled = acc > 0.0;
            }
            if !filled {
                for (k, wk) in w.iter_mut().enumerate() {
                    *wk = k as f64;
                }
            }
            let total = w[b - a];
            for i in a + 1..b {
                let frac = if total > 0.0 { w[i - a] / total } else { 0.0 };
                let t = t_a + ((t_b - t_a) as f64 * frac).round() as u32;
                list[i].arr = Some(t);
                list[i].dep = Some(t);
                list[i].st.timepoint = 0;
                interpolated += 1;
            }
        }
        a = b;
    }
    for r in list.iter_mut() {
        // Both are Some here: endpoints checked above, gaps filled.
        let arr = r.arr.or(r.dep).unwrap_or(0);
        let dep = r.dep.or(r.arr).unwrap_or(arr);
        r.st.arrival_s = arr;
        r.st.departure_s = dep;
    }
    Some(interpolated)
}

/// Load a GTFS zip from bytes into a namespaced FeedStaticBundle.
pub fn load_gtfs_bytes(feed_id: &str, bytes: &[u8]) -> Result<FeedStaticBundle> {
    load_gtfs_bytes_with_horizon(feed_id, bytes, None)
}

/// Like [`load_gtfs_bytes`], but when `static_horizon_days` is `Some(n)`, only
/// keep trips whose `service_id` is active on at least one day in
/// `[today - 1, today + n]` (inclusive, "today" in the feed's agency
/// timezone; the extra day keeps yesterday's after-midnight trips).
/// Reduces packed RAM for huge feeds.
pub fn load_gtfs_bytes_with_horizon(
    feed_id: &str,
    bytes: &[u8],
    static_horizon_days: Option<u32>,
) -> Result<FeedStaticBundle> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let sha256 = hex::encode(hasher.finalize());

    let cursor = Cursor::new(bytes.to_vec());
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| TransitError::Parse(format!("zip: {e}")))?;

    let stops_csv = read_zip_entry(&mut archive, "stops.txt")?;
    let routes_csv = read_zip_entry(&mut archive, "routes.txt")?;
    let trips_csv = read_zip_entry(&mut archive, "trips.txt")?;
    let stop_times_csv = read_zip_entry(&mut archive, "stop_times.txt")?;
    let calendar_csv = optional_zip_entry(&mut archive, "calendar.txt");
    let calendar_dates_csv = optional_zip_entry(&mut archive, "calendar_dates.txt");
    let transfers_csv = optional_zip_entry(&mut archive, "transfers.txt");
    let agency_csv = optional_zip_entry(&mut archive, "agency.txt");
    let feed_info_csv = optional_zip_entry(&mut archive, "feed_info.txt");
    let shapes_csv = optional_zip_entry(&mut archive, "shapes.txt");
    let frequencies_csv = optional_zip_entry(&mut archive, "frequencies.txt");
    let fare_attributes_csv = optional_zip_entry(&mut archive, "fare_attributes.txt");
    let fare_rules_csv = optional_zip_entry(&mut archive, "fare_rules.txt");
    let levels_csv = optional_zip_entry(&mut archive, "levels.txt");
    let pathways_csv = optional_zip_entry(&mut archive, "pathways.txt");
    let object_codes_csv = optional_zip_entry(&mut archive, "object_codes_extension.txt");

    // Build calendar early so we can drop trips outside the static horizon
    // before materializing stop_times (main RAM cost for IDFM-scale feeds).
    let mut calendar = ServiceCalendar::new();
    if let Some(cal) = calendar_csv.as_ref() {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(cal.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let sid = rec.get("service_id").cloned().unwrap_or_default();
            let start = rec
                .get("start_date")
                .and_then(|s| parse_date(s))
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
            let end = rec
                .get("end_date")
                .and_then(|s| parse_date(s))
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(2099, 12, 31).unwrap());
            let flag = |k: &str| rec.get(k).map(|s| s.trim() == "1").unwrap_or(false);
            calendar.add_regular(
                sid,
                start,
                end,
                flag("monday"),
                flag("tuesday"),
                flag("wednesday"),
                flag("thursday"),
                flag("friday"),
                flag("saturday"),
                flag("sunday"),
            );
        }
    }
    if let Some(cd) = calendar_dates_csv.as_ref() {
        let mut bad_exceptions = 0usize;
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(cd.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let sid = rec.get("service_id").cloned().unwrap_or_default();
            let date = match rec.get("date").and_then(|s| parse_date(s)) {
                Some(d) => d,
                None => continue,
            };
            // Missing / unparseable / out-of-range exception_type: skip the
            // row rather than guessing "added" (which would run the service).
            let et = match rec
                .get("exception_type")
                .and_then(|s| s.trim().parse::<u8>().ok())
            {
                Some(et @ (1 | 2)) => et,
                _ => {
                    bad_exceptions += 1;
                    continue;
                }
            };
            calendar.add_exception(sid, date, et);
        }
        if bad_exceptions > 0 {
            warn!(
                feed_id = %feed_id,
                rows = bad_exceptions,
                "calendar_dates: skipped rows with missing/invalid exception_type"
            );
        }
    }

    let active_services: Option<std::collections::HashSet<String>> = static_horizon_days.map(|n| {
        // "Today" in the feed's local timezone, not UTC: around local
        // midnight the UTC date is off by one. Start the window one day
        // earlier so yesterday's after-midnight (> 24:00) trips survive.
        let tz = feed_timezone(agency_csv.as_deref());
        let today = Utc::now().with_timezone(&tz).date_naive();
        let start = today.pred_opt().unwrap_or(today);
        let end = today
            .checked_add_days(chrono::Days::new(n as u64))
            .unwrap_or(today);
        let set = calendar.services_active_in_range(start, end);
        info!(
            feed_id = %feed_id,
            horizon_days = n,
            timezone = %tz,
            from = %start,
            to = %end,
            active_services = set.len(),
            "static horizon: filtering trips to services active in window"
        );
        set
    });

    let mut stops = Vec::new();
    let mut stop_id_to_idx: HashMap<String, u32> = HashMap::new();
    let mut raw_to_ns_stop: HashMap<String, String> = HashMap::new();

    {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(stops_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let rec = match rec {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, "skip stop row");
                    continue;
                }
            };
            let raw_id = rec.get("stop_id").cloned().unwrap_or_default();
            if raw_id.is_empty() {
                continue;
            }
            let id = ns(feed_id, &raw_id);
            let parent_raw = rec.get("parent_station").cloned().unwrap_or_default();
            let parent_id = if parent_raw.is_empty() {
                None
            } else {
                Some(ns(feed_id, &parent_raw))
            };
            let lat = rec
                .get("stop_lat")
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| v.abs() > 0.0 || rec.contains_key("stop_lon"));
            let lon = rec.get("stop_lon").and_then(|s| s.parse::<f64>().ok());
            let location_type = rec
                .get("location_type")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u8);
            let wheelchair = rec
                .get("wheelchair_boarding")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u8);
            let platform_code = rec.get("platform_code").cloned().filter(|s| !s.is_empty());
            let name = rec
                .get("stop_name")
                .cloned()
                .unwrap_or_else(|| raw_id.clone());

            let idx = stops.len() as u32;
            stop_id_to_idx.insert(id.clone(), idx);
            raw_to_ns_stop.insert(raw_id.clone(), id.clone());
            let stop_code = rec.get("stop_code").cloned().filter(|s| !s.is_empty());
            let stop_desc = rec.get("stop_desc").cloned().filter(|s| !s.is_empty());
            let level_id = rec
                .get("level_id")
                .cloned()
                .filter(|s| !s.is_empty())
                .map(|raw| ns(feed_id, &raw));
            // Namespace zone_id so fare_rules origin_id/destination_id/contains_id match.
            let zone_id = rec
                .get("zone_id")
                .cloned()
                .filter(|s| !s.is_empty())
                .map(|raw| ns(feed_id, &raw));
            let stop_url = rec.get("stop_url").cloned().filter(|s| !s.is_empty());
            let stop_timezone = rec.get("stop_timezone").cloned().filter(|s| !s.is_empty());
            stops.push(StopRecord {
                id,
                feed_id: feed_id.to_string(),
                raw_id,
                name,
                lat,
                lon,
                parent_id,
                location_type,
                platform_code,
                wheelchair,
                stop_code,
                stop_desc,
                level_id,
                zone_id,
                stop_url,
                stop_timezone,
            });
        }
    }

    let mut routes: HashMap<String, RouteRecord> = HashMap::new();
    {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(routes_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_id = rec.get("route_id").cloned().unwrap_or_default();
            if raw_id.is_empty() {
                continue;
            }
            let id = ns(feed_id, &raw_id);
            let route_type = rec
                .get("route_type")
                .and_then(|s| s.parse().ok())
                .unwrap_or(3);
            let color = rec.get("route_color").cloned().filter(|s| !s.is_empty());
            let text_color = rec
                .get("route_text_color")
                .cloned()
                .filter(|s| !s.is_empty());
            let desc = rec.get("route_desc").cloned().filter(|s| !s.is_empty());
            let url = rec.get("route_url").cloned().filter(|s| !s.is_empty());
            routes.insert(
                id.clone(),
                RouteRecord {
                    id,
                    feed_id: feed_id.to_string(),
                    short_name: rec.get("route_short_name").cloned().unwrap_or_default(),
                    long_name: rec.get("route_long_name").cloned().unwrap_or_default(),
                    mode: RouteMode::from_gtfs_route_type(route_type),
                    agency_id: rec.get("agency_id").cloned().filter(|s| !s.is_empty()),
                    color,
                    text_color,
                    route_type_raw: route_type,
                    desc,
                    url,
                },
            );
        }
    }

    let mut agencies: HashMap<String, AgencyRecord> = HashMap::new();
    if let Some(agency_csv) = agency_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(agency_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let id = rec
                .get("agency_id")
                .cloned()
                .unwrap_or_else(|| "default".into());
            let name = rec.get("agency_name").cloned().unwrap_or_default();
            agencies.insert(
                id.clone(),
                AgencyRecord {
                    id,
                    name,
                    url: rec.get("agency_url").cloned().filter(|s| !s.is_empty()),
                    timezone: rec
                        .get("agency_timezone")
                        .cloned()
                        .filter(|s| !s.is_empty()),
                    phone: rec.get("agency_phone").cloned().filter(|s| !s.is_empty()),
                },
            );
        }
    }

    // trips: temporary map raw_trip -> meta, then fill stop times
    #[derive(Clone)]
    struct TripMeta {
        route_id: String,
        service_id: String,
        headsign: Option<String>,
        direction_id: Option<u8>,
        short_name: Option<String>,
        wheelchair: u8,
        bikes_allowed: u8,
        block_id: Option<String>,
        shape_id: Option<String>,
    }
    let mut trip_meta: HashMap<String, TripMeta> = HashMap::new();
    {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(trips_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_trip = rec.get("trip_id").cloned().unwrap_or_default();
            if raw_trip.is_empty() {
                continue;
            }
            let raw_route = rec.get("route_id").cloned().unwrap_or_default();
            let shape_id = rec
                .get("shape_id")
                .cloned()
                .filter(|s| !s.is_empty())
                .map(|s| ns(feed_id, &s));
            let block_id = rec.get("block_id").cloned().filter(|s| !s.is_empty());
            let service_id = rec.get("service_id").cloned().unwrap_or_default();
            if let Some(ref active) = active_services {
                if !active.contains(&service_id) {
                    continue;
                }
            }
            trip_meta.insert(
                raw_trip,
                TripMeta {
                    route_id: ns(feed_id, &raw_route),
                    service_id,
                    headsign: rec.get("trip_headsign").cloned().filter(|s| !s.is_empty()),
                    direction_id: rec.get("direction_id").and_then(|s| s.parse().ok()),
                    short_name: rec
                        .get("trip_short_name")
                        .cloned()
                        .filter(|s| !s.is_empty()),
                    wheelchair: rec
                        .get("wheelchair_accessible")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0u8),
                    bikes_allowed: rec
                        .get("bikes_allowed")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0u8),
                    block_id,
                    shape_id,
                },
            );
        }
    }
    if active_services.is_some() {
        info!(
            feed_id = %feed_id,
            kept_trips = trip_meta.len(),
            "static horizon: trips retained after service filter"
        );
    }

    // Intern stop_headsign strings; index 0 = none.
    let mut headsign_pool: Vec<String> = vec![String::new()];
    let mut headsign_to_idx: HashMap<String, u32> = HashMap::new();

    // Group stop_times by trip_id preserving sequence (skip trips dropped by horizon)
    let mut times_by_trip: HashMap<String, Vec<RawStopTime>> = HashMap::new();
    let mut bad_seq_rows = 0usize;
    let mut bad_time_rows = 0usize;
    {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(stop_times_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_trip = rec.get("trip_id").cloned().unwrap_or_default();
            let raw_stop = rec.get("stop_id").cloned().unwrap_or_default();
            if raw_trip.is_empty() || raw_stop.is_empty() {
                continue;
            }
            if !trip_meta.contains_key(&raw_trip) {
                continue;
            }
            let stop_ns = ns(feed_id, &raw_stop);
            let Some(&stop_idx) = stop_id_to_idx.get(&stop_ns) else {
                continue;
            };
            // GTFS: arrival/departure may be empty at non-timepoint stops;
            // keep them missing here and interpolate once the trip is sorted.
            // A non-empty but malformed time is treated as missing too.
            let mut time_field = |k: &str| -> Option<u32> {
                let raw = rec.get(k).map(|s| s.trim()).unwrap_or("");
                if raw.is_empty() {
                    return None;
                }
                let t = parse_time_to_seconds(raw);
                if t.is_none() {
                    bad_time_rows += 1;
                }
                t
            };
            let dep_raw = time_field("departure_time");
            let arr_raw = time_field("arrival_time");
            // Required field: a parse failure must not silently become 0
            // (that would reorder the trip) — drop the row instead.
            let Some(seq) = rec
                .get("stop_sequence")
                .and_then(|s| s.trim().parse::<u32>().ok())
            else {
                bad_seq_rows += 1;
                continue;
            };
            let pickup_type = rec
                .get("pickup_type")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u8);
            let drop_off_type = rec
                .get("drop_off_type")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u8);
            let stop_headsign_idx = rec
                .get("stop_headsign")
                .cloned()
                .filter(|s| !s.is_empty())
                .map(|hs| {
                    if let Some(&i) = headsign_to_idx.get(&hs) {
                        i
                    } else {
                        let i = headsign_pool.len() as u32;
                        headsign_pool.push(hs.clone());
                        headsign_to_idx.insert(hs, i);
                        i
                    }
                })
                .unwrap_or(0);
            // GTFS: omitted timepoint means exact (1).
            let timepoint = rec
                .get("timepoint")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(1u8);
            let shape_dist_traveled = rec.get("shape_dist_traveled").and_then(|s| {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    t.parse::<f32>().ok()
                }
            });
            times_by_trip
                .entry(raw_trip)
                .or_default()
                .push(RawStopTime {
                    arr: arr_raw.or(dep_raw),
                    dep: dep_raw.or(arr_raw),
                    st: PackedStopTime {
                        stop_idx,
                        arrival_s: 0,
                        departure_s: 0,
                        stop_sequence: seq,
                        pickup_type,
                        drop_off_type,
                        stop_headsign_idx,
                        timepoint,
                        shape_dist_traveled,
                    },
                });
        }
    }
    if bad_seq_rows > 0 {
        warn!(
            feed_id = %feed_id,
            rows = bad_seq_rows,
            "stop_times: skipped rows with missing/invalid stop_sequence"
        );
    }
    if bad_time_rows > 0 {
        warn!(
            feed_id = %feed_id,
            fields = bad_time_rows,
            "stop_times: malformed arrival/departure times treated as missing"
        );
    }

    let mut stop_times: Vec<PackedStopTime> = Vec::new();
    let mut trips: Vec<TripRecord> = Vec::new();
    let mut trip_id_to_idx: HashMap<String, u32> = HashMap::new();

    // Deterministic trip order — HashMap iteration is randomly seeded per
    // process, which would otherwise shuffle `epoch.trips`, breaking FLASH-TB
    // disk-cache shape validation and run-to-run reproducibility.
    let mut ordered_trips: Vec<(String, Vec<RawStopTime>)> = times_by_trip.into_iter().collect();
    ordered_trips.sort_by(|a, b| a.0.cmp(&b.0));

    let mut dropped_untimed_trips = 0usize;
    let mut interpolated_stop_times = 0usize;
    for (raw_trip, mut list) in ordered_trips {
        list.sort_by_key(|r| r.st.stop_sequence);
        let Some(meta) = trip_meta.get(&raw_trip) else {
            continue;
        };
        let Some(n_interp) = fill_missing_times(&mut list, &stops) else {
            dropped_untimed_trips += 1;
            continue;
        };
        interpolated_stop_times += n_interp;
        let start = stop_times.len() as u32;
        let len = list.len() as u32;
        for r in list {
            stop_times.push(r.st);
        }
        let id = ns(feed_id, &raw_trip);
        let idx = trips.len() as u32;
        trip_id_to_idx.insert(id.clone(), idx);
        trips.push(TripRecord {
            id,
            feed_id: feed_id.to_string(),
            route_id: meta.route_id.clone(),
            service_id: meta.service_id.clone(),
            headsign: meta.headsign.clone(),
            direction_id: meta.direction_id,
            short_name: meta.short_name.clone(),
            wheelchair: meta.wheelchair,
            bikes_allowed: meta.bikes_allowed,
            block_id: meta.block_id.clone(),
            shape_id: meta.shape_id.clone(),
            stop_time_start: start,
            stop_time_len: len,
        });
    }

    if dropped_untimed_trips > 0 || interpolated_stop_times > 0 {
        info!(
            feed_id = %feed_id,
            dropped_trips = dropped_untimed_trips,
            interpolated_stop_times,
            "stop_times: interpolated blank times; dropped trips without first/last time"
        );
    }

    let mut stop_departures: Vec<Vec<(u32, u32)>> = vec![Vec::new(); stops.len()];
    for (trip_idx, trip) in trips.iter().enumerate() {
        for off in 0..trip.stop_time_len {
            let st = &stop_times[(trip.stop_time_start + off) as usize];
            if st.pickup_type != 1 {
                stop_departures[st.stop_idx as usize].push((trip_idx as u32, off));
            }
        }
    }

    // `calendar` already built above (before trip / stop_times filter).

    let mut transfers = Vec::new();
    if let Some(tr) = transfers_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(tr.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let from = ns(
                feed_id,
                rec.get("from_stop_id").map(|s| s.as_str()).unwrap_or(""),
            );
            let to = ns(
                feed_id,
                rec.get("to_stop_id").map(|s| s.as_str()).unwrap_or(""),
            );
            let Some(&from_idx) = stop_id_to_idx.get(&from) else {
                continue;
            };
            let Some(&to_idx) = stop_id_to_idx.get(&to) else {
                continue;
            };
            let min_s = rec
                .get("min_transfer_time")
                .and_then(|s| s.parse().ok())
                .unwrap_or(120u32);
            let transfer_type = rec
                .get("transfer_type")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u8);
            transfers.push(TransferEdge {
                from_stop_idx: from_idx,
                to_stop_idx: to_idx,
                min_transfer_s: min_s,
                transfer_type,
            });
        }
    }

    let mut feed_start_date = None;
    let mut feed_end_date = None;
    let mut feed_publisher_name = None;
    let mut feed_lang = None;
    if let Some(fi) = feed_info_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(fi.as_bytes());
        if let Some(Ok(rec)) = rdr.deserialize::<HashMap<String, String>>().next() {
            feed_start_date = rec
                .get("feed_start_date")
                .cloned()
                .filter(|s| !s.trim().is_empty());
            feed_end_date = rec
                .get("feed_end_date")
                .cloned()
                .filter(|s| !s.trim().is_empty());
            feed_publisher_name = rec
                .get("feed_publisher_name")
                .cloned()
                .filter(|s| !s.trim().is_empty());
            feed_lang = rec
                .get("feed_lang")
                .cloned()
                .filter(|s| !s.trim().is_empty());
        }
    }

    // frequencies.txt: trip_id, start_time, end_time, headway_secs
    let mut frequencies: HashMap<String, Vec<FrequencyWindow>> = HashMap::new();
    if let Some(freq_csv) = frequencies_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(freq_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_trip = rec.get("trip_id").cloned().unwrap_or_default();
            if raw_trip.is_empty() {
                continue;
            }
            let Some(start_s) = rec.get("start_time").and_then(|s| parse_time_to_seconds(s)) else {
                continue;
            };
            let Some(end_s) = rec.get("end_time").and_then(|s| parse_time_to_seconds(s)) else {
                continue;
            };
            let Some(headway_s) = rec
                .get("headway_secs")
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|&h| h > 0)
            else {
                continue;
            };
            let id = ns(feed_id, &raw_trip);
            frequencies.entry(id).or_default().push(FrequencyWindow {
                start_s,
                end_s,
                headway_s,
            });
        }
        for wins in frequencies.values_mut() {
            wins.sort_by_key(|w| (w.start_s, w.end_s, w.headway_s));
        }
    }

    // shapes.txt: shape_id, shape_pt_lat, shape_pt_lon, shape_pt_sequence
    // Stored as namespaced id → ordered (lat, lon) for Leaflet.
    let mut shapes: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
    if let Some(shapes_csv) = shapes_csv {
        // Collect with sequence then sort per shape.
        let mut raw: HashMap<String, Vec<(u32, f64, f64)>> = HashMap::new();
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(shapes_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_id = rec.get("shape_id").cloned().unwrap_or_default();
            if raw_id.is_empty() {
                continue;
            }
            let lat = match rec.get("shape_pt_lat").and_then(|s| s.parse::<f64>().ok()) {
                Some(v) => v,
                None => continue,
            };
            let lon = match rec.get("shape_pt_lon").and_then(|s| s.parse::<f64>().ok()) {
                Some(v) => v,
                None => continue,
            };
            let seq = rec
                .get("shape_pt_sequence")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u32);
            raw.entry(raw_id).or_default().push((seq, lat, lon));
        }
        for (raw_id, mut pts) in raw {
            pts.sort_by_key(|(seq, _, _)| *seq);
            let id = ns(feed_id, &raw_id);
            shapes.insert(
                id,
                pts.into_iter().map(|(_, lat, lon)| (lat, lon)).collect(),
            );
        }
    }

    // levels.txt (optional indoor floors)
    let mut levels: HashMap<String, LevelRecord> = HashMap::new();
    if let Some(lv_csv) = levels_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(lv_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_id = rec.get("level_id").cloned().unwrap_or_default();
            if raw_id.is_empty() {
                continue;
            }
            let level_index = rec
                .get("level_index")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
            let level_name = rec.get("level_name").cloned().filter(|s| !s.is_empty());
            let id = ns(feed_id, &raw_id);
            levels.insert(
                id.clone(),
                LevelRecord {
                    id,
                    feed_id: feed_id.to_string(),
                    raw_id,
                    level_index,
                    level_name,
                },
            );
        }
    }

    // pathways.txt (optional indoor / station transfer graph)
    let mut pathways: Vec<PathwayRecord> = Vec::new();
    if let Some(pw_csv) = pathways_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(pw_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let pathway_id = rec.get("pathway_id").cloned().unwrap_or_default();
            let from_raw = rec.get("from_stop_id").cloned().unwrap_or_default();
            let to_raw = rec.get("to_stop_id").cloned().unwrap_or_default();
            if from_raw.is_empty() || to_raw.is_empty() {
                continue;
            }
            let from_ns = ns(feed_id, &from_raw);
            let to_ns = ns(feed_id, &to_raw);
            let Some(&from_stop_idx) = stop_id_to_idx.get(&from_ns) else {
                continue;
            };
            let Some(&to_stop_idx) = stop_id_to_idx.get(&to_ns) else {
                continue;
            };
            let pathway_mode = rec
                .get("pathway_mode")
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(1u8);
            let is_bidirectional = rec
                .get("is_bidirectional")
                .and_then(|s| s.trim().parse::<u8>().ok())
                .unwrap_or(0)
                == 1;
            let length_m = rec.get("length").and_then(|s| s.trim().parse::<f64>().ok());
            let traversal_time_s = rec
                .get("traversal_time")
                .and_then(|s| s.trim().parse::<u32>().ok());
            let stair_count = rec
                .get("stair_count")
                .and_then(|s| s.trim().parse::<i32>().ok());
            let max_slope = rec
                .get("max_slope")
                .and_then(|s| s.trim().parse::<f64>().ok());
            let min_width = rec
                .get("min_width")
                .and_then(|s| s.trim().parse::<f64>().ok());
            let signposted_as = rec.get("signposted_as").cloned().filter(|s| !s.is_empty());
            let reversed_signposted_as = rec
                .get("reversed_signposted_as")
                .cloned()
                .filter(|s| !s.is_empty());
            pathways.push(PathwayRecord {
                pathway_id: if pathway_id.is_empty() {
                    format!("{from_raw}->{to_raw}")
                } else {
                    pathway_id
                },
                from_stop_idx,
                to_stop_idx,
                pathway_mode,
                is_bidirectional,
                length_m,
                traversal_time_s,
                stair_count,
                max_slope,
                min_width,
                signposted_as,
                reversed_signposted_as,
            });
        }
    }

    // fare_attributes.txt (optional; often incomplete in French national data)
    let mut fares: Vec<FareAttribute> = Vec::new();
    if let Some(fa_csv) = fare_attributes_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(fa_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_id = rec.get("fare_id").cloned().unwrap_or_default();
            if raw_id.is_empty() {
                continue;
            }
            let Some(price) = rec.get("price").and_then(|s| s.trim().parse::<f64>().ok()) else {
                continue;
            };
            let currency_type = rec
                .get("currency_type")
                .cloned()
                .unwrap_or_default()
                .trim()
                .to_string();
            if currency_type.is_empty() {
                continue;
            }
            let payment_method = rec
                .get("payment_method")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0u8);
            let transfers = rec.get("transfers").and_then(|s| {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    t.parse::<u8>().ok()
                }
            });
            let transfer_duration = rec
                .get("transfer_duration")
                .and_then(|s| s.trim().parse::<u32>().ok());
            fares.push(FareAttribute {
                fare_id: ns(feed_id, &raw_id),
                price,
                currency_type,
                payment_method,
                transfers,
                transfer_duration,
            });
        }
    }

    // fare_rules.txt (optional)
    let mut fare_rules: Vec<FareRule> = Vec::new();
    if let Some(fr_csv) = fare_rules_csv {
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(fr_csv.as_bytes());
        for rec in rdr.deserialize::<HashMap<String, String>>() {
            let Ok(rec) = rec else { continue };
            let raw_fare = rec.get("fare_id").cloned().unwrap_or_default();
            if raw_fare.is_empty() {
                continue;
            }
            let opt_ns = |key: &str| -> Option<String> {
                rec.get(key)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .map(|s| ns(feed_id, &s))
            };
            fare_rules.push(FareRule {
                fare_id: ns(feed_id, &raw_fare),
                route_id: opt_ns("route_id"),
                origin_id: opt_ns("origin_id"),
                destination_id: opt_ns("destination_id"),
                contains_id: opt_ns("contains_id"),
            });
        }
    }

    crate::routing::geometry::backfill_shape_dist_traveled(
        &trips,
        &mut stop_times,
        &stops,
        &shapes,
    );

    let siri_trip_aliases = if let Some(ref oc) = object_codes_csv {
        let kept: std::collections::HashSet<String> = trip_meta.keys().cloned().collect();
        let aliases = build_from_object_codes(oc, feed_id, &kept);
        info!(
            feed_id,
            alias_keys = aliases.to_trip_id.len(),
            "loaded SIRI trip aliases from object_codes_extension"
        );
        aliases
    } else {
        SiriTripAliases::default()
    };

    info!(
        feed_id,
        stops = stops.len(),
        trips = trips.len(),
        stop_times = stop_times.len(),
        routes = routes.len(),
        shapes = shapes.len(),
        pathways = pathways.len(),
        levels = levels.len(),
        fares = fares.len(),
        fare_rules = fare_rules.len(),
        "loaded GTFS feed"
    );

    // silence unused
    let _ = RouteMode::Bus;
    let _ = raw_to_ns_stop;

    Ok(FeedStaticBundle {
        feed_id: feed_id.to_string(),
        stops,
        stop_id_to_idx,
        routes,
        trips,
        trip_id_to_idx,
        stop_times,
        headsign_pool,
        stop_departures,
        frequencies,
        calendar,
        transfers,
        pathways,
        levels,
        agencies,
        shapes,
        fares,
        fare_rules,
        sha256,
        loaded_at: Utc::now(),
        feed_start_date,
        feed_end_date,
        feed_publisher_name,
        feed_lang,
        siri_trip_aliases,
    })
}

pub fn load_gtfs_zip(feed_id: &str, path: &Path) -> Result<FeedStaticBundle> {
    let bytes = std::fs::read(path)?;
    load_gtfs_bytes(feed_id, &bytes)
}

/// Load a GTFS zip from path with optional static horizon (see
/// [`load_gtfs_bytes_with_horizon`]).
pub fn load_gtfs_zip_with_horizon(
    feed_id: &str,
    path: &Path,
    static_horizon_days: Option<u32>,
) -> Result<FeedStaticBundle> {
    let bytes = std::fs::read(path)?;
    load_gtfs_bytes_with_horizon(feed_id, &bytes, static_horizon_days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn tiny_gtfs_zip() -> Vec<u8> {
        let buf = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buf);
        let opts = SimpleFileOptions::default();

        zip.start_file("agency.txt", opts).unwrap();
        zip.write_all(
            b"agency_id,agency_name,agency_url,agency_timezone,agency_phone\n\
              a1,Test Agency,https://example.test,Europe/Paris,+33100000000\n",
        )
        .unwrap();

        zip.start_file("stops.txt", opts).unwrap();
        zip.write_all(
            b"stop_id,stop_name,stop_lat,stop_lon,location_type,parent_station,stop_code,stop_desc,zone_id,stop_url,stop_timezone\n\
              A,Station A,48.86,2.35,1,,GA001,Main hall,Z1,https://stops.example/A,Europe/Paris\n\
              A1,Platform A1,48.86,2.35,0,A,GA001-1,,Z1,,\n\
              B,Station B,45.76,4.86,1,,GB001,,Z2,,\n\
              B1,Platform B1,45.76,4.86,0,B,,,,,\n",
        )
        .unwrap();

        zip.start_file("routes.txt", opts).unwrap();
        zip.write_all(
            b"route_id,route_short_name,route_long_name,route_type,agency_id,route_color,route_text_color,route_desc,route_url\n\
              r1,TGV,Paris-Lyon,2,a1,FF0000,FFFFFF,High-speed rail,https://routes.example/r1\n",
        )
        .unwrap();

        zip.start_file("calendar.txt", opts).unwrap();
        zip.write_all(
            b"service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
              S1,1,1,1,1,1,1,1,20260101,20261231\n",
        )
        .unwrap();

        zip.start_file("trips.txt", opts).unwrap();
        zip.write_all(
            b"route_id,service_id,trip_id,trip_headsign,trip_short_name,direction_id,wheelchair_accessible,bikes_allowed,shape_id,block_id\n\
              r1,S1,t1,Lyon,6611,0,1,2,sh1,BLK1\n",
        )
        .unwrap();

        zip.start_file("stop_times.txt", opts).unwrap();
        zip.write_all(
            b"trip_id,arrival_time,departure_time,stop_id,stop_sequence,stop_headsign,timepoint,shape_dist_traveled\n\
              t1,08:00:00,08:00:00,A1,1,Vers Lyon,1,0.0\n\
              t1,10:00:00,10:00:00,B1,2,,0,462.5\n",
        )
        .unwrap();

        zip.start_file("frequencies.txt", opts).unwrap();
        zip.write_all(
            b"trip_id,start_time,end_time,headway_secs\n\
              t1,06:00:00,12:00:00,600\n",
        )
        .unwrap();

        zip.start_file("shapes.txt", opts).unwrap();
        zip.write_all(
            b"shape_id,shape_pt_lat,shape_pt_lon,shape_pt_sequence\n\
              sh1,48.86,2.35,1\n\
              sh1,45.76,4.86,2\n",
        )
        .unwrap();

        zip.start_file("feed_info.txt", opts).unwrap();
        zip.write_all(
            b"feed_publisher_name,feed_publisher_url,feed_lang,feed_start_date,feed_end_date\n\
              Test Publisher,https://feed.example,fr,20260101,20261231\n",
        )
        .unwrap();

        zip.start_file("fare_attributes.txt", opts).unwrap();
        zip.write_all(
            b"fare_id,price,currency_type,payment_method,transfers,transfer_duration\n\
              flat,2.50,EUR,1,0,\n\
              multi,4.00,EUR,1,2,5400\n",
        )
        .unwrap();

        zip.start_file("fare_rules.txt", opts).unwrap();
        zip.write_all(
            b"fare_id,route_id,origin_id,destination_id,contains_id\n\
              flat,r1,,,\n\
              multi,,,,\n",
        )
        .unwrap();

        let cursor = zip.finish().unwrap();
        cursor.into_inner()
    }

    /// Two platforms + pathway with traversal_time=90 → walk edge 90s.
    fn pathway_gtfs_zip() -> Vec<u8> {
        let buf = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buf);
        let opts = SimpleFileOptions::default();

        zip.start_file("agency.txt", opts).unwrap();
        zip.write_all(
            b"agency_id,agency_name,agency_url,agency_timezone\n\
              a1,Agency,https://ex.test,Europe/Paris\n",
        )
        .unwrap();

        zip.start_file("stops.txt", opts).unwrap();
        zip.write_all(
            b"stop_id,stop_name,stop_lat,stop_lon,location_type,parent_station,level_id\n\
              S,Hub,48.85,2.35,1,,L0\n\
              P1,Platform 1,48.85,2.35,0,S,L0\n\
              P2,Platform 2,48.85,2.35,0,S,L1\n",
        )
        .unwrap();

        zip.start_file("levels.txt", opts).unwrap();
        zip.write_all(
            b"level_id,level_index,level_name\n\
              L0,0,Concourse\n\
              L1,-1,Platforms\n",
        )
        .unwrap();

        zip.start_file("pathways.txt", opts).unwrap();
        zip.write_all(
            b"pathway_id,from_stop_id,to_stop_id,pathway_mode,is_bidirectional,traversal_time,length\n\
              pw1,P1,P2,2,1,90,40\n",
        )
        .unwrap();

        zip.start_file("routes.txt", opts).unwrap();
        zip.write_all(
            b"route_id,route_short_name,route_long_name,route_type,agency_id\n\
              r1,M1,Metro 1,1,a1\n",
        )
        .unwrap();

        zip.start_file("calendar.txt", opts).unwrap();
        zip.write_all(
            b"service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
              S1,1,1,1,1,1,1,1,20260101,20261231\n",
        )
        .unwrap();

        zip.start_file("trips.txt", opts).unwrap();
        zip.write_all(
            b"route_id,service_id,trip_id,trip_headsign\n\
              r1,S1,t1,End\n",
        )
        .unwrap();

        zip.start_file("stop_times.txt", opts).unwrap();
        zip.write_all(
            b"trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
              t1,08:00:00,08:00:00,P1,1\n\
              t1,08:05:00,08:05:00,P2,2\n",
        )
        .unwrap();

        let cursor = zip.finish().unwrap();
        cursor.into_inner()
    }

    #[test]
    fn parse_pathways_and_levels_walk_edge() {
        let bytes = pathway_gtfs_zip();
        let bundle = load_gtfs_bytes("pw", &bytes).unwrap();

        assert_eq!(bundle.levels.len(), 2);
        let l0 = bundle.levels.get("pw:L0").expect("level L0");
        assert_eq!(l0.level_index, 0.0);
        assert_eq!(l0.level_name.as_deref(), Some("Concourse"));
        let l1 = bundle.levels.get("pw:L1").expect("level L1");
        assert_eq!(l1.level_index, -1.0);

        let p1 = bundle.stops.iter().find(|s| s.raw_id == "P1").unwrap();
        let p2 = bundle.stops.iter().find(|s| s.raw_id == "P2").unwrap();
        assert_eq!(p1.level_id.as_deref(), Some("pw:L0"));
        assert_eq!(p2.level_id.as_deref(), Some("pw:L1"));

        assert_eq!(bundle.pathways.len(), 1);
        let pw = &bundle.pathways[0];
        assert_eq!(pw.pathway_mode, 2);
        assert!(pw.is_bidirectional);
        assert_eq!(pw.traversal_time_s, Some(90));
        assert_eq!(pw.duration_s(), 90);
        assert_eq!(pw.mode_name(), "stairs");

        let epoch = super::super::pack::build_epoch(vec![std::sync::Arc::new(bundle)], vec![]);
        let p1_idx = *epoch.stop_id_to_idx.get("pw:P1").unwrap();
        let p2_idx = *epoch.stop_id_to_idx.get("pw:P2").unwrap();

        let edges: Vec<_> = epoch
            .walk_edges
            .iter()
            .filter(|e| {
                (e.from_stop_idx == p1_idx && e.to_stop_idx == p2_idx)
                    || (e.from_stop_idx == p2_idx && e.to_stop_idx == p1_idx)
            })
            .collect();
        assert!(
            edges.iter().any(|e| e.duration_s == 90),
            "expected walk edge with traversal_time 90s, got {:?}",
            edges
        );
        // Bidirectional pathway ⇒ both directions; no 120s parent-default for this pair.
        assert!(
            edges.iter().all(|e| e.duration_s == 90),
            "pathway should prefer over generic parent transfer: {:?}",
            edges
        );
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn parse_tiny() {
        let bytes = tiny_gtfs_zip();
        let bundle = load_gtfs_bytes("test", &bytes).unwrap();
        assert_eq!(bundle.stops.len(), 4);
        assert_eq!(bundle.trips.len(), 1);
        assert!(bundle.stop_id_to_idx.contains_key("test:A"));

        let stop_a = bundle
            .stops
            .iter()
            .find(|s| s.raw_id == "A")
            .expect("stop A");
        assert_eq!(stop_a.stop_code.as_deref(), Some("GA001"));
        assert_eq!(stop_a.stop_desc.as_deref(), Some("Main hall"));
        // zone_id is namespaced for fare_rules origin/destination/contains matching
        assert_eq!(stop_a.zone_id.as_deref(), Some("test:Z1"));
        assert_eq!(stop_a.stop_url.as_deref(), Some("https://stops.example/A"));
        assert_eq!(stop_a.stop_timezone.as_deref(), Some("Europe/Paris"));

        let route = bundle.routes.get("test:r1").expect("route r1");
        assert_eq!(route.color.as_deref(), Some("FF0000"));
        assert_eq!(route.text_color.as_deref(), Some("FFFFFF"));
        assert_eq!(route.route_type_raw, 2);
        assert_eq!(route.desc.as_deref(), Some("High-speed rail"));
        assert_eq!(route.url.as_deref(), Some("https://routes.example/r1"));

        let trip = &bundle.trips[0];
        assert_eq!(trip.short_name.as_deref(), Some("6611"));
        assert_eq!(trip.direction_id, Some(0));
        assert_eq!(trip.wheelchair, 1);
        assert_eq!(trip.bikes_allowed, 2);
        assert_eq!(trip.block_id.as_deref(), Some("BLK1"));
        let freqs = bundle
            .frequencies
            .get("test:t1")
            .expect("frequencies for t1");
        assert_eq!(freqs.len(), 1);
        assert_eq!(freqs[0].start_s, 6 * 3600);
        assert_eq!(freqs[0].end_s, 12 * 3600);
        assert_eq!(freqs[0].headway_s, 600);
        assert_eq!(trip.shape_id.as_deref(), Some("test:sh1"));

        let shape = bundle.shapes.get("test:sh1").expect("shape sh1");
        assert_eq!(shape.len(), 2);
        assert!((shape[0].0 - 48.86).abs() < 1e-9);
        assert!((shape[0].1 - 2.35).abs() < 1e-9);
        assert!((shape[1].0 - 45.76).abs() < 1e-9);
        assert!((shape[1].1 - 4.86).abs() < 1e-9);

        assert!(bundle.headsign_pool.len() >= 2);
        assert_eq!(bundle.headsign_pool[0], "");
        let st0 = &bundle.stop_times[trip.stop_time_start as usize];
        assert_ne!(st0.stop_headsign_idx, 0);
        assert_eq!(
            bundle.headsign_pool[st0.stop_headsign_idx as usize],
            "Vers Lyon"
        );
        assert_eq!(st0.timepoint, 1);
        assert_eq!(st0.shape_dist_traveled, Some(0.0));
        let st1 = &bundle.stop_times[(trip.stop_time_start + 1) as usize];
        assert_eq!(st1.stop_headsign_idx, 0);
        assert_eq!(st1.timepoint, 0);
        assert_eq!(st1.shape_dist_traveled, Some(462.5));

        assert_eq!(bundle.feed_start_date.as_deref(), Some("20260101"));
        assert_eq!(bundle.feed_end_date.as_deref(), Some("20261231"));
        assert_eq!(
            bundle.feed_publisher_name.as_deref(),
            Some("Test Publisher")
        );
        assert_eq!(bundle.feed_lang.as_deref(), Some("fr"));

        let agency = bundle.agencies.get("a1").expect("agency a1");
        assert_eq!(agency.name, "Test Agency");
        assert_eq!(agency.url.as_deref(), Some("https://example.test"));
        assert_eq!(agency.timezone.as_deref(), Some("Europe/Paris"));
        assert_eq!(agency.phone.as_deref(), Some("+33100000000"));

        assert_eq!(bundle.fares.len(), 2);
        let flat = bundle
            .fares
            .iter()
            .find(|f| f.fare_id == "test:flat")
            .expect("flat fare");
        assert!((flat.price - 2.50).abs() < 1e-9);
        assert_eq!(flat.currency_type, "EUR");
        assert_eq!(flat.payment_method, 1);
        assert_eq!(flat.transfers, Some(0));
        assert!(flat.transfer_duration.is_none());
        let multi = bundle
            .fares
            .iter()
            .find(|f| f.fare_id == "test:multi")
            .expect("multi fare");
        assert_eq!(multi.transfers, Some(2));
        assert_eq!(multi.transfer_duration, Some(5400));
        assert_eq!(bundle.fare_rules.len(), 2);
        let rule_flat = bundle
            .fare_rules
            .iter()
            .find(|r| r.fare_id == "test:flat")
            .expect("flat rule");
        assert_eq!(rule_flat.route_id.as_deref(), Some("test:r1"));
        assert!(rule_flat.origin_id.is_none());

        // GlobalTrip denormalized fields
        let epoch = super::super::pack::build_epoch(vec![std::sync::Arc::new(bundle)], vec![]);
        assert_eq!(epoch.fares.len(), 2);
        assert_eq!(epoch.fare_rules.len(), 2);
        let gt = &epoch.trips[0];
        assert_eq!(gt.short_name.as_deref(), Some("6611"));
        assert_eq!(gt.direction_id, Some(0));
        assert_eq!(gt.wheelchair, 1);
        assert_eq!(gt.bikes_allowed, 2);
        assert_eq!(gt.route_color.as_deref(), Some("FF0000"));
        assert_eq!(gt.route_text_color.as_deref(), Some("FFFFFF"));
        assert_eq!(gt.route_type_raw, 2);
        assert_eq!(gt.agency_name.as_deref(), Some("Test Agency"));
        assert_eq!(gt.block_id.as_deref(), Some("BLK1"));
        assert_eq!(gt.frequency_windows.len(), 1);
        assert_eq!(gt.frequency_windows[0].headway_s, 600);
        assert_eq!(gt.shape_id.as_deref(), Some("test:sh1"));
        assert!(epoch.shapes.contains_key("test:sh1"));
        let gst0 = &epoch.stop_times[gt.stop_time_start as usize];
        assert_eq!(
            epoch.headsign_pool[gst0.stop_headsign_idx as usize],
            "Vers Lyon"
        );

        // leg_geometry should be non-empty (shape slice or stop polyline).
        // from_stop_idx / to_stop_idx are offsets within the trip's stop_times.
        let geo = crate::routing::geometry::leg_geometry(&epoch, "test:t1", 0, 1);
        assert!(!geo.is_empty(), "leg_geometry should be non-empty");
        // Leaflet order: [lat, lon]
        assert!((geo[0][0] - 48.86).abs() < 0.01);
    }

    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        for (name, body) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    const BASE_STOPS: &str = "stop_id,stop_name,stop_lat,stop_lon\n\
        A,A,48.80,2.30\n\
        B,B,48.81,2.30\n\
        C,C,48.83,2.30\n\
        D,D,48.84,2.30\n";
    const BASE_ROUTES: &str = "route_id,route_short_name,route_long_name,route_type\nr1,1,One,3\n";
    const BASE_CAL: &str =
        "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
        S1,1,1,1,1,1,1,1,20260101,20261231\n";

    #[test]
    fn time_and_date_parsing_rejects_garbage() {
        assert_eq!(
            parse_time_to_seconds("08:05:30"),
            Some(8 * 3600 + 5 * 60 + 30)
        );
        assert_eq!(
            parse_time_to_seconds(" 25:30:00 "),
            Some(25 * 3600 + 30 * 60)
        );
        assert_eq!(parse_time_to_seconds("7:00:00"), Some(7 * 3600));
        assert_eq!(parse_time_to_seconds(""), None);
        assert_eq!(parse_time_to_seconds("73:00:00"), None);
        assert_eq!(parse_time_to_seconds("4294967295:00:00"), None);
        assert_eq!(parse_time_to_seconds("1193047:00:00"), None);
        assert_eq!(parse_time_to_seconds("08:60:00"), None);
        assert_eq!(parse_time_to_seconds("08:00:00:00"), None);
        assert_eq!(parse_time_to_seconds("+8:00:00"), None);
        assert_eq!(parse_time_to_seconds("é8:00:00"), None);

        assert_eq!(parse_date("20260315"), NaiveDate::from_ymd_opt(2026, 3, 15));
        // 8 bytes but non-ASCII: must not panic on a char-boundary slice.
        assert_eq!("20260é1".len(), 8);
        assert_eq!(parse_date("20260é1"), None);
        assert_eq!(parse_date("2026-3-1"), None);
        assert_eq!(parse_date("+2026031"), None);
        assert_eq!(parse_date("20261340"), None);
    }

    #[test]
    fn blank_stop_times_are_interpolated_and_untimed_trips_dropped() {
        let bytes = zip_of(&[
            ("stops.txt", BASE_STOPS),
            ("routes.txt", BASE_ROUTES),
            ("calendar.txt", BASE_CAL),
            (
                "trips.txt",
                "route_id,service_id,trip_id\nr1,S1,shape\nr1,S1,geo\nr1,S1,noend\nr1,S1,bigseq\n",
            ),
            (
                "stop_times.txt",
                "trip_id,arrival_time,departure_time,stop_id,stop_sequence,shape_dist_traveled\n\
                 shape,08:00:00,08:00:00,A,1,0\n\
                 shape,,,B,2,100\n\
                 shape,,,C,3,300\n\
                 shape,08:40:00,08:41:00,D,4,400\n\
                 geo,10:00:00,10:00:00,A,1,\n\
                 geo,,,B,2,\n\
                 geo,10:30:00,10:30:00,C,3,\n\
                 noend,09:00:00,09:00:00,A,1,\n\
                 noend,,,B,2,\n\
                 bigseq,07:10:00,07:10:00,B,70000,\n\
                 bigseq,07:00:00,07:00:00,A,5,\n\
                 bigseq,07:05:00,07:05:00,C,x,\n",
            ),
        ]);
        let bundle = load_gtfs_bytes("t", &bytes).unwrap();
        let times = |trip: &str| -> Vec<(u32, u32, u32, u8)> {
            let t = &bundle.trips[bundle.trip_id_to_idx[&format!("t:{trip}")] as usize];
            let s = t.stop_time_start as usize;
            bundle.stop_times[s..s + t.stop_time_len as usize]
                .iter()
                .map(|st| (st.stop_sequence, st.arrival_s, st.departure_s, st.timepoint))
                .collect()
        };
        let h = |hh: u32, mm: u32| hh * 3600 + mm * 60;

        // Interpolated by shape_dist_traveled: 8:00 → 8:40 over 0..400.
        let shape = times("shape");
        assert_eq!(shape.len(), 4);
        assert_eq!(shape[1], (2, h(8, 10), h(8, 10), 0));
        assert_eq!(shape[2], (3, h(8, 30), h(8, 30), 0));
        assert_eq!(shape[3], (4, h(8, 40), h(8, 41), 1));

        // No shape_dist: weighted by straight-line distance (A-B ≈ 1.1 km,
        // B-C ≈ 2.2 km) → B at about a third of the 30-minute gap.
        let geo = times("geo");
        assert!(geo[1].1 > h(10, 9) && geo[1].1 < h(10, 11), "{geo:?}");
        for w in geo.windows(2) {
            assert!(w[0].2 <= w[1].1, "times must not go backwards: {geo:?}");
        }

        // Last stop untimed → trip dropped, never 00:00.
        assert!(!bundle.trip_id_to_idx.contains_key("t:noend"));
        assert!(bundle.stop_times.iter().all(|st| st.departure_s != 0));

        // stop_sequence > u16::MAX keeps its value and order; an unparseable
        // stop_sequence row is skipped instead of becoming 0.
        let big = times("bigseq");
        assert_eq!(big.len(), 2, "{big:?}");
        assert_eq!(big[0].0, 5);
        assert_eq!(big[1].0, 70000);
        assert_eq!(big[1].1, h(7, 10));
    }

    #[test]
    fn calendar_dates_missing_exception_type_is_skipped() {
        let bytes = zip_of(&[
            ("stops.txt", BASE_STOPS),
            ("routes.txt", BASE_ROUTES),
            (
                "calendar_dates.txt",
                "service_id,date,exception_type\n\
                 X,20260310,\n\
                 Y,20260310,abc\n\
                 Z,20260310,3\n\
                 OK,20260310,1\n",
            ),
            ("trips.txt", "route_id,service_id,trip_id\nr1,OK,t1\n"),
            (
                "stop_times.txt",
                "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
                 t1,08:00:00,08:00:00,A,1\nt1,08:05:00,08:05:00,B,2\n",
            ),
        ]);
        let bundle = load_gtfs_bytes("t", &bytes).unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 3, 10).unwrap();
        assert!(bundle.calendar.is_active("OK", d));
        assert!(!bundle.calendar.is_active("X", d));
        assert!(!bundle.calendar.is_active("Y", d));
        assert!(!bundle.calendar.is_active("Z", d));
    }

    #[test]
    fn zip_entry_over_cap_is_rejected() {
        let big = "x".repeat(2048);
        let bytes = zip_of(&[("stops.txt", big.as_str())]);
        let mut archive = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let name = find_zip_entry(&mut archive, "STOPS.TXT").unwrap();
        assert!(read_named_zip_entry(&mut archive, &name, 1024).is_err());
        assert_eq!(
            read_named_zip_entry(&mut archive, &name, 4096)
                .unwrap()
                .len(),
            2048
        );
    }
}
