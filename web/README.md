# Transit web (WASM + Leaflet)

Browser UI for the transit GraphQL API: **stop search**, **itinerary planning**, **journey map**, **global live network map**, **journey live vehicle tracking**, **departures board**, and a **mode legend**.

Default UI language: **French** (labels, status, badges).

Stack:

| Piece | Role |
|--------|------|
| **Trunk** | Dev server + `wasm-bindgen` pipeline |
| **Rust (`transit-web`)** | GraphQL client (`gloo-net`), autocomplete / plan / departures / global + journey live VP poll + optional WS |
| **Leaflet (CDN)** | OSM map, polylines, markers (`map.js` bridge) |
| **SVG icons** | `assets/icons/*.svg` (+ inline SVG fallbacks in `map.js`) |

---

## How-to (EN)

### Prerequisites

```bash
rustup target add wasm32-unknown-unknown
# Install trunk if missing (slow):
cargo install trunk
# Or let ./serve.sh download a prebuilt binary into ~/.cargo/bin
```

### Troubleshooting

| Error | Fix |
|-------|-----|
| `trunk: command not found` | `cargo install trunk` **or** `./serve.sh` (downloads prebuilt) |
| `invalid value '1' for '--no-color'` | Environment has `NO_COLOR=1`. Trunk 0.21 only accepts `true`/`false`. Run `unset NO_COLOR CLICOLOR` or use **`./serve.sh`** |
| Map empty / API offline | Start the server: `cd .. && cargo run` → `http://127.0.0.1:8080/health` |

### Run

**Terminal 1 — API** (repo root):

```bash
cd /home/ondonda/rust/transit
cargo run
# GraphQL: http://127.0.0.1:8080/graphql
# Subscriptions: ws://127.0.0.1:8080/ws
```

Wait until `health.stopCount > 0` (static GTFS loaded).

**Terminal 2 — frontend** (preferred):

```bash
cd /home/ondonda/rust/transit/web
chmod +x serve.sh
./serve.sh
# → http://127.0.0.1:8088
```

Manual equivalent:

```bash
unset NO_COLOR CLICOLOR   # required if NO_COLOR=1 is set
export PATH="$HOME/.cargo/bin:$PATH"
trunk serve
```

### GraphQL URL

Default: `http://127.0.0.1:8080/graphql`.

Override without rebuild:

1. Query string: `?graphql=http://127.0.0.1:8080/graphql`
2. `localStorage.setItem("GRAPHQL_URL", "http://…/graphql")` then reload
3. Compile-time: `GRAPHQL_URL=http://… cargo build --target wasm32-unknown-unknown`

### GraphQL WebSocket (optional live)

Default derived: `http://host/graphql` → `ws://host/ws`.

Override:

1. `?graphql_ws=ws://127.0.0.1:8080/ws`
2. `localStorage.setItem("GRAPHQL_WS", "ws://…/ws")`
3. `GRAPHQL_WS=ws://…` at compile time

When available, the UI opens a `graphql-transport-ws` subscription (`watchTrips`) and still **polls** `tripRealtime` every ~12s as a soft fallback.

### Build only (no Trunk)

```bash
cd web
cargo build --target wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release
```

### UI walkthrough (EN)

1. **Sidebar**: origin / destination search, optional time, **Arriver avant** (arrive-by), **Accessible fauteuil**, plan button.
2. **Journeys**: live toggle, cards with times, transfers, RT status, **fare** when present, leg chips + **same vehicle** badge.
3. **Map**: France-centered OSM; **Réseau en direct** global VP layer; selected itinerary; journey live markers.
4. **Departures**: next services for the last picked stop.
5. **Mode legend** and **health** badge.

---

## Mode d’emploi (FR)

### Prérequis

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk   # si besoin
```

### Lancer

**Terminal 1 — API** (racine du dépôt) :

```bash
cd /home/ondonda/rust/transit
cargo run
# GraphQL : http://127.0.0.1:8080/graphql
# Abonnements : ws://127.0.0.1:8080/ws
```

Attendre `health.stopCount > 0` (GTFS statique chargé).

**Terminal 2 — interface** :

```bash
cd /home/ondonda/rust/transit/web
./serve.sh
# → http://127.0.0.1:8088
# si erreur NO_COLOR=1 : unset NO_COLOR puis trunk serve
```

### URL GraphQL

Défaut : `http://127.0.0.1:8080/graphql`.

