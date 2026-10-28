use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;

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
    /// Milliseconds from the start of `download` to the first body byte.
    /// Measurement only: 0 means "no byte arrived yet".
    pub first_byte_ms: Arc<AtomicU64>,
}

impl ProgressHandle {
    /// Records the first byte of the current transfer once; later chunks do not
    /// overwrite it (`compare_exchange` keeps the earliest value).
    fn mark_first_byte(&self, started: std::time::Instant) {
        let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let _ = self.first_byte_ms.compare_exchange(
            0,
            ms,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

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

/// Deletes what an earlier version left behind: files that are exactly one probe
/// chunk long (a download that saved only the first megabyte) and half-written
/// `.part` files. Returns how many files went away.
pub fn sweep_broken_cache(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut dropped = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let broken = if name.ends_with(".part") {
            true
        } else if name.ends_with(".m4a") || name.ends_with(".webm") {
            // A real track is never exactly the probe size: 1 048 576 bytes is
            // the signature of the truncation bug.
            std::fs::metadata(&path)
                .map(|m| m.len() == PROBE_BYTES)
                .unwrap_or(false)
        } else {
            false
        };
        if broken && std::fs::remove_file(&path).is_ok() {
            dropped += 1;
        }
    }
    dropped
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
/// One client for the whole process: its connection pool survives between
/// tracks, which is most of the win for a sequential queue of downloads.
static CLIENT: OnceLock<Mutex<Option<(Option<String>, reqwest::Client)>>> = OnceLock::new();

/// The client for `proxy`, rebuilt only when the proxy changes.
fn shared_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let slot = CLIENT.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock();
    if let Some((key, client)) = guard.as_ref()
        && key.as_deref() == proxy
    {
        return Ok(client.clone());
    }
    let client = build_client(proxy)?;
    *guard = Some((proxy.map(str::to_string), client.clone()));
    Ok(client)
}

/// The probe asks for a byte range and keeps the answer: those bytes are the
/// first chunk of the file, there is no reason to download them twice.
const PROBE_BYTES: u64 = 1024 * 1024;

/// `Content-Range: bytes 0-1048575/19300593` → `19300593`.
fn total_from_content_range(value: &str) -> Option<u64> {
    let total = value.rsplit('/').next()?.trim();
    if total.is_empty() || total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

/// Probe request: the total size, whether the server honours ranges, and the
/// response itself, whose body becomes the first chunk.
/// A connection failure usually means the proxy came or went, so the cached
/// answer is dropped and the request is repeated once.
async fn probe(url: &str) -> Result<(reqwest::Client, reqwest::Response, u64, bool)> {
    let mut retried = false;
    loop {
        let proxy = crate::net::detect_proxy_async().await;
        let client = shared_client(proxy.as_deref())?;
        let attempt = client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Range", format!("bytes=0-{}", PROBE_BYTES - 1))
            .send()
            .await;
        match attempt {
            Ok(response) => {
                let response = response.error_for_status()?;
                let partial = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
                let total = response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(total_from_content_range)
                    .or_else(|| response.content_length())
                    .unwrap_or(0);
                return Ok((client, response, total, partial));
            }
            Err(e) if !retried && (e.is_connect() || e.is_timeout()) => {
                crate::net::invalidate_proxy_cache();
                retried = true;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub async fn download(url: &str, dest: &Path, handle: &ProgressHandle) -> Result<()> {
    let started = std::time::Instant::now();
    let (client, response, total, partial) = probe(url).await?;

    let probe_ms = started.elapsed().as_millis();
    handle.total.store(total, Ordering::Relaxed);
    handle.bytes.store(0, Ordering::Relaxed);
    handle.first_byte_ms.store(0, Ordering::Relaxed);
    handle.done.store(false, Ordering::Relaxed);

    let parent = dest.parent().ok_or_else(|| anyhow!("no parent dir"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = dest.with_extension("part");

    let head = response.content_length().unwrap_or(0);
    let rest = total.saturating_sub(head);
    let chunks = chunk_count(rest);
    // Whenever the answer is a range and the file is longer than that range, the
    // rest has to be fetched. Requiring two chunks here was the bug: a track
    // between one and two megabytes took the "sequential" path with the *probe*
    // response, which only carries the first megabyte, and the truncated file
    // was renamed into the cache as if it were complete.
    let parallel = partial && head > 0 && total > head && rest > 0;
    let written = if parallel {
        parallel_download(&client, url, &tmp, total, head, chunks, handle, started, response)
            .await?
    } else {
        sequential_download(response, &tmp, handle, started).await?;
        0
    };
    let _ = written;

    // The last line of defence: a file that is not exactly the size the server
    // announced never reaches the cache. Without this a short write becomes a
    // "cached" track that can never play.
    let written_len = tokio::fs::metadata(&tmp).await?.len();
    if total > 0 && written_len != total {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow!(
            "the download stopped at {written_len} of {total} bytes"
        ));
    }

    tokio::fs::rename(&tmp, dest).await?;
    handle.done.store(true, Ordering::Relaxed);

    // One line per finished transfer: how long the server took to answer, how
    // long the whole thing took and what the average rate was.
    let elapsed = started.elapsed();
    let secs = elapsed.as_secs_f64().max(0.001);
    let bytes = handle.bytes.load(Ordering::Relaxed);
    eprintln!(
        "[perf] download_ms={} probe_ms={} ttfb_ms={} bytes={} mbps={:.2} parallel={} chunks={} head={}",
        elapsed.as_millis(),
        probe_ms,
        handle.first_byte_ms.load(Ordering::Relaxed),
        bytes,
        bytes as f64 / secs / 1_000_000.0,
        parallel,
        if parallel { chunks + 1 } else { 1 },
        head
    );
    Ok(())
}

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";
const CHUNK_UNIT: u64 = 1024 * 1024;
const MAX_CHUNKS: u64 = 4;
/// Writes go out in 256 KiB blocks instead of whatever the socket delivers.
const WRITE_BUFFER: usize = 256 * 1024;

fn chunk_count(total: u64) -> u64 {
    // A 1.5 MiB file is two pieces, not one: the unit is the *chunk size*,
    // so the threshold and the first split fall together at 1 MiB.
    total.div_ceil(CHUNK_UNIT).clamp(1, MAX_CHUNKS)
}

fn build_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().http1_only();
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

/// A buffered writer positioned at `start`: each chunk task writes one
/// contiguous range, so the buffer never has to seek.
async fn seek_for_write(start: u64, file: tokio::fs::File) -> Result<tokio::io::BufWriter<tokio::fs::File>> {
    use tokio::io::AsyncSeekExt;

    let mut file = file;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    Ok(tokio::io::BufWriter::with_capacity(WRITE_BUFFER, file))
}

async fn sequential_download(
    response: reqwest::Response,
    tmp: &Path,
    handle: &ProgressHandle,
    started: std::time::Instant,
) -> Result<()> {
    let file = tokio::fs::File::create(tmp).await?;
    let sink = CountingSink {
        inner: file,
        bytes: handle.bytes.clone(),
    };
    let mut writer = tokio::io::BufWriter::with_capacity(WRITE_BUFFER, sink);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        handle.mark_first_byte(started);
        tokio::io::AsyncWriteExt::write_all(&mut writer, &chunk).await?;
    }
    tokio::io::AsyncWriteExt::flush(&mut writer).await?;
    Ok(())
}

/// Streams a response body into the file at `start`, counting bytes.
async fn write_stream(
    response: reqwest::Response,
    tmp: &Path,
    start: u64,
    bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    first_byte: std::sync::Arc<std::sync::atomic::AtomicU64>,
    started: std::time::Instant,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let file = tokio::fs::OpenOptions::new().write(true).open(tmp).await?;
    let mut writer = seek_for_write(start, file).await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let _ = first_byte.compare_exchange(
            0,
            started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        writer.write_all(&chunk).await?;
        bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    writer.flush().await?;
    Ok(())
}

/// `head` bytes are already in hand (the probe response); everything after them
/// is fetched in parallel ranges.
#[allow(clippy::too_many_arguments)]
async fn parallel_download(
    client: &reqwest::Client,
    url: &str,
    tmp: &Path,
    total: u64,
    head: u64,
    chunks: u64,
    handle: &ProgressHandle,
    started: std::time::Instant,
    probe: reqwest::Response,
) -> Result<u64> {
    let file = tokio::fs::File::create(tmp).await?;
    file.set_len(total).await?;
    drop(file);

    let mut tasks = Vec::new();

    // The bytes the probe already delivered: no second request for them.
    {
        let tmp = tmp.to_path_buf();
        let bytes = handle.bytes.clone();
        let first_byte = handle.first_byte_ms.clone();
        tasks.push(tokio::spawn(async move {
            write_stream(probe, &tmp, 0, bytes, first_byte, started).await
        }));
    }

    let rest = total.saturating_sub(head);
    let chunk_size = rest.div_ceil(chunks.max(1));
    for i in 0..chunks {
        let start = head + i * chunk_size;
        if start >= total {
            break;
        }
        let end = ((i + 1) * chunk_size + head - 1).min(total - 1);
        let client = client.clone();
        let url = url.to_string();
        let tmp = tmp.to_path_buf();
        let bytes = handle.bytes.clone();
        let first_byte = handle.first_byte_ms.clone();
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
            write_stream(resp, &tmp, start, bytes, first_byte, started).await
        }));
    }
    for task in tasks {
        task.await??;
    }
    Ok(head)
}

struct CountingSink {
    inner: tokio::fs::File,
    bytes: Arc<AtomicU64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// The parallel path hands each chunk its own handle and offset, so the
    /// pieces may land in any order; this checks the assembly itself.
    #[tokio::test]
    async fn chunks_written_out_of_order_rebuild_the_file() {
        let dir = std::env::temp_dir().join(format!("ft_chunks_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("assembled.bin");
        let data = sample(64 * 1024);
        std::fs::write(&path, vec![0u8; data.len()]).expect("seed file");

        let ranges = [
            (0usize, 20_000usize),
            (20_000, 40_000),
            (40_000, 50_000),
            (50_000, data.len()),
        ];
        let mut tasks = Vec::new();
        for (i, (start, end)) in ranges.iter().enumerate() {
            let path = path.clone();
            let slice = data[*start..*end].to_vec();
            let start = *start as u64;
            // Reverse order: the last chunk finishes first.
            let delay = (ranges.len() - i) as u64 * 30;
            tasks.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .await?;
                let mut writer = seek_for_write(start, file).await?;
                tokio::io::AsyncWriteExt::write_all(&mut writer, &slice).await?;
                tokio::io::AsyncWriteExt::flush(&mut writer).await?;
                Ok::<(), anyhow::Error>(())
            }));
        }
        for task in tasks {
            task.await.expect("join").expect("write");
        }

        assert_eq!(std::fs::read(&path).expect("read back"), data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn chunk_count_follows_the_size() {
        assert_eq!(chunk_count(1024), 1, "below a megabyte stays sequential");
        assert_eq!(chunk_count(1536 * 1024), 2, "1.5 MiB is two pieces");
        assert_eq!(chunk_count(2 * 1024 * 1024), 2);
        assert_eq!(chunk_count(3 * 1024 * 1024), 3);
        assert_eq!(chunk_count(12 * 1024 * 1024), MAX_CHUNKS);
        assert_eq!(chunk_count(u64::MAX), MAX_CHUNKS);
    }

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

    #[test]
    fn content_range_gives_the_total_size() {
        assert_eq!(
            total_from_content_range("bytes 0-1048575/19300593"),
            Some(19300593)
        );
        // A suffix range still reports the full size.
        assert_eq!(total_from_content_range("bytes 524288-1048575/4096"), Some(4096));
        // YouTube answers the last probe with "0-.../1522".
        assert_eq!(total_from_content_range("bytes 0-1521/1522"), Some(1522));
    }

    #[test]
    fn an_unknown_or_broken_content_range_is_not_a_size() {
        assert_eq!(total_from_content_range("bytes 0-1023/*"), None);
        assert_eq!(total_from_content_range(""), None);
        assert_eq!(total_from_content_range("garbage"), None);
        assert_eq!(total_from_content_range("bytes 0-1023/abc"), None);
    }


    /// Serves `payload` and honours Range, recording where each request starts.
    async fn serve(payload: Vec<u8>) -> (String, std::sync::Arc<parking_lot::Mutex<Vec<(u64, u64)>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ranges = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen = ranges.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let payload = payload.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 2048];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let mut start = 0u64;
                    let mut end = payload.len() as u64 - 1;
                    for line in request.lines() {
                        let lower = line.to_ascii_lowercase();
                        if let Some(rest) = lower.strip_prefix("range: bytes=") {
                            let mut parts = rest.split('-');
                            start = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
                            end = parts
                                .next()
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(payload.len() as u64 - 1);
                        }
                    }
                    seen.lock().push((start, end));
                    let body = &payload[start as usize..=(end as usize).min(payload.len() - 1)];
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        start,
                        (end as usize).min(payload.len() - 1),
                        payload.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}/file.bin"), ranges)
    }

    #[tokio::test]
    async fn a_file_just_over_the_probe_size_arrives_whole() {
        // 1 MiB probe + a bit: the rest fits in a single chunk, and that chunk
        // still has to be fetched (it used to be skipped, leaving 1 MiB behind).
        let _guard = crate::net::test_proxy_lock();
        crate::net::set_override(Some(None));
        let payload = sample(PROBE_BYTES as usize + 200 * 1024);
        let (url, ranges) = serve(payload.clone()).await;

        let dir = std::env::temp_dir().join(format!("ft-short-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.m4a");
        let handle = ProgressHandle::default();
        let result = download(&url, &dest, &handle).await;
        crate::net::set_override(None);

        result.expect("download failed");
        let written = std::fs::read(&dest).unwrap();
        assert_eq!(
            written.len(),
            payload.len(),
            "the file is not the size the server announced"
        );
        assert!(written == payload, "the bytes are not the served ones");
        let seen = ranges.lock().clone();
        assert!(
            seen.iter().any(|(start, _)| *start == PROBE_BYTES),
            "the tail was never requested: {seen:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_cache_files_are_swept_away() {
        let dir = std::env::temp_dir().join(format!("ft-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let truncated = dir.join("truncated.high.m4a");
        std::fs::write(&truncated, vec![0u8; PROBE_BYTES as usize]).unwrap();
        let part = dir.join("half.part");
        std::fs::write(&part, b"partial").unwrap();
        let whole = dir.join("whole.m4a");
        std::fs::write(&whole, vec![0u8; PROBE_BYTES as usize + 1]).unwrap();
        let meta = dir.join("whole.high.json");
        std::fs::write(&meta, b"title").unwrap();

        let dropped = sweep_broken_cache(&dir);

        assert_eq!(dropped, 2, "the truncated file and the .part go");
        assert!(!truncated.exists());
        assert!(!part.exists());
        assert!(whole.exists(), "a complete file stays");
        assert!(meta.exists(), "metadata is not a cache entry");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_probe_response_is_used_as_the_first_chunk() {
        // No system proxy: the test server is local, and the proxy choice is
        // process-wide, so this serialises with the net tests.
        let _guard = crate::net::test_proxy_lock();
        crate::net::set_override(Some(None));
        let payload = sample(3 * 1024 * 1024 + 1234);
        let (url, ranges) = serve(payload.clone()).await;

        let dir = std::env::temp_dir().join(format!("ft-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.bin");
        let handle = ProgressHandle::default();
        let result = download(&url, &dest, &handle).await;
        crate::net::set_override(None);

        result.expect("download failed");
        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written.len(), payload.len(), "size mismatch");
        assert!(written == payload, "the bytes are not the served ones");

        // The probe asked for the first megabyte and its body was written, so no
        // later request starts at zero again.
        let seen = ranges.lock().clone();
        assert_eq!(
            seen.iter().filter(|(start, _)| *start == 0).count(),
            1,
            "the first chunk was downloaded twice: {seen:?}"
        );
        assert!(seen.len() >= 2, "nothing was fetched in parallel: {seen:?}");
        let total: u64 = seen.iter().map(|(a, b)| b - a + 1).sum();
        assert_eq!(total, payload.len() as u64, "ranges overlap: {seen:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
