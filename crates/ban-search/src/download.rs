//! Download BAN département CSV dumps from adresse.data.gouv.fr.

use crate::types::{BanConfig, BanError};
use flate2::read::GzDecoder;
use std::fs::{self, File};
use std::io::{copy, BufReader, BufWriter, Write};
use std::path::Path;
use tracing::{info, warn};

/// Official BAN historical CSV base (département files).
pub const DEFAULT_BASE_URL: &str = "https://adresse.data.gouv.fr/data/ban/adresses/latest/csv";

/// URL for one département gzip CSV.
pub fn ban_csv_url(base: &str, dept: &str) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}/adresses-{dept}.csv.gz")
}

/// Download configured départements into `cfg.csv_dir()` as plain `.csv` files.
///
/// Skips files that already exist unless `force` is true.
pub fn download_departments(cfg: &BanConfig) -> Result<Vec<std::path::PathBuf>, BanError> {
    download_departments_force(cfg, false)
}

pub fn download_departments_force(
    cfg: &BanConfig,
    force: bool,
) -> Result<Vec<std::path::PathBuf>, BanError> {
    cfg.ensure_dirs()?;
    let client = reqwest::blocking::Client::builder()
        .user_agent("transit-ban-search/0.1 (+https://github.com/local/transit; BAN open data)")
        .timeout(std::time::Duration::from_secs(900))
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(0)
        .build()?;

    let mut out = Vec::new();
    for dept in &cfg.departments {
        let dest = cfg.csv_dir().join(format!("adresses-{dept}.csv"));
        if dest.exists() && !force {
            info!(%dept, path = %dest.display(), "BAN CSV already present — skip download");
            out.push(dest);
            continue;
        }
        let url = ban_csv_url(&cfg.base_url, dept);
        info!(%dept, %url, "downloading BAN CSV");
        let tmp_gz = cfg.csv_dir().join(format!("adresses-{dept}.csv.gz.part"));
        let mut last_err = String::new();
        let mut ok = false;
        for attempt in 1..=4 {
            match client.get(&url).send() {
                Ok(resp) if resp.status().is_success() => {
                    let mut f = BufWriter::new(File::create(&tmp_gz)?);
                    let mut body = resp;
                    match copy(&mut body, &mut f) {
                        Ok(_) => {
                            f.flush()?;
                            ok = true;
                            break;
                        }
                        Err(e) => {
                            last_err = format!("copy attempt {attempt}: {e}");
                            warn!(%dept, attempt, error = %e, "BAN download copy failed — retry");
                        }
                    }
                }
                Ok(resp) => {
                    last_err = format!("HTTP {}", resp.status());
                    warn!(%dept, attempt, status = %resp.status(), "BAN download HTTP error — retry");
                }
                Err(e) => {
                    last_err = e.to_string();
                    warn!(%dept, attempt, error = %e, "BAN download failed — retry");
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2 * attempt as u64));
        }
        if !ok {
            return Err(BanError::Download(format!("{url}: {last_err}")));
        }
        // Decompress
        let gz = File::open(&tmp_gz)?;
        let mut decoder = GzDecoder::new(BufReader::new(gz));
        let tmp_csv = cfg.csv_dir().join(format!("adresses-{dept}.csv.part"));
        {
            let mut out_f = BufWriter::new(File::create(&tmp_csv)?);
            copy(&mut decoder, &mut out_f)?;
            out_f.flush()?;
        }
        fs::rename(&tmp_csv, &dest)?;
        let _ = fs::remove_file(&tmp_gz);
        let meta = fs::metadata(&dest)?;
        info!(
            %dept,
            path = %dest.display(),
            bytes = meta.len(),
            "BAN CSV ready"
        );
        out.push(dest);
    }
    Ok(out)
}

/// True if path looks like a BAN adresses CSV.
pub fn is_ban_csv(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("adresses-") && n.ends_with(".csv"))
}

/// Warn if directory is empty.
#[allow(dead_code)]
pub fn list_csv_files(dir: &Path) -> Result<Vec<std::path::PathBuf>, BanError> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    for e in fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if is_ban_csv(&p) {
            files.push(p);
        }
    }
    files.sort();
    if files.is_empty() {
        warn!(dir = %dir.display(), "no BAN CSV files found");
    }
    Ok(files)
}
