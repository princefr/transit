# IDFM vehicle positions & monitoring (GTFS-RT / SIRI only)

Runbook for **Île-de-France Mobilités (IDFM)** realtime vehicle data via **PRIM**, using **GTFS-RT or SIRI Lite only** (not Navitia JSON APIs).

Research date: **2026-07-27**. Live probes used env `IDFM_PRIM_API_KEY` (value never logged).

---

## Executive summary

| Need | Available today on typical PRIM keys? | Endpoint / approach |
|------|----------------------------------------|---------------------|
| Trip updates (ETA / delay / cancel) | **Yes** | SIRI Lite **EstimatedTimetable** `GET /marketplace/estimated-timetable` |
| Next departures at a stop | **Yes** | SIRI Lite **StopMonitoring** `GET /marketplace/stop-monitoring?MonitoringRef=…` |
| Traffic messages (SIRI GM) | **Yes** (with `LineRef`) | `GET /marketplace/general-message?LineRef=…` |
| **GPS vehicle positions (VP)** | **No** (403) | SIRI **VehicleMonitoring** & GTFS-RT VP marketplace paths |
| GTFS-RT TripUpdates / Alerts / VP | **No** (403) | `/marketplace/gtfs-rt/*` |
| SituationExchange (SIRI SX) | **No** (403) | `/marketplace/situation-exchange` |

**Bottom line:** open PRIM products expose **stop- and line-centric predictions** (SM + ET), not public AVL/GPS. True map tracking needs **marketplace product entitlement** that most developer keys do not have, or **interim UX** (ETA + `VehicleAtStop` + shape interpolation).

