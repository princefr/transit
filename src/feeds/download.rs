use crate::error::{Result, TransitError};
use reqwest::header::{ETAG, IF_NONE_MATCH, USER_AGENT};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, info, warn};
use zip::ZipArchive;

const DOWNLOAD_RETRIES: u32 = 2;
const RETRY_BASE_MS: u64 = 500;

#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
    pub sha256: String,
    pub not_modified: bool,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Required GTFS files that must exist in a static zip before promotion.
const REQUIRED_GTFS_FILES: &[&str] = &["stops.txt", "stop_times.txt"];

/// Validate that zip bytes contain required GTFS tables (case-insensitive basenames).
pub fn validate_gtfs_zip(bytes: &[u8]) -> Result<()> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor)
        .map_err(|e| TransitError::Parse(format!("invalid zip: {e}")))?;

    let mut found: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let file = archive
            .by_index(i)
            .map_err(|e| TransitError::Parse(format!("zip entry: {e}")))?;
        let name = file.name().replace('\\', "/");
        let base = name
            .rsplit('/')
            .next()
            .unwrap_or(&name)
            .to_ascii_lowercase();
        if !base.is_empty() {
            found.push(base);
        }
    }

    for req in REQUIRED_GTFS_FILES {
        let need = req.to_ascii_lowercase();
        if !found.iter().any(|f| f == &need) {
            return Err(TransitError::Parse(format!(
                "GTFS zip missing required file: {req}"
            )));
        }
    }
    Ok(())
}

fn is_transient_http(err: &TransitError) -> bool {
    match err {
        TransitError::Http(msg) => {
            let m = msg.to_ascii_lowercase();
            m.contains("timeout")
                || m.contains("timed out")
                || m.contains("connection")
                || m.contains("reset")
                || m.contains("temporarily")
                || m.contains(" 429")
                || m.contains(" 502")
                || m.contains(" 503")
                || m.contains(" 504")
                || m.contains("-> 429")
                || m.contains("-> 502")
                || m.contains("-> 503")
                || m.contains("-> 504")
        }
        TransitError::Download(_) => true,
        _ => false,
    }
}

async fn download_conditional_once(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    etag: Option<&str>,
    timeout: Duration,
    extra_headers: &[(String, String)],
) -> Result<DownloadResult> {
    let mut req = client
        .get(url)
        .header(USER_AGENT, user_agent)
        .timeout(timeout);
    for (k, v) in extra_headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(e) = etag {
        req = req.header(IF_NONE_MATCH, e);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| TransitError::Http(e.to_string()))?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        debug!(url, "not modified");
        return Ok(DownloadResult {
            bytes: vec![],
            etag: etag.map(|s| s.to_string()),
            sha256: String::new(),
            not_modified: true,
        });
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(TransitError::Http(format!("GET {url} -> {status}")));
    }
    let etag = resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| TransitError::Http(e.to_string()))?
        .to_vec();
    let sha256 = sha256_hex(&bytes);
    info!(url, bytes = bytes.len(), %sha256, "downloaded");
    Ok(DownloadResult {
        bytes,
        etag,
        sha256,
        not_modified: false,
    })
}

/// Conditional GET with up to 2 retries and exponential backoff on transient errors.
pub async fn download_conditional(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    etag: Option<&str>,
    timeout: Duration,
) -> Result<DownloadResult> {
    download_conditional_with_headers(client, url, user_agent, etag, timeout, &[]).await
}

