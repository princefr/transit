//! Disk persistence for FLASH-TB arc-flag data.
//!
//! The forward flag preprocessing takes ~1h on IDFM-scale feeds. Feeds like
//! IDFM reset nightly, so flags are persisted keyed by the **feed content
//! hash**: a server restart within the same dataset version loads flags in
//! seconds, and a nightly rebuild recomputes at most once per dataset version
//! (the previous epoch keeps serving while the new one builds off-thread).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::cell_bitset::CellBitSet;
use super::pack::{StaticEpoch, TripTransferEntry};

/// v2: header carries the pack fingerprint (trip set + pack parameters).
const MAGIC: &[u8; 6] = b"FLSH2\n";
/// Max fingerprint length accepted in the header.
const MAX_FP_LEN: usize = 256;

/// Content hash across all feed bundles (order-independent).
pub fn feed_content_hash(bundles: &[std::sync::Arc<super::pack::FeedStaticBundle>]) -> String {
    use sha2::{Digest, Sha256};
    let mut parts: Vec<String> = bundles
        .iter()
        .map(|b| format!("{}:{}", b.feed_id, b.sha256))
        .collect();
    parts.sort();
    let mut h = Sha256::new();
    for p in &parts {
        h.update(p.as_bytes());
        h.update(b"\n");
    }
    format!("{:x}", h.finalize())
}

pub fn flash_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(format!("flash-{hash}.bin"))
}

/// Serialize `arc_flag_pattern`, `flag_patterns` and the pruned
/// `trip_transfers` from `epoch` to `path`. `fingerprint` identifies the
/// exact packed trip set and pack parameters (see `pack::flash_pack_fingerprint`);
/// [`load_flash`] rejects files whose fingerprint differs.
pub fn save_flash(path: &Path, epoch: &StaticEpoch, fingerprint: &str) -> io::Result<()> {
    if fingerprint.len() > MAX_FP_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fingerprint too long",
        ));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(8 << 20);
    buf.extend_from_slice(MAGIC);
    put_u32(&mut buf, fingerprint.len() as u32);
    buf.extend_from_slice(fingerprint.as_bytes());
    put_u32(&mut buf, epoch.num_cells);
    put_u32(&mut buf, epoch.arc_flag_pattern.len() as u32);

    put_u32(&mut buf, epoch.flag_patterns.len() as u32);
    for p in &epoch.flag_patterns {
        let words = p.words();
        put_u32(&mut buf, words.len() as u32);
        for w in words {
            buf.extend_from_slice(&w.to_le_bytes());
        }
    }

    for &pid in &epoch.arc_flag_pattern {
        put_u32(&mut buf, pid);
    }

    put_u32(&mut buf, epoch.trip_transfers.len() as u32);
    for per_alight in &epoch.trip_transfers {
        put_u32(&mut buf, per_alight.len() as u32);
        for entries in per_alight {
            put_u32(&mut buf, entries.len() as u32);
            for e in entries {
                put_u32(&mut buf, e.target_trip);
                put_u32(&mut buf, e.target_board_off);
                put_u32(&mut buf, e.min_transfer_s);
                put_u32(&mut buf, e.flag_pattern);
            }
        }
    }

    let tmp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Decoded flag cache: `(num_cells, arc_flag_pattern, flag_patterns, trip_transfers)`.
pub type FlashData = (
    u32,
    Vec<u32>,
    Vec<CellBitSet>,
    Vec<Vec<Vec<TripTransferEntry>>>,
);