Community note: reusers (e.g. [kevinbioj/gtfsrt-idfm](https://github.com/kevinbioj/gtfsrt-idfm)) **synthesize GTFS-RT TripUpdates from PRIM EstimatedTimetable**; they do not receive native GPS from IDFM open APIs.

---

## Auth & base URL

| Item | Value |
|------|--------|
| Base | `https://prim.iledefrance-mobilites.fr` |
| Auth header | `apikey: <key>` |
| Env | `IDFM_PRIM_API_KEY` (see `transit/.env`, never commit) |
| Accept | `application/json` for SIRI Lite; protobuf for GTFS-RT if entitled |
| User-Agent | set a stable UA (e.g. `transit-rs/0.1 …`) |

Example:

```bash
curl -sS -H "apikey: $IDFM_PRIM_API_KEY" -H "Accept: application/json" \
  "https://prim.iledefrance-mobilites.fr/marketplace/estimated-timetable?LineRef=STIF%3ALine%3A%3AC01742%3A"
```

Portal: [PRIM catalogue](https://prim.iledefrance-mobilites.fr/fr/catalogue-data) · connect B2B account required for keys and product subscriptions.

Quotas (from PRIM product pages, “nouveaux utilisateurs” post-2024): often **5 req/s**, **~1 000/day** unless upgraded via [Ma consommation API](https://prim.iledefrance-mobilites.fr/fr/ma-consommation-api). Older keys may have higher caps (e.g. SM up to 100 rps / 1M day). **EstimatedTimetable bulk is heavy** (~60 MB uncompressed JSON for full network) — respect quotas.

Licence: **Licence Mobilité** / ODbL (see PRIM CGU).

---

## Static GTFS (required to interpret RT ids)

| Source | URL |
|--------|-----|
| OpenDataSoft FTP | `https://eu.ftp.opendatasoft.com/stif/GTFS/IDFM-gtfs.zip` |
| data.gouv permanent resource | `https://www.data.gouv.fr/api/1/datasets/r/413988ed-d340-467b-8be2-7b999fcd207a` |
| PRIM Datahub dataset | [Horaires prévus (GTFS Datahub)](https://prim.iledefrance-mobilites.fr/fr/jeux-de-donnees/offre-horaires-tc-gtfs-idfm) (login) |

Coverage of realtime: [Périmètre des données TR plateforme IDFM](https://prim.iledefrance-mobilites.fr/fr/jeux-de-donnees/perimetre-des-donnees-tr-disponibles-plateforme-idfm).

**Id conventions (SIRI Lite):**

- Line: `STIF:Line::C01742:` (RER A example; code from référentiel / GTFS `route_id` mapping)
- Stop area: `STIF:StopArea:SP:43152:`
- Quay / stop point: `STIF:StopPoint:Q:473960:`
- Journey refs: operator-specific, e.g. `SNCF_MAGENTA_PRD:VehicleJourney::…:LOC`

---

## Working SIRI Lite products (HTTP)

All methods below: **GET**, header `apikey`, JSON body under root `Siri.ServiceDelivery.*`.

### 1. Estimated Timetable (ET) — trip-level predictions

| | |
|--|--|
| PRIM product | [Prochains passages – requête globale](https://prim.iledefrance-mobilites.fr/fr/apis/idfm-ivtr-requete_globale) |
| Path | `/marketplace/estimated-timetable` |
| Query | Optional `LineRef=STIF:Line::XXXXX:` (URL-encoded). **Omit for full feed** (very large). |
| Alias observed | `/marketplace/requete-ligne?LineRef=…` → same ET shape |
| Live status (2026-07-27) | **200** `application/json` |
| Sample sizes | Full network ~**62 MB** / ~**17 500** journeys; one line (RER A) ~0.9 MB / ~75 journeys |

**Response shape (abbrev):**

```json
{
  "Siri": {
    "ServiceDelivery": {
      "ResponseTimestamp": "2026-07-27T14:41:47.867Z",
      "ProducerRef": "IVTR_HET",
      "EstimatedTimetableDelivery": [{
        "Version": "2.0",
        "Status": "true",
        "EstimatedJourneyVersionFrame": [{
          "EstimatedVehicleJourney": [{
            "RecordedAtTime": "…",
            "LineRef": { "value": "STIF:Line::C01742:" },
            "DatedVehicleJourneyRef": { "value": "…" },
            "DirectionRef": { "value": "…" },
            "OriginRef": { "value": "STIF:StopArea:SP:…" },
            "DestinationRef": { "value": "…" },
            "PublishedLineName": [{ "value": "…" }],
            "EstimatedCalls": {
              "EstimatedCall": [{
                "StopPointRef": { "value": "STIF:StopArea:SP:…" },
                "AimedArrivalTime": "…",
                "ExpectedArrivalTime": "…",
                "AimedDepartureTime": "…",
                "ExpectedDepartureTime": "…",
                "ArrivalStatus": "ON_TIME",
                "DepartureStatus": "ON_TIME",
                "ArrivalPlatformName": { "value": "2" },
                "DeparturePlatformName": { "value": "2" }
              }]
            }
          }]
        }]
      }]
    }
  }
}
```

**Important:** observed journeys have **no** `VehicleLocation` / lat-lon. Use ET for **TripUpdate**-class data only.

**App mapping:** already handled by `src/rt/adapter/siri.rs` → trip updates; poller in `src/prim/`.

---

### 2. Stop Monitoring (SM) — next vehicles at a stop

| | |
|--|--|
| PRIM product | [Prochains passages – requête unitaire](https://prim.iledefrance-mobilites.fr/fr/apis/idfm-ivtr-requete_unitaire) |
| Path | `/marketplace/stop-monitoring` |
| Query | **Required:** `MonitoringRef=<STIF stop id>` |
| Live status | **200** with valid `MonitoringRef`; empty visits if no RT for that stop |
| Path form `/stop-monitoring/{MonitoringRef}` | Documented on portal; **path-style returned 404** on probe — prefer **query param** |

**Working `MonitoringRef` examples:**

| Ref style | Example | Result |
|-----------|---------|--------|
| StopArea | `STIF:StopArea:SP:43152:` | **200**, dozens of visits |
| StopPoint/Quay | `STIF:StopPoint:Q:41286:` | **200**, few visits |
| Bare / wrong | `IDFM:41286`, `StopPoint:Q:…` without `STIF:` | **400** |

**Response shape (abbrev):**

```json
{
  "Siri": {
    "ServiceDelivery": {
      "StopMonitoringDelivery": [{
        "MonitoredStopVisit": [{
          "RecordedAtTime": "…",
          "MonitoringRef": { "value": "STIF:StopArea:SP:43152:" },
          "MonitoredVehicleJourney": {
            "LineRef": { "value": "STIF:Line::C01742:" },
            "FramedVehicleJourneyRef": {
              "DatedVehicleJourneyRef": "SNCF_MAGENTA_PRD:VehicleJourney::…:LOC"
            },
            "DestinationName": [{ "value": "…" }],
            "MonitoredCall": {
              "StopPointName": [{ "value": "Lognes" }],
              "VehicleAtStop": false,
              "ExpectedArrivalTime": "…",
              "ExpectedDepartureTime": "…",
              "AimedArrivalTime": "…",
              "AimedDepartureTime": "…",
              "ArrivalStatus": "ON_TIME",
              "DepartureStatus": "ON_TIME",
              "ArrivalPlatformName": { "value": "…" }
            }
          }
        }]
      }]
    }
  }
}
```

**GPS:** observed SM payloads had **0× `VehicleLocation`**, **0× `VehicleRef`**.  
**Interim “approaching vehicle” signal:** `MonitoredCall.VehicleAtStop` (bool) + short `ExpectedArrivalTime` delta — not a map coordinate.

---

### 3. General Message (GM) — line messages

| | |
|--|--|
| Path | `/marketplace/general-message` |
| Query | **`LineRef` required** in practice (bare call → **400**) |
| Live status | **200** with `LineRef=STIF:Line::C01742:` |
| Shape | `Siri.ServiceDelivery.GeneralMessageDelivery[].InfoMessage[]` |

Useful for service text; not vehicle positions. For bulk disruptions, PRIM also lists **Messages Info Trafic – requête globale** (JSON product; path not fully documented publicly; Navitia `line_reports` works but is **out of scope** for this SIRI/GTFS-RT-only policy).

---

### 4. Line-scoped ET alias

| Path | Status | Notes |
|------|--------|--------|
| `/marketplace/requete-ligne?LineRef=…` | **200** | Same ET delivery as estimated-timetable for that line |

---

## Gated / non-working: VehicleMonitoring & GTFS-RT

### SIRI VehicleMonitoring (true AVL)

| Candidate URL | HTTP | Content-Type |
|---------------|------|--------------|
| `/marketplace/vehicle-monitoring` | **403** | `text/html` |
| `/marketplace/vehicle-monitoring?LineRef=…` | **403** | `text/html` |
| `/marketplace/siri/vehicle-monitoring` | **403** | `text/html` |
| `/marketplace/siri-lite/vehicle-monitoring` | **403** | `text/html` |

403 with HTML (not JSON error body) is consistent with **API Gateway / product not subscribed** for this apikey, not a simple “missing query param” (that usually returns 400 JSON).

Expected SIRI VM fields (when entitled; standard SIRI Lite): `VehicleActivity` → `MonitoredVehicleJourney.VehicleLocation.Latitude/Longitude`, `Bearing`, `VehicleRef`, progress / next call. **Not verified live** (no access).

### GTFS-RT marketplace

Config comments in `docs/IDFM_DAILY.md` list:

```
https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/trip-updates
https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/service-alerts
https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/vehicle-positions
```

| Candidate | HTTP | CT |
|-----------|------|-----|
| `/marketplace/gtfs-rt` | **403** | html |
| `/marketplace/gtfs-rt/trip-updates` | **403** | html |
| `/marketplace/gtfs-rt/vehicle-positions` | **403** | html |
| `/marketplace/gtfs-rt/service-alerts` | **403** | html |
| snake_case / `gtfsrt` / `v2` variants | **403** | html |

**There is no public unauthenticated GTFS-RT VP URL** for full IDFM on transport.data.gouv.fr comparable to many other French networks. PAN indexes many regional GTFS-RT feeds; **IDFM’s open realtime surface is PRIM SIRI Lite SM/ET**, not a national open GTFS-RT dump.

### Other gated

| Path | Status |
|------|--------|
| `/marketplace/situation-exchange` | **403** |
| `/marketplace/siri/stop-monitoring` | **403** (use `/marketplace/stop-monitoring`) |

---

## How to request Vehicle Positions / VM access

1. **Create / login** PRIM B2B: [connect.iledefrance-mobilites.fr](https://connect.iledefrance-mobilites.fr) (realm `connect-b2b`, client `prim`).
2. **Subscribe** to realtime API products in the catalogue (prochains passages, and any product explicitly naming **GTFS-RT** or **Vehicle Monitoring** if listed for your org type).
3. Open **[Ma consommation API](https://prim.iledefrance-mobilites.fr/fr/ma-consommation-api)** — request **quota increase** and ask which products unlock `/marketplace/vehicle-monitoring` and `/marketplace/gtfs-rt/*`.
4. Contact PRIM support / community (portal aide & contact; Slack community mentioned on PRIM hackathon dataset) with:
   - Organisation + use case (passenger info app, non-commercial vs commercial)
   - Apikey prefix (never full key)
   - Paths returning 403: `vehicle-monitoring`, `gtfs-rt/vehicle-positions`
5. Accept **Licence Mobilité** / CGU; some products are restricted to certified reusers or operators.

Until entitlement is granted, treat VP as **unavailable** in production.

---

## Interim alternatives (no GPS entitlement)

### A. StopMonitoring as “approaching vehicle” (recommended UX)

For a stop board or “where is my train/bus” **without a map pin**:

1. Resolve GTFS `stop_id` → SIRI `MonitoringRef` (`STIF:StopArea:SP:…` preferred for aggregation).
2. Poll `GET /marketplace/stop-monitoring?MonitoringRef=…` every 15–30 s (stay under quota).
3. Show `ExpectedArrivalTime` / `ExpectedDepartureTime`, status, destination, platform.
4. Highlight when `VehicleAtStop == true` or ETA &lt; N minutes as “at / near stop”.

No lat/lon required.

### B. StopMonitoring + EstimatedTimetable → synthetic geolocation (**implemented**)

Code: `src/rt/estimate.rs` → `estimate_vehicle_pos` (used by GraphQL `vehicles` via
`synthetic_vehicle_from_trip_update`).

1. Continuous SM/ET pollers fill `FeedRtState.trips` with expected arr/dep per call.
2. For each journey without real GPS:
   - **Dwell**: `now` ∈ [ExpectedArrival, ExpectedDeparture] → pin at stop (`ESTIMATED_STOPPED_AT`).
   - **Multi-call (ET)** / **single SM approach**: time-fraction between prev dep and next arr,
     then **arc-length** along GTFS `shapes.txt` when available (`ESTIMATED_ON_SHAPE`), else
     along the full intermediate **stop-chain** polyline from static stop_times
     (`ESTIMATED_ON_STOP_CHAIN`) — not crow-flies between endpoints only. Reverse shapes and
     multi-branch nearest-index failures fall back to stop-chain. Frac 0/1 snap to endpoints;
     intermediate stops without coords are skipped. Canceled trips are omitted.
3. GraphQL status is always `ESTIMATED_*` (not GPS). Real VP wins when present.

This is what many IDFM map demos approximate; [gtfsrt-idfm](https://github.com/kevinbioj/gtfsrt-idfm) focuses on **TripUpdates from ET**, not true VP.

### C. GTFS-RT self-host (optional)

If the app must speak GTFS-RT internally:

- Ingest PRIM ET → emit TripUpdate protobuf (open-source pattern above).
- Emit VehiclePosition only if you interpolate (B) or later unlock native VM/VP.

Do **not** point `vehicle_positions_url` at PRIM GTFS-RT until 200 is confirmed for your key.

---

## Implementation status in this repo (2026-07)

| Priority | Item | Status |
|----------|------|--------|
| P0 | SM `VehicleLocation` → real GPS `VehiclePos` | **Done** — SIRI SM adapter + departures overlay merge |
| P0 | ET estimation + `ESTIMATED_*` status / GraphQL `positionSource` + `isEstimated` | **Done** — `estimate.rs` + `vehicles` / `tripRealtime` / `watchTrips` |
| P1 | Viewport-driven ET LineRefs + SM interest | **Done** — `register_viewport_live_interest` (40 lines / viewport), GraphQL `warmLiveViewport`, map pan |
| P1 | SIRI `DatedVehicleJourneyRef` ↔ GTFS `trip_id` | **Done** — `object_codes_extension.txt` at pack; SNCF UUID + NeTEx aliases; RATP dated refs still partial |
| P2 | PRIM **VehicleMonitoring** bulk GPS | **Blocked** — open key returns **403**; do not poll until product entitled |
| P2 | Marketplace GTFS-RT VP | **Blocked** — same 403; no free bulk IDF VP feed found for open keys |
| UI | GPS vs Estimé badge on map markers | **Done** — solid ring + dashed/`≈` for estimated |

### P2 probe notes (open PRIM key)

```text
GET /marketplace/vehicle-monitoring          → 403
GET /marketplace/gtfs-rt/vehicle-positions   → 403 (path may vary)
```

When a B2B product unlocks VM or GTFS-RT VP: wire URL in feed config → existing `apply_vehicle_positions` / SIRI VM adapter path; UI already prefers non-`ESTIMATED` over synthetic.

## Recommended implementation path (this repo)

Priority order for `transit`:

1. **Keep SIRI-only PRIM pollers** (`src/prim/`) on:
   - `estimated-timetable?LineRef=…` (viewport + seed lines)
   - `general-message?LineRef=…` (traffic ticker)
   - SM for boards + optional `VehicleLocation` GPS
2. **Do not enable** `trip_updates_url` / `vehicle_positions_url` GTFS-RT marketplace URLs until probes return **200**.
3. **Map vehicles (product):** GPS from SM/ET when present; else shape/stop-chain estimate with clear badge.
4. **Id mapping:** STIF SIRI refs ↔ GTFS (Datahub + référentiel).
5. **Quotas:** line-filtered ET; SM for visible stops; soft viewport interest (no HTTP) via `warmLiveViewport`.

Config reminder (`docs/IDFM_DAILY.md`):

```toml
[[feeds]]
id = "idfm"
static_url = "https://www.data.gouv.fr/api/1/datasets/r/413988ed-d340-467b-8be2-7b999fcd207a"
# Leave commented until marketplace returns 200:
# trip_updates_url = "https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/trip-updates"
# vehicle_positions_url = "https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/vehicle-positions"
# service_alerts_url = "https://prim.iledefrance-mobilites.fr/marketplace/gtfs-rt/service-alerts"
# auth_header = "apikey"
# auth_env = "IDFM_PRIM_API_KEY"
```

Realtime adapters: `docs/REALTIME_ADAPTERS.md`, `src/rt/adapter/siri.rs`.

---

## Live probe log (status + content-type only)

Date: 2026-07-27 · key present · base `https://prim.iledefrance-mobilites.fr`

| Path | Status | Content-Type |
|------|--------|--------------|
| `/marketplace/estimated-timetable` | 200 | application/json |
| `/marketplace/estimated-timetable?LineRef=STIF:Line::C01742:` | 200 | application/json |
| `/marketplace/requete-ligne?LineRef=…` | 200 | application/json |
| `/marketplace/stop-monitoring?MonitoringRef=STIF:StopPoint:Q:412986:` | 200 | application/json |
| `/marketplace/stop-monitoring?MonitoringRef=STIF:StopArea:SP:43152:` | 200 | application/json (visits &gt; 0) |
| `/marketplace/general-message` | 400 | application/json |
| `/marketplace/general-message?LineRef=…` | 200 | application/json |
| `/marketplace/vehicle-monitoring` (+ LineRef) | **403** | text/html |
| `/marketplace/situation-exchange` | **403** | text/html |
| `/marketplace/gtfs-rt/*` (all tried variants) | **403** | text/html |
| Public GTFS zip OpenDataSoft | 200 | application/zip |

---

## Official product map (PRIM catalogue names)

| Catalogue name | Theme | Wire format | Marketplace path (observed / documented) |
|----------------|-------|-------------|------------------------------------------|
| Prochains passages – requête unitaire | Prochains passages | SIRI Lite SM | `GET /marketplace/stop-monitoring` |
| Prochains passages – requête globale | Prochains passages | SIRI Lite ET | `GET /marketplace/estimated-timetable` |
| Prochains passages – requête ligne | Prochains passages | SIRI Lite ET | `GET /marketplace/requete-ligne` |
| Messages Info Trafic – requête globale | Info trafic | JSON (not SIRI GM) | product-specific (prefer GM with LineRef for SIRI) |
| Messages affichés écrans (IVTR) | Info trafic | separate product | not probed as VP |
| GTFS Datahub | Theoretical | GTFS zip | dataset download, not RT |

Vehicle Monitoring / GTFS-RT VP are **expected operationally by operators toward IDFM** (SIRI Lite VM into PRIM; industry blogs describe VM as required for bus SAEIV compliance) but **redistribution as an open consumer API is not available on standard developer keys**.

---

## Quick decision tree

```
Need live map GPS for IDFM vehicles?
├─ Have PRIM product that returns 200 on /marketplace/vehicle-monitoring
│  or /marketplace/gtfs-rt/vehicle-positions ?
│  ├─ YES → poll that feed; decode SIRI VM or GTFS-RT VP
│  └─ NO  → request access (portal + Ma consommation API)
│           meanwhile:
│           ├─ stop UX → StopMonitoring
│           ├─ journey UX → EstimatedTimetable
│           └─ map estimate → interpolate on shape from ET (label estimated)
└─ Never use Navitia JSON for this app’s RT pipeline (policy)
```

---

## References

- [PRIM home / catalogue](https://prim.iledefrance-mobilites.fr/)
- [API: stop-monitoring (requête unitaire)](https://prim.iledefrance-mobilites.fr/fr/apis/idfm-ivtr-requete_unitaire)
- [API: estimated-timetable (requête globale)](https://prim.iledefrance-mobilites.fr/fr/apis/idfm-ivtr-requete_globale)
- [API: requête ligne](https://prim.iledefrance-mobilites.fr/fr/apis/idfm-ivtr-requete_ligne)
- [Périmètre données TR](https://prim.iledefrance-mobilites.fr/fr/jeux-de-donnees/perimetre-des-donnees-tr-disponibles-plateforme-idfm)
- [Explorer les API (doc)](https://prim.iledefrance-mobilites.fr/fr/aide-et-contact/documentation/donnees-disponibles/api/explorer-les-api)
- [Ma consommation API](https://prim.iledefrance-mobilites.fr/fr/ma-consommation-api)
- [SIRI Profil France](https://normes.transport.data.gouv.fr/normes/siri/profil-france/)
- Community ET→GTFS-RT: [kevinbioj/gtfsrt-idfm](https://github.com/kevinbioj/gtfsrt-idfm)

---

## Changelog

| Date | Note |
|------|------|
| 2026-07-27 | Initial research + live probe matrix; VP/VM/GTFS-RT gated (403); SM/ET/GM working |
