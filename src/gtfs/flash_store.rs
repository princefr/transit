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

const MAGIC: &[u8; 6] = b"FLSH1\n";

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
/// `trip_transfers` from `epoch` to `path`.
pub fn save_flash(path: &Path, epoch: &StaticEpoch) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 << 20);
    buf.extend_from_slice(MAGIC);
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

/// Load flag data previously written by [`save_flash`].
/// Returns `(num_cells, arc_flag_pattern, flag_patterns, trip_transfers)`.
pub fn load_flash(
    path: &Path,
) -> io::Result<Option<(u32, Vec<u32>, Vec<CellBitSet>, Vec<Vec<Vec<TripTransferEntry>>>)>> {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut r = Reader { data: &data, pos: 0 };
    if r.data.len() < MAGIC.len() || &r.data[..MAGIC.len()] != MAGIC {
        return Ok(None);
    }
    r.pos = MAGIC.len();
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

    Ok(Some((num_cells, arc_flag_pattern, flag_patterns, trip_transfers)))
}

/// Remove stale flag files in `dir`, keeping only `keep`.
pub fn prune_flash_dir(dir: &Path, keep: &str) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("flash-") && name.ends_with(".bin") && name != format!("flash-{keep}.bin")
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

        let epoch_like = () // save_flash takes &StaticEpoch; build a minimal one
        ;
        let _ = epoch_like;
        let mut epoch = StaticEpoch::empty();
        epoch.num_cells = num_cells;
        epoch.arc_flag_pattern = arc_flag_pattern.clone();
        epoch.flag_patterns = flag_patterns.clone();
        epoch.trip_transfers = trip_transfers.clone();

        save_flash(&path, &epoch).unwrap();
        let (cells, ap, fp, tt) = load_flash(&path).unwrap().unwrap();
        assert_eq!(cells, num_cells);
        assert_eq!(ap, arc_flag_pattern);
        assert_eq!(fp.len(), 2);
        assert_eq!(fp[1].get_bit(200), true);
        assert_eq!(fp[1].get_bit(8), false);
        assert_eq!(tt, trip_transfers);

        // Corrupt magic → treated as absent
        std::fs::write(&path, b"garbage").unwrap();
        assert!(load_flash(&path).unwrap().is_none());
    }
}
