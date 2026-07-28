# IDFM daily ops runbook

Keep **Île-de-France Mobilités** static GTFS fresh, wire PRIM realtime keys, and run the `transit` process with enough RAM for a large multi-feed epoch.

## What runs daily

| Step | Action | Notes |
|------|--------|--------|
| 1 | Download IDFM GTFS zip → `data/idfm/current.zip` | Public resource on data.gouv.fr |
| 2 | (Optional) Refresh line geometries | `scripts/build_idfm_lines.py` if référentiel + traces exist |
| 3 | Restart `transit` (or wait for next static poll) | No SIGHUP reload today; restart loads disk zip |
| 4 | PRIM GTFS-RT pollers | Need process up + `IDFM_PRIM_API_KEY` in `.env` |

Ideal cadence for Datahub / data.gouv static: **1–3× per day** (see `scripts/transit.timer`).

### Official static URL

```
https://www.data.gouv.fr/api/1/datasets/r/413988ed-d340-467b-8be2-7b999fcd207a
```

Override with `GTFS_URL=...` if the permanent resource id changes.

## RAM and disk

| Asset | Order of magnitude |
|-------|--------------------|
| IDFM GTFS zip alone | Often **several hundred MB** compressed; multi‑GB uncompressed tables in memory after parse |
| SNCF + IDFM together | Plan **8–16 GB+ RAM** for comfortable RAPTOR + dual static epochs; more if other feeds |
| Disk | Keep `data/<feed>/incoming/` pruned; each successful download may leave a stamped zip |

On constrained hosts: enable only one heavy feed, lower GraphQL concurrency, or pre-download overnight and start the server after `daily_sync` finishes.

First boot of IDFM can take **minutes** (download + `spawn_blocking` parse). `/health` may show `stopCount=0` until the first static load succeeds.

## Secrets

```bash
cp .env.example .env
# edit (two separate PRIM keys — do not reuse the same value):
#   IDFM_PRIM_API_KEY=...     # realtime SIRI / GTFS-RT: apikey header
#   PRIM_DATASET_KEY=...      # GTFS zip download: X-API-KEY (legacy: DATASETS_API_KEY, DATAGOUV_API_KEY)
chmod 600 .env
```

Never commit `.env`. `scripts/run_server.sh` and `EnvironmentFile=` in systemd load it without logging values. `main` only logs whether `IDFM_PRIM_API_KEY` is **set**.

## Config: enable the IDFM feed

Default `config/default.toml` already enables IDFM static. Optional PRIM RT (uncomment when subscribed):

```toml
[[feeds]]
id = "idfm"
enabled = true
static_url = "https://www.data.gouv.fr/api/1/datasets/r/413988ed-d340-467b-8be2-7b999fcd207a"
# trip_updates_url = "https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/trip-updates"
# service_alerts_url = "https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/service-alerts"
# vehicle_positions_url = "https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/vehicle-positions"
# auth_header = "apikey"
# auth_env = "IDFM_PRIM_API_KEY"
static_check_interval_secs = 3600
rt_poll_interval_secs = 30
user_agent = "transit-rs/0.1 (France multimodal GTFS server)"
priority = 50
# Memory guard: pack only trips whose service is active in [today, today+N] UTC
static_horizon_days = 14
```

Behaviour:

- **On start**: if `data/idfm/current.zip` exists and validates, load from disk first (fast path after `daily_sync`).
- **On interval**: conditional download from `static_url` (ETag/hash) when the process has network (`static_check_interval_secs = 3600`).
- **Ops pre-seed**: `daily_sync.sh` always refreshes disk so restarts and cold boots are consistent even if the in-process poll is delayed.
- **Horizon**: `static_horizon_days` drops inactive trips/stop_times after calendar check (still multi‑GB; full schedule often wants 16GB+).

## Scripts

| Script | Role |
|--------|------|
| `scripts/daily_sync.sh` | flock, curl retries, sha256 log, promote zip, optional lines + restart |
| `scripts/install_cron.sh` | user crontab **03:30 Europe/Paris** for `daily_sync.sh` |
| `scripts/run_server.sh` | `source .env` + `cargo run --release` (or `SKIP_CARGO=1` → `target/release/transit`) |
| `scripts/build_idfm_lines.py` | Merge référentiel + traces → `web/assets/idfm/lignes.geojson` |
| `scripts/smoke_live.sh` | Health + GraphQL smoke against a running server |
| `scripts/transit.service` | Long-running server unit |
| `scripts/transit-daily-sync.service` | Oneshot wrapper for `daily_sync.sh` |
| `scripts/transit.timer` | 05:15 / 13:15 / 21:15 calendar triggers |

### `daily_sync.sh` features

- **flock** on `/tmp/transit-daily-sync-${FEED_ID}.lock` — concurrent runs exit 0 (no pile-up).
- **curl** `--retry` / `--retry-all-errors`, long `--max-time` for large zips.
- **checksum** `sha256` logged; skip promote if unchanged.
- **validation** — zip must contain `stops.txt` + `stop_times.txt`.
- **atomic promote** via temp + `mv` into `data/idfm/current.zip`.
- **exit code** non-zero on failure → systemd/cron can alert.
- `RESTART=1` → `systemctl restart transit` when the unit exists.
- `BUILD_LINES=1` → optional geometry rebuild when inputs are present.

Logs: `data/logs/daily_sync-idfm.log`.

## First boot (exact commands)

