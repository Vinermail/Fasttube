#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth_window;
mod browser;
mod downloader;
mod engine;
mod fsutil;
mod grpc;
mod home;
mod net;
mod playlist;
mod queue;
mod secrets;
mod sidecar;

use downloader::ProgressHandle;
use engine::{Cmd, EngineHandle, Snapshot};
use playlist::{Playlist, Track};
use queue::{Action, Queue, QueueItem, RepeatMode};
use serde::{Deserialize, Serialize};
use sidecar::{ApiStatus, SharedApiStatus};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use tauri::{Emitter, Manager};

#[derive(Serialize, Clone)]
struct PlaylistInfo {
    name: String,
    tracks: usize,
}

/// One row of a search section, as the UI wants it.
#[derive(Serialize, Clone)]
struct SearchItemDto {
    kind: String,
    id: String,
    title: String,
    subtitle: String,
    thumbnail: String,
    duration_secs: f64,
    #[serde(skip_serializing_if = "String::is_empty")]
    video_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    browse_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    playlist_id: String,
    /// Set for local-library rows: the file to play instead of a video id.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Serialize, Clone)]
struct SearchSectionDto {
    title: String,
    kind: String,
    items: Vec<SearchItemDto>,
}

/// Album/artist page as the browse view wants it.
#[derive(Serialize)]
struct BrowsePage {
    title: String,
    subtitle: String,
    cover: String,
    sections: Vec<SearchSectionDto>,
    error: String,
}

#[derive(Serialize)]
struct SearchPayload {
    sections: Vec<SearchSectionDto>,
    error: String,
}

impl From<grpc::SearchSection> for SearchSectionDto {
    fn from(section: grpc::SearchSection) -> Self {
        Self {
            title: section.title,
            kind: section.kind,
            items: section
                .items
                .into_iter()
                .map(|item| SearchItemDto {
                    kind: item.kind,
                    id: item.id,
                    title: item.title,
                    subtitle: item.subtitle,
                    thumbnail: item.thumbnail,
                    duration_secs: item.duration_secs,
                    video_type: item.video_type,
                    browse_id: item.browse_id,
                    playlist_id: item.playlist_id,
                    path: None,
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct SearchTrack {
    video_id: String,
    title: String,
    artist: String,
    duration_secs: f64,
    thumbnail: String,
}

#[derive(Serialize, Clone)]
struct UserPlaylistInfo {
    id: String,
    name: String,
    track_count: i32,
    thumbnail: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    #[serde(default = "default_quality")]
    audio_quality: String,
    #[serde(default = "default_normalize_level")]
    normalize_level: String,
    #[serde(default = "default_true")]
    volume_curve: bool,
    /// Slider position (0..1), restored on start.
    #[serde(default = "default_volume")]
    volume: f32,
    /// Restored too: an app that was left muted must not start loud.
    #[serde(default)]
    muted: bool,
    /// Off by default: using the account with yt-dlp can get it rate-limited.
    #[serde(default)]
    streaming_account: bool,
    /// Audio cache budget in MiB; 0 disables eviction.
    #[serde(default = "default_cache_limit_mb")]
    cache_limit_mb: u64,
    /// Last search tab: "", songs, videos, albums, artists, playlists.
    #[serde(default = "default_search_filter")]
    search_filter: String,
    /// Last played track and where it stopped: feeds the «Продолжить» shelf.
    #[serde(default)]
    resume: Option<ResumeState>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct ResumeState {
    id: String,
    title: String,
    artist: String,
    thumbnail: String,
    position_secs: f64,
    /// Local files continue by path, network tracks by video id.
    is_local: bool,
    #[serde(default)]
    path: String,
    #[serde(default)]
    saved_at: f64,
}

fn default_search_filter() -> String {
    "songs".to_string()
}

fn default_cache_limit_mb() -> u64 {
    2048
}

fn default_quality() -> String {
    "standard".to_string()
}

fn default_normalize_level() -> String {
    "medium".to_string()
}

fn default_true() -> bool {
    true
}

fn default_volume() -> f32 {
    0.8
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio_quality: default_quality(),
            normalize_level: default_normalize_level(),
            volume_curve: true,
            volume: default_volume(),
            muted: false,
            streaming_account: false,
            cache_limit_mb: default_cache_limit_mb(),
            search_filter: default_search_filter(),
            resume: None,
        }
    }
}

/// Target RMS for each normalization level ("off" disables normalization).
fn target_rms(level: &str) -> Option<f64> {
    match level {
        "light" => Some(0.08),
        "medium" => Some(0.12),
        "strong" => Some(0.18),
        _ => None,
    }
}

fn normalize_enabled(settings: &Settings) -> bool {
    target_rms(settings.normalize_level.as_str()).is_some()
}

fn load_settings() -> Settings {
    std::fs::read_to_string(downloader::settings_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Writes settings at most every 400 ms: the volume slider would otherwise
/// rewrite settings.json sixty times a second while it is dragged.
fn spawn_settings_flusher(app: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("settings-flusher".to_string())
        .spawn(move || {
            use std::sync::atomic::Ordering;
            let state = app.state::<AppState>();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(400));
                if !state.settings_dirty.swap(false, Ordering::SeqCst) {
                    continue;
                }
                let snapshot = state.settings.lock().clone();
                save_settings(&snapshot);
            }
        })
        .expect("failed to spawn the settings flusher");
}

fn save_settings(settings: &Settings) {
    if let Err(e) = fsutil::write_json_atomic(&downloader::settings_file(), settings, true) {
        eprintln!("[store] cannot save settings: {e}");
    }
}

fn current_quality(state: &AppState) -> String {
    state.settings.lock().audio_quality.clone()
}

/// Whether stream resolution may use the signed-in account (opt-in).
fn streaming_account(state: &AppState) -> bool {
    state.settings.lock().streaming_account
}

fn history_file() -> std::path::PathBuf {
    downloader::settings_file()
        .parent()
        .unwrap()
        .join("history.json")
}

fn favorites_file() -> std::path::PathBuf {
    downloader::settings_file()
        .parent()
        .unwrap()
        .join("favorites.json")
}

fn loudness_file() -> std::path::PathBuf {
    downloader::settings_file()
        .parent()
        .unwrap()
        .join("loudness.json")
}

type LoudnessCache = Arc<Mutex<std::collections::HashMap<String, f32>>>;

fn load_loudness() -> LoudnessCache {
    let mut map: std::collections::HashMap<String, f32> = load_json(&loudness_file());
    // Cache files gained a quality segment; keep the gains already computed.
    if migrate_loudness_keys(&mut map) {
        save_json(&loudness_file(), &map);
    }
    Arc::new(Mutex::new(map))
}

/// ReplayGain entries are keyed by cache file path. Cache names changed from
/// `<id>.m4a` to `<id>.<quality>.m4a`, so rewrite the affected keys to
/// `standard` (the only quality the old files could have been downloaded at)
/// instead of silently re-scanning every cached track.
fn migrate_loudness_keys(map: &mut std::collections::HashMap<String, f32>) -> bool {
    let dir = downloader::cache_dir();
    let mut renames = Vec::new();
    for key in map.keys() {
        let path = std::path::Path::new(key);
        if path.parent() != Some(dir.as_path()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((stem, ext)) = name.rsplit_once('.') else {
            continue;
        };
        if ext != "m4a" || stem.ends_with(".standard") || stem.ends_with(".high") {
            continue;
        }
        let new = dir
            .join(format!("{stem}.{}.m4a", downloader::QUALITY_STANDARD))
            .to_string_lossy()
            .into_owned();
        renames.push((key.clone(), new));
    }
    for (old, new) in &renames {
        if let Some(value) = map.remove(old) {
            map.insert(new.clone(), value);
        }
    }
    !renames.is_empty()
}

fn save_loudness(cache: &LoudnessCache) {
    let snapshot = cache.lock().clone();
    save_json(&loudness_file(), &snapshot);
}

/// Fast loudness estimate: RMS over the first ~2 seconds of actual audio
/// (silence skipped). Applied immediately on first play.
///
/// Deliberately conservative: without knowing the full-track peak, boosting
/// could clip the loud parts later in the track, so the quick gain only
/// attenuates (<= 1.0). The full ReplayGain scan then replaces it with the
/// peak-safe value.
fn quick_gain(path: &std::path::Path, target: f64) -> Option<f32> {
    const ACTIVE_TARGET: u64 = 200_000; // ~2.3s of audio content
    const MAX_DECODE: usize = 1_000_000; // safety cap
    const SILENCE: f32 = 0.005;
    let file = std::fs::File::open(path).ok()?;
    let decoder = rodio::Decoder::try_from(file).ok()?;
    let mut sum = 0f64;
    let mut n = 0u64;
    for sample in decoder.take(MAX_DECODE) {
        let a = sample.abs();
        if a < SILENCE {
            continue;
        }
        let v = sample as f64;
        sum += v * v;
        n += 1;
        if n >= ACTIVE_TARGET {
            break;
        }
    }
    if n == 0 {
        return None;
    }
    let rms = (sum / n as f64).sqrt();
    if rms < 1e-6 {
        return None;
    }
    Some((target / rms).clamp(0.1, 1.0) as f32)
}

/// Full-track loudness analysis using the proper ReplayGain 2.0 (EBU R128)
/// algorithm. The peak is used to cap the gain so playback never clips.
fn full_replaygain(path: &std::path::Path) -> Option<f32> {
    use rodio::Source;

    let file = std::fs::File::open(path).ok()?;
    let decoder = rodio::Decoder::try_from(file).ok()?;
    let channels = decoder.channels().get();
    let rate = decoder.sample_rate().get();

    if channels == 2
        && let Some(mut rg) = replaygain::ReplayGain::new(rate as usize)
    {
        let mut buf: Vec<f32> = Vec::with_capacity(16384);
        for sample in decoder {
            buf.push(sample);
            if buf.len() >= 16384 {
                rg.process_samples(&buf);
                buf.clear();
            }
        }
        if !buf.is_empty() {
            rg.process_samples(&buf);
        }
        let (gain_db, peak) = rg.finish();
        let linear = 10f32.powf(gain_db / 20.0);
        let limited = if peak > 0.0 {
            linear.min(0.95 / peak)
        } else {
            linear.min(1.0)
        };
        return Some(limited.max(0.05));
    }

    // Fallback: full-file RMS for mono / unsupported sample rates.
    let decoder = std::fs::File::open(path)
        .ok()
        .and_then(|f| rodio::Decoder::try_from(f).ok())?;
    let mut sum = 0f64;
    let mut peak = 0f32;
    let mut n = 0u64;
    for sample in decoder {
        let v = sample as f64;
        sum += v * v;
        peak = peak.max(sample.abs());
        n += 1;
    }
    if n == 0 {
        return None;
    }
    let rms = (sum / n as f64).sqrt();
    if rms < 1e-6 {
        return None;
    }
    let rms_gain = 0.12 / rms;
    let limited = if peak > 0.0 {
        rms_gain.min(0.95 / peak as f64)
    } else {
        rms_gain.min(1.0)
    };
    Some(limited.clamp(0.1, 4.0) as f32)
}

/// Fills `Track.gain` from the loudness cache (when normalization is on).
fn apply_gains(playlist: &mut Playlist, cache: &LoudnessCache, normalize: bool) {
    let map = cache.lock();
    for track in playlist.tracks.iter_mut() {
        if !normalize {
            track.gain = None;
            continue;
        }
        // Keep ReplayGain tags; prefer the scanned cache when present.
        if let Some(gain) = map.get(&track.path.to_string_lossy().into_owned()) {
            track.gain = Some(*gain);
        }
    }
}

/// Scans uncached tracks in the background (two parallel workers) and pushes
/// fresh gains into the engine, which ramps them in smoothly.
/// How many tracks from the current one get their gain computed. Scanning a
/// whole 350-track playlist meant decoding hundreds of megabytes on open, for
/// tracks the listener may never reach.
const LOUDNESS_WINDOW: usize = 6;

/// The tracks worth scanning now: the one about to play and the next few.
fn loudness_window(
    paths: Vec<std::path::PathBuf>,
    start: usize,
) -> Vec<std::path::PathBuf> {
    paths
        .into_iter()
        .skip(start)
        .take(LOUDNESS_WINDOW)
        .collect()
}

/// ReplayGain is a background chore: keep it below the audio callback and the
/// UI instead of competing with them for a core.
fn lower_thread_priority() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // SAFETY: the pseudo handle refers to this thread and needs no cleanup.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

fn spawn_loudness_scan(
    paths: Vec<std::path::PathBuf>,
    start: usize,
    cache: LoudnessCache,
    engine: EngineHandle,
    target: f64,
) {
    use std::collections::VecDeque;

    let paths = loudness_window(paths, start);
    if paths.is_empty() {
        return;
    }

    static SCANNING: std::sync::OnceLock<Arc<Mutex<std::collections::HashSet<String>>>> =
        std::sync::OnceLock::new();
    let in_progress = SCANNING
        .get_or_init(|| Arc::new(Mutex::new(std::collections::HashSet::new())))
        .clone();

    let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
    for _ in 0..2 {
        let queue = queue.clone();
        let cache = cache.clone();
        let engine = engine.clone();
        let in_progress = in_progress.clone();
        std::thread::Builder::new()
            .name("loudness-scan".to_string())
            .spawn(move || {
                lower_thread_priority();
                loop {
                    let path = queue.lock().pop_front();
                    let Some(path) = path else { break };
                    let key = path.to_string_lossy().into_owned();
                    {
                        let mut set = in_progress.lock();
                        if cache.lock().contains_key(&key) || !set.insert(key.clone()) {
                            continue;
                        }
                    }
                    if let Some(gain) = quick_gain(&path, target) {
                        engine.send(Cmd::SetGain(key.clone(), gain));
                    }
                    if let Some(gain) = full_replaygain(&path) {
                        cache.lock().insert(key.clone(), gain);
                        save_loudness(&cache);
                        engine.send(Cmd::SetGain(key.clone(), gain));
                    }
                    in_progress.lock().remove(&key);
                }
            })
            .expect("failed to spawn loudness scan");
    }
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &std::path::Path) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(path: &std::path::Path, value: &T) {
    if let Err(e) = fsutil::write_json_atomic(path, value, false) {
        eprintln!("[store] cannot save {}: {e}", path.display());
    }
}

/// Deletes the least recently used cached tracks until the cache fits the
/// configured budget. The playing file and anything being downloaded now are
/// never candidates.
fn run_cache_eviction(state: &AppState) {
    let limit_mb = state.settings.lock().cache_limit_mb;
    let quality = current_quality(state);
    let mut keep: Vec<std::path::PathBuf> = Vec::new();
    if let Some(track) = state.engine.lock().snapshot().and_then(|s| s.track)
        && let Some(path) = track.path
    {
        keep.push(std::path::PathBuf::from(path));
    }
    let fetching: Vec<String> = state.fetching.lock().iter().cloned().collect();
    for id in fetching {
        keep.push(downloader::cached_file(&id, &quality));
    }
    let (removed, freed) = downloader::evict_cache(limit_mb.saturating_mul(1024 * 1024), &keep);
    if removed > 0 {
        eprintln!(
            "[cache] evicted {removed} track(s), freed {:.0} MiB; cache is now {:.0} MiB",
            freed as f64 / (1024.0 * 1024.0),
            downloader::cache_size() as f64 / (1024.0 * 1024.0)
        );
    }
}

/// Sweeps once at startup and then every five minutes.
fn spawn_cache_sweeper(app: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("cache-sweeper".to_string())
        .spawn(move || {
            let mut first = true;
            loop {
                if !first {
                    std::thread::sleep(std::time::Duration::from_secs(300));
                }
                first = false;
                run_cache_eviction(&app.state::<AppState>());
            }
        })
        .expect("failed to spawn cache sweeper");
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Reads history.json, giving old entries (no `played_at`) descending
/// timestamps before anything reads them. The file is already newest-first, so
/// the visible order does not change; the timestamps only make "recent" and
/// "continue" meaningful.
fn load_history() -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = load_json(&history_file());
    let mut next = now_secs();
    let mut migrated = false;
    for item in items.iter_mut() {
        if item.played_at <= 0.0 {
            item.played_at = next;
            next -= 60.0;
            migrated = true;
        }
    }
    if migrated {
        save_json(&history_file(), &items);
    }
    items
}

fn record_history(state: &AppState, item: &HistoryItem) {
    let mut history = state.history.lock();
    history.retain(|h| h.id != item.id);
    history.insert(0, item.clone());
    history.truncate(100);
    let snapshot = history.clone();
    drop(history);
    save_json(&history_file(), &snapshot);
}

type FetchGuard = Arc<Mutex<std::collections::HashSet<String>>>;

#[derive(Serialize, Deserialize, Clone)]
struct HistoryItem {
    id: String,
    title: String,
    artist: String,
    thumbnail: String,
    /// Unix seconds. Files written before this field existed get synthetic
    /// stamps on load, so the order they already had survives.
    #[serde(default)]
    played_at: f64,
}

#[derive(Serialize, Deserialize, Clone)]
struct FavoriteTrack {
    path: String,
    title: String,
    artist: String,
    duration: f64,
    cover: Option<String>,
}

struct AppState {
    engine: Mutex<EngineHandle>,
    library: Mutex<Vec<Playlist>>,
    local_current: Mutex<Option<Vec<engine::TrackInfo>>>,
    settings: Mutex<Settings>,
    api_status: SharedApiStatus,
    download: ProgressHandle,
    downloading: Mutex<bool>,
    queue: Mutex<Queue>,
    repeat: Mutex<RepeatMode>,
    fetching: FetchGuard,
    history: Mutex<Vec<HistoryItem>>,
    favorites: Mutex<Vec<FavoriteTrack>>,
    sleep_until: Mutex<Option<std::time::Instant>>,
    loudness: LoudnessCache,
    /// When the session was last validated (for the "check before loading"
    /// rule). `None` = never checked in this process.
    session_checked: Mutex<Option<std::time::Instant>>,
    /// Set by anything that changes settings, cleared when the flusher saves.
    settings_dirty: std::sync::atomic::AtomicBool,
}

fn track_to_info(t: &Track) -> engine::TrackInfo {
    engine::TrackInfo {
        title: t.title.clone(),
        artist: t.artist.clone(),
        duration: t.duration.unwrap_or(0.0),
        cover: t.cover.as_ref().map(|p| p.to_string_lossy().into_owned()),
        path: Some(t.path.to_string_lossy().into_owned()),
    }
}

fn to_playlist_info(pl: &Playlist) -> PlaylistInfo {
    PlaylistInfo {
        name: pl.name.clone(),
        tracks: pl.tracks.len(),
    }
}

#[tauri::command]
fn get_state(state: tauri::State<AppState>) -> Option<Snapshot> {
    state.engine.lock().snapshot()
}

#[tauri::command]
fn get_track_list(state: tauri::State<AppState>) -> Vec<engine::TrackInfo> {
    state
        .local_current
        .lock()
        .clone()
        .unwrap_or_default()
}

#[tauri::command]
fn get_library(state: tauri::State<AppState>) -> Vec<PlaylistInfo> {
    state
        .library
        .lock()
        .iter()
        .map(to_playlist_info)
        .collect()
}

#[tauri::command]
async fn open_folder(
    path: String,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<PlaylistInfo>, String> {
    let root = std::path::PathBuf::from(path);
    let cover_dir = downloader::covers_dir();
    let library =
        tauri::async_runtime::spawn_blocking(move || playlist::collect(&root, &cover_dir))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
    if library.is_empty() {
        return Err("no audio files found in this folder".to_string());
    }
    let infos: Vec<PlaylistInfo> = library.iter().map(to_playlist_info).collect();
    let first = library.first().cloned();
    {
        let mut lib = state.library.lock();
        *lib = library;
    }
    if let Some(pl) = first {
        let track_infos: Vec<engine::TrackInfo> = pl.tracks.iter().map(track_to_info).collect();
        *state.local_current.lock() = Some(track_infos);
        let mut pl = pl;
        let normalize = normalize_enabled(&state.settings.lock());
        apply_gains(&mut pl, &state.loudness, normalize);
        let scan_paths: Vec<std::path::PathBuf> =
            pl.tracks.iter().map(|t| t.path.clone()).collect();
        state.engine.lock().send(Cmd::SetPlaylist(pl));
        spawn_loudness_scan(
            scan_paths,
            0,
            state.loudness.clone(),
            state.engine.lock().clone(),
            target_rms(&state.settings.lock().normalize_level).unwrap_or(0.12),
        );
    }
    Ok(infos)
}

/// Opens a playlist without starting playback: the track list is shown on
/// the right, playback begins only when a track is clicked.
#[tauri::command]
fn open_playlist(
    index: usize,
    state: tauri::State<AppState>,
) -> Result<Vec<engine::TrackInfo>, String> {
    let pl = state
        .library
        .lock()
        .get(index)
        .cloned()
        .ok_or("playlist not found")?;
    Ok(load_local_playlist(pl, 0, &state))
}

/// Hands a local playlist to the engine without starting playback, and
/// publishes its track list to the UI. The queue stops driving playback.
fn load_local_playlist(pl: Playlist, start: usize, state: &AppState) -> Vec<engine::TrackInfo> {
    let infos: Vec<engine::TrackInfo> = pl.tracks.iter().map(track_to_info).collect();
    *state.local_current.lock() = Some(infos.clone());
    state.queue.lock().deactivate();
    let mut pl = pl;
    let normalize = normalize_enabled(&state.settings.lock());
    apply_gains(&mut pl, &state.loudness, normalize);
    let scan_paths: Vec<std::path::PathBuf> = pl.tracks.iter().map(|t| t.path.clone()).collect();
    state.engine.lock().send(Cmd::OpenPlaylist(pl));
    spawn_loudness_scan(
        scan_paths,
        start,
        state.loudness.clone(),
        state.engine.lock().clone(),
        target_rms(&state.settings.lock().normalize_level).unwrap_or(0.12),
    );
    infos
}

/// Plays one file of the local library, from the folder it belongs to (so
/// "next" keeps walking that folder).
#[tauri::command]
fn play_local_path(path: String, state: tauri::State<AppState>) -> Result<(), String> {
    let (playlist, index) = {
        let library = state.library.lock();
        let mut found = None;
        for pl in library.iter() {
            if let Some(idx) = pl.tracks.iter().position(|t| t.path.to_string_lossy() == path) {
                found = Some((pl.clone(), idx));
                break;
            }
        }
        found.ok_or("track is not in the library")?
    };
    load_local_playlist(playlist, index, &state);
    state.engine.lock().send(Cmd::PlayTrack(index));
    Ok(())
}

/// Plays one file that may not belong to the loaded library (a cached track
/// from «Избранное», for instance): play_local_path only knows library files.
#[tauri::command]
fn play_local_file(path: String, state: tauri::State<AppState>) -> Result<(), String> {
    let file = std::path::PathBuf::from(&path);
    if !file.is_file() {
        return Err("file not found".to_string());
    }
    let track = playlist::read_track(&file, &downloader::covers_dir());
    let playlist = Playlist {
        name: "File".to_string(),
        tracks: vec![track],
    };
    load_local_playlist(playlist, 0, &state);
    state.engine.lock().send(Cmd::PlayTrack(0));
    Ok(())
}

#[tauri::command]
fn play_track(index: usize, state: tauri::State<AppState>) {
    // A local track replaces any active YouTube queue: otherwise the queue
    // watcher would hijack playback when the track ends.
    {
        let mut q = state.queue.lock();
        if q.active() {
            q.deactivate();
            q.bump_version();
        }
    }
    // The clicked track and the five after it, so "next" stays gapless in
    // level as well.
    if normalize_enabled(&state.settings.lock()) {
        let paths: Vec<std::path::PathBuf> = state
            .local_current
            .lock()
            .as_ref()
            .map(|tracks| {
                tracks
                    .iter()
                    .filter_map(|t| t.path.as_ref().map(std::path::PathBuf::from))
                    .collect()
            })
            .unwrap_or_default();
        if !paths.is_empty() {
            spawn_loudness_scan(
                paths,
                index,
                state.loudness.clone(),
                state.engine.lock().clone(),
                target_rms(&state.settings.lock().normalize_level).unwrap_or(0.12),
            );
        }
    }
    state.engine.lock().send(Cmd::PlayTrack(index));
}

#[tauri::command]
fn toggle_pause(state: tauri::State<AppState>) {
    state.engine.lock().send(Cmd::TogglePause);
}

#[tauri::command]
async fn next_track(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let action = state.queue.lock().next();
    match action {
        // The queue is not driving playback: let the engine's own
        // playlist handle it.
        None => {
            state.engine.lock().send(Cmd::Next);
            Ok(())
        }
        Some(Action::Stop) => {
            state.engine.lock().send(Cmd::Stop);
            Ok(())
        }
        Some(_) => play_current(&state).await,
    }
}

#[tauri::command]
async fn prev_track(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let elapsed = state
        .engine
        .lock()
        .snapshot()
        .map(|s| s.elapsed)
        .unwrap_or(0.0);
    let action = state.queue.lock().prev(elapsed);
    match action {
        None => {
            state.engine.lock().send(Cmd::Prev);
            Ok(())
        }
        Some(_) => play_current(&state).await,
    }
}

#[tauri::command]
fn stop(state: tauri::State<AppState>) {
    state.queue.lock().mark_manual_stop();
    state.engine.lock().send(Cmd::Stop);
}

#[tauri::command]
fn seek(secs: f64, state: tauri::State<AppState>) {
    state.engine.lock().send(Cmd::Seek(secs));
}

#[tauri::command]
fn set_volume(volume: f32, state: tauri::State<AppState>) {
    let volume = volume.clamp(0.0, 1.0);
    state.engine.lock().send(Cmd::SetVolume(volume));
    {
        let mut settings = state.settings.lock();
        settings.volume = volume;
        // Dragging the slider while muted means "I want to hear it again".
        if settings.muted {
            settings.muted = false;
            state.engine.lock().send(Cmd::SetMuted(false));
        }
    }
    // Dragging fires dozens of events: the flusher writes them, not this call.
    state
        .settings_dirty
        .store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Mutes or unmutes the whole app and returns the new state.
#[tauri::command]
fn toggle_mute(state: tauri::State<AppState>) -> bool {
    let muted = {
        let mut settings = state.settings.lock();
        settings.muted = !settings.muted;
        settings.muted
    };
    state.engine.lock().send(Cmd::SetMuted(muted));
    save_settings(&state.settings.lock().clone());
    state
        .settings_dirty
        .store(false, std::sync::atomic::Ordering::SeqCst);
    muted
}

#[tauri::command]
fn get_api_status(state: tauri::State<AppState>) -> ApiStatus {
    state.api_status.read().clone()
}

#[tauri::command]
fn get_download_progress(state: tauri::State<AppState>) -> downloader::DownloadProgress {
    state.download.snapshot()
}

/// YouTube Music search with the local library prepended.
///
/// The library is scanned in its own scope: no lock is held across the gRPC
/// await, so fast typing in the UI cannot queue up on a mutex.
#[tauri::command]
async fn search_music(
    query: String,
    filter: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<SearchPayload, String> {
    let query = query.trim().to_string();
    let filter = filter.unwrap_or_default();
    if query.is_empty() {
        return Ok(SearchPayload {
            sections: Vec::new(),
            error: String::new(),
        });
    }

    let mut sections: Vec<SearchSectionDto> = Vec::new();
    if filter.is_empty() || filter == "songs" {
        let local: Vec<SearchItemDto> = {
            let library = state.library.lock();
            playlist::search_library(&library, &query, 10)
                .into_iter()
                .map(|track| {
                    let path = track.path.to_string_lossy().into_owned();
                    SearchItemDto {
                        kind: "song".to_string(),
                        id: path.clone(),
                        title: track.title.clone(),
                        subtitle: track.artist.clone(),
                        thumbnail: track
                            .cover
                            .as_ref()
                            .map(|c| c.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        duration_secs: track.duration.unwrap_or(0.0),
                        video_type: String::new(),
                        browse_id: String::new(),
                        playlist_id: String::new(),
                        path: Some(path),
                    }
                })
                .collect()
        };
        if !local.is_empty() {
            sections.push(SearchSectionDto {
                title: "Моя библиотека".to_string(),
                kind: "song".to_string(),
                items: local,
            });
        }
    }

    match grpc::search(&query, &filter, 20).await {
        Ok(remote) => {
            sections.extend(remote.into_iter().map(SearchSectionDto::from));
            Ok(SearchPayload {
                sections,
                error: String::new(),
            })
        }
        // Keep the library hits: the user can still play those.
        Err(e) => Ok(SearchPayload {
            sections,
            error: e.to_string(),
        }),
    }
}

#[tauri::command]
async fn open_album(browse_id: String) -> Result<BrowsePage, String> {
    let resp = grpc::get_album(&browse_id).await.map_err(|e| e.to_string())?;
    Ok(BrowsePage {
        title: resp.title,
        subtitle: resp.subtitle,
        cover: resp.thumbnail,
        sections: resp
            .tracks
            .into_iter()
            .filter(|section| !section.items.is_empty())
            .map(SearchSectionDto::from)
            .collect(),
        error: resp.error,
    })
}

#[tauri::command]
async fn open_artist(browse_id: String) -> Result<BrowsePage, String> {
    let resp = grpc::get_artist(&browse_id).await.map_err(|e| e.to_string())?;
    Ok(BrowsePage {
        title: resp.name,
        subtitle: resp.subtitle,
        cover: resp.thumbnail,
        sections: resp
            .sections
            .into_iter()
            .filter(|section| !section.items.is_empty())
            .map(SearchSectionDto::from)
            .collect(),
        error: resp.error,
    })
}

#[tauri::command]
async fn search_suggestions(query: String) -> Result<Vec<String>, String> {
    if query.trim().len() < 2 {
        return Ok(Vec::new());
    }
    grpc::suggest(&query).await.map_err(|e| e.to_string())
}

/// One page of a long list plus what the UI needs to keep paging.
#[derive(Serialize, Clone)]
struct TrackPage {
    tracks: Vec<SearchTrack>,
    /// 0 = unknown, keep asking until a short page comes back.
    total: i32,
    /// "disk" when this answer came from the cache, "net" otherwise.
    source: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct CachedTracks {
    #[serde(default)]
    saved_at: f64,
    #[serde(default)]
    total: i32,
    #[serde(default)]
    tracks: Vec<SearchTrack>,
}

/// Playlists and likes are re-read at most once every four hours from disk; the
/// sidecar revalidates them by track count anyway.
const TRACK_CACHE_TTL: f64 = 4.0 * 3600.0;

fn track_cache_file(key: &str) -> std::path::PathBuf {
    // A colon is legal in the key but not in a Windows file name ("playlist:UC…"
    // turns into an alternate-data-stream path and the write fails with 87).
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    downloader::data_dir()
        .join("playlists")
        .join(format!("{safe}.json"))
}

fn load_track_cache(key: &str) -> Option<CachedTracks> {
    let path = track_cache_file(key);
    let raw = std::fs::read_to_string(&path).ok()?;
    let cached: CachedTracks = serde_json::from_str(&raw).ok()?;
    if cached.tracks.is_empty() || now_secs() - cached.saved_at > TRACK_CACHE_TTL {
        return None;
    }
    Some(cached)
}

/// Merges one page into the cache file: pages arrive in order, so the file
/// grows to `offset + tracks.len()`.
fn save_track_page(key: &str, offset: i32, tracks: &[SearchTrack], total: i32) {
    let path = track_cache_file(key);
    let mut cached = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<CachedTracks>(&raw).ok())
        .unwrap_or_default();
    let offset = offset.max(0) as usize;
    if cached.tracks.len() < offset {
        return; // a gap: keep what we have instead of writing holes
    }
    cached.tracks.truncate(offset);
    cached.tracks.extend(tracks.iter().cloned());
    if total > 0 {
        cached.total = total;
    }
    cached.saved_at = now_secs();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = fsutil::write_json_atomic(&path, &cached, false) {
        eprintln!("[store] cannot save the playlist cache: {e}");
    }
}

/// The cached lists belong to the account that fetched them.
fn drop_track_cache() {
    let dir = downloader::data_dir().join("playlists");
    if let Err(e) = std::fs::remove_dir_all(&dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("[store] cannot drop the playlist cache: {e}");
    }
}

fn to_search_track(t: grpc::Track) -> SearchTrack {
    SearchTrack {
        video_id: t.video_id,
        title: t.title,
        artist: t.artist,
        duration_secs: t.duration_secs,
        thumbnail: t.thumbnail,
    }
}

/// Compares the cached track count with the one the account has now, and drops
/// the file when they differ. Runs in the background: the first page is already
/// on screen by then.
fn revalidate_track_cache(app: tauri::AppHandle, key: String, playlist_id: Option<String>) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let _ = ensure_session_fresh(&app, &state).await;
        let fresh = match playlist_id {
            Some(id) => grpc::get_playlist_tracks(&id, 0, 1).await,
            None => grpc::get_liked_songs(0, 1).await,
        };
        let Ok(resp) = fresh else {
            return; // offline: keep serving the cache
        };
        if let Some(cached) = load_track_cache(&key)
            && resp.total > 0
            && resp.total != cached.total
        {
            eprintln!(
                "[cache] {key} changed: {} tracks cached, {} now",
                cached.total, resp.total
            );
            let _ = std::fs::remove_file(track_cache_file(&key));
        }
    });
}

async fn fetch_track_page(
    app: &tauri::AppHandle,
    state: &AppState,
    key: &str,
    playlist_id: Option<&str>,
    offset: i32,
    limit: i32,
) -> Result<TrackPage, String> {
    if offset == 0
        && let Some(cached) = load_track_cache(key)
    {
        revalidate_track_cache(app.clone(), key.to_string(), playlist_id.map(str::to_string));
        let end = (limit.max(1) as usize).min(cached.tracks.len());
        return Ok(TrackPage {
            tracks: cached.tracks[..end].to_vec(),
            total: cached.total,
            source: "disk".to_string(),
        });
    }

    ensure_session_fresh(app, state).await;
    let resp = match playlist_id {
        Some(id) => grpc::get_playlist_tracks(id, offset, limit).await,
        None => grpc::get_liked_songs(offset, limit).await,
    }
    .map_err(|e| e.to_string())?;
    if !resp.error.is_empty() {
        return Err(resp.error);
    }
    let tracks: Vec<SearchTrack> = resp.tracks.into_iter().map(to_search_track).collect();
    if !tracks.is_empty() {
        save_track_page(key, offset, &tracks, resp.total);
    }
    Ok(TrackPage {
        tracks,
        total: resp.total,
        source: "net".to_string(),
    })
}

#[tauri::command]
async fn get_liked_songs(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    offset: Option<i32>,
    limit: Option<i32>,
) -> Result<TrackPage, String> {
    let offset = offset.unwrap_or(0).max(0);
    let limit = limit.unwrap_or(100).clamp(1, 500);
    fetch_track_page(&app, &state, "liked", None, offset, limit).await
}

#[derive(Serialize, Clone)]
pub(crate) struct SessionInfo {
    pub state: String, // signed_out | active | expired
    pub account_name: String,
    pub error: String,
}

impl SessionInfo {
    fn from_status(status: &grpc::SessionStatus) -> Self {
        let state = match status.state() {
            grpc::SessionState::Active => "active",
            grpc::SessionState::Expired => "expired",
            _ => "signed_out",
        };
        Self {
            state: state.to_string(),
            account_name: status.account_name.clone(),
            error: status.error.clone(),
        }
    }
}

fn set_session_status(app: &tauri::AppHandle, info: &SessionInfo) {
    if let Some(state) = app.try_state::<AppState>() {
        let mut status = state.api_status.write();
        status.session_state = info.state.clone();
        status.account_name = info.account_name.clone();
        status.auth_ok = info.state == "active";
        *state.session_checked.lock() = Some(std::time::Instant::now());
    }
}

/// Installs a session in the sidecar, validates it, and only then persists the
/// bundle. A session YouTube already rejects must never overwrite a working
/// one. Shared by the login window and the advanced paste path.
pub(crate) async fn install_session(
    app: &tauri::AppHandle,
    bundle: secrets::AuthBundle,
) -> Result<SessionInfo, String> {
    let status = grpc::set_session(&bundle.cookie, &bundle.user_agent, &bundle.x_goog_authuser)
        .await
        .map_err(|e| e.to_string())?;
    let installed = SessionInfo::from_status(&status);
    if installed.state != "active" {
        return Err(if installed.error.is_empty() {
            "the sidecar rejected the session".to_string()
        } else {
            installed.error
        });
    }

    let validated = grpc::validate_session().await.map_err(|e| e.to_string())?;
    let info = SessionInfo::from_status(&validated);
    if info.state != "active" {
        // Do not keep a dead session around (and do not touch auth.bin).
        let _ = grpc::clear_session().await;
        return Err(if info.error.is_empty() {
            "YouTube rejected the session".to_string()
        } else {
            info.error
        });
    }

    secrets::save(&bundle)?;
    drop_track_cache();
    // A snapshot from another account must not be shown to this one.
    if let Err(e) = std::fs::remove_file(home_file())
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("[home] cannot drop the snapshot on sign-in: {e}");
    }
    set_session_status(app, &info);
    Ok(info)
}

const SESSION_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(120);
const SESSION_BACKGROUND_EVERY: std::time::Duration = std::time::Duration::from_secs(15 * 60);

fn signed_out() -> SessionInfo {
    SessionInfo {
        state: "signed_out".to_string(),
        account_name: String::new(),
        error: String::new(),
    }
}

/// One validation round: updates the shared status, announces an expiry once.
async fn validate_and_publish(app: &tauri::AppHandle) -> Option<SessionInfo> {
    let status = match grpc::validate_session().await {
        Ok(status) => status,
        Err(e) => {
            // Sidecar down or network error: keep whatever state we have.
            eprintln!("[auth] session validation failed: {e}");
            return None;
        }
    };
    let info = SessionInfo::from_status(&status);
    let was_expired = app
        .try_state::<AppState>()
        .map(|s| s.api_status.read().session_state == "expired")
        .unwrap_or(false);
    set_session_status(app, &info);
    if info.state == "expired" && !was_expired {
        eprintln!("[auth] session expired");
        let _ = app.emit("auth-expired", &info);
    }
    Some(info)
}

/// Loads the encrypted bundle (or the legacy plain-text one) and installs the
/// session in a freshly started sidecar.
async fn restore_session(app: &tauri::AppHandle) {
    let mut bundle = secrets::load();
    if bundle.is_none()
        && let Some(imported) = import_legacy_browser_json(app).await
    {
        let _ = imported; // import already installed and validated the session
        return;
    }
    let Some(bundle) = bundle.take() else {
        set_session_status(app, &signed_out());
        return;
    };

    match grpc::set_session(&bundle.cookie, &bundle.user_agent, &bundle.x_goog_authuser).await {
        Ok(status) => {
            let info = SessionInfo::from_status(&status);
            set_session_status(app, &info);
            match info.state.as_str() {
                "active" => {
                    eprintln!("[auth] restored the session from auth.bin");
                    validate_and_publish(app).await;
                }
                "expired" => {
                    let _ = app.emit("auth-expired", &info);
                }
                _ => {}
            }
        }
        Err(e) => eprintln!("[auth] cannot restore the session: {e}"),
    }
}

/// One-time import of the pre-3.x `api/browser.json` (plain-text cookies).
/// The file is deleted once its fate is known: imported when the session still
/// works, dropped when it is already dead (a plain-text cookie file must not
/// survive on disk either way). A network failure keeps it for the next start.
async fn import_legacy_browser_json(app: &tauri::AppHandle) -> Option<secrets::AuthBundle> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("api")
        .join("browser.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let field = |name: &str| -> String {
        value
            .get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let cookie = field("cookie");
    if cookie.is_empty() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let bundle = secrets::AuthBundle::new(
        cookie,
        field("user-agent"),
        field("x-goog-authuser"),
    );

    let status = match grpc::set_session(&bundle.cookie, &bundle.user_agent, &bundle.x_goog_authuser)
        .await
    {
        Ok(status) => status,
        Err(e) => {
            eprintln!("[auth] legacy import skipped (sidecar unavailable): {e}");
            return None;
        }
    };
    if SessionInfo::from_status(&status).state != "active" {
        drop_legacy_browser_file(app, &path, "the stored cookie is unusable");
        return None;
    }

    let validated = match grpc::validate_session().await {
        Ok(validated) => validated,
        Err(e) => {
            // Cannot judge the session right now; keep the file for next start.
            eprintln!("[auth] legacy import deferred (validation failed): {e}");
            return None;
        }
    };
    let info = SessionInfo::from_status(&validated);
    if info.state != "active" {
        drop_legacy_browser_file(app, &path, "the session had expired");
        return None;
    }
    if secrets::save(&bundle).is_err() {
        return None;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => eprintln!("[auth] imported api/browser.json into auth.bin and deleted it"),
        Err(e) => eprintln!("[auth] imported api/browser.json but could not delete it: {e}"),
    }
    set_session_status(app, &info);
    let _ = app.emit(
        "auth-notice",
        "Your previous YouTube session was imported from api/browser.json. \
         The plain-text file has been deleted.",
    );
    Some(bundle)
}

/// The old file cannot be imported; make sure no plain-text cookies stay on
/// disk and tell the user why they have to sign in again.
fn drop_legacy_browser_file(app: &tauri::AppHandle, path: &std::path::Path, reason: &str) {
    let _ = std::fs::remove_file(path);
    eprintln!("[auth] deleted legacy api/browser.json: {reason}");
    let _ = app.emit(
        "auth-notice",
        format!(
            "The old plain-text YouTube session (api/browser.json) was removed because {reason}. \
             Sign in again."
        ),
    );
}

/// Restores the session once the sidecar is up, then re-validates every 15 min.
fn spawn_session_bootstrap(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        for _ in 0..60 {
            if grpc::health().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        restore_session(&app).await;
        loop {
            tokio::time::sleep(SESSION_BACKGROUND_EVERY).await;
            validate_and_publish(&app).await;
        }
    });
}

/// "Check before loading the library/playlists": at most one network round
/// per [`SESSION_CHECK_EVERY`], so opening a playlist does not add latency.
async fn ensure_session_fresh(app: &tauri::AppHandle, state: &AppState) {
    let stale = {
        let checked = state.session_checked.lock();
        checked
            .map(|at| at.elapsed() >= SESSION_CHECK_EVERY)
            .unwrap_or(true)
    };
    if stale {
        validate_and_publish(app).await;
    }
}

#[tauri::command]
async fn yt_login_start(app: tauri::AppHandle) -> Result<(), String> {
    auth_window::open(&app)
}

/// Opens the YouTube Music sign-in page in the user's default browser.
///
/// The app cannot read cookies out of a browser profile it does not
/// control, so this only starts the human part of the flow; the session
/// arrives through `yt_login_advanced` below, pasted from DevTools.
#[tauri::command]
fn yt_open_login_page() -> Result<(), String> {
    browser::open("https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fmusic.youtube.com%2F")
}

/// Takes the request headers copied from the browser, normalises them on the
/// sidecar and installs the session. The headers go straight into
/// `secrets::AuthBundle` — no cookie file anywhere.
#[tauri::command]
async fn yt_login_advanced(
    auth_text: String,
    app: tauri::AppHandle,
) -> Result<SessionInfo, String> {
    let fields = grpc::parse_auth_headers(&auth_text)
        .await
        .map_err(|e| e.to_string())?;
    let bundle = secrets::AuthBundle::new(
        fields.cookie,
        fields.user_agent,
        fields.x_goog_authuser,
    );
    install_session(&app, bundle).await
}

#[tauri::command]
async fn logout(state: tauri::State<'_, AppState>) -> Result<(), String> {
    // Best effort: even with the sidecar down, the local secrets must go.
    if let Err(e) = grpc::clear_session().await {
        eprintln!("[auth] clear_session failed: {e}");
    }
    secrets::clear()?;
    drop_track_cache();
    // The snapshot may hold this account's picks: it must not outlive the
    // session that fetched it.
    if let Err(e) = std::fs::remove_file(home_file())
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("[home] cannot drop the snapshot on logout: {e}");
    }
    {
        let mut status = state.api_status.write();
        status.auth_ok = false;
        status.session_state = "signed_out".to_string();
        status.account_name.clear();
    }
    Ok(())
}

#[tauri::command]
async fn get_user_playlists(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<UserPlaylistInfo>, String> {
    ensure_session_fresh(&app, &state).await;
    let resp = grpc::get_user_playlists()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.has_auth {
        return Err(if resp.hint.is_empty() {
            "no youtube auth".to_string()
        } else {
            resp.hint
        });
    }
    if !resp.error.is_empty() {
        return Err(resp.error);
    }
    Ok(resp
        .playlists
        .into_iter()
        .map(|p| UserPlaylistInfo {
            id: p.id,
            name: p.name,
            track_count: p.track_count,
            thumbnail: p.thumbnail,
        })
        .collect())
}

#[tauri::command]
async fn get_playlist_tracks(
    playlist_id: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    offset: Option<i32>,
    limit: Option<i32>,
) -> Result<TrackPage, String> {
    let offset = offset.unwrap_or(0).max(0);
    let limit = limit.unwrap_or(100).clamp(1, 500);
    let key = format!("playlist:{playlist_id}");
    fetch_track_page(&app, &state, &key, Some(&playlist_id), offset, limit).await
}

#[tauri::command]
async fn play_video(
    video_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<PlaylistInfo, String> {
    let id = extract_video_id(&video_id).ok_or("invalid video id or url")?;

    {
        let mut busy = state.downloading.lock();
        if *busy {
            return Err("download already in progress".to_string());
        }
        *busy = true;
    }

    let result = play_video_inner(&id, &state).await;
    *state.downloading.lock() = false;
    result
}

async fn play_video_inner(id: &str, state: &AppState) -> Result<PlaylistInfo, String> {
    let quality = current_quality(state);
    let dest = downloader::cached_file(id, &quality);
    let meta = downloader::cached_meta(id, &quality);

    let title = if dest.exists() {
        if let Some(t) = read_cached_title(&meta) {
            t
        } else {
            // file cached but metadata lost: resolve just for the title
            let resolved = grpc::resolve(id, &quality, streaming_account(state))
                .await
                .map_err(|e| e.to_string())?;
            let track = resolved
                .track
                .ok_or_else(|| "no track metadata in response".to_string())?;
            track.title.clone()
        }
    } else {
        let resolved = grpc::resolve(id, &quality, streaming_account(state))
            .await
            .map_err(|e| e.to_string())?;
        if !resolved.ok {
            return Err(if resolved.error.is_empty() {
                "failed to resolve stream url".to_string()
            } else {
                resolved.error
            });
        }
        let track = resolved
            .track
            .ok_or_else(|| "no track metadata in response".to_string())?;
        let title = track.title.clone();
        state.download.title.lock().clone_from(&title);
        downloader::download(&resolved.stream_url, &dest, &state.download)
            .await
            .map_err(|e| e.to_string())?;
        if let Err(e) = fsutil::write_atomic(&meta, title.as_bytes()) {
            eprintln!("[yt] failed to write metadata: {e}");
        }
        title
    };

    let playlist = Playlist {
        name: title.clone(),
        tracks: vec![Track::new(dest, title.clone())],
    };
    state
        .engine
        .lock()
        .send(Cmd::SetPlaylist(playlist));

    Ok(PlaylistInfo {
        name: title,
        tracks: 1,
    })
}

fn read_cached_title(meta: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(meta).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

async fn play_current(state: &AppState) -> Result<(), String> {
    let started = std::time::Instant::now();
    let item = state
        .queue
        .lock()
        .current_item()
        .ok_or_else(|| "queue is empty".to_string())?;

    let quality = current_quality(state);
    let dest = downloader::cached_file(&item.id, &quality);
    let cached = dest.exists();
    if !dest.exists() {
        // Claim this id so a concurrent prefetch cannot write the same file.
        let claimed = {
            let mut set = state.fetching.lock();
            set.insert(item.id.clone())
        };
        if claimed {
            let result = play_current_download(state, &item, &dest).await;
            state.fetching.lock().remove(&item.id);
            result?;
        } else {
            // Another task is downloading this track; wait until it finishes.
            for _ in 0..600 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if dest.exists() {
                    break;
                }
            }
            if !dest.exists() {
                return Err("timed out waiting for track download".to_string());
            }
        }
    }

    // Playback is the strongest "last used" signal for the eviction.
    downloader::touch(&dest);

    let playlist = Playlist {
        name: item.title.clone(),
        tracks: vec![Track {
            path: dest.clone(),
            title: item.title.clone(),
            artist: item.artist.clone(),
            album: String::new(),
            duration: if item.duration_secs > 0.0 {
                Some(item.duration_secs)
            } else {
                None
            },
            cover: None,
            gain: if normalize_enabled(&state.settings.lock()) {
                state
                    .loudness
                    .lock()
                    .get(&dest.to_string_lossy().into_owned())
                    .copied()
            } else {
                None
            },
        }],
    };
    state
        .engine
        .lock()
        .send(Cmd::SetPlaylist(playlist));
    // The queue asked for this track at `started`; this is the moment the audio
    // thread is told what to play.
    eprintln!(
        "[perf] time_to_audio_ms={} cached={} track={}",
        started.elapsed().as_millis(),
        cached,
        item.id
    );
    record_history(
        state,
        &HistoryItem {
            id: item.id.clone(),
            title: item.title.clone(),
            artist: item.artist.clone(),
            thumbnail: item.thumbnail.clone(),
            played_at: now_secs(),
        },
    );
    if normalize_enabled(&state.settings.lock()) {
        spawn_loudness_scan(
            vec![dest],
            0,
            state.loudness.clone(),
            state.engine.lock().clone(),
            target_rms(&state.settings.lock().normalize_level).unwrap_or(0.12),
        );
    }
    Ok(())
}

async fn play_current_download(
    state: &AppState,
    item: &QueueItem,
    dest: &std::path::Path,
) -> Result<(), String> {
    // Immediate feedback in the UI while resolving + downloading.
    state.download.title.lock().clone_from(&item.title);
    state
        .download
        .done
        .store(false, std::sync::atomic::Ordering::Relaxed);
    state
        .download
        .bytes
        .store(0, std::sync::atomic::Ordering::Relaxed);

    let quality = current_quality(state);
    let resolved = grpc::resolve(&item.id, &quality, streaming_account(state))
        .await
        .map_err(|e| e.to_string())?;
    if !resolved.ok {
        return Err(if resolved.error.is_empty() {
            "failed to resolve stream url".to_string()
        } else {
            resolved.error
        });
    }

    match downloader::download(&resolved.stream_url, dest, &state.download).await {
        Ok(()) => {}
        Err(first_err) => {
            // The cached URL may have expired: force a fresh resolve and retry.
            eprintln!(
                "[yt] download failed ({first_err}), re-resolving {}",
                item.id
            );
            let fresh = grpc::resolve_forced(&item.id, &quality, streaming_account(state))
                .await
                .map_err(|e| e.to_string())?;
            if !fresh.ok {
                return Err(first_err.to_string());
            }
            if fresh.stream_url == resolved.stream_url {
                return Err(first_err.to_string());
            }
            downloader::download(&fresh.stream_url, dest, &state.download)
                .await
                .map_err(|e| e.to_string())?;
        }
    }

    let meta = downloader::cached_meta(&item.id, &quality);
    if let Err(e) = fsutil::write_atomic(&meta, item.title.as_bytes()) {
        eprintln!("[cache] failed to write the title sidecar: {e}");
    }
    Ok(())
}

/// Pre-resolves (and pre-downloads the first track of) a freshly shown list,
/// so clicking starts playback instantly.
#[tauri::command]
async fn prefetch_tracks(
    mut items: Vec<QueueItem>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    items.retain(|i| downloader::is_valid_video_id(&i.id));
    if items.is_empty() {
        return Ok(());
    }
    let quality = current_quality(&state);
    let use_account = streaming_account(&state);
    let ids: Vec<String> = items.iter().take(4).map(|i| i.id.clone()).collect();
    // Resolve in parallel on the sidecar; failures are non-fatal.
    let _ = grpc::prefetch_resolve(&ids, &quality, use_account).await;

    // Download only the first track in the background (click = instant play).
    let first = items[0].clone();
    let fetching = state.fetching.clone();
    tauri::async_runtime::spawn(async move {
        let _ = ensure_cached(&first, &fetching, &quality, use_account).await;
    });
    Ok(())
}

async fn ensure_cached(
    item: &QueueItem,
    fetching: &FetchGuard,
    quality: &str,
    use_account: bool,
) -> Result<(), String> {
    let dest = downloader::cached_file(&item.id, quality);
    if dest.exists() {
        return Ok(());
    }
    {
        let mut set = fetching.lock();
        if !set.insert(item.id.clone()) {
            // another task is already downloading this item
            return Ok(());
        }
    }
    let result = async {
        let resolved = grpc::resolve(&item.id, quality, use_account)
            .await
            .map_err(|e| e.to_string())?;
        if !resolved.ok {
            return Ok(());
        }
        let handle = downloader::ProgressHandle::default();
        if let Err(first_err) = downloader::download(&resolved.stream_url, &dest, &handle).await {
            // Cached URL may have expired: force a fresh resolve and retry.
            let fresh = grpc::resolve_forced(&item.id, quality, use_account)
                .await
                .map_err(|e| e.to_string())?;
            if fresh.ok && fresh.stream_url != resolved.stream_url {
                downloader::download(&fresh.stream_url, &dest, &handle)
                    .await
                    .map_err(|e| e.to_string())?;
            } else {
                return Err(first_err.to_string());
            }
        }
        let meta = downloader::cached_meta(&item.id, quality);
        if let Err(e) = fsutil::write_atomic(&meta, item.title.as_bytes()) {
            eprintln!("[cache] failed to write the title sidecar: {e}");
        }
        Ok(())
    }
    .await;
    fetching.lock().remove(&item.id);
    result
}

#[derive(Serialize, Clone)]
struct QueueInfo {
    active: bool,
    current: usize,
    items: Vec<QueueItem>,
    repeat: String,
    version: u64,
}

#[tauri::command]
fn get_queue_info(state: tauri::State<AppState>) -> QueueInfo {
    let q = state.queue.lock();
    let repeat = state.repeat.lock().as_str().to_string();
    QueueInfo {
        active: q.active(),
        current: q.current(),
        items: q.items().to_vec(),
        repeat,
        version: q.version(),
    }
}

/// Cheap version counter: the UI polls it and fetches the full queue only
/// when it changed (avoids serializing thousands of items every 250 ms).
#[tauri::command]
fn get_queue_version(state: tauri::State<AppState>) -> u64 {
    state.queue.lock().version()
}

#[tauri::command]
fn get_settings(state: tauri::State<AppState>) -> Settings {
    state.settings.lock().clone()
}

#[tauri::command]
fn set_audio_quality(quality: String, state: tauri::State<AppState>) -> Result<(), String> {
    if quality != "standard" && quality != "high" {
        return Err("unknown audio quality".to_string());
    }
    let mut settings = state.settings.lock();
    settings.audio_quality = quality;
    save_settings(&settings);
    Ok(())
}

#[tauri::command]
fn set_cache_limit(limit_mb: u64, state: tauri::State<AppState>) {
    {
        let mut settings = state.settings.lock();
        settings.cache_limit_mb = limit_mb;
        save_settings(&settings);
    }
    // Free the space right away instead of waiting for the next sweep.
    run_cache_eviction(&state);
}

#[tauri::command]
fn set_search_filter(filter: String, state: tauri::State<AppState>) -> Result<(), String> {
    const FILTERS: [&str; 6] = ["", "songs", "videos", "albums", "artists", "playlists"];
    if !FILTERS.contains(&filter.as_str()) {
        return Err("unknown search filter".to_string());
    }
    let mut settings = state.settings.lock();
    settings.search_filter = filter;
    save_settings(&settings);
    Ok(())
}

#[tauri::command]
fn set_streaming_account(enabled: bool, state: tauri::State<AppState>) {
    let mut settings = state.settings.lock();
    settings.streaming_account = enabled;
    save_settings(&settings);
}

#[tauri::command]
fn set_volume_curve(enabled: bool, state: tauri::State<AppState>) {
    {
        let mut settings = state.settings.lock();
        settings.volume_curve = enabled;
        save_settings(&settings);
    }
    state
        .engine
        .lock()
        .send(Cmd::SetVolumeCurve(enabled));
}

#[tauri::command]
fn set_normalize_level(level: String, state: tauri::State<AppState>) -> Result<(), String> {
    if !matches!(level.as_str(), "off" | "light" | "medium" | "strong") {
        return Err("unknown normalize level".to_string());
    }
    {
        let mut settings = state.settings.lock();
        settings.normalize_level = level.clone();
        save_settings(&settings);
    }
    // Gains depend on the target level: drop the old cache and re-scan.
    state.loudness.lock().clear();
    let _ = std::fs::remove_file(loudness_file());
    state.engine.lock().send(Cmd::ClearGains);

    // Rescan the visible library and the current queue track immediately,
    // so the new level is audible right away.
    if let Some(target) = target_rms(&level) {
        let mut paths: Vec<std::path::PathBuf> = state
            .local_current
            .lock()
            .as_ref()
            .map(|tracks| {
                tracks
                    .iter()
                    .filter_map(|t| t.path.as_ref().map(std::path::PathBuf::from))
                    .collect()
            })
            .unwrap_or_default();
        {
            let q = state.queue.lock();
            if q.active()
                && let Some(item) = q.items().get(q.current())
            {
                paths.push(downloader::cached_file(&item.id, &current_quality(&state)));
            }
        }
        let current = state
            .engine
            .lock()
            .snapshot()
            .map(|snapshot| snapshot.idx)
            .unwrap_or(0);
        spawn_loudness_scan(
            paths,
            current,
            state.loudness.clone(),
            state.engine.lock().clone(),
            target,
        );
    }
    Ok(())
}

#[tauri::command]
fn clear_cache() -> Result<(), String> {
    downloader::clear_cache().map_err(|e| e.to_string())
}

#[tauri::command]
fn cycle_repeat(state: tauri::State<AppState>) -> String {
    let mut r = state.repeat.lock();
    *r = match *r {
        RepeatMode::Off => RepeatMode::All,
        RepeatMode::All => RepeatMode::One,
        RepeatMode::One => RepeatMode::Off,
    };
    r.as_str().to_string()
}

#[tauri::command]
fn set_speed(speed: f32, state: tauri::State<AppState>) {
    state.engine.lock().send(Cmd::SetSpeed(speed));
}

#[derive(Serialize, Clone)]
struct LyricsInfo {
    ok: bool,
    synced: String,
    plain: String,
    error: String,
}

#[tauri::command]
async fn get_lyrics(
    artist: String,
    title: String,
    duration_secs: f64,
    _state: tauri::State<'_, AppState>,
) -> Result<LyricsInfo, String> {
    let resp = grpc::get_lyrics(&artist, &title, duration_secs)
        .await
        .map_err(|e| e.to_string())?;
    Ok(LyricsInfo {
        ok: resp.ok,
        synced: resp.synced,
        plain: resp.plain,
        error: resp.error,
    })
}

#[tauri::command]
async fn recommend_tracks(
    video_id: String,
    limit: Option<i32>,
    _state: tauri::State<'_, AppState>,
) -> Result<Vec<SearchTrack>, String> {
    let tracks = grpc::recommend(&video_id, limit.unwrap_or(20))
        .await
        .map_err(|e| e.to_string())?;
    Ok(tracks
        .into_iter()
        .map(|t| SearchTrack {
            video_id: t.video_id,
            title: t.title,
            artist: t.artist,
            duration_secs: t.duration_secs,
            thumbnail: t.thumbnail,
        })
        .collect())
}

#[tauri::command]
async fn queue_add(
    mut items: Vec<QueueItem>,
    play_next: Option<bool>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    items.retain(|i| downloader::is_valid_video_id(&i.id));
    if items.is_empty() {
        return Ok(());
    }
    let start_fresh = {
        let q = state.queue.lock();
        !q.active() || q.is_empty()
    };
    if start_fresh {
        return play_queue(items, 0, state).await;
    }
    state.queue.lock().add(items, play_next.unwrap_or(false));
    Ok(())
}

#[tauri::command]
fn queue_remove(index: usize, state: tauri::State<AppState>) -> Result<(), String> {
    let action = state.queue.lock().remove(index).map_err(str::to_string)?;
    if action == Action::Stop {
        state.engine.lock().send(Cmd::Stop);
    }
    Ok(())
}

#[tauri::command]
fn queue_clear(state: tauri::State<AppState>) {
    state.queue.lock().clear();
    state.engine.lock().send(Cmd::Stop);
}

fn home_file() -> std::path::PathBuf {
    downloader::settings_file()
        .parent()
        .map(|dir| dir.join("home.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("home.json"))
}

fn home_signed_in(state: &AppState) -> bool {
    state.api_status.read().session_state == "active"
}

/// Network shelves of the last successful refresh. Local shelves are rebuilt
/// every time, so a snapshot never goes stale in the parts that matter offline.
#[derive(Serialize, Deserialize, Clone, Default)]
struct HomeSnapshot {
    #[serde(default)]
    generated_at: f64,
    #[serde(default)]
    personal: bool,
    #[serde(default)]
    shelves: Vec<home::Shelf>,
}

#[derive(Serialize, Clone)]
struct HomePage {
    shelves: Vec<home::Shelf>,
    generated_at: f64,
    /// "snapshot" for the cached page, "fresh" after a successful refresh.
    source: String,
    /// True when the page includes shelves fetched with an account.
    personal: bool,
}

fn local_home_input(state: &AppState) -> home::LocalInput {
    let resume = state
        .settings
        .lock()
        .resume
        .clone()
        .filter(|r| !r.id.is_empty())
        .map(|r| home::Resume {
            id: r.id,
            title: r.title,
            artist: r.artist,
            thumbnail: r.thumbnail,
            position_secs: r.position_secs,
            is_local: r.is_local,
            path: r.path,
        });

    let history = state
        .history
        .lock()
        .iter()
        .map(|h| home::HistoryEntry {
            id: h.id.clone(),
            title: h.title.clone(),
            artist: h.artist.clone(),
            thumbnail: h.thumbnail.clone(),
            played_at: h.played_at,
        })
        .collect();

    let favorites = state
        .favorites
        .lock()
        .iter()
        .map(|f| home::FavoriteEntry {
            path: f.path.clone(),
            title: f.title.clone(),
            artist: f.artist.clone(),
            cover: f.cover.clone().unwrap_or_default(),
        })
        .collect();

    // Newest local files by mtime, newest first.
    let mut recent_files: Vec<home::FileEntry> = {
        let library = state.library.lock();
        let mut files = Vec::new();
        for playlist in library.iter() {
            for track in playlist.tracks.iter() {
                let mtime = std::fs::metadata(&track.path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                files.push(home::FileEntry {
                    path: track.path.to_string_lossy().into_owned(),
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    cover: track
                        .cover
                        .as_ref()
                        .map(|c| c.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    mtime,
                });
            }
        }
        files
    };
    recent_files.sort_by(|a, b| {
        b.mtime
            .partial_cmp(&a.mtime)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    recent_files.truncate(60);

    home::LocalInput {
        resume,
        history,
        favorites,
        recent_files,
    }
}

fn shelf_from_pb(shelf: grpc::Shelf) -> home::Shelf {
    home::Shelf {
        title: shelf.title,
        kind: shelf.kind,
        items: shelf
            .items
            .into_iter()
            .filter(|item| !item.id.is_empty())
            .map(|item| home::Item {
                kind: item.kind,
                id: item.id,
                title: item.title,
                subtitle: item.subtitle,
                thumbnail: item.thumbnail,
                duration_secs: item.duration_secs,
                video_type: item.video_type,
                browse_id: item.browse_id,
                playlist_id: item.playlist_id,
                path: None,
                start_secs: 0.0,
            })
            .collect(),
    }
}

/// Local shelves first, then whatever the snapshot still may show.
#[tauri::command]
fn get_home_cached(state: tauri::State<AppState>) -> HomePage {
    let signed_in = home_signed_in(&state);
    let snapshot: HomeSnapshot = load_json(&home_file());
    let mut shelves = home::local_shelves(&local_home_input(&state), 20);
    shelves.extend(home::visible_network_shelves(&snapshot.shelves, signed_in));
    HomePage {
        shelves,
        generated_at: snapshot.generated_at,
        source: "snapshot".to_string(),
        personal: snapshot.personal && signed_in,
    }
}

/// Fetches the network shelves, waits out a sidecar that is still starting, and
/// stores the result so the next cold start is instant.
async fn fetch_home_shelves() -> Result<Vec<home::Shelf>, String> {
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
    const PAUSE: std::time::Duration = std::time::Duration::from_millis(1500);

    let started = std::time::Instant::now();
    loop {
        match grpc::get_home(4).await {
            Ok(shelves) => {
                return Ok(shelves.into_iter().map(shelf_from_pb).collect());
            }
            Err(e) => {
                let message = e.to_string();
                if !grpc::is_retryable_failure(&e) || started.elapsed() >= DEADLINE {
                    return Err(message);
                }
                tokio::time::sleep(PAUSE).await;
            }
        }
    }
}

/// The home page must reflect the account that is actually signed in. Restoring
/// auth.bin happens right after the sidecar comes up, so asking GetHome before
/// that would cache a guest page for a signed-in listener — and then overwrite
/// the personal snapshot with public shelves.
async fn wait_for_session_ready(state: &AppState) -> bool {
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(15);
    let deadline = std::time::Instant::now() + LIMIT;
    while state.session_checked.lock().is_none() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    true
}

#[tauri::command]
async fn refresh_home(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<HomePage, String> {
    wait_for_session_ready(&state).await;
    let network = fetch_home_shelves().await?;
    let signed_in = home_signed_in(&state);
    let personal = network
        .iter()
        .any(|shelf| shelf.kind == home::KIND_PERSONAL);
    let generated_at = now_secs();

    let snapshot = HomeSnapshot {
        generated_at,
        personal,
        shelves: network.clone(),
    };
    if let Err(e) = fsutil::write_json_atomic(&home_file(), &snapshot, true) {
        eprintln!("[store] cannot save the home snapshot: {e}");
    }

    let mut shelves = home::local_shelves(&local_home_input(&state), 20);
    shelves.extend(home::visible_network_shelves(&network, signed_in));

    let page = HomePage {
        shelves,
        generated_at,
        source: "fresh".to_string(),
        personal,
    };
    let _ = app.emit("home-updated", page.clone());
    Ok(page)
}

/// Remembers the last track and position so the home page can offer
/// «Продолжить». Polls the engine rather than hooking every playback path.
fn spawn_resume_saver(app: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("resume-saver".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let Some(state) = app.try_state::<AppState>() else {
                    continue;
                };
                let snapshot = match state.engine.lock().snapshot() {
                    Some(snapshot) if snapshot.status == "playing" => snapshot,
                    _ => continue,
                };
                let Some(track) = snapshot.track.clone() else {
                    continue;
                };
                // A network track downloads to a cache file, so the engine path
                // alone cannot tell local playback from YouTube: the queue can.
                // It is inactive only for local files (play_track deactivates it).
                let queue_item = {
                    let queue = state.queue.lock();
                    if queue.active() {
                        queue.current_item()
                    } else {
                        None
                    }
                };
                let (id, title, artist, thumbnail, is_local, path) = match queue_item {
                    Some(item) => (
                        item.id,
                        item.title,
                        item.artist,
                        item.thumbnail,
                        false,
                        String::new(),
                    ),
                    None => match track.path.clone() {
                        Some(path) => (
                            path.clone(),
                            track.title,
                            track.artist,
                            track.cover.unwrap_or_default(),
                            true,
                            path,
                        ),
                        None => continue,
                    },
                };
                let next = ResumeState {
                    id,
                    title,
                    artist,
                    thumbnail,
                    position_secs: snapshot.elapsed,
                    is_local,
                    path,
                    saved_at: now_secs(),
                };
                let mut settings = state.settings.lock();
                let unchanged = settings
                    .resume
                    .as_ref()
                    .map(|r| r.id == next.id && (r.position_secs - next.position_secs).abs() < 5.0)
                    .unwrap_or(false);
                if unchanged {
                    continue;
                }
                settings.resume = Some(next);
                let to_save = settings.clone();
                drop(settings);
                save_settings(&to_save);
            }
        })
        .expect("failed to spawn the resume saver");
}

#[tauri::command]
fn get_history(state: tauri::State<AppState>) -> Vec<HistoryItem> {
    state.history.lock().clone()
}

#[tauri::command]
fn toggle_favorite(state: tauri::State<AppState>) -> Result<bool, String> {
    let track = {
        let snap = state.engine.lock().snapshot();
        snap.and_then(|s| s.track)
    };
    let Some(track) = track else {
        return Err("nothing playing".to_string());
    };
    let Some(path) = track.path.clone() else {
        return Err("not a local track".to_string());
    };
    let mut favorites = state.favorites.lock();
    if let Some(pos) = favorites.iter().position(|f| f.path == path) {
        favorites.remove(pos);
        let snapshot = favorites.clone();
        drop(favorites);
        save_json(&favorites_file(), &snapshot);
        Ok(false)
    } else {
        favorites.push(FavoriteTrack {
            path,
            title: track.title,
            artist: track.artist,
            duration: track.duration,
            cover: track.cover,
        });
        let snapshot = favorites.clone();
        drop(favorites);
        save_json(&favorites_file(), &snapshot);
        Ok(true)
    }
}

#[tauri::command]
fn is_favorite(state: tauri::State<AppState>) -> bool {
    let Some(snap) = state.engine.lock().snapshot() else {
        return false;
    };
    let Some(track) = snap.track else {
        return false;
    };
    let Some(path) = track.path else {
        return false;
    };
    state
        .favorites
        .lock()
        .iter()
        .any(|f| f.path == path)
}

#[tauri::command]
fn open_favorites(state: tauri::State<AppState>) -> Result<Vec<engine::TrackInfo>, String> {
    let favorites = state.favorites.lock().clone();
    let tracks: Vec<Track> = favorites
        .into_iter()
        .map(|f| Track {
            path: std::path::PathBuf::from(f.path),
            title: f.title,
            artist: f.artist,
            album: String::new(),
            duration: if f.duration > 0.0 {
                Some(f.duration)
            } else {
                None
            },
            cover: f.cover.map(std::path::PathBuf::from),
            gain: None,
        })
        .collect();
    if tracks.is_empty() {
        return Err("no favorite tracks yet".to_string());
    }
    let playlist = Playlist {
        name: "Favorites".to_string(),
        tracks: tracks.clone(),
    };
    let infos: Vec<engine::TrackInfo> = tracks.iter().map(track_to_info).collect();
    *state.local_current.lock() = Some(infos.clone());
    state.queue.lock().deactivate();
    let mut playlist = playlist;
    let normalize = normalize_enabled(&state.settings.lock());
    apply_gains(&mut playlist, &state.loudness, normalize);
    let scan_paths: Vec<std::path::PathBuf> =
        playlist.tracks.iter().map(|t| t.path.clone()).collect();
    state
        .engine
        .lock()
        .send(Cmd::OpenPlaylist(playlist));
    spawn_loudness_scan(
        scan_paths,
        0,
        state.loudness.clone(),
        state.engine.lock().clone(),
        target_rms(&state.settings.lock().normalize_level).unwrap_or(0.12),
    );
    Ok(infos)
}

#[tauri::command]
fn set_sleep_timer(
    minutes: f64,
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<(), String> {
    if minutes <= 0.0 {
        *state.sleep_until.lock() = None;
        return Ok(());
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(minutes * 60.0);
    *state.sleep_until.lock() = Some(until);
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs_f64(minutes * 60.0)).await;
        let app_state = app.state::<AppState>();
        {
            let mut su = app_state.sleep_until.lock();
            match *su {
                Some(t) if t <= std::time::Instant::now() => {
                    *su = None;
                }
                _ => return, // timer was replaced or cancelled
            }
        }
        app_state.queue.lock().mark_manual_stop();
        app_state.engine.lock().send(Cmd::Stop);
    });
    Ok(())
}

#[tauri::command]
fn get_sleep_timer(state: tauri::State<AppState>) -> Option<f64> {
    state.sleep_until.lock().map(|t| {
        t.saturating_duration_since(std::time::Instant::now())
            .as_secs_f64()
    })
}

#[tauri::command]
async fn play_queue(
    mut items: Vec<QueueItem>,
    start: usize,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    items.retain(|i| downloader::is_valid_video_id(&i.id));
    if items.is_empty() {
        return Err("empty queue".to_string());
    }
    state.queue.lock().set_items(items, start);
    // The preload of the next two tracks is triggered by the engine's
    // TrackStarted event, so a queue started from here and a track started by
    // "next" follow exactly the same path.
    play_current(&state).await?;
    Ok(())
}

/// Downloads the given queue items in the background (silently, without
/// touching the visible download progress).
fn spawn_prefetch(items: Vec<QueueItem>, fetching: FetchGuard, quality: String, use_account: bool) {
    if items.is_empty() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        for item in items {
            if ensure_cached(&item, &fetching, &quality, use_account).await.is_err() {
                break;
            }
        }
    });
}

/// Drives the queue from the engine's own signal.
///
/// `TrackEnded` arrives the moment the decoder runs dry, so advancing no longer
/// waits for the next 250 ms tick of a polling loop. `TrackStarted` triggers the
/// preload of the next tracks.
///
/// TODO(gapless): the switch between two tracks still has a small gap (engine
/// tick + decoder open). Sample-accurate gapless needs the engine to append the
/// next source to the same `Player` before the current one ends, and to report
/// which playlist entry is playing when it rolls over. Plan:
///   1. `Cmd::QueueNext(Playlist)` — engine keeps a `pending: Option<Track>` and
///      appends its decoder to the same player when `remaining() < 2 s`.
///   2. `Event::TrackChanged { idx }` so the queue advances its cursor without
///      calling play_current (no download, no SetPlaylist round trip).
///   3. The queue hands over the next cached file as soon as `TrackStarted`
///      fires, capped at one track ahead; repeat-one keeps the current source.
/// Only worth doing once playback is gapless in the engine: rodio's `Player`
/// already supports queueing sources, so this is an engine-local change.
fn spawn_queue_watcher(handle: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("queue-watcher".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build queue watcher runtime");
            rt.block_on(async move {
                let state = handle.state::<AppState>();
                let Some(mut events) = state.engine.lock().take_events() else {
                    eprintln!("[queue] engine events were already taken");
                    return;
                };
                let mut prefetched_for: Option<usize> = None;

                while let Some(event) = events.recv().await {
                    match event {
                        engine::Event::TrackEnded => {
                            let action = {
                                let mut q = state.queue.lock();
                                if !q.active() || q.manual_stop() {
                                    None
                                } else {
                                    let repeat = *state.repeat.lock();
                                    Some(q.advance(repeat))
                                }
                            };
                            if action == Some(Action::PlayCurrent) {
                                prefetched_for = None;
                                if let Err(e) = play_current(&state).await {
                                    eprintln!("[queue] play_current failed: {e}");
                                }
                            }
                        }
                        engine::Event::TrackStarted => {
                            let plan = {
                                let q = state.queue.lock();
                                if !q.active() || q.manual_stop() {
                                    None
                                } else if prefetched_for == Some(q.current()) {
                                    None
                                } else if *state.repeat.lock() == RepeatMode::One {
                                    // Same file again: nothing to preload, but
                                    // remember the index.
                                    Some((q.current(), Vec::new()))
                                } else {
                                    Some((q.current(), q.upcoming(2)))
                                }
                            };
                            match plan {
                                Some((current, items)) => {
                                    prefetched_for = Some(current);
                                    if !items.is_empty() {
                                        spawn_prefetch(
                                            items,
                                            state.fetching.clone(),
                                            current_quality(&state),
                                            streaming_account(&state),
                                        );
                                    }
                                }
                                // Local playback or a stopped queue: forget the
                                // index so the next queue start preloads again.
                                None => prefetched_for = None,
                            }
                        }
                    }
                }
            });
        })
        .expect("failed to spawn queue watcher");
}

/// Generates a random token shared between the app and the sidecar so that
/// other local processes cannot use the gRPC endpoint. 32 bytes straight from
/// the OS CSPRNG, hex encoded; never written to disk.
fn generate_api_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random number generator failed");
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    token
}

fn extract_video_id(input: &str) -> Option<String> {
    let s = input.trim();
    if s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Some(s.to_string());
    }
    for part in s.split(['?', '&', '/']) {
        if let Some(id) = part.strip_prefix("v=")
            && id.len() == 11
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Some(id.to_string());
        }
    }
    if let Some(idx) = s.find("youtu.be/") {
        let cand = &s[idx + 9..];
        let id: String = cand
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if id.len() == 11 {
            return Some(id);
        }
    }
    None
}

fn main() {
    downloader::migrate_legacy_dir();
    // Older builds persisted the token to %LOCALAPPDATA%\fasttube\api_token.
    // It authenticates nothing now (a new token is generated per launch), so
    // drop the leftover secret instead of leaving it on disk.
    let _ = std::fs::remove_file(downloader::data_dir().join("api_token"));
    let api_token = generate_api_token();
    let initial_dir = std::env::args()
        .skip(1)
        .map(std::path::PathBuf::from)
        .find(|p| p.is_dir());

    let app_state = AppState {
        engine: Mutex::new(EngineHandle::new()),
        library: Mutex::new(Vec::new()),
        local_current: Mutex::new(None),
        settings: Mutex::new(load_settings()),
        api_status: Arc::new(RwLock::new(ApiStatus {
            state: "starting".to_string(),
            version: String::new(),
            auth_ok: false,
            session_state: "signed_out".to_string(),
            account_name: String::new(),
        })),
        download: ProgressHandle::default(),
        downloading: Mutex::new(false),
        queue: Mutex::new(Queue::default()),
        repeat: Mutex::new(RepeatMode::Off),
        fetching: Arc::new(Mutex::new(std::collections::HashSet::new())),
        history: Mutex::new(load_history()),
        favorites: Mutex::new(load_json(&favorites_file())),
        sleep_until: Mutex::new(None),
        loudness: load_loudness(),
        session_checked: Mutex::new(None),
        settings_dirty: std::sync::atomic::AtomicBool::new(false),
    };
    let api_status = app_state.api_status.clone();
    {
        let (curve, volume, muted) = {
            let settings = app_state.settings.lock();
            (
                settings.volume_curve,
                settings.volume.clamp(0.0, 1.0),
                settings.muted,
            )
        };
        let engine = app_state.engine.lock().clone();
        engine.send(Cmd::SetVolumeCurve(curve));
        engine.send(Cmd::SetVolume(volume));
        engine.send(Cmd::SetMuted(muted));
    }
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sidecar_shutdown = shutdown.clone();
    let sidecar = sidecar::spawn_and_supervise(api_status, sidecar_shutdown, api_token);

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .setup(move |app| {
            spawn_queue_watcher(app.handle().clone());
            spawn_resume_saver(app.handle().clone());
            spawn_session_bootstrap(app.handle().clone());
            spawn_cache_sweeper(app.handle().clone());
            spawn_settings_flusher(app.handle().clone());
            if let Some(dir) = initial_dir {
                let handle = app.handle().clone();
                std::thread::Builder::new()
                    .name("library-scan".to_string())
                    .spawn(move || {
                        let cover_dir = downloader::covers_dir();
                        match playlist::collect(&dir, &cover_dir) {
                            Ok(library) if !library.is_empty() => {
                                let first = library.first().cloned();
                                let state = handle.state::<AppState>();
                                *state.library.lock() = library;
                                if let Some(pl) = first {
                                    let track_infos: Vec<engine::TrackInfo> =
                                        pl.tracks.iter().map(track_to_info).collect();
                                    *state.local_current.lock() = Some(track_infos);
                                    let mut pl = pl;
                                    let normalize =
                                        normalize_enabled(&state.settings.lock());
                                    apply_gains(&mut pl, &state.loudness, normalize);
                                    let scan_paths: Vec<std::path::PathBuf> =
                                        pl.tracks.iter().map(|t| t.path.clone()).collect();
                                    state.engine.lock().send(Cmd::SetPlaylist(pl));
                                    spawn_loudness_scan(
                                        scan_paths,
                                        0,
                                        state.loudness.clone(),
                                        state.engine.lock().clone(),
                                        target_rms(&state.settings.lock().normalize_level)
                                            .unwrap_or(0.12),
                                    );
                                }
                                let _ = handle.emit("library-loaded", ());
                            }
                            Ok(_) => eprintln!("no audio files found under {}", dir.display()),
                            Err(e) => eprintln!("failed to load folder {}: {e}", dir.display()),
                        }
                    })
                    .expect("failed to spawn library scan thread");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_track_list,
            get_library,
            get_api_status,
            get_download_progress,
            search_music,
            search_suggestions,
            get_home_cached,
            refresh_home,
            open_album,
            open_artist,
            set_search_filter,
            play_local_path,
            play_local_file,
            get_user_playlists,
            get_playlist_tracks,
            get_liked_songs,
            yt_login_start,
            yt_open_login_page,
            yt_login_advanced,
            logout,
            play_video,
            play_queue,
            prefetch_tracks,
            get_queue_info,
            get_queue_version,
            get_settings,
            set_audio_quality,
            set_streaming_account,
            set_cache_limit,
            set_normalize_level,
            set_volume_curve,
            clear_cache,
            cycle_repeat,
            set_speed,
            get_lyrics,
            recommend_tracks,
            queue_add,
            queue_remove,
            queue_clear,
            get_history,
            toggle_favorite,
            is_favorite,
            open_favorites,
            set_sleep_timer,
            get_sleep_timer,
            open_folder,
            open_playlist,
            play_track,
            toggle_pause,
            next_track,
            prev_track,
            stop,
            seek,
            set_volume,
            toggle_mute
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(move |_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }
        ) {
            shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
            // Last chance for a volume change made in the final 400 ms.
            let state = _handle.state::<AppState>();
            save_settings(&state.settings.lock().clone());
            sidecar.kill();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::migrate_loudness_keys;

    #[test]
    fn api_token_is_32_os_bytes_as_hex() {
        let token = super::generate_api_token();
        assert_eq!(token.len(), 64, "32 bytes -> 64 hex chars");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, super::generate_api_token(), "must not repeat");
    }

    #[test]
    fn loudness_keys_follow_cache_layout() {
        let dir = crate::downloader::cache_dir();
        let legacy = dir.join("abc12345678.m4a").to_string_lossy().into_owned();
        let already = dir.join("abc12345678.high.m4a").to_string_lossy().into_owned();
        let outside = std::path::Path::new("C:/tmp/song.m4a")
            .to_string_lossy()
            .into_owned();
        let mut map = std::collections::HashMap::from([
            (legacy.clone(), 1.0f32),
            (already.clone(), 2.0f32),
            (outside.clone(), 3.0f32),
        ]);

        assert!(migrate_loudness_keys(&mut map));

        let migrated = dir
            .join("abc12345678.standard.m4a")
            .to_string_lossy()
            .into_owned();
        assert_eq!(map.get(&migrated), Some(&1.0));
        assert!(!map.contains_key(&legacy));
        assert_eq!(map.get(&already), Some(&2.0));
        assert_eq!(map.get(&outside), Some(&3.0));
    }

    use super::loudness_window;

    fn paths(count: usize) -> Vec<std::path::PathBuf> {
        (0..count)
            .map(|i| std::path::PathBuf::from(format!("{i}.mp3")))
            .collect()
    }

    #[test]
    fn loudness_scans_the_current_track_and_the_next_five() {
        let window = loudness_window(paths(50), 10);
        assert_eq!(window.len(), 6);
        assert_eq!(window[0], std::path::PathBuf::from("10.mp3"));
        assert_eq!(window[5], std::path::PathBuf::from("15.mp3"));
    }

    #[test]
    fn loudness_stops_at_the_end_of_a_playlist() {
        let window = loudness_window(paths(8), 6);
        assert_eq!(window.len(), 2);
    }

    #[test]
    fn loudness_ignores_a_start_past_the_end() {
        assert!(loudness_window(paths(3), 9).is_empty());
        assert!(loudness_window(Vec::new(), 0).is_empty());
    }
}
