# transit

**Île-de-France & France multimodal transit** — journey planning, live network map, and estimated vehicle positions, built entirely from **raw open data**. No Navitia, Hove.io, or other commercial journey APIs.

<p align="center">
  <a href="https://github.com/princefr/transit/raw/main/docs/demo.mp4">
    <img src="docs/demo-poster.png" alt="Transit demo — click to play video" width="800" />
  </a>
</p>

<p align="center"><strong><a href="docs/demo.mp4">▶ Watch demo</a></strong> (2½ min) · <a href="https://github.com/princefr/transit/raw/main/docs/demo.mp4">raw MP4</a></p>

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

---

## What it does

- **Multimodal routing** — RAPTOR over GTFS static schedules with walk transfers, hub links, and optional OSRM street geometry
- **Realtime overlay** — trip delays, cancellations, alerts, and vehicle positions from GTFS-RT and PRIM SIRI Lite (StopMonitoring, EstimatedTimetable, GeneralMessage)
- **Position estimation** — when GPS is unavailable, vehicles are interpolated along GTFS shapes using schedule + realtime progress
- **GraphQL API** — stops, departures, trip detail, itineraries, live vehicles, subscriptions
- **Web map (WASM + Leaflet)** — search stops, plan journeys, view polylines and live vehicles on an IDFM-style map

