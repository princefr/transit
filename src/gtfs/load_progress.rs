//! Global dataset-loading progress, surfaced via `/health` so the frontend
//! can show a percentage + ETA while the epoch builds.
//!
//! Phases are coarse except FLASH-TB flag preprocessing (the dominant cost on
//! large feeds), which reports real per-source throughput for a linear ETA.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_DOWNLOAD: u8 = 1;
pub const PHASE_PARSE: u8 = 2;
pub const PHASE_PACK: u8 = 3;
pub const PHASE_FLAGS_LOAD: u8 = 4;
pub const PHASE_FLAGS_COMPUTE: u8 = 5;
pub const PHASE_READY: u8 = 6;

struct LoadProgress {
    phase: AtomicU8,
    /// Flags-compute counters (sources × departure samples).
    done: AtomicU64,
    total: AtomicU64,
    /// Wall-clock ms when the current phase started (UNIX ms).
    started_ms: AtomicU64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn instance() -> &'static LoadProgress {
    static I: OnceLock<LoadProgress> = OnceLock::new();
    I.get_or_init(|| LoadProgress {
        phase: AtomicU8::new(PHASE_IDLE),
        done: AtomicU64::new(0),
        total: AtomicU64::new(0),
        started_ms: AtomicU64::new(now_ms()),
    })
}

/// Transition to a new phase (resets sub-progress + timer).
pub fn set_phase(phase: u8) {
    let p = instance();
    let prev = p.phase.swap(phase, Ordering::SeqCst);
    if prev != phase {
        p.done.store(0, Ordering::SeqCst);
        p.total.store(0, Ordering::SeqCst);
        p.started_ms.store(now_ms(), Ordering::SeqCst);
    }
}

/// Report incremental flags-compute progress (cheap atomics; call per batch).
pub fn flags_tick(done_delta: u64) {
    instance().done.fetch_add(done_delta, Ordering::Relaxed);
}

pub fn flags_set_total(total: u64) {
    let p = instance();
    p.total.store(total, Ordering::SeqCst);
    p.done.store(0, Ordering::SeqCst);
    p.started_ms.store(now_ms(), Ordering::SeqCst);
}

fn phase_label(phase: u8) -> &'static str {
    match phase {
        PHASE_DOWNLOAD => "Téléchargement du dataset GTFS…",
        PHASE_PARSE => "Analyse des fichiers GTFS…",
        PHASE_PACK => "Préparation du réseau (lignes, partition, correspondances)…",
        PHASE_FLAGS_LOAD => "Chargement des drapeaux précalculés…",
        PHASE_FLAGS_COMPUTE => "Précalcul FLASH-TB des drapeaux (une fois par version de données)…",
        _ => "Réseau prêt",
    }
}

/// JSON blob appended to `/health`: `{phase, label, percent, etaSeconds}`.
pub fn snapshot_json() -> serde_json::Value {
    use serde_json::json;
    let p = instance();
    let phase = p.phase.load(Ordering::SeqCst);
    let label = phase_label(phase);
    let (percent, eta_seconds) = match phase {
        PHASE_PACK => (Some(6), None),
        PHASE_FLAGS_LOAD => (Some(92), None),
        PHASE_FLAGS_COMPUTE => {
            let total = p.total.load(Ordering::Relaxed);
            let done = p.done.load(Ordering::Relaxed).min(total);
            if total == 0 {
                (Some(10), None)
            } else {
                let frac = done as f64 / total as f64;
                let elapsed = now_ms().saturating_sub(p.started_ms.load(Ordering::Relaxed)) / 1000;
                let eta = if done > 0 {
                    Some((elapsed as f64 * (1.0 - frac) / frac) as u64)
                } else {
                    None
                };
                (Some((10.0 + 85.0 * frac) as u32), eta)
            }
        }
        PHASE_READY => (Some(100), None),
        _ => (None, None),
    };
    json!({
        "phase": phase,
        "label": label,
        "percent": percent,
        "etaSeconds": eta_seconds,
    })
}
