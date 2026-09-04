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
    pub title: Arc<std::sync::Mutex<String>>,
}

impl ProgressHandle {
    pub fn snapshot(&self) -> DownloadProgress {
        DownloadProgress {
            bytes: self.bytes.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            done: self.done.load(Ordering::Relaxed),
            title: self.title.lock().unwrap().clone(),
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

pub fn cached_file(video_id: &str) -> PathBuf {
    cache_dir().join(format!("{video_id}.m4a"))
}

pub fn cached_meta(video_id: &str) -> PathBuf {
    cache_dir().join(format!("{video_id}.json"))
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
