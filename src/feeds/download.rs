use crate::error::{Result, TransitError};
use reqwest::header::{ETAG, IF_NONE_MATCH, USER_AGENT};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, info, warn};
use zip::ZipArchive;

const DOWNLOAD_RETRIES: u32 = 2;
const RETRY_BASE_MS: u64 = 500;

/// Upper bound for a static GTFS zip download (IDFM is ~100 MB today).
pub const MAX_STATIC_ZIP_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// Upper bound for a single GTFS-RT / SIRI realtime payload.
pub const MAX_RT_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB
/// Timestamped zips kept in `incoming/` after a promotion (most recent first).
const KEEP_INCOMING_ZIPS: usize = 2;

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
        let name = file
            .name()
            .map_err(|e| TransitError::Parse(format!("zip entry name: {e}")))?
            .replace('\\', "/");
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
    max_bytes: u64,
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
    let bytes = read_body_capped(resp, url, max_bytes).await?;
    // Hashing a ~100 MB static zip takes long enough to stall an async worker.
    let (bytes, sha256) = if bytes.len() > 1024 * 1024 {
        tokio::task::spawn_blocking(move || {
            let sha = sha256_hex(&bytes);
            (bytes, sha)
        })
        .await
        .map_err(|e| TransitError::Other(e.into()))?
    } else {
        let sha = sha256_hex(&bytes);
        (bytes, sha)
    };
    info!(url, bytes = bytes.len(), %sha256, "downloaded");
    Ok(DownloadResult {
        bytes,
        etag,
        sha256,
        not_modified: false,
    })
}

fn body_too_large(url: &str, max_bytes: u64) -> TransitError {
    TransitError::Http(format!(
        "GET {url}: response body exceeds limit of {max_bytes} bytes"
    ))
}

/// Stream the response body, rejecting it as soon as it exceeds `max_bytes`
/// (also up-front when the server announces a larger `Content-Length`).
async fn read_body_capped(
    mut resp: reqwest::Response,
    url: &str,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let announced = resp.content_length();
    if announced.is_some_and(|n| n > max_bytes) {
        return Err(body_too_large(url, max_bytes));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(announced.unwrap_or(0) as usize);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| TransitError::Http(e.to_string()))?
    {
        if buf.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(body_too_large(url, max_bytes));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Conditional GET with up to 2 retries and exponential backoff on transient errors.
/// Body is capped at [`MAX_STATIC_ZIP_BYTES`].
pub async fn download_conditional(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    etag: Option<&str>,
    timeout: Duration,
) -> Result<DownloadResult> {
    download_conditional_with_headers(
        client,
        url,
        user_agent,
        etag,
        timeout,
        &[],
        MAX_STATIC_ZIP_BYTES,
    )
    .await
}

/// Same as [`download_conditional`] with optional extra HTTP headers (e.g. PRIM `apikey`)
/// and an explicit response body cap.
pub async fn download_conditional_with_headers(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    etag: Option<&str>,
    timeout: Duration,
    extra_headers: &[(String, String)],
    max_bytes: u64,
) -> Result<DownloadResult> {
    let mut attempt = 0u32;
    loop {
        match download_conditional_once(
            client,
            url,
            user_agent,
            etag,
            timeout,
            extra_headers,
            max_bytes,
        )
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
    download_bytes_with_headers(client, url, user_agent, timeout, &[], MAX_RT_BYTES).await
}

pub async fn download_bytes_with_headers(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    timeout: Duration,
    extra_headers: &[(String, String)],
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let r = download_conditional_with_headers(
        client,
        url,
        user_agent,
        None,
        timeout,
        extra_headers,
        max_bytes,
    )
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

/// Validate GTFS tables, stage bytes in incoming/, then promote to current.zip.
///
/// The staged file is consumed by the promotion (renamed into place) and removed
/// on failure, so `incoming/` does not grow with every static update; leftovers
/// from older versions / crashes are pruned to the newest [`KEEP_INCOMING_ZIPS`].
pub fn write_current_zip(data_dir: &Path, feed_id: &str, bytes: &[u8]) -> Result<PathBuf> {
    validate_gtfs_zip(bytes)?;
    let dir = ensure_feed_dirs(data_dir, feed_id)?;
    let incoming_dir = dir.join("incoming");
    let incoming = incoming_dir.join(format!(
        "{}.zip",
        chrono::Utc::now().format("%Y%m%d%H%M%S%3f")
    ));
    let current = dir.join("current.zip");
    let promoted = std::fs::write(&incoming, bytes).and_then(|()| {
        // Same directory tree → rename is atomic; fall back to copy across devices.
        std::fs::rename(&incoming, &current).or_else(|_| {
            let tmp = dir.join("current.zip.tmp");
            let r = std::fs::copy(&incoming, &tmp)
                .and_then(|_| std::fs::rename(&tmp, &current));
            let _ = std::fs::remove_file(&tmp);
            r
        })
    });
    // Rename consumed it on success; remove whatever is left otherwise.
    let _ = std::fs::remove_file(&incoming);
    prune_incoming(&incoming_dir, KEEP_INCOMING_ZIPS);
    promoted?;
    Ok(current)
}

/// Keep only the newest `keep` `*.zip` files in `incoming/` (names are timestamps).
fn prune_incoming(incoming_dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(incoming_dir) else {
        return;
    };
    let mut zips: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "zip"))
        .collect();
    if zips.len() <= keep {
        return;
    }
    zips.sort();
    let drop_n = zips.len() - keep;
    for p in zips.into_iter().take(drop_n) {
        if let Err(e) = std::fs::remove_file(&p) {
            warn!(path = %p.display(), error = %e, "failed to prune incoming zip");
        }
    }
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
        // Staged file is consumed by the promotion.
        let incoming = dir.path().join("f").join("incoming");
        assert_eq!(std::fs::read_dir(&incoming).unwrap().count(), 0);
    }

    #[test]
    fn write_current_prunes_leftover_incoming() {
        let dir = tempfile::tempdir().unwrap();
        let incoming = dir.path().join("f").join("incoming");
        std::fs::create_dir_all(&incoming).unwrap();
        for name in ["20200101000000.zip", "20200102000000.zip", "20200103000000.zip"] {
            std::fs::write(incoming.join(name), b"old").unwrap();
        }
        let good = make_zip_with(&[("stops.txt", "s"), ("stop_times.txt", "t")]);
        write_current_zip(dir.path(), "f", &good).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&incoming)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["20200102000000.zip", "20200103000000.zip"]);
    }

    #[test]
    fn sha256_stable() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
