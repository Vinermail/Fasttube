use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Serialize, Clone, Default)]
pub struct DownloadProgress {
    pub bytes: u64,
    pub total: u64,
    pub done: bool,
    pub title: String,
}

#[derive(Clone, Default)]
pub struct ProgressHandle {
    pub bytes: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    pub done: Arc<std::sync::atomic::AtomicBool>,
    pub title: Arc<parking_lot::Mutex<String>>,
}

impl ProgressHandle {
    pub fn snapshot(&self) -> DownloadProgress {
        DownloadProgress {
            bytes: self.bytes.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            done: self.done.load(Ordering::Relaxed),
            title: self.title.lock().clone(),
        }
    }
}

const DATA_DIR_NAME: &str = "fasttube";
const LEGACY_DIR_NAME: &str = "player_llm";

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(DATA_DIR_NAME)
}

/// One-time migration of the pre-rebrand data directory.
pub fn migrate_legacy_dir() {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let legacy = base.join(LEGACY_DIR_NAME);
    let current = base.join(DATA_DIR_NAME);
    if legacy.is_dir() && !current.exists() {
        let _ = std::fs::rename(&legacy, &current);
    }
}

pub fn cache_dir() -> PathBuf {
    data_dir().join("cache")
}

pub fn covers_dir() -> PathBuf {
    cache_dir().join("covers")
}

pub fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}

/// Removes the whole download cache (audio files and covers).
pub fn clear_cache() -> std::io::Result<()> {
    let dir = cache_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(dir.join("covers"))?;
    Ok(())
}