/// Same as [`download_conditional`] with optional extra HTTP headers (e.g. PRIM `apikey`).
pub async fn download_conditional_with_headers(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    etag: Option<&str>,
    timeout: Duration,
    extra_headers: &[(String, String)],
) -> Result<DownloadResult> {
    let mut attempt = 0u32;
    loop {
        match download_conditional_once(client, url, user_agent, etag, timeout, extra_headers)
            .await
        {
            Ok(r) => return Ok(r),
            Err(e) if attempt < DOWNLOAD_RETRIES && is_transient_http(&e) => {
                attempt += 1;
                let wait = Duration::from_millis(RETRY_BASE_MS * 2u64.pow(attempt - 1));
                warn!(
                    url,
                    attempt,
                    retries = DOWNLOAD_RETRIES,
                    wait_ms = wait.as_millis() as u64,
                    error = %e,
                    "download transient failure; retrying"
                );
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

pub async fn download_bytes(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    download_bytes_with_headers(client, url, user_agent, timeout, &[]).await
}

pub async fn download_bytes_with_headers(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    timeout: Duration,
    extra_headers: &[(String, String)],
) -> Result<Vec<u8>> {
    let r =
        download_conditional_with_headers(client, url, user_agent, None, timeout, extra_headers)
            .await?;
    Ok(r.bytes)
}

pub fn feed_data_dir(data_dir: &Path, feed_id: &str) -> PathBuf {
    data_dir.join(feed_id)
}

pub fn ensure_feed_dirs(data_dir: &Path, feed_id: &str) -> Result<PathBuf> {
    let dir = feed_data_dir(data_dir, feed_id);
    std::fs::create_dir_all(dir.join("incoming"))?;
    Ok(dir)
}

pub fn current_zip_path(data_dir: &Path, feed_id: &str) -> PathBuf {
    feed_data_dir(data_dir, feed_id).join("current.zip")
}

pub fn meta_path(data_dir: &Path, feed_id: &str) -> PathBuf {
    feed_data_dir(data_dir, feed_id).join("current.meta.json")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
pub struct FeedMeta {
    pub etag: Option<String>,
    pub sha256: Option<String>,
    pub loaded_at: Option<String>,
    pub static_url: Option<String>,
}

pub fn read_meta(data_dir: &Path, feed_id: &str) -> FeedMeta {
    let p = meta_path(data_dir, feed_id);
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_meta(data_dir: &Path, feed_id: &str, meta: &FeedMeta) -> Result<()> {
    ensure_feed_dirs(data_dir, feed_id)?;
    let p = meta_path(data_dir, feed_id);
    let s = serde_json::to_string_pretty(meta).map_err(|e| TransitError::Other(e.into()))?;
    std::fs::write(p, s)?;
    Ok(())
}

/// Write bytes to incoming/, validate GTFS tables, then promote to current.zip.
pub fn write_current_zip(data_dir: &Path, feed_id: &str, bytes: &[u8]) -> Result<PathBuf> {
    validate_gtfs_zip(bytes)?;
    let dir = ensure_feed_dirs(data_dir, feed_id)?;
    let incoming = dir.join("incoming").join(format!(
        "{}.zip",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    ));
    std::fs::write(&incoming, bytes)?;
    let current = dir.join("current.zip");
    // Atomic-ish promote: write temp then rename when possible
    let tmp = dir.join("current.zip.tmp");
    std::fs::copy(&incoming, &tmp)?;
    std::fs::rename(&tmp, &current).or_else(|_| {
        std::fs::copy(&incoming, &current)?;
        let _ = std::fs::remove_file(&tmp);
        Ok::<(), std::io::Error>(())
    })?;
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn make_zip_with(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zw = ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, content) in files {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(content.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        buf
    }

    #[test]
    fn meta_roundtrip_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let meta = FeedMeta {
            etag: Some("\"abc\"".into()),
            sha256: Some("deadbeef".into()),
            loaded_at: Some("2020-01-01T00:00:00Z".into()),
            static_url: Some("https://example.com/gtfs.zip".into()),
        };
        write_meta(dir.path(), "testfeed", &meta).unwrap();
        let got = read_meta(dir.path(), "testfeed");
        assert_eq!(got, meta);
        assert!(meta_path(dir.path(), "testfeed").exists());
    }

    #[test]
    fn read_meta_missing_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let got = read_meta(dir.path(), "nope");
        assert_eq!(got, FeedMeta::default());
    }

    #[test]
    fn validate_zip_requires_stops_and_stop_times() {
        let bad = make_zip_with(&[("agency.txt", "a")]);
        assert!(validate_gtfs_zip(&bad).is_err());

        let good = make_zip_with(&[
            ("stops.txt", "stop_id,stop_name\n1,A\n"),
            ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n"),
            ("agency.txt", "x"),
        ]);
        assert!(validate_gtfs_zip(&good).is_ok());
    }

    #[test]
    fn validate_zip_nested_paths() {
        let good = make_zip_with(&[
            ("gtfs/stops.txt", "x"),
            ("gtfs/stop_times.txt", "y"),
        ]);
        assert!(validate_gtfs_zip(&good).is_ok());
    }

    #[test]
    fn write_current_rejects_bad_zip() {
        let dir = tempfile::tempdir().unwrap();
        let bad = make_zip_with(&[("agency.txt", "a")]);
        assert!(write_current_zip(dir.path(), "f", &bad).is_err());
        assert!(!current_zip_path(dir.path(), "f").exists());
    }

    #[test]
    fn write_current_promotes_good_zip() {
        let dir = tempfile::tempdir().unwrap();
        let good = make_zip_with(&[
            ("stops.txt", "s"),
            ("stop_times.txt", "t"),
        ]);
        let path = write_current_zip(dir.path(), "f", &good).unwrap();
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), good);
    }

    #[test]
    fn sha256_stable() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