/// Load flag data previously written by [`save_flash`]. Returns `Ok(None)`
/// when the file is absent, has an old/unknown format, or was written for a
/// different `fingerprint`. Callers must still [`validate_flash`] it against
/// the epoch before use.
pub fn load_flash(path: &Path, fingerprint: &str) -> io::Result<Option<FlashData>> {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut r = Reader {
        data: &data,
        pos: 0,
    };
    if r.data.len() < MAGIC.len() || &r.data[..MAGIC.len()] != MAGIC {
        return Ok(None);
    }
    r.pos = MAGIC.len();
    let fp_len = r.u32()? as usize;
    if fp_len > MAX_FP_LEN || r.bytes(fp_len)? != fingerprint.as_bytes() {
        return Ok(None);
    }
    let num_cells = r.u32()?;
    let walk_n = r.u32()? as usize;

    let n_patterns = r.u32()? as usize;
    let mut flag_patterns = Vec::with_capacity(n_patterns.min(1 << 20));
    for _ in 0..n_patterns {
        let n_words = r.u32()? as usize;
        if n_words > 4096 {
            return Ok(None); // corrupt
        }
        let mut words = Vec::with_capacity(n_words);
        for _ in 0..n_words {
            words.push(u64::from_le_bytes(r.bytes(8)?.try_into().unwrap()));
        }
        flag_patterns.push(CellBitSet::from_words(words));
    }

    if r.data.len() < r.pos + walk_n * 4 {
        return Ok(None);
    }
    let mut arc_flag_pattern = Vec::with_capacity(walk_n);
    for _ in 0..walk_n {
        arc_flag_pattern.push(r.u32()?);
    }

    let n_trips = r.u32()? as usize;
    let mut trip_transfers = Vec::with_capacity(n_trips.min(4 << 20));
    for _ in 0..n_trips {
        let offs = r.u32()? as usize;
        let mut per_alight = Vec::with_capacity(offs.min(1024));
        for _ in 0..offs {
            let n_entries = r.u32()? as usize;
            let mut entries = Vec::with_capacity(n_entries.min(4096));
            for _ in 0..n_entries {
                entries.push(TripTransferEntry {
                    target_trip: r.u32()?,
                    target_board_off: r.u32()?,
                    min_transfer_s: r.u32()?,
                    flag_pattern: r.u32()?,
                });
            }
            per_alight.push(entries);
        }
        trip_transfers.push(per_alight);
    }

    Ok(Some((
        num_cells,
        arc_flag_pattern,
        flag_patterns,
        trip_transfers,
    )))
}

/// Check every index in cached flag data against `epoch` (shape and
/// offsets). Any mismatch means the cache does not belong to this epoch and
/// must be discarded — the query code indexes `stop_times` /
/// `flag_patterns` with these values.
pub fn validate_flash(epoch: &StaticEpoch, data: &FlashData) -> Result<(), String> {
    let (num_cells, arc_flag_pattern, flag_patterns, trip_transfers) = data;
    if *num_cells != epoch.num_cells {
        return Err(format!("num_cells {} != {}", num_cells, epoch.num_cells));
    }
    let max_words = (*num_cells as usize).div_ceil(64);
    if let Some(i) = flag_patterns
        .iter()
        .position(|p| p.words().len() > max_words)
    {
        return Err(format!("flag pattern {i} wider than {num_cells} cells"));
    }
    let n_patterns = flag_patterns.len();
    if arc_flag_pattern.len() != epoch.walk_edges.len() {
        return Err(format!(
            "arc_flag_pattern len {} != walk edges {}",
            arc_flag_pattern.len(),
            epoch.walk_edges.len()
        ));
    }
    if let Some(i) = arc_flag_pattern
        .iter()
        .position(|&p| p as usize >= n_patterns)
    {
        return Err(format!("walk edge {i} pattern out of range"));
    }
    if trip_transfers.len() != epoch.trips.len() {
        return Err(format!(
            "trip_transfers len {} != trips {}",
            trip_transfers.len(),
            epoch.trips.len()
        ));
    }
    for (ti, per_alight) in trip_transfers.iter().enumerate() {
        let trip = &epoch.trips[ti];
        if per_alight.len() != trip.stop_time_len as usize {
            return Err(format!(
                "trip {ti}: {} alight slots != stop_time_len {}",
                per_alight.len(),
                trip.stop_time_len
            ));
        }
        for e in per_alight.iter().flatten() {
            let Some(target) = epoch.trips.get(e.target_trip as usize) else {
                return Err(format!(
                    "trip {ti}: target_trip {} out of range",
                    e.target_trip
                ));
            };
            if e.target_board_off >= target.stop_time_len {
                return Err(format!(
                    "trip {ti}: target_board_off {} >= len {} of trip {}",
                    e.target_board_off, target.stop_time_len, e.target_trip
                ));
            }
            if e.flag_pattern as usize >= n_patterns {
                return Err(format!(
                    "trip {ti}: flag_pattern {} out of range",
                    e.flag_pattern
                ));
            }
        }
    }
    Ok(())
}