/// True for a well-formed YouTube video id (11 chars of [A-Za-z0-9_-]).
pub fn is_valid_video_id(video_id: &str) -> bool {
    video_id.len() == 11
        && video_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Last line of defense against path traversal: strips everything except
/// [A-Za-z0-9_-] and caps the length, so arbitrary input can never escape
/// the cache directory.
fn safe_id(video_id: &str) -> String {
    video_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

/// Quality tag used by the on-disk cache layout.
pub const QUALITY_STANDARD: &str = "standard";
pub const QUALITY_HIGH: &str = "high";

/// Cache keys are baked into file names, so only known qualities are allowed;
/// anything else falls back to `standard` instead of escaping the directory.
fn safe_quality(quality: &str) -> &str {
    match quality {
        QUALITY_HIGH => QUALITY_HIGH,
        _ => QUALITY_STANDARD,
    }
}

/// Renames a pre-quality cache file (`<id>.<ext>`) to its `standard` form.
/// Those files were always downloaded at standard quality, so they belong to
/// that bucket. Best-effort: a failed rename just means a re-download.
fn migrate_legacy_cache_file(video_id: &str, ext: &str) -> PathBuf {
    migrate_legacy_cache_file_in(&cache_dir(), video_id, ext)
}

fn migrate_legacy_cache_file_in(dir: &Path, video_id: &str, ext: &str) -> PathBuf {
    let id = safe_id(video_id);
    let current = dir.join(format!("{id}.{QUALITY_STANDARD}.{ext}"));
    if current.exists() {
        return current;
    }
    let legacy = dir.join(format!("{id}.{ext}"));
    if legacy.exists() {
        match std::fs::rename(&legacy, &current) {
            Ok(()) => eprintln!("[cache] migrated {} -> {}", legacy.display(), current.display()),
            Err(e) => eprintln!("[cache] failed to migrate {}: {e}", legacy.display()),
        }
    }
    current
}

/// Audio file for one video at one quality: `<id>.<quality>.m4a`.
pub fn cached_file(video_id: &str, quality: &str) -> PathBuf {
    let quality = safe_quality(quality);
    if quality == QUALITY_STANDARD {
        return migrate_legacy_cache_file(video_id, "m4a");
    }
    cache_dir().join(format!("{}.{quality}.m4a", safe_id(video_id)))
}

/// Title sidecar matching [`cached_file`]: `<id>.<quality>.json`.
pub fn cached_meta(video_id: &str, quality: &str) -> PathBuf {
    let quality = safe_quality(quality);
    if quality == QUALITY_STANDARD {
        return migrate_legacy_cache_file(video_id, "json");
    }
    cache_dir().join(format!("{}.{quality}.json", safe_id(video_id)))
}

// ------------------------------------------------------------ cache eviction

/// One cached audio file as the eviction sees it.
#[derive(Debug)]
pub struct CacheEntry {
    pub path: PathBuf,
    pub size: u64,
    /// Last use: the mtime, which [`touch`] refreshes whenever a track starts
    /// playing. Windows does not update atime reliably, so mtime is the clock.
    pub used: std::time::SystemTime,
}

/// Marks a cached track as just used, so eviction treats it as the newest.
pub fn touch(path: &Path) {
    // A read handle cannot update the timestamp on Windows (SetFileTime needs
    // FILE_WRITE_ATTRIBUTES), so the file is opened for writing.
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// All audio files in the cache (covers and in-flight `.part` files excluded).
fn cache_entries() -> Vec<CacheEntry> {
    cache_entries_in(&cache_dir())
}

fn cache_entries_in(dir: &Path) -> Vec<CacheEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for item in read.flatten() {
        let path = item.path();
        if path.extension().and_then(|e| e.to_str()) != Some("m4a") {
            continue;
        }
        let Ok(meta) = item.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        entries.push(CacheEntry {
            path,
            size: meta.len(),
            used: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        });
    }
    entries
}

/// Total audio size of the download cache.
pub fn cache_size() -> u64 {
    cache_entries().iter().map(|e| e.size).sum()
}

/// Deletes the least recently used tracks until the cache fits `limit_bytes`
/// (`0` = no limit). Paths in `keep` (playing right now, being downloaded) are
/// never touched. Returns how many files went and how many bytes were freed.
pub fn evict_cache(limit_bytes: u64, keep: &[PathBuf]) -> (usize, u64) {
    evict_cache_in(&cache_dir(), limit_bytes, keep)
}

fn evict_cache_in(dir: &Path, limit_bytes: u64, keep: &[PathBuf]) -> (usize, u64) {
    if limit_bytes == 0 {
        return (0, 0);
    }
    let mut entries = cache_entries_in(dir);
    let mut total: u64 = entries.iter().map(|e| e.size).sum();
    if total <= limit_bytes {
        return (0, 0);
    }
    entries.sort_by_key(|e| e.used);

    let mut removed = 0;
    let mut freed = 0;
    for entry in entries {
        if total <= limit_bytes {
            break;
        }
        if keep.iter().any(|k| k == &entry.path) {
            continue;
        }
        // The title sidecar belongs to this file; drop it together so it does
        // not resurrect as an orphan.
        let meta = entry.path.with_extension("json");
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&meta);
                total = total.saturating_sub(entry.size);
                removed += 1;
                freed += entry.size;
            }
            // Locked by a reader (or already gone): leave it for the next sweep.
            Err(_) => continue,
        }
    }
    (removed, freed)
}

/// Downloads `url` to `dest`, publishing progress via `handle`.
/// Requires the caller to hold a tokio runtime context.
///
/// Large files are downloaded with several parallel byte-range connections,
/// which is dramatically faster through latency-heavy proxies.
pub async fn download(url: &str, dest: &Path, handle: &ProgressHandle) -> Result<()> {
    let client = build_client()?;

    // Probe request: obtains the total size (and confirms Range support).
    let response = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Range", "bytes=0-")
        .send()
        .await?
        .error_for_status()?;

    let total = response.content_length().unwrap_or(0);
    handle.total.store(total, Ordering::Relaxed);
    handle.bytes.store(0, Ordering::Relaxed);
    handle.done.store(false, Ordering::Relaxed);

    let parent = dest.parent().ok_or_else(|| anyhow!("no parent dir"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = dest.with_extension("part");

    if total > PARALLEL_THRESHOLD && response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        drop(response);
        parallel_download(&client, url, &tmp, total, handle).await?;
    } else {
        sequential_download(response, &tmp, handle).await?;
    }

    tokio::fs::rename(&tmp, dest).await?;
    handle.done.store(true, Ordering::Relaxed);
    Ok(())
}

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";
const PARALLEL_THRESHOLD: u64 = 8 * 1024 * 1024;
const MAX_CHUNKS: u64 = 6;

fn build_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().http1_only();
    if let Some(proxy) = crate::net::detect_proxy() {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder
        .timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?)
}

