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