/// Remove stale flag files in `dir`, keeping only `keep`.
pub fn prune_flash_dir(dir: &Path, keep: &str) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("flash-")
                && name.ends_with(".bin")
                && name != format!("flash-{keep}.bin")
            {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated"));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::cell_bitset::CellBitSet;

    #[test]
    fn flash_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash-abc.bin");

        let num_cells = 256u32;
        let arc_flag_pattern = vec![0u32, 1, 2, 1, 0];
        let mut p0 = CellBitSet::new(num_cells);
        p0.set_bit(3);
        let mut p1 = CellBitSet::new(num_cells);
        p1.set_bit(7);
        p1.set_bit(200);
        let flag_patterns = vec![p0, p1];
        let trip_transfers = vec![
            vec![
                vec![],
                vec![TripTransferEntry {
                    target_trip: 5,
                    target_board_off: 2,
                    min_transfer_s: 120,
                    flag_pattern: 1,
                }],
            ],
            vec![vec![]],
        ];

        let mut epoch = StaticEpoch::empty();
        epoch.num_cells = num_cells;
        epoch.arc_flag_pattern = arc_flag_pattern.clone();
        epoch.flag_patterns = flag_patterns.clone();
        epoch.trip_transfers = trip_transfers.clone();

        save_flash(&path, &epoch, "fp-a").unwrap();
        // Different pack fingerprint (e.g. trip set changed under the same
        // zip SHA) → cache ignored.
        assert!(load_flash(&path, "fp-b").unwrap().is_none());
        let (cells, ap, fp, tt) = load_flash(&path, "fp-a").unwrap().unwrap();
        assert_eq!(cells, num_cells);
        assert_eq!(ap, arc_flag_pattern);
        assert_eq!(fp.len(), 2);
        assert!(fp[1].get_bit(200));
        assert!(!fp[1].get_bit(8));
        assert_eq!(tt, trip_transfers);

        // Corrupt magic → treated as absent
        std::fs::write(&path, b"garbage").unwrap();
        assert!(load_flash(&path, "fp-a").unwrap().is_none());
    }

    #[test]
    fn validate_rejects_out_of_range_offsets() {
        use crate::gtfs::pack::GlobalTrip;
        use crate::gtfs::pack::RouteMode;
        let trip = |len: u32| GlobalTrip {
            id: "t".into(),
            feed_id: "f".into(),
            route_id: "r".into(),
            service_id: "s".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Bus,
            route_short_name: String::new(),
            route_long_name: String::new(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 3,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len: len,
            frequency_windows: vec![],
        };
        let mut epoch = StaticEpoch::empty();
        epoch.num_cells = 64;
        epoch.trips = vec![trip(2), trip(3)];
        let entry = |target_trip, target_board_off| TripTransferEntry {
            target_trip,
            target_board_off,
            min_transfer_s: 120,
            flag_pattern: 0,
        };
        let good: FlashData = (
            64,
            vec![],
            vec![CellBitSet::new(64)],
            vec![
                vec![vec![entry(1, 2)], vec![]],
                vec![vec![], vec![], vec![]],
            ],
        );
        assert!(validate_flash(&epoch, &good).is_ok());

        let mut bad_off = good.clone();
        bad_off.3[0][0][0].target_board_off = 3; // trip 1 has 3 stops
        assert!(validate_flash(&epoch, &bad_off).is_err());

        let mut bad_trip = good.clone();
        bad_trip.3[0][0][0].target_trip = 2;
        assert!(validate_flash(&epoch, &bad_trip).is_err());

        let mut bad_pat = good.clone();
        bad_pat.3[0][0][0].flag_pattern = 1;
        assert!(validate_flash(&epoch, &bad_pat).is_err());

        let mut bad_len = good;
        bad_len.3[1].pop();
        assert!(validate_flash(&epoch, &bad_len).is_err());
    }
}