Sans recompiler :

1. Query : `?graphql=http://127.0.0.1:8080/graphql`
2. `localStorage.setItem("GRAPHQL_URL", "http://…/graphql")` puis recharger
3. Compile : `GRAPHQL_URL=http://… cargo build --target wasm32-unknown-unknown`

### WebSocket GraphQL (temps réel optionnel)

Dérivé par défaut : `http://hôte/graphql` → `ws://hôte/ws`.

Surcharge :

1. `?graphql_ws=ws://127.0.0.1:8080/ws`
2. `localStorage.setItem("GRAPHQL_WS", "ws://…/ws")`
3. Variable d’environnement `GRAPHQL_WS` à la compilation

Si le WS est joignable, l’UI s’abonne via `graphql-transport-ws` (`watchTrips`) et **conserve le poll** HTTP (~12 s) en secours.

### Compiler uniquement

```bash
cd web
cargo build --target wasm32-unknown-unknown
```

### Parcours UI (FR)

1. **Barre latérale** : **Départ** / **Arrivée** (recherche), heure optionnelle, **Arriver avant**, **Accessible fauteuil**, **Calculer l’itinéraire**.
2. **Itinéraires** : case **Temps réel**, cartes (horaires, correspondances, statut RT, **tarif** si `fareAmount`, badge **Même véhicule** sur les étapes).
3. **Carte** : fond OSM France ; **Réseau en direct** (véhicules VP sur le viewport) ; tracé de l’itinéraire sélectionné ; live itinéraire.
4. **Départs** : prochains services de l’arrêt choisi.
5. **Modes** + pastille santé API.

---

## How the map uses GraphQL geometry

```
stops(search)  → autocomplete origin/destination (id, name, code, lat, lon)
itineraries    → journeys[] with legs (TransitLeg | WalkLeg), fare*, sameVehicle
departures     → board when a stop is selected
health         → sidebar badge (status, stop/trip counts)
tripRealtime   → live VP poll
watchTrips     → optional WS subscription
```

**Drawing a selected journey**

1. Plan via `itineraries(input: { from: { stopId }, to: { stopId }, departureAt | arriveBy, wheelchair, … })`.
2. Sidebar lists journeys; selecting one calls `journey_to_map` → `TransitMap.drawJourney`.
3. For each leg the WASM layer builds a polyline:

| Priority | Source |
|----------|--------|
| 1 | `geometry { lat lon }` on **TransitLeg** / **WalkLeg** |
| 2 (fallback) | Transit: stop chain; Walk: endpoints |

**Colors**: `routeColor` when valid hex, else mode defaults (walk = gray dashed).

**Markers**: origin / destination / transfers; snapshot vehicle on plan; **live** layer polled (and optionally WS).

## Live vehicle tracking (journey)

When a journey is selected and **Temps réel** is checked (default **on**):

1. WASM collects namespaced `tripId` values from each `TransitLeg`.
2. Every **12s** (and immediately on select) it queries aliased `tripRealtime(tripId)`.
3. If a GraphQL WS URL is available, also subscribe to `watchTrips(tripIds: …)` over `graphql-transport-ws`. On WS failure, poll alone continues.
4. Positions go to `TransitMap.setLiveVehicles` (JS interpolates ~1s, bearing rotates the chevron).
5. Empty VP → subtle empty state: *Aucune position temps réel pour ces courses*.

## Réseau en direct (global live network)

Independent of origin/destination and itinerary planning. Sidebar toggle **« Réseau en direct »** (default **on**):

