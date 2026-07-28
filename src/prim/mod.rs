//! Île-de-France Mobilités **PRIM** — raw **SIRI Lite** (and optional GTFS) only.
//!
//! Does **not** use Navitia marketplace JSON (`/v2/navitia/*`).
//!
//! ## Continuous realtime ([`spawn_prim_pollers`])
//!
//! | Service | Endpoint | Soft budget |
//! |---------|----------|-------------|
//! | Prochains passages — unitaire | `GET /marketplace/stop-monitoring?MonitoringRef=` | ~800k/day (cap 1M) |
//! | Prochains passages — globale | `GET /marketplace/estimated-timetable?LineRef=` | ~900/day (cap 1k) |
//! | Messages écrans | `GET /marketplace/general-message?LineRef=` | ~18k/day (cap 20k) |
//!
//! GraphQL live boards call stop-monitoring on demand and
//! [`register_sm_interest`] so the warm SM poller keeps that stop refreshed.
//! Map `vehicles(bbox|near)` also registers nearby stations for SM interest.
//!
//! Static schedules: configure `[[feeds]] id = "idfm"` with the public GTFS zip.

pub mod client;
pub mod poller;
pub mod stop_board;

pub use client::PrimClient;
pub use poller::{
    new_shared_prim, register_et_line_interest, register_nearby_sm_interest,
    register_sm_interest, register_viewport_live_interest, sm_try_consume_quota,
    spawn_prim_poller, spawn_prim_pollers, PrimCatalog, SharedPrim,
};
pub use stop_board::{
    fetch_live_board, fetch_live_departures, monitoring_ref_from_stop_id, LiveBoardResult,
    LiveDeparture,
};
