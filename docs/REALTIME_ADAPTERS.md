# Realtime adapters: GTFS-RT & SIRI → app model

## Goal

Ingest **raw** industry formats only (no Navitia JSON):

| Wire format | Detection | Output |
|-------------|-----------|--------|
| GTFS-RT protobuf | binary / non-SIRI | `FeedRtState` trips / vehicles / alerts |
| SIRI Estimated Timetable (JSON) | `EstimatedTimetableDelivery` | trip updates (expected times, delays) |
| SIRI Stop Monitoring (JSON) | `StopMonitoringDelivery` | trips + optional GPS vehicles |
| SIRI General Message (JSON) | `GeneralMessageDelivery` | service alerts |

## API

```rust
use transit::rt::{ingest_realtime, RealtimeFormat, merge_delta_into};

let delta = ingest_realtime("idfm", &bytes, RealtimeFormat::Auto)?;
// or force: RealtimeFormat::SiriEstimatedTimetable / GtfsRt / …

let mut state = FeedRtState::new("idfm");
merge_delta_into(&mut state, &delta);
```

## PRIM (Île-de-France)

With `IDFM_PRIM_API_KEY` in `.env`:

- Continuous **SIRI Lite** pollers (quota-aware):
  - **SM** `GET /marketplace/stop-monitoring?MonitoringRef=…` — next arrivals (warm seeds + interest set; budget soft ~800k/day of 1M)
  - **ET** `GET /marketplace/estimated-timetable?LineRef=…` — multi-line sample per tick (budget soft ~900/day of 1k)
  - **GM** `GET /marketplace/general-message?LineRef=…` — screen / traffic messages (budget ~18k/day of 20k)
- Built-in **~75 LineRefs** (all metro, RER A–E, major Transilien, tram T1–T14, core Paris bus) and **~24 hub MonitoringRefs** (expand via `line_refs` / `seed_monitoring_refs`).
- **ET batching:** `et_lines_per_tick` (default 3) is a cap; each cycle uses  
  `min(cap, ceil(remaining_budget / ticks_left_in_UTC_day))` so the soft budget is paced, not front-loaded.
- **SM batching:** each cycle polls up to `sm_max_stops_per_cycle` (default 24), also paced (~80% of soft budget so GraphQL live keeps headroom).
- **SM interest registration:**
  - GraphQL `departures(… live)` → on-demand SM + `register_sm_interest`
  - GraphQL `vehicles(bbox|near)` → registers nearby stations (no extra PRIM call on that query; warm poller refreshes them)
- Partial samples **merge** into `RealtimeOverlay` feed `idfm` (see `FeedEvent::merge`). Absolute expected times from SIRI feed the map estimate path.
- **GTFS-RT marketplace URLs return 403** on many keys — use SIRI ET instead.
- Static GTFS: public zip  
  `https://eu.ftp.opendatasoft.com/stif/GTFS/IDFM-gtfs.zip`  
  or data.gouv `…/r/413988ed-d340-467b-8be2-7b999fcd207a`

### Default quota math (24h continuous)

| API | Soft budget | Interval | Cap / cycle | Approx. samples / day | Coverage |
|-----|-------------|----------|-------------|----------------------|----------|
| ET  | 900         | 90s      | ≤3 lines    | ~900 line-fetches    | ~75 lines × ~12×/day (~2h cadence) |
| SM  | 800_000     | 20s      | ≤24 stops   | tens of k (paced)    | seeds + live/viewport interest |
| GM  | 18_000      | 45s      | 1 line      | ~1_920               | same LineRef list round-robin |

## Feed config for GTFS-RT (when you have a working URL)

```toml
[[feeds]]
id = "idfm"
static_url = "https://eu.ftp.opendatasoft.com/stif/GTFS/IDFM-gtfs.zip"
trip_updates_url = "https://…"   # GTFS-RT protobuf
vehicle_positions_url = "https://…"
service_alerts_url = "https://…"
auth_header = "apikey"
auth_env = "IDFM_PRIM_API_KEY"
```

The feed RT poller uses `ingest_realtime(..., Auto)` so the same path accepts SIRI JSON if the URL returns SIRI.

## Files

- `src/rt/adapter/mod.rs` — entry + merge
- `src/rt/adapter/detect.rs` — sniff format
- `src/rt/adapter/gtfs_rt.rs` — protobuf
- `src/rt/adapter/siri.rs` — ET / SM / GM JSON
- `src/prim/poller.rs` — SIRI-only PRIM pollers
