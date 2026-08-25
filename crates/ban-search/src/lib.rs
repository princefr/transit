//! Base Adresse Nationale (BAN) — download, compact index, autocomplete.
//!
//! Data: official open dumps from [adresse.data.gouv.fr](https://adresse.data.gouv.fr)
//! (CSV département files, semicolon-separated, UTF-8).
//!
//! # Typical flow
//!
//! ```ignore
//! use ban_search::{BanConfig, BanIndex, download_departments};
//!
//! let cfg = BanConfig::idf_default("./data/ban");
//! download_departments(&cfg)?;
//! let index = BanIndex::build_from_csv_dir(&cfg.csv_dir(), &cfg.departments)?;
//! index.save(&cfg.index_path())?;
//! let hits = index.suggest("12 rue de rivoli paris", 8);
//! ```

mod download;
mod index;
mod normalize;
mod types;

pub use download::{
    ban_csv_url, download_departments, download_departments_force, DEFAULT_BASE_URL,
};
pub use index::BanIndex;
pub use normalize::{normalize_query, strip_accents};
pub use types::{AddressHit, BanConfig, BanError, BanStats};

/// Île-de-France départements (default coverage for the transit map).
pub const IDF_DEPARTMENTS: &[&str] = &["75", "77", "78", "91", "92", "93", "94", "95"];

/// All départements covered by the BAN CSV dumps (metropolitan France with
/// 2A/2B for Corsica, plus overseas). An empty department list means "all".
pub const FRANCE_DEPARTMENTS: &[&str] = &[
    "01", "02", "03", "04", "05", "06", "07", "08", "09",
    "10", "11", "12", "13", "14", "15", "16", "17", "18", "19",
    "2A", "2B",
    "21", "22", "23", "24", "25", "26", "27", "28", "29",
    "30", "31", "32", "33", "34", "35", "36", "37", "38", "39",
    "40", "41", "42", "43", "44", "45", "46", "47", "48", "49",
    "50", "51", "52", "53", "54", "55", "56", "57", "58", "59",
    "60", "61", "62", "63", "64", "65", "66", "67", "68", "69",
    "70", "71", "72", "73", "74", "75", "76", "77", "78", "79",
    "80", "81", "82", "83", "84", "85", "86", "87", "88", "89",
    "90", "91", "92", "93", "94", "95",
    "971", "972", "973", "974", "975", "976",
];
