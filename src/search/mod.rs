pub mod stops;

pub use stops::{
    build_stop_search_index, is_place_like, near, near_stops, place_kind_score, search_stops,
    search_stops_filtered, strip_accents, StopSearchEntry,
};
