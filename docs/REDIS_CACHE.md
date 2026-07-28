# Redis cache

Optional Redis layer for **hot, repeat GraphQL queries** and `/health`. Disabled by default — the server runs fully in-process without Redis.

## Recommendation summary

| Layer | Redis? | Why |
|-------|--------|-----|
| Static GTFS packed epoch (`StaticEpoch`) | **No** | Hundreds of MB–GB; already in `ArcSwap` per process. Serializing/deserializing to Redis would be slower than RAM and does not fix the ~3 min cold parse (that is disk zip + CPU parse). |
| RT overlay (`RealtimeOverlay`) | **No** | Updates every poll; `ArcSwap` + `rt.version` is the right model. |
| RAPTOR inputs (stop departures, walk graph) | **No** | Same as epoch — must live in process memory to route. |
| Search (`stops`, `addresses`, `places`) | **Yes** | Cheap per call but high QPS on autocomplete; stable per `epoch_id`. |
| Vehicles (`bbox` / `near`) | **Yes** | Map pan/zoom hammers this; short TTL (10s default). |
| Itineraries | **Yes** | RAPTOR is seconds–minutes on IDFM scale; cache core result keyed by `epoch_id` + `rt.version`. |
| `/health` | **Yes** | Short TTL for load-balancer probes across instances. |
| SIRI trip alias map | **Deferred** | Already in epoch; lookup is O(1) in memory. |

## Run Redis locally

```bash
docker run -d --name transit-redis -p 6379:6379 redis:7-alpine
```

Enable in config:

```toml
[redis]
enabled = true
url = "redis://127.0.0.1:6379"
```

Or via env:

```bash
export TRANSIT__REDIS__ENABLED=true
export TRANSIT__REDIS__URL=redis://127.0.0.1:6379
cargo run --release
```

If Redis is unreachable at startup, the server logs a warning and continues **without** caching.

## What is cached

| Key pattern | Resolver / endpoint | Default TTL | Invalidation |
|-------------|-------------------|-------------|--------------|
| `transit:v1:stops:{epoch_id}:{hash}` | `stops` | 300s | New `epoch_id` on static reload |
| `transit:v1:addr:{hash}` | `addresses` | 300s | BAN index is process-local; restart to refresh |
| `transit:v1:places:{epoch_id}:{hash}` | `places` | 300s | New `epoch_id` |
| `transit:v1:veh:{epoch_id}:rt{N}:{hash}` | `vehicles` (bbox/near only) | 10s | `rt.version` in key + TTL |
| `transit:v1:itin:{epoch_id}:rt{N}:{hash}` | `itineraries` | 45s | `rt.version` in key + TTL |
| `transit:v1:health:{epoch_id}:rt{N}` | `GET /health` | 5s | `rt.version` in key + TTL |

Hash inputs include normalized query text, geo filters, limits, and (for itineraries) origin/destination + departure minute bucket.

On static epoch swap, old epoch keys are best-effort deleted via `SCAN` (optional hygiene; TTL + new `epoch_id` also prevent stale hits).

## Verify

1. Start Redis and enable `[redis]`.
2. `curl -s http://127.0.0.1:8080/health | jq .cache` → `redis_enabled: true`, `redis_connected: true`.
3. Repeat a GraphQL search; server logs `redis cache hit` at `debug` level (`RUST_LOG=transit=debug`).
4. `redis-cli KEYS 'transit:v1:*'` shows keys after traffic.
5. Disable Redis (`enabled = false`) — app still serves requests.

## Expected latency wins

| Path | Cold (no Redis) | Warm (cache hit) |
|------|-----------------|------------------|
| `places("Châtelet")` | ~5–30 ms (stop scan + BAN) | ~1–3 ms |
| `vehicles(bbox: …)` | ~20–80 ms (overlay + synth) | ~1–5 ms |
| `itineraries` IDFM A→B | **5–90 s** (RAPTOR) | ~50–200 ms (deserialize + RT enrich) |
| `GET /health` | ~1–5 ms | ~1 ms |

Multi-instance: identical keys let N pods share itinerary and map snapshots without N× RAPTOR work.

## Not implemented (deferred)

- Full epoch blob in Redis (shared cold start across pods) — poor ROI vs shared NFS/disk zip + per-pod `ArcSwap`.
- RT overlay replication — use one writer or accept per-instance PRIM pollers.
- GraphQL response-level HTTP cache headers — app-layer JSON cache only.
- Redis pub/sub for cross-instance RT invalidation — short TTLs + `rt.version` in keys are sufficient for now.
