# Routing graph completeness

## Principle

**If routing must special-case gares (hardcoded “Gare du Nord”, via-hub stitching, manual GTFS checks), the graph is incomplete.**

RAPTOR only boards trips and walks **edges that exist** in `StaticEpoch.walk_edges` / `walk_adj`. Transfers RER → Métro only work if those boardable points are linked at **pack time**.

## What we build (no named hubs)

| Layer | Source | Role |
|-------|--------|------|
| Pathways | GTFS `pathways.txt` | Indoor / official connections |
| Transfers | GTFS `transfers.txt` | Operator-declared min transfer |
| Parent/children | `parent_station` | Platform ↔ station siblings |
| **Boardable transfer graph** | Grid among stops with **departures** (+ monomodal/multimodal places) within **~450–550 m** | Completes monomodal RER ↔ Métro/tram/bus without hardcoding names |
| Hub-link (supervisor) | Grid among station + monomodal + boardable points | Extra short walks; **never skipped** solely because the network is large |

## User-facing places

- Search collapses to **one station/city name** (prefer boardable monomodal).
- `resolve_access_stops` expands a pick to the **place cluster** (same-name + nearby boardable monomodals). End users never choose a quay.

## Edge cases (document, fix in graph/access — not hardcoded lists)

| Case | Symptom | Fix location |
|------|---------|--------------|
| Empty monomodal shell | Search picks place with no departures | Demote empty places in search; prefer boardable |
| RER monomodal ≠ Métro monomodal same gare | Direct RAPTOR finds A→GDN but not A→Châtelet | Boardable transfer edges at pack; place cluster on dest |
| Hub link skipped for IDFM | No walks between modes | Do not skip grid links; build boardable subgraph |
| Pure walk preferred over transit | max_walk too large, OD close | Tune max_walk / ranking (not hub names) |
| SIRI trip id ≠ GTFS | Live vehicles missing | Estimate stop-id mapping (separate from routing graph) |
| PRIM 429 | Sparse RT | Rate limit pollers (not graph) |
| Itinerary timeout | Too many RAPTOR seeds / huge access set | Cap k-best on large epochs; keep graph walks O(1) adj |

## What not to do

- ❌ Hardcode gare / hub name lists for routing
- ❌ “If no path, query GTFS for known hubs”
- ❌ Skip hub linking because `stops.len() > N` (leaves graph incomplete)

## Ops check

After load, log `walk_edges` on epoch swap. For SNCF+IDFM expect **hundreds of thousands** of walk edges if the transfer graph is healthy. Near-zero walks ⇒ multi-mode itineraries will fail.
