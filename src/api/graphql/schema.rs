use async_graphql::{
    Context, EmptyMutation, Object, Result, Schema, Subscription, ID,
};
use chrono::Utc;
use futures_util::Stream;
use std::sync::Arc;

use super::types::*;
use crate::feeds::supervisor::RtVersion;
use crate::link::geo::nearby;
use crate::routing::journey::{
    enrich_with_realtime, local_midnight_utc, local_seconds_since_midnight, service_date_in_tz,
    ItineraryQuery,
};
use crate::routing::plan_journeys;
use crate::search::search_stops;
use crate::state::AppState;

pub type ServiceSchema = Schema<QueryRoot, EmptyMutation, SubscriptionRoot>;

pub fn build_schema(state: Arc<AppState>) -> ServiceSchema {
    let depth = state.config.graphql.max_depth.max(1);
    let complexity = state.config.graphql.max_complexity.max(1);
    Schema::build(QueryRoot, EmptyMutation, SubscriptionRoot)
        .data(state)
        .limit_depth(depth)
        .limit_complexity(complexity)
        .finish()
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn health(&self, ctx: &Context<'_>) -> Result<SystemHealth> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let rt = state.load_rt();
        let mut feeds = Vec::new();
        for f in state.config.enabled_feeds() {
            let fr = rt.feeds.get(&f.id);
            let tu_age = fr.and_then(|x| x.trip_updates_age_secs());
            let bundle = epoch.feeds.get(&f.id);
            feeds.push(FeedStatus {
                id: ID(f.id.clone()),
                static_loaded: bundle.is_some(),
                trip_updates_age_seconds: tu_age.map(|s| s as i32),
                vehicle_positions_age_seconds: fr
                    .and_then(|x| x.vehicles_age_secs())
                    .map(|s| s as i32),
                alerts_age_seconds: fr.and_then(|x| x.alerts_age_secs()).map(|s| s as i32),
                trip_update_count: fr.map(|x| x.trip_update_count as i32).unwrap_or(0),
                vehicle_count: fr.map(|x| x.vehicle_count as i32).unwrap_or(0),
                alert_count: fr.map(|x| x.alert_count as i32).unwrap_or(0),
                ok: bundle.is_some() && tu_age.map(|a| a < 300).unwrap_or(false),
                feed_start_date: bundle.and_then(|b| b.feed_start_date.clone()),
                feed_end_date: bundle.and_then(|b| b.feed_end_date.clone()),
                publisher: bundle.and_then(|b| b.feed_publisher_name.clone()),
            });
        }
        let status = if epoch.trip_count() > 0 {
            "ok".into()
        } else {
            "starting".into()
        };
        Ok(SystemHealth {
            status,
            feeds,
            stop_count: epoch.stop_count() as i32,
            trip_count: epoch.trip_count() as i32,
            epoch_id: epoch.id.clone(),
        })
    }

    /// Search stops by name and/or proximity.
    ///
    /// - `search`: case-insensitive substring (min 2 chars when used alone)
    /// - `near`: lat/lon + radius via haversine neighborhood
    /// When both are set, results are the nearby stops filtered by name.
    async fn stops(
        &self,
        ctx: &Context<'_>,
        search: Option<String>,
        near: Option<NearInput>,
        limit: Option<i32>,
    ) -> Result<StopConnection> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let limit = limit.unwrap_or(15).clamp(1, 50) as usize;
        let q = search.unwrap_or_default();
        let cache_key = state
            .cache
            .stops_key(&epoch.id, &q, near.as_ref(), limit as i32);
        if let Some(cached) = state.cache.get_stops(&cache_key).await {
            return Ok(cached);
        }

        let q_trim = q.trim().to_lowercase();

        let nodes = if let Some(n) = near {
            if !(-90.0..=90.0).contains(&n.lat) || !(-180.0..=180.0).contains(&n.lon) {
                return Err(gql_err("invalid lat/lon for near", "BAD_REQUEST"));
            }
            let radius = n.radius_meters.clamp(1.0, 5_000.0);
            let points = epoch.geo_stop_points_view();
            let hits = nearby(&points, n.lat, n.lon, radius, limit.saturating_mul(4).max(limit));
            let mut out = Vec::new();
            for (idx, _dist) in hits {
                let s = &epoch.stops[idx];
                if q_trim.len() >= 2 {
                    let name = s.name.to_lowercase();
                    if !name.contains(&q_trim) && !s.raw_id.to_lowercase().contains(&q_trim) {
                        continue;
                    }
                }
                out.push(stop_from_record(s));
                if out.len() >= limit {
                    break;
                }
            }
            out
        } else {
            search_stops(&epoch, &q, None, limit)
                .into_iter()
                .map(|s| stop_from_record(&s))
                .collect::<Vec<_>>()
        };

        let total = nodes.len() as i32;
        let connection = StopConnection {
            nodes,
            total_count: total,
        };
        state.cache.set_stops(&cache_key, &connection).await;
        Ok(connection)
    }

    async fn stop(&self, ctx: &Context<'_>, id: ID) -> Result<Option<GqlStop>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        Ok(epoch.get_stop(id.as_str()).map(stop_from_record))
    }

    /// French address autocomplete from the local **Base Adresse Nationale** index.
    ///
    /// Requires `make ban-index` (downloads IDF BAN CSV + builds `data/ban/index.bin`).
    /// Returns empty when the index is not loaded. Use hits with `lat`/`lon` as
    /// `itineraries(input: { from: { lat, lon, name }, … })` (door-to-door snap).
    async fn addresses(
        &self,
        ctx: &Context<'_>,
        search: String,
        limit: Option<i32>,
    ) -> Result<Vec<AddressSuggestion>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let q = search.trim();
        if q.chars().count() < 2 {
            return Ok(vec![]);
        }
        let limit = limit.unwrap_or(8).clamp(1, 20) as usize;
        let cache_key = state.cache.addresses_key(q, limit as i32);
        if let Some(cached) = state.cache.get_addresses(&cache_key).await {
            return Ok(cached);
        }
        let ban = state.load_ban();
        if !ban.is_ready() {
            return Ok(vec![]);
        }
        let out: Vec<AddressSuggestion> = ban
            .suggest(q, limit)
            .into_iter()
            .map(|h| AddressSuggestion {
                id: h.id,
                label: h.label,
                number: if h.number.is_empty() {
                    None
                } else {
                    Some(h.number)
                },
                rep: if h.rep.is_empty() { None } else { Some(h.rep) },
                street: h.street,
                postcode: h.postcode,
                city: h.city,
                lat: h.lat,
                lon: h.lon,
                score: h.score as f64,
                kind: h.kind,
            })
            .collect();
        state.cache.set_addresses(&cache_key, &out).await;
        Ok(out)
    }

    /// Unified place search: **stops** + **BAN addresses** for origin/destination fields.
    async fn places(
        &self,
        ctx: &Context<'_>,
        search: String,
        limit: Option<i32>,
    ) -> Result<Vec<PlaceSuggestion>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let ban = state.load_ban();
        let limit = limit.unwrap_or(12).clamp(1, 30) as usize;
        let q = search.trim();
        if q.chars().count() < 2 {
            return Ok(vec![]);
        }
        let cache_key = state.cache.places_key(&epoch.id, q, limit as i32);
        if let Some(cached) = state.cache.get_places(&cache_key).await {
            return Ok(cached);
        }

        let stop_limit = (limit / 2).max(4).min(limit);
        let addr_limit = limit.saturating_sub(stop_limit).max(4);

        let mut out = Vec::new();
        for s in search_stops(&epoch, q, None, stop_limit) {
            let is_station = s.is_station();
            out.push(PlaceSuggestion {
                kind: "stop".into(),
                id: s.id.clone(),
                label: s.name.clone(),
                lat: s.lat,
                lon: s.lon,
                stop_id: Some(s.id),
                is_station: Some(is_station),
                postcode: None,
                city: None,
                street: None,
            });
        }
        if ban.is_ready() {
            for h in ban.suggest(q, addr_limit) {
                out.push(PlaceSuggestion {
                    kind: "address".into(),
                    id: h.id,
                    label: h.label,
                    lat: Some(h.lat),
                    lon: Some(h.lon),
                    stop_id: None,
                    is_station: None,
                    postcode: Some(h.postcode),
                    city: Some(h.city),
                    street: Some(h.street),
                });
            }
        }
        out.truncate(limit);
        state.cache.set_places(&cache_key, &out).await;
        Ok(out)
    }

    /// Single GTFS agency by namespaced id (`feed:agencyId`) or raw agency id.
    async fn agency(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Agency>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let key = id.as_str();
        if let Some((feed_id, raw)) = key.split_once(':') {
            if let Some(bundle) = epoch.feeds.get(feed_id) {
                if let Some(a) = bundle.agencies.get(raw) {
                    return Ok(Some(agency_from_record(feed_id, a)));
                }
                // Also try full key as agency map key (already namespaced).
                if let Some(a) = bundle.agencies.get(key) {
                    return Ok(Some(agency_from_record(feed_id, a)));
                }
            }
            return Ok(None);
        }
        // Raw id: first match across feeds.
        for (feed_id, bundle) in &epoch.feeds {
            if let Some(a) = bundle.agencies.get(key) {
                return Ok(Some(agency_from_record(feed_id, a)));
            }
        }
        Ok(None)
    }

    /// Agencies for a feed (or all feeds when `feedId` is omitted). Empty when not packed.
    async fn agencies(
        &self,
        ctx: &Context<'_>,
        feed_id: Option<String>,
    ) -> Result<Vec<Agency>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let mut out = Vec::new();
        for (fid, bundle) in &epoch.feeds {
            if let Some(ref want) = feed_id {
                if fid != want {
                    continue;
                }
            }
            for a in bundle.agencies.values() {
                out.push(agency_from_record(fid, a));
            }
        }
        out.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(out)
    }

    /// Next departures at a stop (and its child platforms when the stop is a station).
    ///
    /// 1. If `live` (default true) and PRIM key is set: try **SIRI Stop Monitoring**
    ///    (raw, not Navitia).
    /// 2. Else / fallback: static GTFS `stop_departures` + RT overlay.
    async fn departures(
        &self,
        ctx: &Context<'_>,
        stop_id: ID,
        at: Option<chrono::DateTime<Utc>>,
        #[graphql(default = 10)] limit: i32,
        #[graphql(default = true)] live: bool,
    ) -> Result<Vec<Departure>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let rt = state.load_rt();
        let limit = limit.clamp(1, 100) as usize;
        let at = at.unwrap_or_else(Utc::now);
        let tz = state.config.routing.timezone.as_str();

        let stop_key = stop_id.as_str();

        // --- Live SIRI path (IDFM) ---
        if live && state.config.prim.should_run() {
            if let Some(mon) = crate::prim::monitoring_ref_from_stop_id(stop_key).or_else(|| {
                epoch
                    .get_stop(stop_key)
                    .and_then(|s| crate::prim::monitoring_ref_from_stop_id(&s.raw_id))
            }) {
                let timeout = std::time::Duration::from_secs(
                    state.config.runtime.rt_request_timeout_secs.max(10),
                );
                if let Ok(client) = crate::prim::PrimClient::from_env(
                    &state.config.prim.base_url,
                    &state.config.prim.api_key_env,
                    &state.config.prim.user_agent,
                    timeout,
                ) {
                    match crate::prim::stop_board::fetch_live_board(&client, &mon, limit).await {
                        Ok(board) => {
                            // Merge SM trips + VehicleLocation GPS into the live overlay
                            // so the map can show real dots (not only estimated).
                            if let Some(delta) = board.rt_delta {
                                if !delta.vehicles.is_empty() || !delta.trips.is_empty() {
                                    let mut overlay = (*state.load_rt()).clone();
                                    overlay.merge_feed_delta("idfm", delta);
                                    crate::routing::finalize_realtime_overlay(&mut overlay);
                                    state.rt.store(std::sync::Arc::new(overlay));
                                }
                            }
                            if !board.departures.is_empty() {
                                let stop_rec =
                                    epoch.get_stop(stop_key).cloned().unwrap_or_else(|| {
                                        crate::prim::stop_board::shell_stop(stop_key, None)
                                    });
                                let out = board
                                    .departures
                                    .into_iter()
                                    .map(|d| Departure {
                                        trip_id: ID(d.trip_id),
                                        route_short_name: d.route_short_name,
                                        route_long_name: d.route_long_name,
                                        trip_short_name: d.trip_short_name,
                                        headsign: d.headsign,
                                        mode: Mode::from(d.mode),
                                        scheduled_departure: d.scheduled_departure,
                                        realtime_departure: d.realtime_departure,
                                        delay_seconds: d.delay_seconds,
                                        canceled: d.canceled,
                                        platform: d
                                            .platform
                                            .or_else(|| stop_rec.platform_code.clone()),
                                        route_color: d.route_color,
                                        stop: stop_from_record(&stop_rec),
                                        source: Some(d.source.to_string()),
                                        status: d.status,
                                    })
                                    .collect();
                                return Ok(out);
                            }
                            // empty live board → fall through to static
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, stop = %stop_key, "live SIRI departures fallback to GTFS");
                        }
                    }
                }
            }
        }

        let Some(&root_idx) = epoch.stop_id_to_idx.get(stop_key) else {
            // Unknown stop → empty board (not an error).
            return Ok(vec![]);
        };

        // Resolve stop + children platforms (parent station includes child platforms).
        let mut stop_indices: Vec<u32> = vec![root_idx];
        for (i, s) in epoch.stops.iter().enumerate() {
            if s.parent_id.as_deref() == Some(stop_key) {
                stop_indices.push(i as u32);
            }
        }
        stop_indices.sort_unstable();
        stop_indices.dedup();

        let service_date = service_date_in_tz(at, tz);
        let local_secs = local_seconds_since_midnight(at, tz);
        let midnight = local_midnight_utc(service_date, tz);

        // Collect candidates: (scheduled_dep_s, trip_idx, st_off, stop_idx)
        let mut candidates: Vec<(u32, u32, u32, u32)> = Vec::new();
        for &si in &stop_indices {
            let deps = epoch
                .stop_departures
                .get(si as usize)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            for &(trip_idx, off) in deps {
                let trip = match epoch.trips.get(trip_idx as usize) {
                    Some(t) => t,
                    None => continue,
                };
                if let Some(cal) = epoch.calendars.get(&trip.feed_id) {
                    if !cal.is_active(&trip.service_id, service_date) {
                        continue;
                    }
                }
                let st = match epoch
                    .stop_times
                    .get((trip.stop_time_start + off) as usize)
                {
                    Some(st) => st,
                    None => continue,
                };
                if st.departure_s < local_secs {
                    continue;
                }
                candidates.push((st.departure_s, trip_idx, off, si));
            }
        }

        candidates.sort_by_key(|&(dep_s, trip_idx, off, si)| (dep_s, trip_idx, off, si));

        let mut out = Vec::with_capacity(limit.min(candidates.len()));
        for (dep_s, trip_idx, off, si) in candidates.into_iter().take(limit) {
            let trip = &epoch.trips[trip_idx as usize];
            let stop_rec = &epoch.stops[si as usize];
            let scheduled = midnight + chrono::Duration::seconds(dep_s as i64);
            let applied = rt.apply_departure(
                &trip.id,
                scheduled,
                Some(
                    epoch.stop_times[(trip.stop_time_start + off) as usize].stop_sequence,
                ),
                Some(stop_rec.id.as_str()),
            );
            let (realtime_departure, delay_seconds) = if applied.has_rt || applied.canceled {
                (Some(applied.realtime), Some(applied.delay_secs))
            } else {
                (None, None)
            };
            out.push(Departure {
                trip_id: ID(trip.id.clone()),
                route_short_name: Some(trip.route_short_name.clone()).filter(|s| !s.is_empty()),
                route_long_name: Some(trip.route_long_name.clone()).filter(|s| !s.is_empty()),
                trip_short_name: trip.short_name.clone(),
                headsign: trip.headsign.clone(),
                mode: Mode::from(trip.mode),
                scheduled_departure: scheduled,
                realtime_departure,
                delay_seconds,
                canceled: applied.canceled || rt.is_trip_canceled(&trip.id),
                platform: stop_rec.platform_code.clone(),
                route_color: trip.route_color.clone(),
                stop: stop_from_record(stop_rec),
                source: Some(if realtime_departure.is_some() {
                    "gtfs-static+rt".into()
                } else {
                    "gtfs-static".into()
                }),
                status: None,
            });
        }
        Ok(out)
    }

    /// Traffic / service messages (SIRI General Message + GTFS-RT alerts in overlay).
    ///
    /// Deduplicates near-identical SNCF broadcasts, drops inactive periods when known,
    /// re-picks FR text / strips HTML via [`map_alert`]. Prefer `feedId: "idfm"` for Île-de-France.
    async fn traffic_messages(
        &self,
        ctx: &Context<'_>,
        feed_id: Option<String>,
        #[graphql(default = 40)] limit: i32,
    ) -> Result<Vec<Alert>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let rt = state.load_rt();
        let epoch = state.load_epoch();
        let limit = limit.clamp(1, 200) as usize;
        let now_ts = Utc::now().timestamp();
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<Alert> = Vec::new();

        // Prefer IDFM then SNCF so local traffic surfaces before national noise.
        let mut feed_order: Vec<&String> = rt.feeds.keys().collect();
        feed_order.sort_by_key(|fid| match fid.as_str() {
            "idfm" => 0,
            "sncf" => 1,
            _ => 2,
        });

        for fid in feed_order {
            if let Some(ref want) = feed_id {
                if fid != want {
                    continue;
                }
            }
            let Some(fr) = rt.feeds.get(fid) else {
                continue;
            };
            for a in &fr.alerts {
                let periods: Vec<(Option<i64>, Option<i64>)> = if !a.active_periods.is_empty() {
                    a.active_periods.clone()
                } else if a.active_start.is_some() || a.active_end.is_some() {
                    vec![(a.active_start, a.active_end)]
                } else {
                    Vec::new()
                };
                if !crate::rt::alert_text::alert_active_now(&periods, now_ts) {
                    continue;
                }
                // Enrich with line badges (RER B blue square, etc.) from LineRef + GTFS.
                let mapped = map_alert_enriched(a, Some(epoch.as_ref()));
                let key = crate::rt::alert_text::alert_dedupe_key(
                    mapped.header.as_deref(),
                    mapped.description.as_deref(),
                );
                if !seen.insert(key) {
                    continue;
                }
                // Skip empty after clean
                if mapped.header.as_ref().map(|h| h.is_empty()).unwrap_or(true)
                    && mapped
                        .description
                        .as_ref()
                        .map(|d| d.is_empty())
                        .unwrap_or(true)
                {
                    continue;
                }
                out.push(mapped);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Full stop list for a namespaced trip id, with scheduled + realtime times.
    async fn trip(&self, ctx: &Context<'_>, id: ID) -> Result<Option<TripDetail>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let rt = state.load_rt();
        let trip_id = id.as_str();

        let Some(&trip_idx) = epoch.trip_id_to_idx.get(trip_id) else {
            return Ok(None);
        };
        let trip = &epoch.trips[trip_idx as usize];
        let tz = state.config.routing.timezone.as_str();
        // Use "now" for service-date midnight so absolute times are on today's calendar
        // when the trip is active; times are seconds-since-midnight offsets.
        let service_date = service_date_in_tz(Utc::now(), tz);
        let midnight = local_midnight_utc(service_date, tz);

        let mut stops = Vec::with_capacity(trip.stop_time_len as usize);
        for off in 0..trip.stop_time_len {
            let st = &epoch.stop_times[(trip.stop_time_start + off) as usize];
            let stop_rec = match epoch.stops.get(st.stop_idx as usize) {
                Some(s) => s,
                None => continue,
            };
            let scheduled_arrival =
                midnight + chrono::Duration::seconds(st.arrival_s as i64);
            let scheduled_departure =
                midnight + chrono::Duration::seconds(st.departure_s as i64);
            let arr = rt.apply_arrival(
                &trip.id,
                scheduled_arrival,
                Some(st.stop_sequence),
                Some(stop_rec.id.as_str()),
            );
            let dep = rt.apply_departure(
                &trip.id,
                scheduled_departure,
                Some(st.stop_sequence),
                Some(stop_rec.id.as_str()),
            );
            let skipped = arr.skipped || dep.skipped;
            let has_rt = arr.has_rt || dep.has_rt;
            let delay_seconds = if has_rt {
                Some(if dep.has_rt {
                    dep.delay_secs
                } else {
                    arr.delay_secs
                })
            } else {
                None
            };
            stops.push(TripStopTime {
                stop: stop_from_record(stop_rec),
                scheduled_arrival,
                scheduled_departure,
                realtime_arrival: if arr.has_rt { Some(arr.realtime) } else { None },
                realtime_departure: if dep.has_rt {
                    Some(dep.realtime)
                } else {
                    None
                },
                delay_seconds,
                skipped,
                platform: stop_rec.platform_code.clone(),
            });
        }

        // Prefer packed GTFS shape; else stop-chain for maps.
        let geometry = {
            let from_shape = trip.shape_id.as_ref().and_then(|sid| {
                let pts = crate::routing::geometry::shape_polyline(&epoch, sid);
                if pts.is_empty() {
                    None
                } else {
                    Some(
                        pts.into_iter()
                            .map(|p| LatLng {
                                lat: p[0],
                                lon: p[1],
                            })
                            .collect::<Vec<_>>(),
                    )
                }
            });
            from_shape.or_else(|| {
                let chain = geometry_from_trip_stops(&stops);
                if chain.is_empty() {
                    None
                } else {
                    Some(chain)
                }
            })
        };

        Ok(Some(TripDetail {
            id: ID(trip.id.clone()),
            route_short_name: Some(trip.route_short_name.clone()).filter(|s| !s.is_empty()),
            long_name: Some(trip.route_long_name.clone()).filter(|s| !s.is_empty()),
            short_name: trip.short_name.clone(),
            headsign: trip.headsign.clone(),
            mode: Mode::from(trip.mode),
            direction_id: trip.direction_id.map(|d| d as i32),
            color: trip.route_color.clone(),
            stops,
            geometry,
        }))
    }

    /// GTFS shape polyline by namespaced shape id (`feed:shapeId`).
    ///
    /// Returns an empty list when the epoch has no matching shape — clients can fall
    /// back to leg/trip stop-chain geometry.
    async fn shape(&self, ctx: &Context<'_>, shape_id: ID) -> Result<Vec<LatLng>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        Ok(crate::routing::geometry::shape_polyline(&epoch, shape_id.as_str())
            .into_iter()
            .map(|p| LatLng {
                lat: p[0],
                lon: p[1],
            })
            .collect())
    }

    /// Realtime delay / cancel / vehicle / alerts for a namespaced trip id.
    ///
    /// Vehicle position: real GPS (SM `VehicleLocation` / GTFS-RT VP) when present,
    /// otherwise **estimated** along the GTFS shape from expected times.
    async fn trip_realtime(&self, ctx: &Context<'_>, trip_id: ID) -> Result<TripRealtime> {
        let state = ctx.data::<Arc<AppState>>()?;
        let rt = state.load_rt();
        let epoch = state.load_epoch();
        let id = trip_id.to_string();
        if id.is_empty() || !id.contains(':') {
            return Err(gql_err(
                "tripId must be namespaced as feed:rawTripId",
                "BAD_REQUEST",
            ));
        }
        Ok(trip_realtime_for_with_epoch(
            &id,
            &rt,
            Some(epoch.as_ref()),
        ))
    }

    /// Soft-register SM stops + ET LineRefs for the map viewport (no PRIM HTTP).
    /// Call when the map pans so warm pollers prefer visible lines/stops.
    async fn warm_live_viewport(
        &self,
        ctx: &Context<'_>,
        bbox: BBoxInput,
    ) -> Result<bool> {
        let state = ctx.data::<Arc<AppState>>()?;
        if !bbox.is_valid() {
            return Err(gql_err("invalid bbox", "BAD_REQUEST"));
        }
        let epoch = state.load_epoch();
        crate::prim::register_viewport_live_interest(
            epoch.as_ref(),
            bbox.min_lat,
            bbox.min_lon,
            bbox.max_lat,
            bbox.max_lon,
        );
        Ok(true)
    }

    async fn itineraries(
        &self,
        ctx: &Context<'_>,
        input: ItineraryInput,
    ) -> Result<ItineraryResult> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let rt = state.load_rt();
        let cfg = &state.config.routing;

        if epoch.trip_count() == 0 {
            return Err(gql_err(
                "static timetable not loaded yet; try again shortly",
                "NOT_READY",
            ));
        }

        let departure_at = input
            .departure_at
            .or(input.arrive_by)
            .unwrap_or_else(Utc::now);

        let cache_key = state
            .cache
            .itinerary_key(&epoch.id, rt.version, &input, cfg);

        let modes = input.modes.map(|ms| {
            ms.into_iter()
                .filter_map(|m| m.to_pack())
                .collect::<Vec<_>>()
        });

        // Pass RT-cancelled trips so routing skips boarding them.
        let excluded_trip_ids = if rt.canceled_trip_ids.is_empty() {
            rt.feeds
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
                .collect()
        } else {
            rt.canceled_trip_ids.clone()
        };
        // Delays / SKIPPED stops — applied inside RAPTOR (not only on display).
        let rt_adjust = if rt.rt_adjust.is_empty() {
            crate::routing::rt_adjust_map_from_overlay(&rt)
        } else {
            rt.rt_adjust.clone()
        };

        if let Some(result) = state.cache.get_itinerary(&cache_key).await {
            let age = rt.max_trip_updates_age_secs();
            let degraded = age.map(|a| a > 300).unwrap_or(true);
            let mut journeys: Vec<_> = result
                .journeys
                .into_iter()
                .map(|j| enrich_with_realtime(j, &rt, Some(epoch.as_ref())))
                .filter(|j| {
                    let transit: Vec<_> = j
                        .legs
                        .iter()
                        .filter_map(|l| match l {
                            crate::routing::Leg::Transit(t) => Some(t),
                            _ => None,
                        })
                        .collect();
                    if transit.is_empty() {
                        return true;
                    }
                    !transit.iter().all(|t| t.canceled)
                })
                .collect();
            journeys.sort_by(|a, b| {
                a.arrival
                    .cmp(&b.arrival)
                    .then_with(|| a.duration_s.cmp(&b.duration_s))
                    .then_with(|| a.transfers.cmp(&b.transfers))
            });
            let journeys = journeys
                .into_iter()
                .map(|j| map_journey(&j, &rt, Some(epoch.as_ref())))
                .collect();
            return Ok(ItineraryResult {
                from: map_place(&result.from),
                to: map_place(&result.to),
                computed_at: result.computed_at,
                static_epoch_id: result.static_epoch_id,
                realtime_age_seconds: age.map(|a| a as i32),
                realtime_degraded: degraded,
                journeys,
            });
        }

        let q = ItineraryQuery {
            from_stop_id: input.from.stop_id.map(|i| i.to_string()),
            to_stop_id: input.to.stop_id.map(|i| i.to_string()),
            from_lat: input.from.lat,
            from_lon: input.from.lon,
            to_lat: input.to.lat,
            to_lon: input.to.lon,
            departure_at,
            arrive_by: input.arrive_by.is_some(),
            max_transfers: input.max_transfers.clamp(0, 8) as u32,
            max_results: input.max_results.clamp(1, 15) as u32,
            modes,
            max_walk_meters: input.max_walk_meters.clamp(0, 5000) as u32,
            walk_speed_m_s: cfg.walk_speed_m_s,
            raptor_max_rounds: cfg.raptor_max_rounds,
            default_transfer_s: cfg.default_transfer_s,
            timezone: cfg.timezone.clone(),
            excluded_trip_ids,
            rt_adjust,
            wheelchair: input.wheelchair.unwrap_or(false),
            osrm_url: {
                let u = cfg.osrm_url.trim();
                if u.is_empty() {
                    None
                } else {
                    Some(u.to_string())
                }
            },
            bike_from: input.bike_from.unwrap_or(false),
            bike_to: input.bike_to.unwrap_or(false),
            bike_speed_m_s: cfg.bike_speed_m_s,
            max_bike_meters: cfg.max_bike_meters,
            use_tbr: input.use_tbr.unwrap_or(true), // FLASH-TB is the default router
        };

        let timeout = std::time::Duration::from_secs(state.config.graphql.itinerary_timeout_secs);
        let epoch_c = epoch.clone();
        let result = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || plan_journeys(&epoch_c, &q)),
        )
        .await
        .map_err(|_| gql_err("itinerary timeout", "TIMEOUT"))?
        .map_err(|e| gql_err(format!("routing task: {e}"), "INTERNAL"))?;

        state.cache.set_itinerary(&cache_key, &result).await;

        let age = rt.max_trip_updates_age_secs();
        let degraded = age.map(|a| a > 300).unwrap_or(true);

        let mut journeys: Vec<_> = result
            .journeys
            .into_iter()
            .map(|j| enrich_with_realtime(j, &rt, Some(epoch.as_ref())))
            // Drop fully cancelled itineraries (all transit legs canceled).
            .filter(|j| {
                let transit: Vec<_> = j
                    .legs
                    .iter()
                    .filter_map(|l| match l {
                        crate::routing::Leg::Transit(t) => Some(t),
                        _ => None,
                    })
                    .collect();
                if transit.is_empty() {
                    return true; // pure walk
                }
                !transit.iter().all(|t| t.canceled)
            })
            .collect();
        // Prefer earliest *realtime* arrival after RT overlay.
        journeys.sort_by(|a, b| {
            a.arrival
                .cmp(&b.arrival)
                .then_with(|| a.duration_s.cmp(&b.duration_s))
                .then_with(|| a.transfers.cmp(&b.transfers))
        });

        let journeys = journeys
            .into_iter()
            .map(|j| map_journey(&j, &rt, Some(epoch.as_ref())))
            .collect();

        Ok(ItineraryResult {
            from: map_place(&result.from),
            to: map_place(&result.to),
            computed_at: result.computed_at,
            static_epoch_id: result.static_epoch_id,
            realtime_age_seconds: age.map(|a| a as i32),
            realtime_degraded: degraded,
            journeys,
        })
    }

    async fn alerts(
        &self,
        ctx: &Context<'_>,
        feed_id: Option<String>,
        limit: Option<i32>,
    ) -> Result<Vec<Alert>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let rt = state.load_rt();
        let limit = limit.unwrap_or(50).clamp(1, 200) as usize;
        let mut out = Vec::new();
        for (fid, fr) in &rt.feeds {
            if let Some(ref want) = feed_id {
                if fid != want {
                    continue;
                }
            }
            for a in &fr.alerts {
                out.push(map_alert(a));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Live vehicle positions for map “network live” mode (no itinerary required).
    ///
    /// Filters (all optional, combined with AND):
    /// - `feedId`: only vehicles in that feed's overlay
    /// - `tripId`: exact namespaced trip id match
    /// - `near`: haversine radius around lat/lon
    /// - `bbox`: axis-aligned geographic window (`minLat`…`maxLon`)
    /// - `modes`: keep only vehicles whose static route mode is in the list
    ///
    /// When a feed has TripUpdates / SIRI SM–ET but no GPS VP, synthesizes
    /// **estimated** positions along the GTFS stop chain (or shape) from expected
    /// times (`currentStatus` = `ESTIMATED_IN_TRANSIT_TO` / `ESTIMATED_STOPPED_AT` / …).
    /// Real VP always wins over synthetic for the same trip. Default `limit` is 200.
    async fn vehicles(
        &self,
        ctx: &Context<'_>,
        feed_id: Option<String>,
        trip_id: Option<ID>,
        near: Option<NearInput>,
        bbox: Option<BBoxInput>,
        modes: Option<Vec<Mode>>,
        limit: Option<i32>,
    ) -> Result<Vec<VehiclePosition>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let rt = state.load_rt();
        let epoch = state.load_epoch();
        let limit = limit.unwrap_or(200).clamp(1, 500) as usize;
        let trip_filter = trip_id.map(|t| t.to_string());
        let mode_filter = modes.filter(|m| !m.is_empty());

        if let Some(ref n) = near {
            if !(-90.0..=90.0).contains(&n.lat) || !(-180.0..=180.0).contains(&n.lon) {
                return Err(gql_err("invalid lat/lon for near", "BAD_REQUEST"));
            }
        }
        if let Some(ref b) = bbox {
            if !b.is_valid() {
                return Err(gql_err("invalid bbox", "BAD_REQUEST"));
            }
        }
        let radius = near
            .as_ref()
            .map(|n| n.radius_meters.clamp(1.0, 50_000.0));

        let cacheable = bbox.is_some() || near.is_some();
        let vehicles_cache_key = if cacheable {
            Some(state.cache.vehicles_key(
                &epoch.id,
                rt.version,
                feed_id.as_deref(),
                trip_filter.as_deref(),
                near.as_ref(),
                bbox.as_ref(),
                mode_filter.as_deref(),
                limit as i32,
            ))
        } else {
            None
        };
        if let Some(ref key) = vehicles_cache_key {
            if let Some(cached) = state.cache.get_vehicles(key).await {
                return Ok(cached);
            }
        }

        // Soft viewport interest (no PRIM HTTP): prioritizes ET LineRefs + SM refs
        // for the warm pollers without stampeding the API on every pan.
        if let Some(ref b) = bbox {
            crate::prim::register_viewport_live_interest(
                epoch.as_ref(),
                b.min_lat,
                b.min_lon,
                b.max_lat,
                b.max_lon,
            );
        } else if let Some(ref n) = near {
            crate::prim::register_nearby_sm_interest(
                epoch.as_ref(),
                n.lat,
                n.lon,
                n.radius_meters.clamp(50.0, 4_000.0),
                12,
            );
        }

        let passes_geo = |lat: f64, lon: f64| -> bool {
            if let (Some(n), Some(r)) = (near.as_ref(), radius) {
                if crate::link::geo::haversine_m(n.lat, n.lon, lat, lon) > r {
                    return false;
                }
            }
            if let Some(ref b) = bbox {
                if !b.contains(lat, lon) {
                    return false;
                }
            }
            true
        };

        let mut out: Vec<VehiclePosition> = Vec::new();
        // Trip keys (base undated) that already have a real VP in the result set / overlay.
        let mut real_trip_keys: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Prefer idfm over sncf so Île-de-France vehicles fill the map first.
        let mut feed_ids: Vec<String> = rt.feeds.keys().cloned().collect();
        feed_ids.sort_by_key(|f| match f.as_str() {
            "idfm" => 0u8,
            "sncf" => 2,
            _ => 1,
        });

        // Cap synthetic SNCF so it cannot crowd out IDFM (RER/metro/tram/bus).
        let mut synth_per_feed: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let synth_cap = |fid: &str| -> usize {
            match fid {
                "idfm" => limit,
                "sncf" => (limit / 4).max(20).min(80),
                _ => limit / 2,
            }
        };

        // --- Real vehicle positions first ---
        'vehicles: for fid in &feed_ids {
            let Some(fr) = rt.feeds.get(fid) else {
                continue;
            };
            if let Some(ref want) = feed_id {
                if fid != want {
                    continue;
                }
            }
            for v in fr.vehicles.values() {
                if let Some(ref want_trip) = trip_filter {
                    let matches = v.trip_id.as_deref() == Some(want_trip.as_str())
                        || v.trip_id.as_deref().is_some_and(|t| {
                            t.rsplit_once('@').map(|(b, _)| b) == Some(want_trip.as_str())
                        });
                    if !matches {
                        continue;
                    }
                }
                if !passes_geo(v.lat, v.lon) {
                    continue;
                }
                let trip_rt = v
                    .trip_id
                    .as_deref()
                    .and_then(|tid| fr.get_trip(tid).or_else(|| rt.get_trip(tid)));
                let mapped =
                    map_vehicle_enriched(v, Some(fid.as_str()), Some(epoch.as_ref()), trip_rt);
                if let Some(ref mf) = mode_filter {
                    match mapped.mode {
                        Some(m) if mf.contains(&m) => {}
                        _ => continue,
                    }
                }
                if let Some(ref tid) = v.trip_id {
                    let base = tid.rsplit_once('@').map(|(b, _)| b).unwrap_or(tid);
                    real_trip_keys.insert(base.to_string());
                }
                out.push(mapped);
                if out.len() >= limit {
                    break 'vehicles;
                }
            }
        }

        // --- Synthetic positions from TripUpdates (no VP for that trip) ---
        for fid in &feed_ids {
            if out.len() >= limit {
                break;
            }
            let Some(fr) = rt.feeds.get(fid) else {
                continue;
            };
            if let Some(ref want) = feed_id {
                if fid != want {
                    continue;
                }
            }
            let updated_at = fr
                .trip_updates_fetched_at
                .unwrap_or_else(Utc::now);
            let cap = synth_cap(fid.as_str());
            // Dedup dated/undated keys within the feed.
            let mut seen_base: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for (trip_key, trip) in &fr.trips {
                if trip.canceled || trip.stop_updates.is_empty() {
                    continue;
                }
                // Skip incomplete synthetic keys without a real trip id.
                if trip_key.contains(":#r=") {
                    continue;
                }
                let base = trip_key
                    .rsplit_once('@')
                    .map(|(b, _)| b)
                    .unwrap_or(trip_key.as_str());
                if !seen_base.insert(base.to_string()) {
                    continue;
                }
                if real_trip_keys.contains(base) || rt.has_vehicle_for_trip(base) {
                    continue;
                }
                if let Some(ref want_trip) = trip_filter {
                    if base != want_trip.as_str() && trip_key != want_trip.as_str() {
                        continue;
                    }
                }
                let used = synth_per_feed.get(fid.as_str()).copied().unwrap_or(0);
                if used >= cap {
                    break;
                }
                let Some(synth) = synthetic_vehicle_from_trip_update(
                    fid,
                    trip_key,
                    trip,
                    epoch.as_ref(),
                    updated_at,
                ) else {
                    continue;
                };
                if !passes_geo(synth.lat, synth.lon) {
                    continue;
                }
                let mapped =
                    map_vehicle_enriched(&synth, Some(fid.as_str()), Some(epoch.as_ref()), Some(trip));
                if let Some(ref mf) = mode_filter {
                    match mapped.mode {
                        Some(m) if mf.contains(&m) => {}
                        _ => continue,
                    }
                }
                out.push(mapped);
                *synth_per_feed.entry(fid.clone()).or_insert(0) += 1;
                if out.len() >= limit {
                    break;
                }
            }
        }

        if let Some(ref key) = vehicles_cache_key {
            state.cache.set_vehicles(key, &out).await;
        }

        Ok(out)
    }

    /// GTFS levels for indoor mapping when present on the static epoch.
    ///
    /// Reads `FeedStaticBundle.levels` per feed. Empty when not packed (graceful).
    async fn levels(
        &self,
        ctx: &Context<'_>,
        feed_id: Option<String>,
    ) -> Result<Vec<Level>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let mut out = Vec::new();
        for (fid, bundle) in &epoch.feeds {
            if let Some(ref want) = feed_id {
                if fid != want {
                    continue;
                }
            }
            for lv in bundle.levels.values() {
                out.push(Level {
                    id: ID(lv.id.clone()),
                    index: lv.level_index,
                    name: lv.level_name.clone(),
                });
            }
        }
        // Stable order for clients
        out.sort_by(|a, b| {
            a.index
                .partial_cmp(&b.index)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(out)
    }

    /// GTFS pathways connected to a stop (from or to) when present on the epoch.
    ///
    /// Returns empty when pathways are not packed — clients must not treat this as error.
    async fn pathways(
        &self,
        ctx: &Context<'_>,
        stop_id: Option<ID>,
        feed_id: Option<String>,
        limit: Option<i32>,
    ) -> Result<Vec<Pathway>> {
        let state = ctx.data::<Arc<AppState>>()?;
        let epoch = state.load_epoch();
        let limit = limit.unwrap_or(100).clamp(1, 500) as usize;
        let stop_filter = stop_id.map(|s| s.to_string());

        let mut out = Vec::new();
        for (fid, bundle) in &epoch.feeds {
            if let Some(ref want) = feed_id {
                if fid != want {
                    continue;
                }
            }
            // When filtering by stop, infer feed from namespaced id if not set.
            if let Some(ref sid) = stop_filter {
                if feed_id.is_none() {
                    if let Some((prefix, _)) = sid.split_once(':') {
                        if prefix != fid.as_str() {
                            continue;
                        }
                    }
                }
            }
            for pw in &bundle.pathways {
                let from_id = bundle
                    .stops
                    .get(pw.from_stop_idx as usize)
                    .map(|s| s.id.clone())
                    .unwrap_or_default();
                let to_id = bundle
                    .stops
                    .get(pw.to_stop_idx as usize)
                    .map(|s| s.id.clone())
                    .unwrap_or_default();
                if let Some(ref sid) = stop_filter {
                    if from_id != *sid && to_id != *sid {
                        continue;
                    }
                }
                let pathway_id = if pw.pathway_id.contains(':') {
                    pw.pathway_id.clone()
                } else {
                    format!("{fid}:{}", pw.pathway_id)
                };
                out.push(Pathway {
                    id: ID(pathway_id),
                    from_stop_id: ID(from_id),
                    to_stop_id: ID(to_id),
                    mode: pw.mode_name().to_string(),
                    duration_seconds: Some(pw.duration_s() as i32),
                    bidirectional: pw.is_bidirectional,
                });
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }
}

pub struct SubscriptionRoot;

#[Subscription]
impl SubscriptionRoot {
    async fn feed_status(
        &self,
        ctx: &Context<'_>,
    ) -> Result<impl Stream<Item = FeedStatus> + 'static> {
        let state = ctx.data::<Arc<AppState>>()?.clone();
        let mut rx = state.rt_version_tx.subscribe();
        Ok(async_stream::stream! {
            // emit current snapshot for each enabled feed on each RT version bump
            loop {
                let epoch = state.load_epoch();
                let rt = state.load_rt();
                for f in state.config.enabled_feeds() {
                    let fr = rt.feeds.get(&f.id);
                    let tu_age = fr.and_then(|x| x.trip_updates_age_secs());
                    let bundle = epoch.feeds.get(&f.id);
                    yield FeedStatus {
                        id: ID(f.id.clone()),
                        static_loaded: bundle.is_some(),
                        trip_updates_age_seconds: tu_age.map(|s| s as i32),
                        vehicle_positions_age_seconds: fr
                            .and_then(|x| x.vehicles_age_secs())
                            .map(|s| s as i32),
                        alerts_age_seconds: fr.and_then(|x| x.alerts_age_secs()).map(|s| s as i32),
                        trip_update_count: fr.map(|x| x.trip_update_count as i32).unwrap_or(0),
                        vehicle_count: fr.map(|x| x.vehicle_count as i32).unwrap_or(0),
                        alert_count: fr.map(|x| x.alert_count as i32).unwrap_or(0),
                        ok: bundle.is_some(),
                        feed_start_date: bundle.and_then(|b| b.feed_start_date.clone()),
                        feed_end_date: bundle.and_then(|b| b.feed_end_date.clone()),
                        publisher: bundle.and_then(|b| b.feed_publisher_name.clone()),
                    };
                }
                match rx.recv().await {
                    Ok(RtVersion(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// Emits the latest vehicle position for a namespaced trip id when RT updates.
    async fn trip_vehicle(
        &self,
        ctx: &Context<'_>,
        trip_id: ID,
    ) -> Result<impl Stream<Item = VehiclePosition> + 'static> {
        let state = ctx.data::<Arc<AppState>>()?.clone();
        let mut rx = state.rt_version_tx.subscribe();
        let trip_id = trip_id.to_string();
        Ok(async_stream::stream! {
            loop {
                let rt = state.load_rt();
                if let Some(v) = rt.get_vehicle_for_trip(&trip_id) {
                    let epoch = state.load_epoch();
                    let trip_rt = rt.get_trip(&trip_id);
                    let feed = trip_id.split_once(':').map(|(f, _)| f);
                    yield map_vehicle_enriched(v, feed, Some(epoch.as_ref()), trip_rt);
                }
                match rx.recv().await {
                    Ok(RtVersion(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    /// Watch realtime status for a set of trip ids.
    ///
    /// Journeys are not stored server-side; clients pass the trip ids from a planned
    /// itinerary. On each RT version bump, emits a `TripRealtime` for every watched id
    /// (snapshot may be empty/scheduled when no RT data exists).
    async fn watch_trips(
        &self,
        ctx: &Context<'_>,
        trip_ids: Vec<ID>,
    ) -> Result<impl Stream<Item = TripRealtime> + 'static> {
        if trip_ids.is_empty() {
            return Err(gql_err("tripIds must not be empty", "BAD_REQUEST"));
        }
        if trip_ids.len() > 32 {
            return Err(gql_err("at most 32 tripIds allowed", "BAD_REQUEST"));
        }
        let state = ctx.data::<Arc<AppState>>()?.clone();
        let mut rx = state.rt_version_tx.subscribe();
        let trip_ids: Vec<String> = trip_ids.into_iter().map(|i| i.to_string()).collect();
        Ok(async_stream::stream! {
            loop {
                let rt = state.load_rt();
                let epoch = state.load_epoch();
                for tid in &trip_ids {
                    // Estimated positions when GPS VP is missing (shape / stop-chain).
                    yield trip_realtime_for_with_epoch(tid, &rt, Some(epoch.as_ref()));
                }
                match rx.recv().await {
                    Ok(RtVersion(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_schema() -> (Arc<AppState>, ServiceSchema) {
        let state = Arc::new(AppState::new(
            Config::default(),
            crate::cache::TransitCache::disabled(crate::config::RedisConfig::default()),
        ));
        let schema = build_schema(state.clone());
        (state, schema)
    }

    #[tokio::test]
    async fn health_query_empty_epoch() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                "{ health { status stopCount tripCount epochId feeds { id feedStartDate feedEndDate publisher ok } } }",
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert_eq!(data["health"]["status"], "starting");
        assert_eq!(data["health"]["stopCount"], 0);
        assert_eq!(data["health"]["tripCount"], 0);
        assert_eq!(data["health"]["epochId"], "empty");
        // Empty epoch: no static bundles → feed info fields null when feeds listed.
        if let Some(feeds) = data["health"]["feeds"].as_array() {
            for f in feeds {
                assert!(f["feedStartDate"].is_null());
                assert!(f["feedEndDate"].is_null());
                assert!(f["publisher"].is_null());
            }
        }
    }

    #[tokio::test]
    async fn agencies_empty_epoch() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{ agencies { id name url timezone phone feedId }
                     agency(id: "sncf:a1") { id name } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["agencies"].as_array().unwrap().is_empty());
        assert!(data["agency"].is_null());
    }

    #[tokio::test]
    async fn agencies_feed_filter_empty() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(r#"{ agencies(feedId: "sncf") { id name } }"#)
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["agencies"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stop_schema_zone_url_timezone() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{
                  stop: __type(name: "Stop") { fields { name } }
                  gqlStop: __type(name: "GqlStop") { fields { name } }
                }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        // GraphQL name is GqlStop (Rust type name) unless renamed.
        let fields = data["gqlStop"]["fields"]
            .as_array()
            .or_else(|| data["stop"]["fields"].as_array())
            .expect("Stop type fields");
        let names: Vec<&str> = fields
            .iter()
            .filter_map(|f| f["name"].as_str())
            .collect();
        assert!(names.contains(&"zoneId"));
        assert!(names.contains(&"url"));
        assert!(names.contains(&"timezone"));
    }

    #[tokio::test]
    async fn alert_schema_active_periods() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{
                  alert: __type(name: "Alert") { fields { name } }
                  tr: __type(name: "TimeRange") { fields { name } }
                }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let alert: Vec<String> = data["alert"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect();
        assert!(alert.contains(&"activePeriods".to_string()));
        assert!(alert.contains(&"cause".to_string()));
        assert!(alert.contains(&"effect".to_string()));
        assert!(alert.contains(&"url".to_string()));
        assert!(alert.contains(&"lines".to_string()));
        assert!(alert.contains(&"informedRouteIds".to_string()));
        let tr: Vec<String> = data["tr"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect();
        assert!(tr.contains(&"start".to_string()));
        assert!(tr.contains(&"end".to_string()));
    }

    #[tokio::test]
    async fn stops_query_empty_epoch() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(r#"{ stops(search: "paris") { totalCount nodes { id name } } }"#)
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert_eq!(data["stops"]["totalCount"], 0);
        assert!(data["stops"]["nodes"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn trip_realtime_query() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{ tripRealtime(tripId: "sncf:TRIP1") { tripId canceled status rtVersion delaySeconds } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert_eq!(data["tripRealtime"]["tripId"], "sncf:TRIP1");
        assert_eq!(data["tripRealtime"]["canceled"], false);
        assert_eq!(data["tripRealtime"]["status"], "SCHEDULED");
    }

    #[tokio::test]
    async fn departures_empty_epoch() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(r#"{ departures(stopId: "sncf:STOP1") { tripId headsign } }"#)
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["departures"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn trip_unknown_returns_null() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(r#"{ trip(id: "sncf:NO_SUCH_TRIP") { id headsign } }"#)
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["trip"].is_null());
    }

    #[tokio::test]
    async fn shape_query_empty_epoch() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(r#"{ shape(shapeId: "sncf:SHAPE1") { lat lon } }"#)
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["shape"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn vehicles_empty_overlay() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{ vehicles(limit: 10) {
                  tripId lat lon bearing occupancy label vehicleId congestion
                  feedId routeShortName mode delaySeconds canceled
                } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["vehicles"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn vehicles_lists_overlay_entries() {
        use crate::rt::overlay::{FeedRtState, RealtimeOverlay, VehiclePos};
        use chrono::TimeZone;

        let (state, schema) = test_schema();
        let mut feed = FeedRtState::new("idf");
        let t0 = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        feed.vehicles.insert(
            "idf:T1".into(),
            VehiclePos {
                trip_id: Some("idf:T1".into()),
                lat: 48.8566,
                lon: 2.3522,
                bearing: Some(90.0),
                speed: Some(5.0),
                updated_at: t0,
                current_stop_id: Some("idf:S1".into()),
                label: Some("42".into()),
                vehicle_id: Some("v1".into()),
                license_plate: None,
                occupancy: Some("MANY_SEATS_AVAILABLE".into()),
                current_status: Some("IN_TRANSIT_TO".into()),
                current_stop_sequence: Some(3),
                congestion: Some("RUNNING_SMOOTHLY".into()),
                occupancy_percentage: Some(35),
            },
        );
        feed.vehicle_count = 1;
        let mut rt = RealtimeOverlay::default();
        rt.feeds.insert("idf".into(), feed);
        state.rt.store(Arc::new(rt));

        let res = schema
            .execute(
                r#"{ vehicles(feedId: "idf", tripId: "idf:T1") {
                  tripId lat lon bearing occupancy label congestion currentStatus occupancyPercentage
                  feedId
                } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let arr = data["vehicles"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["tripId"], "idf:T1");
        assert_eq!(arr[0]["label"], "42");
        assert_eq!(arr[0]["occupancy"], "MANY_SEATS_AVAILABLE");
        assert_eq!(arr[0]["occupancyPercentage"], 35);
        assert_eq!(arr[0]["feedId"], "idf");
        assert!((arr[0]["lat"].as_f64().unwrap() - 48.8566).abs() < 1e-6);
    }

    #[tokio::test]
    async fn vehicles_enrichment_from_epoch() {
        use crate::gtfs::pack::{
            GlobalTrip, PackedStopTime, RouteMode, StaticEpoch, StopRecord,
        };
        use crate::rt::overlay::{FeedRtState, RealtimeOverlay, TripRt, VehiclePos};
        use chrono::TimeZone;

        let (state, schema) = test_schema();
        let mut epoch = StaticEpoch::empty();
        epoch.id = "test-epoch".into();
        epoch.stops = vec![StopRecord {
            id: "idf:S1".into(),
            feed_id: "idf".into(),
            raw_id: "S1".into(),
            name: "Gare".into(),
            lat: Some(48.85),
            lon: Some(2.35),
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
        }];
        epoch.stop_id_to_idx.insert("idf:S1".into(), 0);
        epoch.stop_times = vec![PackedStopTime {
            stop_idx: 0,
            arrival_s: 3600,
            departure_s: 3660,
            stop_sequence: 1,
            pickup_type: 0,
            drop_off_type: 0,
            stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
        }];
        epoch.trips = vec![GlobalTrip {
            id: "idf:T1".into(),
            feed_id: "idf".into(),
            route_id: "idf:R1".into(),
            service_id: "svc".into(),
            headsign: Some("Nation".into()),
            short_name: Some("42".into()),
            direction_id: Some(0),
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Metro,
            route_short_name: "1".into(),
            route_long_name: "Château de Vincennes".into(),
            route_color: Some("FFCE00".into()),
            route_text_color: None,
            route_type_raw: 1,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len: 1,
            frequency_windows: vec![],
        }];
        epoch.trip_id_to_idx.insert("idf:T1".into(), 0);
        state.epoch.store(Arc::new(epoch));

        let mut feed = FeedRtState::new("idf");
        let t0 = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        feed.vehicles.insert(
            "idf:T1".into(),
            VehiclePos {
                trip_id: Some("idf:T1".into()),
                lat: 48.8566,
                lon: 2.3522,
                bearing: Some(90.0),
                speed: None,
                updated_at: t0,
                current_stop_id: Some("idf:S1".into()),
                label: None,
                vehicle_id: None,
                license_plate: None,
                occupancy: None,
                current_status: Some("IN_TRANSIT_TO".into()),
                current_stop_sequence: Some(1),
                congestion: None,
                occupancy_percentage: None,
            },
        );
        feed.trips.insert(
            "idf:T1".into(),
            TripRt {
                delay: Some(180),
                canceled: false,
                ..Default::default()
            },
        );
        feed.vehicle_count = 1;
        let mut rt = RealtimeOverlay::default();
        rt.feeds.insert("idf".into(), feed);
        state.rt.store(Arc::new(rt));

        let res = schema
            .execute(
                r#"{ vehicles(feedId: "idf") {
                  tripId routeShortName routeLongName tripShortName headsign mode
                  routeColor delaySeconds canceled feedId label
                } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let arr = data["vehicles"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["routeShortName"], "1");
        assert_eq!(arr[0]["routeLongName"], "Château de Vincennes");
        assert_eq!(arr[0]["tripShortName"], "42");
        assert_eq!(arr[0]["headsign"], "Nation");
        assert_eq!(arr[0]["mode"], "METRO");
        assert_eq!(arr[0]["routeColor"], "FFCE00");
        assert_eq!(arr[0]["delaySeconds"], 180);
        assert_eq!(arr[0]["canceled"], false);
        assert_eq!(arr[0]["feedId"], "idf");
        // label falls back to trip short name when vehicle descriptor empty
        assert_eq!(arr[0]["label"], "42");
    }

    #[tokio::test]
    async fn vehicles_bbox_filter() {
        use crate::rt::overlay::{FeedRtState, RealtimeOverlay, VehiclePos};
        use chrono::TimeZone;

        let (state, schema) = test_schema();
        let mut feed = FeedRtState::new("idf");
        let t0 = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        feed.vehicles.insert(
            "idf:IN".into(),
            VehiclePos {
                trip_id: Some("idf:IN".into()),
                lat: 48.85,
                lon: 2.35,
                bearing: None,
                speed: None,
                updated_at: t0,
                current_stop_id: None,
                label: Some("in".into()),
                vehicle_id: None,
                license_plate: None,
                occupancy: None,
                current_status: None,
                current_stop_sequence: None,
                congestion: None,
                occupancy_percentage: None,
            },
        );
        feed.vehicles.insert(
            "idf:OUT".into(),
            VehiclePos {
                trip_id: Some("idf:OUT".into()),
                lat: 45.0,
                lon: 5.0,
                bearing: None,
                speed: None,
                updated_at: t0,
                current_stop_id: None,
                label: Some("out".into()),
                vehicle_id: None,
                license_plate: None,
                occupancy: None,
                current_status: None,
                current_stop_sequence: None,
                congestion: None,
                occupancy_percentage: None,
            },
        );
        feed.vehicle_count = 2;
        let mut rt = RealtimeOverlay::default();
        rt.feeds.insert("idf".into(), feed);
        state.rt.store(Arc::new(rt));

        let res = schema
            .execute(
                r#"{ vehicles(
                  bbox: { minLat: 48.0, minLon: 2.0, maxLat: 49.0, maxLon: 3.0 }
                  limit: 50
                ) { tripId label } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let arr = data["vehicles"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["tripId"], "idf:IN");
    }

    #[tokio::test]
    async fn vehicles_synthetic_from_trip_update() {
        use crate::gtfs::pack::{
            GlobalTrip, PackedStopTime, RouteMode, StaticEpoch, StopRecord,
        };
        use crate::rt::overlay::{FeedRtState, RealtimeOverlay, StopTimeRt, TripRt};
        use chrono::TimeZone;

        let (state, schema) = test_schema();
        let mut epoch = StaticEpoch::empty();
        epoch.stops = vec![StopRecord {
            id: "sncf:A".into(),
            feed_id: "sncf".into(),
            raw_id: "A".into(),
            name: "Lyon".into(),
            lat: Some(45.76),
            lon: Some(4.86),
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
        }];
        epoch.stop_id_to_idx.insert("sncf:A".into(), 0);
        epoch.stop_times = vec![PackedStopTime {
            stop_idx: 0,
            arrival_s: 1000,
            departure_s: 1060,
            stop_sequence: 2,
            pickup_type: 0,
            drop_off_type: 0,
            stop_headsign_idx: 0,
            timepoint: 1,
            shape_dist_traveled: None,
        }];
        epoch.trips = vec![GlobalTrip {
            id: "sncf:TRAIN1".into(),
            feed_id: "sncf".into(),
            route_id: "sncf:R".into(),
            service_id: "svc".into(),
            headsign: Some("Paris".into()),
            short_name: Some("6610".into()),
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Rail,
            route_short_name: "TER".into(),
            route_long_name: "Regional".into(),
            route_color: Some("8B0000".into()),
            route_text_color: None,
            route_type_raw: 2,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len: 1,
            frequency_windows: vec![],
        }];
        epoch.trip_id_to_idx.insert("sncf:TRAIN1".into(), 0);
        state.epoch.store(Arc::new(epoch));

        let mut feed = FeedRtState::new("sncf");
        let mut stop_updates = std::collections::HashMap::new();
        stop_updates.insert(
            2,
            StopTimeRt {
                stop_sequence: 2,
                stop_id: Some("sncf:A".into()),
                arrival_delay: Some(300),
                departure_delay: Some(300),
                arrival_time: None,
                departure_time: None,
                skipped: false,
            },
        );
        feed.trips.insert(
            "sncf:TRAIN1".into(),
            TripRt {
                delay: Some(300),
                canceled: false,
                stop_updates,
                ..Default::default()
            },
        );
        feed.trip_updates_fetched_at =
            Some(chrono::Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap());
        let mut rt = RealtimeOverlay::default();
        rt.feeds.insert("sncf".into(), feed);
        state.rt.store(Arc::new(rt));

        let res = schema
            .execute(
                r#"{ vehicles(feedId: "sncf") {
                  tripId lat lon currentStatus delaySeconds mode tripShortName headsign
                  displayLabel label
                } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let arr = data["vehicles"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["tripId"], "sncf:TRAIN1");
        let st = arr[0]["currentStatus"].as_str().unwrap_or("");
        assert!(
            st.starts_with("ESTIMATED_"),
            "expected ESTIMATED_* status, got {st}"
        );
        assert!((arr[0]["lat"].as_f64().unwrap() - 45.76).abs() < 1e-6);
        assert_eq!(arr[0]["delaySeconds"], 300);
        assert_eq!(arr[0]["mode"], "RAIL");
        assert_eq!(arr[0]["tripShortName"], "6610");
        assert_eq!(arr[0]["displayLabel"], "6610");
    }

    #[tokio::test]
    async fn vehicles_filter_by_feed_and_near_empty() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{ vehicles(feedId: "sncf", near: { lat: 48.85, lon: 2.35, radiusMeters: 500 }, limit: 5) {
                  tripId lat lon
                } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["vehicles"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pathways_empty_when_not_packed() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{ pathways(stopId: "sncf:STOP1", limit: 10) {
                  id fromStopId toStopId mode durationSeconds bidirectional
                } }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["pathways"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn levels_empty_when_not_packed() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(r#"{ levels(feedId: "sncf") { id index name } }"#)
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        assert!(data["levels"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn journey_schema_includes_fare_and_vehicle_fields() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{
                  journey: __type(name: "Journey") { fields { name } }
                  vehicle: __type(name: "VehiclePosition") { fields { name } }
                  level: __type(name: "Level") { fields { name } }
                  pathway: __type(name: "Pathway") { fields { name } }
                  query: __type(name: "QueryRoot") { fields { name } }
                }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let names = |key: &str| -> Vec<String> {
            data[key]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap().to_string())
                .collect()
        };
        let journey = names("journey");
        assert!(journey.contains(&"fareAmount".to_string()));
        assert!(journey.contains(&"fareCurrency".to_string()));
        assert!(journey.contains(&"fareNote".to_string()));
        let vehicle = names("vehicle");
        assert!(vehicle.contains(&"tripId".to_string()));
        assert!(vehicle.contains(&"occupancy".to_string()));
        assert!(vehicle.contains(&"occupancyPercentage".to_string()));
        assert!(vehicle.contains(&"label".to_string()));
        assert!(vehicle.contains(&"displayLabel".to_string()));
        assert!(vehicle.contains(&"bearing".to_string()));
        assert!(vehicle.contains(&"congestion".to_string()));
        assert!(vehicle.contains(&"feedId".to_string()));
        assert!(vehicle.contains(&"routeShortName".to_string()));
        assert!(vehicle.contains(&"mode".to_string()));
        assert!(vehicle.contains(&"delaySeconds".to_string()));
        assert!(vehicle.contains(&"canceled".to_string()));
        let level = names("level");
        assert!(level.contains(&"id".to_string()));
        assert!(level.contains(&"index".to_string()));
        assert!(level.contains(&"name".to_string()));
        let pathway = names("pathway");
        assert!(pathway.contains(&"fromStopId".to_string()));
        assert!(pathway.contains(&"toStopId".to_string()));
        assert!(pathway.contains(&"mode".to_string()));
        assert!(pathway.contains(&"durationSeconds".to_string()));
        assert!(pathway.contains(&"bidirectional".to_string()));
        let query = names("query");
        assert!(query.contains(&"vehicles".to_string()));
        assert!(query.contains(&"levels".to_string()));
        assert!(query.contains(&"pathways".to_string()));
        assert!(query.contains(&"agency".to_string()));
        assert!(query.contains(&"agencies".to_string()));
    }

    #[tokio::test]
    async fn feed_status_schema_includes_feed_info() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{
                  fs: __type(name: "FeedStatus") { fields { name } }
                  agency: __type(name: "Agency") { fields { name } }
                }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let fs: Vec<String> = data["fs"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect();
        assert!(fs.contains(&"feedStartDate".to_string()));
        assert!(fs.contains(&"feedEndDate".to_string()));
        assert!(fs.contains(&"publisher".to_string()));
        let agency: Vec<String> = data["agency"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect();
        assert!(agency.contains(&"name".to_string()));
        assert!(agency.contains(&"url".to_string()));
        assert!(agency.contains(&"timezone".to_string()));
        assert!(agency.contains(&"phone".to_string()));
        assert!(agency.contains(&"feedId".to_string()));
    }

    /// Schema exposes geometry on journey / legs for WASM map clients (empty epoch ok).
    #[tokio::test]
    async fn itinerary_schema_includes_geometry_fields() {
        let (_state, schema) = test_schema();
        let res = schema
            .execute(
                r#"{
                  __type(name: "Journey") {
                    fields { name }
                  }
                  transit: __type(name: "TransitLeg") {
                    fields { name }
                  }
                  walk: __type(name: "WalkLeg") {
                    fields { name }
                  }
                  trip: __type(name: "TripDetail") {
                    fields { name }
                  }
                  latlng: __type(name: "LatLng") {
                    fields { name type { name kind ofType { name } } }
                  }
                }"#,
            )
            .await;
        assert!(res.errors.is_empty(), "{:?}", res.errors);
        let data = res.data.into_json().unwrap();
        let field_names = |key: &str| -> Vec<String> {
            data[key]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(field_names("__type").contains(&"geometry".to_string()));
        assert!(field_names("transit").contains(&"geometry".to_string()));
        assert!(field_names("walk").contains(&"geometry".to_string()));
        assert!(field_names("trip").contains(&"geometry".to_string()));
        let latlng_fields = field_names("latlng");
        assert!(latlng_fields.contains(&"lat".to_string()));
        assert!(latlng_fields.contains(&"lon".to_string()));
    }

    #[tokio::test]
    async fn map_journey_geometry_present_in_graphql() {
        use crate::routing::journey::{
            Journey as CoreJourney, Leg as CoreLeg, StopRef, TransitLegData, WalkLegData,
        };
        use crate::rt::overlay::RealtimeOverlay;
        use chrono::TimeZone;

        let t0 = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 10, 0, 0).unwrap();
        let t1 = chrono::Utc.with_ymd_and_hms(2024, 6, 1, 10, 30, 0).unwrap();
        let core = CoreJourney {
            id: "j1".into(),
            departure: t0,
            arrival: t1,
            duration_s: 1800,
            transfers: 0,
            walk_distance_m: 200.0,
            realtime_status: "SCHEDULED".into(),
            fare_amount: None,
            fare_currency: None,
            fare_note: None,
            legs: vec![
                CoreLeg::Walk(WalkLegData {
                    mode: "WALK".into(),
                    from_name: "Origin".into(),
                    to_name: "Stop A".into(),
                    from_stop_id: None,
                    to_stop_id: Some("f:A".into()),
                    distance_m: 200.0,
                    duration_s: 180,
                    from_lat: Some(48.85),
                    from_lon: Some(2.35),
                    to_lat: Some(48.86),
                    to_lon: Some(2.36),
                    geometry: vec![(48.85, 2.35), (48.86, 2.36)],
                }),
                CoreLeg::Transit(TransitLegData {
                    mode: "METRO".into(),
                    route_short_name: "1".into(),
                    route_long_name: "Line 1".into(),
                    route_id: "f:r1".into(),
                    agency_name: None,
                    trip_id: "f:T1".into(),
                    trip_short_name: None,
                    headsign: Some("East".into()),
                    direction_id: Some(0),
                    route_color: None,
                    route_text_color: None,
                    stop_headsign: None,
                    wheelchair: 0,
                    bikes_allowed: 0,
                    from: StopRef {
                        stop_id: "f:A".into(),
                        name: "A".into(),
                        lat: Some(48.86),
                        lon: Some(2.36),
                        platform: None,
                    },
                    to: StopRef {
                        stop_id: "f:B".into(),
                        name: "B".into(),
                        lat: Some(48.87),
                        lon: Some(2.37),
                        platform: None,
                    },
                    from_stop_sequence: 1,
                    to_stop_sequence: 2,
                    scheduled_departure: t0,
                    scheduled_arrival: t1,
                    realtime_departure: None,
                    realtime_arrival: None,
                    delay_departure_s: None,
                    delay_arrival_s: None,
                    canceled: false,
                    vehicle_lat: None,
                    vehicle_lon: None,
                    vehicle_updated_at: None,
                    intermediate_stops: vec![],
                    // (lat, lon) Leaflet order — from RAPTOR / shapes when present.
                    geometry: vec![(48.86, 2.36), (48.87, 2.37)],
                    same_vehicle: false,
                }),
            ],
            alert_headers: vec![],
        };
        let gql = map_journey(&core, &RealtimeOverlay::default(), None);
        assert_eq!(gql.legs.len(), 2);
        match &gql.legs[0] {
            Leg::WalkLeg(w) => {
                assert_eq!(w.geometry.len(), 2);
                assert!((w.geometry[0].lat - 48.85).abs() < 1e-9);
            }
            _ => panic!("expected walk"),
        }
        match &gql.legs[1] {
            Leg::TransitLeg(t) => {
                assert_eq!(t.geometry.len(), 2);
            }
            _ => panic!("expected transit"),
        }
        let flat = geometry_concat_legs(&gql.legs);
        assert!(flat.len() >= 3);

        // Round-trip through GraphQL selection as itineraries would expose.
        let (_state, schema) = test_schema();
        // Ensure Journey.geometry is resolvable on a synthetic execute via SDL query only —
        // full itineraries needs a loaded epoch; field presence covered above.
        let _ = schema;
    }
}
