use crate::cache::TransitCache;
use crate::config::Config;
use crate::equipment::{new_shared_equipment, EquipmentCatalog, SharedEquipment};
use crate::gtfs::pack::StaticEpoch;
use crate::prim::poller::{new_shared_prim, PrimCatalog, SharedPrim};
use crate::prim::PrimClient;
use crate::rt::overlay::RealtimeOverlay;
use arc_swap::ArcSwap;
use ban_search::BanIndex;
use std::sync::{Arc, OnceLock};
use tokio::sync::{broadcast, Semaphore};
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
    /// Optional Redis cache (no-op when disabled or unreachable).
    pub cache: TransitCache,
    /// Global cap on concurrent itinerary planning jobs (each occupies one
    /// blocking-pool thread until RAPTOR finishes — timeouts do not cancel it).
    pub plan_slots: Arc<Semaphore>,
    /// Global cap on open `/ws` subscription connections.
    pub ws_slots: Arc<Semaphore>,
    /// Global cap on in-flight non-websocket HTTP requests.
    pub http_slots: Arc<Semaphore>,
    /// Shared PRIM client (lazily created on first use; `None` when no API key).
    pub prim_client: Arc<OnceLock<Option<Arc<PrimClient>>>>,
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
            // Also rebuild when a present file failed to load (old format / corrupt).
            if !index_path.exists() || ban.load().address_count() == 0 {
                let ban2 = ban.clone();
                let _ = std::thread::Builder::new()
                    .name("ban-provision".into())
                    .spawn(move || provision_ban_index(ban2, dir, depts));
            }
        }
        let plan_slots = Arc::new(Semaphore::new(
            config.runtime.effective_max_concurrent_plans(),
        ));
        let ws_slots = Arc::new(Semaphore::new(config.server.max_ws_connections.max(1)));
        let http_slots = Arc::new(Semaphore::new(config.server.max_concurrent_requests.max(1)));
        let epoch = Arc::new(ArcSwap::from_pointee(StaticEpoch::empty()));
        // Lets `prim::stop_board` validate on-demand stop ids against the live epoch.
        crate::prim::stop_board::install_epoch_source(epoch.clone());
        Self {
            plan_slots,
            ws_slots,
            http_slots,
            prim_client: Arc::new(OnceLock::new()),
            config: Arc::new(config),
            epoch,
            rt: Arc::new(ArcSwap::from_pointee(RealtimeOverlay::default())),
            rt_version_tx,
            equipment: new_shared_equipment(),
            prim: new_shared_prim(),
            ban,
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
