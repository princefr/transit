use crate::cache::TransitCache;
use crate::config::Config;
use crate::equipment::{new_shared_equipment, EquipmentCatalog, SharedEquipment};
use crate::gtfs::pack::StaticEpoch;
use crate::prim::poller::{new_shared_prim, PrimCatalog, SharedPrim};
use crate::rt::overlay::RealtimeOverlay;
use arc_swap::ArcSwap;
use ban_search::BanIndex;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::feeds::supervisor::RtVersion;

pub type SharedBan = Arc<ArcSwap<BanIndex>>;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub epoch: Arc<ArcSwap<StaticEpoch>>,
    pub rt: Arc<ArcSwap<RealtimeOverlay>>,
    pub rt_version_tx: broadcast::Sender<RtVersion>,
    /// Elevator / escalator status (PRIM + open data).
    pub equipment: SharedEquipment,
    /// PRIM Navitia disruptions catalog.
    pub prim: SharedPrim,
    /// Base Adresse Nationale local index (may be empty if not built).
    pub ban: SharedBan,
    /// GBFS bike/scooter-share station snapshot (empty when disabled).
    pub gbfs: crate::gbfs::SharedGbfs,
    /// Optional Redis cache (no-op when disabled or unreachable).
    pub cache: TransitCache,
}

impl AppState {
    pub fn new(config: Config, cache: TransitCache) -> Self {
        let (rt_version_tx, _) = broadcast::channel(64);
        let ban = load_ban_index(&config);
        let ban: SharedBan = Arc::new(ArcSwap::from_pointee(ban));
        // Missing index? Download + build all configured départements in the
        // background, then hot-swap into the live state — no restart needed.
        if config.ban.enabled && config.ban.auto_download {
            let dir = config.ban.resolved_data_dir(&config.runtime.data_dir);
            let depts = config.ban.resolved_departments();
            let index_path = dir.join("index.bin");
            if !index_path.exists() {
                let ban2 = ban.clone();
                let _ = std::thread::Builder::new()
                    .name("ban-provision".into())
                    .spawn(move || provision_ban_index(ban2, dir, depts));
            }
        }
        let gbfs: crate::gbfs::SharedGbfs =
            Arc::new(arc_swap::ArcSwap::from_pointee(crate::gbfs::empty_snapshot()));
        crate::gbfs::spawn_gbfs_poller_self(&config, gbfs.clone());
        Self {
            config: Arc::new(config),
            epoch: Arc::new(ArcSwap::from_pointee(StaticEpoch::empty())),
            rt: Arc::new(ArcSwap::from_pointee(RealtimeOverlay::default())),
            rt_version_tx,
            equipment: new_shared_equipment(),
            prim: new_shared_prim(),
            ban,
            gbfs,
            cache,
        }
    }

    pub fn load_epoch(&self) -> Arc<StaticEpoch> {
        self.epoch.load_full()
    }

    pub fn load_rt(&self) -> Arc<RealtimeOverlay> {
        self.rt.load_full()
    }

    pub fn load_equipment(&self) -> Arc<EquipmentCatalog> {
        self.equipment.load_full()
    }

    pub fn load_prim(&self) -> Arc<PrimCatalog> {
        self.prim.load_full()
    }

    pub fn load_ban(&self) -> Arc<BanIndex> {
        self.ban.load_full()
    }
}

fn load_ban_index(config: &Config) -> BanIndex {
    if !config.ban.enabled {
        info!("BAN address index disabled");
        return BanIndex::empty();
    }
    let dir = config.ban.resolved_data_dir(&config.runtime.data_dir);
    let path = dir.join("index.bin");
    match BanIndex::load(&path) {
        Ok(idx) => {
            info!(
                path = %path.display(),
                streets = idx.street_count(),
                addresses = idx.address_count(),
                "BAN address index ready"
            );
            idx
        }
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "BAN index not loaded — building it in the background (or run `make ban-index`)"
            );
            BanIndex::empty()
        }
    }
}

/// Background provisioning: download missing département CSVs, build
/// `index.bin`, persist, and hot-swap into the running state.
fn provision_ban_index(ban: SharedBan, dir: std::path::PathBuf, depts: Vec<String>) {
    let mut cfg = ban_search::BanConfig::france_default(&dir);
    cfg.departments = depts;
    if let Err(e) = cfg.ensure_dirs() {
        warn!(error = %e, "BAN auto-provision: mkdir failed");
        return;
    }
    info!(
        departments = if cfg.departments.is_empty() {
            "all France".to_string()
        } else {
            cfg.departments.join(",")
        },
        "BAN auto-provision: downloading missing département CSVs (background)"
    );
    if let Err(e) = ban_search::download_departments(&cfg) {
        warn!(error = %e, "BAN auto-provision: download failed — run `make ban-index-fr` manually");
        return;
    }
    info!("BAN auto-provision: building index (this can take a few minutes for all France)");
    match BanIndex::build_from_csv_dir(&cfg.csv_dir(), &cfg.departments) {
        Ok(idx) => {
            let path = cfg.index_path();
            if let Err(e) = idx.save(&path) {
                warn!(error = %e, "BAN auto-provision: save failed");
            }
            info!(
                streets = idx.street_count(),
                addresses = idx.address_count(),
                "BAN auto-provision: index ready — address search is live"
            );
            ban.store(Arc::new(idx));
        }
        Err(e) => {
            warn!(error = %e, "BAN auto-provision: build failed — run `make ban-index-fr` manually");
        }
    }
}
