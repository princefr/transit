//! Library surface for the transit itinerary server.
//! Enables integration tests and optional embedding without the HTTP binary.

pub mod api;
pub mod cache;
pub mod config;
pub mod equipment;
pub mod error;
pub mod gbfs;
pub mod feeds;
pub mod gtfs;
pub mod link;
pub mod prim;
pub mod routing;
pub mod rt;
pub mod search;
pub mod state;
