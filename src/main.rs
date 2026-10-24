#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth_window;
mod downloader;
mod engine;
mod fsutil;
mod grpc;
mod net;
mod playlist;
mod secrets;
mod sidecar;

use downloader::ProgressHandle;
use engine::{Cmd, EngineHandle, Snapshot};
use playlist::{Playlist, Track};
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

#[derive(Serialize, Clone)]
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
struct QueueItem {
    id: String,
    title: String,
    artist: String,
    thumbnail: String,
    #[serde(default)]
    duration_secs: f64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    #[serde(default = "default_quality")]
    audio_quality: String,
    #[serde(default = "default_normalize_level")]
    normalize_level: String,
    #[serde(default = "default_true")]
    volume_curve: bool,
    /// Off by default: using the account with yt-dlp can get it rate-limited.
    #[serde(default)]
    streaming_account: bool,
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

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio_quality: default_quality(),
            normalize_level: default_normalize_level(),
            volume_curve: true,
            streaming_account: false,
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
fn spawn_loudness_scan(
    paths: Vec<std::path::PathBuf>,
    cache: LoudnessCache,
    engine: EngineHandle,
    target: f64,
) {
    use std::collections::VecDeque;

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

fn record_history(state: &AppState, item: &HistoryItem) {
    let mut history = state.history.lock();
    history.retain(|h| h.id != item.id);
    history.insert(0, item.clone());
    history.truncate(100);
    let snapshot = history.clone();
    drop(history);
    save_json(&history_file(), &snapshot);
}

#[derive(Default)]
struct QueueState {
    items: Vec<QueueItem>,
    current: usize,
    active: bool,
    manual_stop: bool,
    prev_playing: bool,
    version: u64,
}

type FetchGuard = Arc<Mutex<std::collections::HashSet<String>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    fn as_str(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct HistoryItem {
    id: String,
    title: String,
    artist: String,
    thumbnail: String,
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
    queue: Mutex<QueueState>,
    repeat: Mutex<RepeatMode>,
    fetching: FetchGuard,
    history: Mutex<Vec<HistoryItem>>,
    favorites: Mutex<Vec<FavoriteTrack>>,
    sleep_until: Mutex<Option<std::time::Instant>>,
    loudness: LoudnessCache,
    /// When the session was last validated (for the "check before loading"
    /// rule). `None` = never checked in this process.
    session_checked: Mutex<Option<std::time::Instant>>,
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
    let infos: Vec<engine::TrackInfo> = pl.tracks.iter().map(track_to_info).collect();
    *state.local_current.lock() = Some(infos.clone());
    state.queue.lock().active = false;
    let mut pl = pl;
    let normalize = normalize_enabled(&state.settings.lock());
    apply_gains(&mut pl, &state.loudness, normalize);
    let scan_paths: Vec<std::path::PathBuf> = pl.tracks.iter().map(|t| t.path.clone()).collect();
    state.engine.lock().send(Cmd::OpenPlaylist(pl));
    spawn_loudness_scan(
        scan_paths,
        state.loudness.clone(),
        state.engine.lock().clone(),
        target_rms(&state.settings.lock().normalize_level).unwrap_or(0.12),
    );
    Ok(infos)
}

#[tauri::command]
fn play_track(index: usize, state: tauri::State<AppState>) {
    // A local track replaces any active YouTube queue: otherwise the queue
    // watcher would hijack playback when the track ends.
    {
        let mut q = state.queue.lock();
        if q.active {
            q.active = false;
            q.version += 1;
        }
    }
    // Prioritize loudness scanning of the clicked track.
    if normalize_enabled(&state.settings.lock())
        && let Some(track) = state
            .local_current
            .lock()
            .as_ref()
            .and_then(|tracks| tracks.get(index))
        && let Some(path) = track.path.as_ref()
    {
        spawn_loudness_scan(
            vec![std::path::PathBuf::from(path)],
            state.loudness.clone(),
            state.engine.lock().clone(),
            target_rms(&state.settings.lock().normalize_level).unwrap_or(0.12),
        );
    }
    state.engine.lock().send(Cmd::PlayTrack(index));
}

#[tauri::command]
fn toggle_pause(state: tauri::State<AppState>) {
    state.engine.lock().send(Cmd::TogglePause);
}

enum QueueAction {
    EngineCommand(Cmd),
    PlayCurrent,
}

fn queue_next(state: &AppState) -> QueueAction {
    let mut q = state.queue.lock();
    if !q.active {
        return QueueAction::EngineCommand(Cmd::Next);
    }
    if q.current + 1 >= q.items.len() {
        q.manual_stop = true;
        q.active = false;
        q.version += 1;
        return QueueAction::EngineCommand(Cmd::Stop);
    }
    q.current += 1;
    q.manual_stop = false;
    q.version += 1;
    QueueAction::PlayCurrent
}

fn queue_prev(state: &AppState) -> QueueAction {
    let mut q = state.queue.lock();
    if !q.active {
        return QueueAction::EngineCommand(Cmd::Prev);
    }
    let elapsed = state
        .engine
        .lock()
        .snapshot()
        .map(|s| s.elapsed)
        .unwrap_or(0.0);
    if elapsed > 3.0 && q.current > 0 {
        q.current -= 1;
        q.version += 1;
    }
    q.manual_stop = false;
    QueueAction::PlayCurrent
}

#[tauri::command]
async fn next_track(state: tauri::State<'_, AppState>) -> Result<(), String> {
    match queue_next(&state) {
        QueueAction::EngineCommand(cmd) => {
            state.engine.lock().send(cmd);
            Ok(())
        }
        QueueAction::PlayCurrent => play_current(&state).await,
    }
}

#[tauri::command]
async fn prev_track(state: tauri::State<'_, AppState>) -> Result<(), String> {
    match queue_prev(&state) {
        QueueAction::EngineCommand(cmd) => {
            state.engine.lock().send(cmd);
            Ok(())
        }
        QueueAction::PlayCurrent => play_current(&state).await,
    }
}

#[tauri::command]
fn stop(state: tauri::State<AppState>) {
    {
        let mut q = state.queue.lock();
        if q.active {
            q.manual_stop = true;
        }
    }
    state.engine.lock().send(Cmd::Stop);
}

#[tauri::command]
fn seek(secs: f64, state: tauri::State<AppState>) {
    state.engine.lock().send(Cmd::Seek(secs));
}

#[tauri::command]
fn set_volume(volume: f32, state: tauri::State<AppState>) {
    state.engine.lock().send(Cmd::SetVolume(volume));
}

#[tauri::command]
fn get_api_status(state: tauri::State<AppState>) -> ApiStatus {
    state.api_status.read().clone()
}

#[tauri::command]
fn get_download_progress(state: tauri::State<AppState>) -> downloader::DownloadProgress {
    state.download.snapshot()
}

#[tauri::command]
async fn search_youtube(
    query: String,
    limit: Option<i32>,
    music: Option<bool>,
    _state: tauri::State<'_, AppState>,
) -> Result<Vec<SearchTrack>, String> {
    let tracks = grpc::search(
        &query,
        limit.unwrap_or(20).clamp(1, 100),
        music.unwrap_or(false),
    )
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
async fn get_liked_songs(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SearchTrack>, String> {
    ensure_session_fresh(&app, &state).await;
    let resp = grpc::get_liked_songs().await.map_err(|e| e.to_string())?;
    if !resp.error.is_empty() {
        return Err(resp.error);
    }
    Ok(resp
        .tracks
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

/// Advanced fallback (3.5): paste request headers from a real browser. Parsed
/// by the sidecar with the same normaliser the old flow used, then stored via
/// the encrypted bundle — no cookie file anywhere.
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
) -> Result<Vec<SearchTrack>, String> {
    ensure_session_fresh(&app, &state).await;
    let resp = grpc::get_playlist_tracks(&playlist_id)
        .await
        .map_err(|e| e.to_string())?;
    if !resp.error.is_empty() {
        return Err(resp.error);
    }
    Ok(resp
        .tracks
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
        if let Err(e) = std::fs::write(&meta, &title) {
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
    let item = {
        let q = state.queue.lock();
        q.items
            .get(q.current)
            .cloned()
            .ok_or_else(|| "queue is empty".to_string())?
    };

    let quality = current_quality(state);
    let dest = downloader::cached_file(&item.id, &quality);
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

    let playlist = Playlist {
        name: item.title.clone(),
        tracks: vec![Track {
            path: dest.clone(),
            title: item.title.clone(),
            artist: item.artist.clone(),
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
    record_history(
        state,
        &HistoryItem {
            id: item.id.clone(),
            title: item.title.clone(),
            artist: item.artist.clone(),
            thumbnail: item.thumbnail.clone(),
        },
    );
    if normalize_enabled(&state.settings.lock()) {
        spawn_loudness_scan(
            vec![dest],
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
    let _ = std::fs::write(&meta, &item.title);
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
        let _ = std::fs::write(&meta, &item.title);
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
        active: q.active,
        current: q.current,
        items: q.items.clone(),
        repeat,
        version: q.version,
    }
}

/// Cheap version counter: the UI polls it and fetches the full queue only
/// when it changed (avoids serializing thousands of items every 250 ms).
#[tauri::command]
fn get_queue_version(state: tauri::State<AppState>) -> u64 {
    state.queue.lock().version
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
            if q.active
                && let Some(item) = q.items.get(q.current)
            {
                paths.push(downloader::cached_file(&item.id, &current_quality(&state)));
            }
        }
        spawn_loudness_scan(
            paths,
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
    let start_fresh;
    {
        let q = state.queue.lock();
        start_fresh = !q.active || q.items.is_empty();
    }
    if start_fresh {
        return play_queue(items, 0, state).await;
    }
    {
        let mut q = state.queue.lock();
        let at = if play_next.unwrap_or(false) {
            (q.current + 1).min(q.items.len())
        } else {
            q.items.len()
        };
        for (i, item) in items.into_iter().enumerate() {
            q.items.insert(at + i, item);
        }
        q.version += 1;
    }
    Ok(())
}

#[tauri::command]
fn queue_remove(index: usize, state: tauri::State<AppState>) -> Result<(), String> {
    let mut q = state.queue.lock();
    if index >= q.items.len() {
        return Err("index out of range".to_string());
    }
    if q.items.len() == 1 {
        q.active = false;
        q.items.clear();
        state.engine.lock().send(Cmd::Stop);
        return Ok(());
    }
    q.items.remove(index);
    if q.active && index < q.current {
        q.current -= 1;
    } else if q.active && q.current >= q.items.len() {
        q.current = q.items.len() - 1;
    }
    q.version += 1;
    Ok(())
}

#[tauri::command]
fn queue_clear(state: tauri::State<AppState>) {
    let mut q = state.queue.lock();
    q.active = false;
    q.items.clear();
    q.manual_stop = true;
    q.version += 1;
    drop(q);
    state.engine.lock().send(Cmd::Stop);
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
    state.queue.lock().active = false;
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
        app_state.queue.lock().manual_stop = true;
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
    let start = start.min(items.len() - 1);
    {
        let mut q = state.queue.lock();
        q.items = items;
        q.current = start;
        q.active = true;
        q.manual_stop = false;
        q.prev_playing = false;
        q.version += 1;
    }
    play_current(&state).await?;

    // Preload the next two tracks in the background so advancing is instant.
    let next_items = {
        let q = state.queue.lock();
        q.items
            .iter()
            .skip(q.current + 1)
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
    };
    spawn_prefetch(
        next_items,
        state.fetching.clone(),
        current_quality(&state),
        streaming_account(&state),
    );
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

fn spawn_queue_watcher(handle: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("queue-watcher".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build queue watcher runtime");
            rt.block_on(async move {
                let mut prefetched_for: Option<usize> = None;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    let state = handle.state::<AppState>();

                    let snapshot = state.engine.lock().snapshot();
                    let playing_now = snapshot
                        .as_ref()
                        .map(|s| s.status == "playing")
                        .unwrap_or(false);
                    let stopped_now = snapshot
                        .as_ref()
                        .map(|s| s.status == "stopped")
                        .unwrap_or(false);

                    let mut advance = false;
                    let mut prefetch: Vec<QueueItem> = Vec::new();
                    {
                        let mut q = state.queue.lock();
                        if !q.active || q.manual_stop {
                            q.prev_playing = false;
                            prefetched_for = None;
                            continue;
                        }
                        let should_advance = q.prev_playing && stopped_now;
                        if should_advance {
                            let repeat = *state.repeat.lock();
                            match repeat {
                                RepeatMode::One => {
                                    q.prev_playing = false;
                                    advance = true; // replay the same track
                                }
                                RepeatMode::All => {
                                    q.current = (q.current + 1) % q.items.len();
                                    q.prev_playing = false;
                                    q.version += 1;
                                    advance = true;
                                }
                                RepeatMode::Off => {
                                    if q.current + 1 < q.items.len() {
                                        q.current += 1;
                                        q.prev_playing = false;
                                        q.version += 1;
                                        advance = true;
                                    } else {
                                        q.active = false;
                                        q.prev_playing = false;
                                        q.version += 1;
                                    }
                                }
                            }
                            if advance {
                                prefetched_for = None;
                            }
                        } else {
                            q.prev_playing = playing_now;
                        }

                        // Once the current track starts playing, prefetch the
                        // next two in the background.
                        if playing_now && prefetched_for != Some(q.current) {
                            prefetched_for = Some(q.current);
                            let repeat = *state.repeat.lock();
                            if repeat != RepeatMode::One {
                                prefetch = q
                                    .items
                                    .iter()
                                    .skip(q.current + 1)
                                    .take(2)
                                    .cloned()
                                    .collect();
                            }
                        }
                    }

                    if advance && let Err(e) = play_current(&state).await {
                        eprintln!("[queue] play_current failed: {e}");
                    }
                    if !prefetch.is_empty() {
                        spawn_prefetch(
                            prefetch,
                            state.fetching.clone(),
                            current_quality(&state),
                            streaming_account(&state),
                        );
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
        queue: Mutex::new(QueueState::default()),
        repeat: Mutex::new(RepeatMode::Off),
        fetching: Arc::new(Mutex::new(std::collections::HashSet::new())),
        history: Mutex::new(load_json(&history_file())),
        favorites: Mutex::new(load_json(&favorites_file())),
        sleep_until: Mutex::new(None),
        loudness: load_loudness(),
        session_checked: Mutex::new(None),
    };
    let api_status = app_state.api_status.clone();
    {
        let curve = app_state.settings.lock().volume_curve;
        app_state
            .engine
            .lock()
            .send(Cmd::SetVolumeCurve(curve));
    }
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sidecar_shutdown = shutdown.clone();
    let sidecar = sidecar::spawn_and_supervise(api_status, sidecar_shutdown, api_token);

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .setup(move |app| {
            spawn_queue_watcher(app.handle().clone());
            spawn_session_bootstrap(app.handle().clone());
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
            search_youtube,
            get_user_playlists,
            get_playlist_tracks,
            get_liked_songs,
            yt_login_start,
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
            set_volume
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(move |_handle, event| {
        if matches!(
            event,
            tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. }
        ) {
            shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
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
}