Data sources include [Île-de-France Mobilités](https://prim.iledefrance-mobilites.fr/) (GTFS + PRIM SIRI), [SNCF open data](https://transport.data.gouv.fr), and the [transport.data.gouv.fr](https://transport.data.gouv.fr) PAN. **Transit data** remains under each producer's licence (often ODbL); **application code** is MIT.

---

## Architecture

Everything is built from first principles on public feeds — no third-party routing SaaS.

```
  Open data feeds                    Processing pipeline                    Clients
 ┌─────────────────┐               ┌──────────────────────────┐          ┌─────────────┐
 │ IDFM GTFS       │──download──►  │ Parse & pack static    │          │ GraphQL API │
 │ SNCF GTFS       │               │ epoch (ArcSwap swap)   │──RAPTOR──►│ (port 8080) │
 │ PRIM SIRI Lite  │──poll──────►  │ Realtime overlay       │          │             │
 │ GTFS-RT (opt.)  │               │ Vehicle estimation     │          │ WASM map UI │
 └─────────────────┘               └──────────────────────────┘          │ (port 8088) │
                                                                          └─────────────┘
```

1. **Download / parse GTFS** — conditional fetch (ETag/hash), streaming parse, horizon filter for large feeds
2. **Pack static epoch** — stops, trips, stop times, shapes, pathways, hub links (grid spatial index)
3. **RAPTOR routing** — multimodal legs with walk access/egress and transfer graph
4. **Realtime overlay** — concurrent Tokio pollers merge SIRI StopMonitoring / EstimatedTimetable / TripUpdates into the live view
5. **Vehicle estimation** — shape-aware interpolation when positions are missing
6. **GraphQL + WASM frontend** — API serves queries; Trunk-built map UI talks to the same backend

See also [`docs/IDFM_DAILY.md`](docs/IDFM_DAILY.md), [`docs/REDIS_CACHE.md`](docs/REDIS_CACHE.md), and [`docs/IDFM_VEHICLE_MONITORING.md`](docs/IDFM_VEHICLE_MONITORING.md).

---

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| **Rust** 1.75+ | `rustup` recommended |
| **RAM** 8–16 GB+ | IDFM + SNCF together is memory-heavy |
| **Disk** ~2 GB+ | GTFS zips cached under `./data/` |
| **`IDFM_PRIM_API_KEY`** | Required for IDFM realtime SIRI Lite ([PRIM marketplace](https://prim.iledefrance-mobilites.fr/)) |
| **`DATASETS_API_KEY`** or **`DATAGOUV_API_KEY`** | Required when [data.gouv.fr](https://www.data.gouv.fr/) / dataset portals expect `X-API-KEY` for GTFS downloads (`make sync-idfm`, `daily_sync.sh`); set one alias, not both |
| **Redis** (optional) | Response cache for search / vehicles / itineraries |
| **OSRM** (optional) | Street-level walk geometry; public demo URL in config |

---

## Quick start

### 1. Clone & configure

```bash
git clone https://github.com/princefr/transit.git
cd transit

make setup          # wasm target, scripts, .env from .env.example
```

Edit `.env` and set API keys (see [`.env.example`](.env.example)):

```bash
# PRIM SIRI Lite — live IDFM delays, vehicles, alerts
IDFM_PRIM_API_KEY=your_prim_api_key_here

# data.gouv.fr / transport.data.gouv.fr GTFS downloads (X-API-KEY header)
# Use DATASETS_API_KEY or DATAGOUV_API_KEY (aliases; first wins in scripts)
DATASETS_API_KEY=your_datasets_api_key_here
```

| Variable | Used for | Where to get it |
|----------|----------|-----------------|
| `IDFM_PRIM_API_KEY` | PRIM SIRI Lite + optional GTFS-RT (`apikey` header) | [prim.iledefrance-mobilites.fr](https://prim.iledefrance-mobilites.fr/) marketplace |
| `DATASETS_API_KEY` | GTFS zip fetch via `make sync-idfm` / `scripts/daily_sync.sh` | [data.gouv.fr](https://www.data.gouv.fr/) account → API key (if portal requires auth) |
| `DATAGOUV_API_KEY` | Same as `DATASETS_API_KEY` (alternate name) | Same |

### 2. Pre-download IDFM GTFS (recommended)

The IDFM zip is large (~170 MB). Download before first boot:

```bash
make sync-idfm      # → data/idfm/current.zip
```

### 3. Build & run the API

```bash
make release        # or: cargo build --release
make run            # GraphQL API → http://127.0.0.1:8080
```

First start also fetches SNCF GTFS into `./data/sncf/`. Wait until `GET /health` reports `stopCount > 0`.

### 4. Run the web map

In a second terminal:

```bash
make web            # WASM UI → http://127.0.0.1:8088
```

Open **http://127.0.0.1:8088** — search two stops, plan a journey, explore the live map.

### 5. Smoke test

```bash
make smoke          # probes /health and GraphQL against BASE_URL
```

---

## Endpoints

| URL | Purpose |
|-----|---------|
| `http://127.0.0.1:8080/health` | Process & feed status (JSON) |
| `http://127.0.0.1:8080/graphql` | GraphQL queries |
| `http://127.0.0.1:8080/graphiql` | GraphiQL playground |
| `ws://127.0.0.1:8080/ws` | GraphQL subscriptions |
| `http://127.0.0.1:8088` | Map UI (Trunk dev server) |

---

## Configuration

- **File:** `config/default.toml`
- **Override:** `TRANSIT_CONFIG=/path/to.toml`
- **Secrets:** `.env` (loaded by `make run`, `scripts/run_server.sh`, systemd) — never commit
- **Env overrides:** `TRANSIT__SERVER__BIND=127.0.0.1:8080` (figment `__` nesting)

Key sections: `[server]`, `[routing]`, `[graphql]`, `[prim]`, `[[feeds]]`, optional `[redis]` and `[ban]`.

Feed downloads can also use per-feed auth in TOML (`auth_header` + `auth_env`); see comments in `config/default.toml` for `IDFM_PRIM_API_KEY` and `DATASETS_API_KEY`.

```bash
# Optional: address autocomplete (Base Adresse Nationale, Île-de-France)
make ban-index
```

---

## Key features

- **Journeys** with scheduled + estimated departure/arrival times and delay badges
- **Live network** — vehicles on map, occupancy, trip status
- **French stop search** — accent-insensitive, station-biased ranking
- **IDFM loading UX** — progress while the large feed parses
- **Multi-feed** — namespaced ids (`idfm:…`, `sncf:…`), configurable `[[feeds]]`
- **Docker** — `docker compose up --build` (mounts `./data` for GTFS cache)

---

## Development

```bash
make test           # unit + integration tests
make gate           # fmt + lib tests + wasm check
make dev            # print two-terminal run instructions
make help           # all make targets
```

---

## Docker

```bash
docker compose up --build -d
BASE_URL=http://127.0.0.1:8080 make smoke
```

---

## Disclaimer

Best-effort open data — not an official IDFM or SNCF journey planner. No ticketing or fares. Results are indicative; verify before travel.

---

## License

**MIT** — see [LICENSE](LICENSE). Transit **data** remains under each producer's licence.