```bash
cd /home/ondonda/rust/transit

# 1. Secrets
cp -n .env.example .env
chmod 600 .env
# put IDFM_PRIM_API_KEY=... in .env

# 2. IDFM [[feeds]] is enabled in config/default.toml (static_horizon_days = 14)

# 3. Pre-download static GTFS (recommended before first cargo run)
chmod +x scripts/daily_sync.sh scripts/run_server.sh scripts/install_cron.sh
./scripts/daily_sync.sh
# inspect:
ls -lh data/idfm/current.zip
tail -n 20 data/logs/daily_sync-idfm.log
# optional cron:
./scripts/install_cron.sh

# 4. Build + run (foreground)
cargo build --release
./scripts/run_server.sh
# or: SKIP_CARGO=1 ./scripts/run_server.sh

# 5. Smoke
./scripts/smoke_live.sh
# GraphQL health should eventually list feed id "idfm" with stopCount > 0
```

Docker alternative (same data mount):

```bash
./scripts/daily_sync.sh
docker compose up --build -d
BASE_URL=http://127.0.0.1:8080 ./scripts/smoke_live.sh
```

## Daily automation

### Option A — systemd timer (preferred)

```bash
cd /home/ondonda/rust/transit
cargo build --release

# Edit User / WorkingDirectory / paths in the three unit files if not ondonda@this host
sudo cp scripts/transit.service \
        scripts/transit-daily-sync.service \
        scripts/transit.timer \
        /etc/systemd/system/

sudo systemctl daemon-reload
sudo systemctl enable --now transit.service
sudo systemctl enable --now transit.timer

systemctl status transit.service
systemctl list-timers | grep transit
# manual sync:
sudo systemctl start transit-daily-sync.service
journalctl -u transit-daily-sync.service -n 80 --no-pager
```

`transit-daily-sync.service` sets `RESTART=1` so a new zip is loaded by restarting the server.

### Option B — crontab

```cron
# m h  dom mon dow  command
15 5,13,21 * * * /home/ondonda/rust/transit/scripts/daily_sync.sh >>/home/ondonda/rust/transit/data/logs/cron.log 2>&1
# Optional restart after sync (if not using systemd for the app):
# 20 5,13,21 * * * systemctl restart transit
```

Cron only notifies on failure if you wrap with mail/`|| notify-send`; non-zero exit from `daily_sync.sh` is the signal.

### Option C — process only (no external sync)

Point `static_url` at the data.gouv resource and rely on `static_check_interval_secs`. Still useful to run `daily_sync.sh` once for first boot so cold start does not depend on a long download inside the critical path.

## Trip ID mapping + static repack

IDFM GTFS includes `object_codes_extension.txt` (~480k trip rows). At parse time the server builds a
SIRI `DatedVehicleJourneyRef` → GTFS `trip_id` alias table (`src/gtfs/siri_trip_map.rs`) used by
shape-based vehicle estimation and GraphQL enrichment.

| RT ref pattern | Static match |
|----------------|--------------|
| `SNCF_MAGENTA_PRD:VehicleJourney::{uuid}:LOC` | `idfm:IDFM:TN:SNCF:{uuid}` via object_codes + UUID fast path |
| `stif:VehicleJourney:local-…:LOC` (in object_codes) | `idfm:IDFM:stif:local-…` |
| `RATP-SIV:VehicleJourney::20260727.*.C01371:LOC` | No object_codes row — stop-chain / chord estimate only |

Pack-time extras (no separate download):

- `shape_dist_traveled` backfill from shapes + stop coords (`geometry::backfill_shape_dist_traveled`)
- SIRI trip aliases from `object_codes_extension.txt`

**Reload packed epoch** after GTFS zip change or mapping code change (no SIGHUP reload):

```bash
# Download fresh zip + restart (systemd sets RESTART=1):
make sync-idfm-restart

# Re-parse existing data/idfm/current.zip only:
make repack-idfm
# or: systemctl restart transit
```

First IDFM load after repack logs `loaded SIRI trip aliases from object_codes_extension` with
`alias_keys` count (expect hundreds of thousands for full feed).

## Optional line geometries

If you have:

- `data/idfm/referentiel-des-lignes.geojson`
- `data/idfm/traces.geojson` (or set `IDFM_TRACES`)

then:

```bash
BUILD_LINES=1 ./scripts/daily_sync.sh
# or directly:
python3 scripts/build_idfm_lines.py data/idfm/referentiel-des-lignes.geojson data/idfm/traces.geojson
```

Outputs `web/assets/idfm/lignes.geojson` (and a copy under `data/idfm/`). Map UI reads the web assets tree; rebuild the WASM frontend if you ship `web/dist`.

## Failure playbook

| Symptom | Check |
|---------|--------|
| `daily_sync` curl fails | Network, data.gouv status, raise `CURL_MAX_TIME` / retries |
| “file too small” | HTML error page saved as zip — inspect `data/idfm/incoming/` |
| Validation missing stops.txt | Wrong resource URL / truncated download |
| Server OOM on load | Disable other feeds; add RAM; do not run debug builds for IDFM |
| RT empty / 401 | `IDFM_PRIM_API_KEY`, `auth_header=apikey`, PRIM product URLs |
| Zip on disk new but routes old | Restart transit (`RESTART=1` or `systemctl restart transit`) |
| Lock skip | Another sync holds flock — wait; check `/tmp/transit-daily-sync-idfm.lock` |

## Licence / attribution

IDFM open data remains under the producer licence (often ODbL). Attribute **Île-de-France Mobilités** when redistributing derived data. Application code is separate (see root README).