async fn sequential_download(
    response: reqwest::Response,
    tmp: &Path,
    handle: &ProgressHandle,
) -> Result<()> {
    let file = tokio::fs::File::create(tmp).await?;
    let mut sink = CountingSink {
        inner: file,
        bytes: handle.bytes.clone(),
    };
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        tokio::io::AsyncWriteExt::write_all(&mut sink, &chunk).await?;
    }
    Ok(())
}

async fn parallel_download(
    client: &reqwest::Client,
    url: &str,
    tmp: &Path,
    total: u64,
    handle: &ProgressHandle,
) -> Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let file = tokio::fs::File::create(tmp).await?;
    file.set_len(total).await?;
    drop(file);

    let chunks = MAX_CHUNKS.min(total);
    let chunk_size = total.div_ceil(chunks);
    let mut tasks = Vec::new();
    for i in 0..chunks {
        let start = i * chunk_size;
        if start >= total {
            break;
        }
        let end = ((i + 1) * chunk_size - 1).min(total - 1);
        let client = client.clone();
        let url = url.to_string();
        let tmp = tmp.to_path_buf();
        let bytes = handle.bytes.clone();
        tasks.push(tokio::spawn(async move {
            let resp = client
                .get(&url)
                .header("User-Agent", USER_AGENT)
                .header("Range", format!("bytes={start}-{end}"))
                .send()
                .await?
                .error_for_status()?;
            if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                anyhow::bail!("chunk request returned {}", resp.status());
            }
            let mut file = tokio::fs::OpenOptions::new().write(true).open(&tmp).await?;
            let mut stream = resp.bytes_stream();
            let mut pos = start;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                file.seek(std::io::SeekFrom::Start(pos)).await?;
                file.write_all(&chunk).await?;
                pos += chunk.len() as u64;
                bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Ok::<(), anyhow::Error>(())
        }));
    }
    for task in tasks {
        task.await??;
    }
    Ok(())
}

struct CountingSink {
    inner: tokio::fs::File,
    bytes: Arc<AtomicU64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake id, never a real video: keeps the test away from user cache files.
    const ID: &str = "testVid0001";

    #[test]
    fn cache_paths_are_quality_scoped() {
        let high = cached_file(ID, "high");
        assert_eq!(
            high.file_name().unwrap().to_string_lossy(),
            "testVid0001.high.m4a"
        );
        let meta = cached_meta(ID, "high");
        assert_eq!(
            meta.file_name().unwrap().to_string_lossy(),
            "testVid0001.high.json"
        );
        let standard = cached_file(ID, QUALITY_STANDARD);
        assert_eq!(
            standard.file_name().unwrap().to_string_lossy(),
            "testVid0001.standard.m4a"
        );
    }

