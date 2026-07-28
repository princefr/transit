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
    /// Optional Redis cache (no-op when disabled or unreachable).
    pub cache: TransitCache,
}

impl AppState {
    pub fn new(config: Config, cache: TransitCache) -> Self {
        let (rt_version_tx, _) = broadcast::channel(64);
        let ban = load_ban_index(&config);
        Self {
            config: Arc::new(config),
            epoch: Arc::new(ArcSwap::from_pointee(StaticEpoch::empty())),
            rt: Arc::new(ArcSwap::from_pointee(RealtimeOverlay::default())),
            rt_version_tx,
            equipment: new_shared_equipment(),
            prim: new_shared_prim(),
            ban: Arc::new(ArcSwap::from_pointee(ban)),
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
                "BAN index not loaded — run `make ban-index` (addresses disabled until then)"
            );
            BanIndex::empty()
        }
    }
}
