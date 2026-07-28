//! Elevator / escalator status for Île-de-France (IDFM).
//!
//! Primary source: PRIM Navitia disruptions titled “Panne d'un ascenseur”
//! (equipment_reports and the open `etat-des-ascenseurs` dataset are often
//! empty or not provisioned on a given API key). Optional ODS export is tried
//! as a secondary source when records are available.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Equipment category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EquipmentKind {
    Elevator,
    Escalator,
    Other,
}

impl EquipmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Elevator => "Elevator",
            Self::Escalator => "Escalator",
            Self::Other => "Other",
        }
    }

    pub fn parse(s: &str) -> Self {
        let l = s.to_ascii_lowercase();
        if l.contains("escal") || l.contains("escalier") {
            Self::Escalator
        } else if l.contains("elev") || l.contains("ascenseur") || l.contains("lift") {
            Self::Elevator
        } else {
            Self::Other
        }
    }
}

/// Availability of a piece of equipment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EquipmentAvailability {
    Available,
    Unavailable,
    Unknown,
}

impl EquipmentAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Unavailable => "Unavailable",
            Self::Unknown => "Unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        let l = s.to_ascii_lowercase();
        if matches!(
            l.as_str(),
            "available" | "ok" | "up" | "fonctionnel" | "available_status" | "1"
        ) || l.contains("disponible")
            || l.contains("en service")
        {
            Self::Available
        } else if matches!(
            l.as_str(),
            "unavailable" | "down" | "out" | "hs" | "0" | "unavailable_status"
        ) || l.contains("indisponible")
            || l.contains("panne")
            || l.contains("hors service")
        {
            Self::Unavailable
        } else {
            Self::Unknown
        }
    }
}

/// Snapshot of one elevator / escalator (or other accessibility equipment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquipmentStatus {
    pub id: String,
    /// Stop id when known (namespaced `idfm:…` or GTFS `feed:raw`).
    pub stop_id: String,
    pub name: String,
    pub kind: EquipmentKind,
    pub status: EquipmentAvailability,
    pub updated_at: DateTime<Utc>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub stop_name: Option<String>,
    pub detail: Option<String>,
    pub source: String,
}

/// In-memory equipment catalog (immutable snapshot under ArcSwap).
#[derive(Debug, Clone, Default)]
pub struct EquipmentCatalog {
    pub items: Vec<EquipmentStatus>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub source_note: String,
}

impl EquipmentCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn by_stop<'a>(&'a self, stop_id: &str) -> Vec<&'a EquipmentStatus> {
        let want = normalize_stop_key(stop_id);
        self.items
            .iter()
            .filter(|e| {
                normalize_stop_key(&e.stop_id) == want
                    || e.stop_id == stop_id
                    || stop_id_suffix_match(&e.stop_id, stop_id)
            })
            .collect()
    }

    pub fn unavailable(&self) -> impl Iterator<Item = &EquipmentStatus> {
        self.items
            .iter()
            .filter(|e| e.status == EquipmentAvailability::Unavailable)
    }
}

/// Loose match for IDFM stop ids (`stop_area:IDFM:71517` vs `idfm:71517` vs GTFS).
pub fn normalize_stop_key(id: &str) -> String {
    let s = id.trim().to_ascii_lowercase();
    // strip common prefixes
    let s = s
        .strip_prefix("stop_area:")
        .or_else(|| s.strip_prefix("stop_point:"))
        .unwrap_or(&s);
    let s = s
        .strip_prefix("idfm:")
        .or_else(|| s.strip_prefix("idfm%3a"))
        .unwrap_or(s);
    s.replace("idfm:", "").replace(":", "")
}

fn stop_id_suffix_match(a: &str, b: &str) -> bool {
    let na = normalize_stop_key(a);
    let nb = normalize_stop_key(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na == nb || na.ends_with(&nb) || nb.ends_with(&na)
}

/// Shared handle type used in AppState.
pub type SharedEquipment = Arc<arc_swap::ArcSwap<EquipmentCatalog>>;

pub fn new_shared_equipment() -> SharedEquipment {
    Arc::new(arc_swap::ArcSwap::from_pointee(EquipmentCatalog::empty()))
}
