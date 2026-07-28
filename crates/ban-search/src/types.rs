use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for BAN data layout and download.
#[derive(Debug, Clone)]
pub struct BanConfig {
    /// Root directory (e.g. `./data/ban`).
    pub data_dir: PathBuf,
    /// Département codes to index (`"75"`, …).
    pub departments: Vec<String>,
    /// BAN CSV base URL (no trailing slash).
    pub base_url: String,
    /// Max addresses kept in the in-memory index (0 = unlimited).
    pub max_addresses: usize,
}

impl BanConfig {
    pub fn idf_default(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            departments: crate::IDF_DEPARTMENTS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            base_url: crate::DEFAULT_BASE_URL.to_string(),
            max_addresses: 0,
        }
    }

    pub fn csv_dir(&self) -> PathBuf {
        self.data_dir.join("csv")
    }

    pub fn index_path(&self) -> PathBuf {
        self.data_dir.join("index.bin")
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.csv_dir())?;
        std::fs::create_dir_all(&self.data_dir)?;
        Ok(())
    }
}

/// One autocomplete / geocode hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddressHit {
    /// Stable BAN id when known.
    pub id: String,
    /// Full display label, e.g. `12 Rue de Rivoli 75001 Paris`.
    pub label: String,
    /// House number (may be empty for street-level hits).
    pub number: String,
    /// Repetition (`bis`, `ter`, …).
    pub rep: String,
    /// Street name.
    pub street: String,
    /// Postal code.
    pub postcode: String,
    /// City name.
    pub city: String,
    /// WGS84 latitude.
    pub lat: f64,
    /// WGS84 longitude.
    pub lon: f64,
    /// Ranking score (higher is better).
    pub score: f32,
    /// `address` (with number) or `street` (centroid).
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BanStats {
    pub address_count: usize,
    pub street_count: usize,
    pub departments: Vec<String>,
    pub built_at_unix: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BanError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("csv: {0}")]
    Csv(#[from] csv::Error),
    #[error("download: {0}")]
    Download(String),
    #[error("index: {0}")]
    Index(String),
    #[error("bincode: {0}")]
    Bincode(String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}

impl BanError {
    pub fn index(msg: impl Into<String>) -> Self {
        Self::Index(msg.into())
    }
}