1. On load (and every **12s**), WASM reads the Leaflet viewport via `TransitMap.getBounds()` (also exposes `getCenterZoom()`).
2. Queries GraphQL `vehicles(limit: 300, bbox: { minLat, minLon, maxLat, maxLon })` — server enriches with mode/route/headsign/delay and can estimate positions from trip updates when VP is missing.
3. Positions go to a **separate** layer: `TransitMap.setGlobalLiveVehicles` (same animation as journey live; does **not** clear journey polylines or journey live markers).
4. Map badge: **« N véhicules en direct »**. Empty overlay → *Aucune position temps réel (beaucoup de flux SNCF n’ont pas de VehiclePositions ; l’API peut estimer via trip updates)*.
5. Pan/zoom (`onMoveEnd`) debounces a refresh (~400 ms). Turning the toggle **off** clears only the global layer.

Journey **Temps réel** and **Réseau en direct** can both be on at once (two Leaflet layer groups).

### GraphQL contract for WASM (`vehicles`)

```graphql
query NetworkLive(
  $bbox: BBoxInput
  $modes: [Mode!]
  $feedId: String
  $limit: Int = 200
) {
  vehicles(bbox: $bbox, modes: $modes, feedId: $feedId, limit: $limit) {
    tripId lat lon bearing speed label currentStatus occupancy
    feedId routeShortName routeLongName tripShortName headsign
    mode routeColor delaySeconds canceled updatedAt
  }
}
```

| Arg / field | Notes |
|-------------|--------|
| `bbox` | `{ minLat, minLon, maxLat, maxLon }` — preferred map viewport filter |
| `near` | `{ lat, lon, radiusMeters }` — optional radius (max 50 km) |
| `modes` | e.g. `[RAIL, METRO, BUS]` |
| `feedId` / `tripId` | optional scopes |
| `limit` | default **200**, max **500** |
| `currentStatus` | real GTFS-RT status, or `ESTIMATED_FROM_TRIP_UPDATE` when synthesized |
| Enrichment | route/mode/headsign/color/delay from static epoch + trip RT |

Real VehiclePositions always preferred over synthetic markers for the same trip.

## Project layout

```
web/
  Cargo.toml          # transit-web cdylib
  Trunk.toml          # serve on :8088
  index.html          # shell FR + Leaflet CDN + trunk hooks
  styles.css
  map.js              # window.TransitMap Leaflet bridge
  src/lib.rs          # WASM entry, GraphQL, DOM UI
  assets/icons/       # mode + pin SVGs
  README.md
```

## CORS note

The browser origin is typically `http://127.0.0.1:8088` while GraphQL is on `:8080`. The transit server applies a **permissive CORS** layer. WebSocket subscriptions need the API origin reachable as `ws://127.0.0.1:8080/ws` (no extra CORS preflight for WS; same-host or open firewall).

## Troubleshooting

| Symptom | Check |
|---------|--------|
| Health badge “API offline” | `cargo run` in transit root; open `/graphql` |
| Empty stop search | `health.stopCount` still 0 — wait for feed download |
| No itineraries | Valid OD stops; try later time; feeds with overlapping network |
| Map blank | Leaflet CSS/JS + OSM tiles; `#map` height 100% |
| No live markers | VP feed empty for those trips; try toggle off/on |
| WS never connects | `GRAPHQL_WS` / derived `ws://…/ws`; browser console |
| CORS errors | Confirm API CORS; GraphQL URL in footer |

## UI feature list

| Feature | Notes |
|---------|--------|
| FR default labels | Départ, Arrivée, Rechercher, Itinéraires, Réseau en direct, Temps réel, Accessible fauteuil, … |
| Stop autocomplete | Name + **code** badge when present |
| Plan form | Optional datetime, **Arriver avant** → `arriveBy`, **Accessible fauteuil** → `wheelchair: true` |
| Journey cards | Times, duration, correspondances, RT pill, **fare** if `fareAmount` |
| Leg badges | **Même véhicule** when `sameVehicle` |
| Map geometry | Per-leg colors + walk dashed |
| Live vehicles (journey) | Poll 12s + optional `watchTrips` WS; soft fallback |
| Réseau en direct | Global `vehicles` poll 12s by viewport; separate layer |
| Departures board | After stop pick |
| Mode legend | Train, métro, tram, bus, marche, ferry, fauteuil, alerte |
| Health badge | API status + stop/trip counts |
