# Product gap: open data vs `transit` code

Audit date: **2026-07-27** (post “fix all remaining gaps” agent wave).  
Sources: current `src/gtfs`, `src/routing`, `src/rt`, `src/api`, `web/`.

## Legend

| Status | Meaning |
|--------|---------|
| **Used** | Parsed and affects routing and/or GraphQL / map |
| **Partial** | Present but limited fidelity |
| **Missing / out of scope** | Not productized |

---

## Static GTFS

| Field / file | Status |
|--------------|--------|
| Core stops, routes, trips, stop_times, calendar, transfers | **Used** |
| stop_code, stop_desc, wheelchair_boarding | **Used** |
| zone_id, stop_url, stop_timezone | **Used** (packed + GraphQL `zoneId` / `url` / `timezone`) |
| route colors, trip_short_name, direction, headsign, stop_headsign | **Used** |
| wheelchair / bikes on trips | **Used** |
| block_id → sameVehicle | **Used** |
| shapes + geometry | **Used** |
| frequencies | **Used** |
| pathways / levels → walk edges | **Used** (wheelchair soft-penalizes stairs/escalators) |
| fare_attributes / fare_rules | **Used** (route + zone OD + contains; best-effort) |
| agency name/url/timezone/phone | **Used** (`agency` / `agencies` queries) |
| feed_info start/end/publisher/lang | **Used** (on FeedStatus / health) |
| timepoint, shape_dist_traveled | **Partial** (packed; geometry still stop/shape based) |
| route_desc / route_url | **Partial** (on RouteRecord; not always on legs) |
| Ticketing / NeTEx / GBFS | **Out of scope** |

---

## GTFS-RT

| Field | Status |
|-------|--------|
| Trip updates, cancel, skip, absolute times | **Used** |
| Vehicle position, bearing, occupancy, label, status | **Used** |
| occupancy_percentage | **Used** (GraphQL) |
| Alert cause/effect/url + **all** active_periods | **Used** |
| TripDescriptor route_id / direction / start_time | **Used** (match without trip_id when possible) |
| Multi-carriage detail beyond avg occupancy | **Partial** |

---

## Routing & product API

| Feature | Status |
|---------|--------|
| Multimodal RAPTOR + RT enrich | **Used** |
| Departures board, trip detail, shape, vehicles | **Used** |
| Arrive-by + Pareto k-best (McRAPTOR-lite) | **Used** |
| Zone-aware fares | **Used** |
| Wheelchair pathway preference | **Used** |
| Optional OSRM foot routing (`osrm_url`) | **Used** (disabled by default) |
| PAN multi-feed helper script + IDFM config comments | **Used** |
| OSM full offline graph | **Missing** (use OSRM or haversine) |
| Ticket purchase | **Out of scope** |
| Indoor 3D level UI | **Missing** (2D map + GraphQL levels/pathways only) |
| Bulk “all vehicles” WebSocket | **Partial** (per-trip WS + poll; not global fan-out) |

---

## WASM map (`web/`)

| Feature | Status |
|---------|--------|
| Search, plan, polylines, icons, departures | **Done** |
| FR UI, fare badge, same-vehicle, arrive-by, wheelchair | **Done** |
| Live vehicles (poll + optional WS) | **Done** |
| Indoor floor plans | **Missing** |

---

## Still open (honest residual)

1. **Offline OSM street graph** — no local pbf router; optional public OSRM only.  
2. **Indoor 3D / multi-floor map UI** — data + GraphQL only.  
3. **route_desc / route_url on TransitLeg** — on routes pack, not denormalized to every leg.  
4. **timepoint / shape_dist** unused for slicing polish.  
5. **Validated commercial fares / booking** — open-data estimate only.  
6. **Auth, rate limits, production multi-region ops**.  
7. **Global vehicle WS channel** — map uses poll + per-trip subscription.

---

## Agent waves (closed)

| Wave | Scope |
|------|--------|
| P0–P1 | Core model, RT, RAPTOR, GraphQL product |
| P2 | Shapes, frequencies, block, map geometry, WASM, icons |
| P3 | Pathways, fares, live vehicles |
| Gap-fix | Zones fares, wheelchair paths, arrive-by, RT match, OSRM, GraphQL polish, FR UI, doc rewrite |