    #[test]
    fn unknown_quality_falls_back_and_cannot_escape_cache_dir() {
        let path = cached_file(ID, "../../evil");
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "testVid0001.standard.m4a"
        );
        assert_eq!(path.parent().unwrap(), cache_dir());
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fasttube-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_cache_file_is_renamed_to_standard() {
        let dir = temp_dir("migrate");
        let legacy = dir.join("abc12345678.m4a");
        std::fs::write(&legacy, b"old track").unwrap();

        let got = migrate_legacy_cache_file_in(&dir, "abc12345678", "m4a");

        assert_eq!(got, dir.join("abc12345678.standard.m4a"));
        assert!(got.exists(), "renamed file must exist");
        assert_eq!(std::fs::read(&got).unwrap(), b"old track");
        assert!(!legacy.exists(), "legacy name must be gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_file_is_left_alone_when_standard_already_exists() {
        let dir = temp_dir("keep");
        let legacy = dir.join("abc12345678.m4a");
        let current = dir.join("abc12345678.standard.m4a");
        std::fs::write(&legacy, b"old").unwrap();
        std::fs::write(&current, b"new").unwrap();

        let got = migrate_legacy_cache_file_in(&dir, "abc12345678", "m4a");

        assert_eq!(got, current);
        assert_eq!(std::fs::read(&got).unwrap(), b"new");
        assert!(legacy.exists(), "legacy file must not be deleted");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn write_cached(dir: &Path, name: &str, size: usize, age_secs: u64) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; size]).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
        path
    }

    #[test]
    fn eviction_drops_the_least_recently_used_first() {
        let dir = temp_dir("evict-order");
        let old = write_cached(&dir, "aaaaaaaaaaa.standard.m4a", 1000, 300);
        let mid = write_cached(&dir, "bbbbbbbbbbb.standard.m4a", 1000, 200);
        let fresh = write_cached(&dir, "ccccccccccc.standard.m4a", 1000, 10);

        let (removed, freed) = evict_cache_in(&dir, 2000, &[]);

        assert_eq!((removed, freed), (1, 1000));
        assert!(!old.exists(), "oldest entry must go");
        assert!(mid.exists() && fresh.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eviction_keeps_the_playing_track_even_if_it_is_the_oldest() {
        let dir = temp_dir("evict-keep");
        let playing = write_cached(&dir, "ddddddddddd.standard.m4a", 1000, 500);
        let old = write_cached(&dir, "eeeeeeeeeee.standard.m4a", 1000, 400);
        let fresh = write_cached(&dir, "fffffffffff.standard.m4a", 1000, 5);

        let (removed, _) = evict_cache_in(&dir, 1000, std::slice::from_ref(&playing));

        assert_eq!(removed, 2);
        assert!(playing.exists(), "the playing file must survive");
        assert!(!old.exists() && !fresh.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eviction_removes_the_title_sidecar_with_the_audio() {
        let dir = temp_dir("evict-meta");
        let audio = write_cached(&dir, "ggggggggggg.standard.m4a", 1000, 100);
        let meta = dir.join("ggggggggggg.standard.json");
        std::fs::write(&meta, b"{}").unwrap();

        let (removed, _) = evict_cache_in(&dir, 500, &[]);

        assert_eq!(removed, 1);
        assert!(!audio.exists() && !meta.exists(), "both files must go");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zero_limit_means_unlimited() {
        let dir = temp_dir("evict-unlimited");
        let a = write_cached(&dir, "hhhhhhhhhhh.standard.m4a", 5000, 100);

        assert_eq!(evict_cache_in(&dir, 0, &[]), (0, 0));
        assert!(a.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entries_skip_covers_and_partial_downloads() {
        let dir = temp_dir("evict-scan");
        write_cached(&dir, "iiiiiiiiiii.standard.m4a", 10, 5);
        std::fs::write(dir.join("iiiiiiiiiii.standard.m4a.part"), b"partial").unwrap();
        std::fs::create_dir_all(dir.join("covers")).unwrap();
        std::fs::write(dir.join("covers").join("iiiiiiiiiii.jpg"), b"jpg").unwrap();

        let entries = cache_entries_in(&dir);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 10);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

impl tokio::io::AsyncWrite for CountingSink {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &poll {
            let now = self.bytes.load(Ordering::Relaxed) + *n as u64;
            self.bytes.store(now, Ordering::Relaxed);
        }
        poll
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
