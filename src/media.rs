//! What Windows shows about the app: the volume overlay, the media keys and the
//! task-switcher. One thread keeps it in step with the engine.

use crate::AppState;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::ffi::c_void;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

/// What the overlay needs to know about the current track.
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    /// `file://…` for a local cover, `https://…` for a cached thumbnail.
    pub cover: String,
}

enum Message {
    Metadata(Box<NowPlaying>),
    State { playing: bool, position: f64, duration: f64 },
}

/// The SMTC session. Created on the first tick, when the window and its HWND
/// exist; `None` on a platform or a state where the overlay is not available.
pub struct MediaSession {
    tx: Sender<Message>,
    _thread: std::thread::JoinHandle<()>,
}

impl MediaSession {
    pub fn start(app: AppHandle) -> Option<Self> {
        let Some(window) = app.get_webview_window("main") else {
            eprintln!("[media] no main window yet");
            return None;
        };
        // A raw pointer is not Send; the integer is, and the thread turns
        // it back into a handle before touching it.
        let hwnd = match window.hwnd() {
            Ok(hwnd) => hwnd.0 as isize,
            Err(e) => {
                eprintln!("[media] no window handle: {e}");
                return None;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel::<Message>();

        let thread = std::thread::Builder::new()
            .name("media-controls".to_string())
            .spawn(move || run(hwnd, rx, app.clone()))
            .ok()?;

        Some(Self { tx, _thread: thread })
    }

    pub fn set_metadata(&self, meta: NowPlaying) {
        let _ = self.tx.send(Message::Metadata(Box::new(meta)));
    }

    pub fn set_state(&self, playing: bool, position: f64, duration: f64) {
        let _ = self.tx.send(Message::State {
            playing,
            position,
            duration,
        });
    }
}

fn run(hwnd: isize, rx: Receiver<Message>, app: AppHandle) {
    let hwnd = hwnd as *mut c_void;
    let config = PlatformConfig {
        display_name: "Fasttube",
        dbus_name: "fasttube",
        hwnd: Some(hwnd),
    };
    let mut controls = match MediaControls::new(config) {
        Ok(controls) => controls,
        Err(e) => {
            eprintln!("[media] the system media controls are unavailable: {e}");
            return;
        }
    };
    let emitter = app.clone();
    let attached = controls.attach(move |event| {
        // The commands live in the frontend: it owns the queue and the play
        // state, so the keys take the same path as a click.
        let action = match event {
            MediaControlEvent::Play => Some("play".to_string()),
            MediaControlEvent::Pause => Some("pause".to_string()),
            MediaControlEvent::Toggle => Some("toggle".to_string()),
            MediaControlEvent::Next => Some("next".to_string()),
            MediaControlEvent::Previous => Some("prev".to_string()),
            MediaControlEvent::Stop => Some("stop".to_string()),
            MediaControlEvent::SetPosition(MediaPosition(position)) => {
                Some(format!("seek:{}", position.as_secs_f64()))
            }
            MediaControlEvent::Seek(direction) | MediaControlEvent::SeekBy(direction, _) => {
                Some(match direction {
                    souvlaki::SeekDirection::Forward => "seek-by:5".to_string(),
                    souvlaki::SeekDirection::Backward => "seek-by:-5".to_string(),
                })
            }
            _ => None,
        };
        if let Some(action) = action {
            eprintln!("[media] {action}");
            let _ = emitter.emit("media-control", action);
        }
    });
    if let Err(e) = attached {
        eprintln!("[media] cannot attach to the media keys: {e}");
        return;
    }

    let mut last: Option<MediaPlayback> = None;
    while let Ok(message) = rx.recv_timeout(Duration::from_secs(5)) {
        let result = match message {
            Message::Metadata(meta) => {
                let cover = if meta.cover.is_empty() {
                    None
                } else {
                    Some(meta.cover.as_str())
                };
                controls.set_metadata(MediaMetadata {
                    title: Some(meta.title.as_str()),
                    artist: Some(meta.artist.as_str()),
                    album: Some(meta.album.as_str()),
                    cover_url: cover,
                    duration: (meta.duration > 0.0)
                        .then(|| Duration::from_secs_f64(meta.duration)),
                })
            }
            Message::State {
                playing,
                position,
                duration,
            } => {
                let progress = (position > 0.0).then(|| MediaPosition(Duration::from_secs_f64(position)));
                let playback = if playing {
                    MediaPlayback::Playing { progress }
                } else if duration > 0.0 || position > 0.0 {
                    MediaPlayback::Paused { progress }
                } else {
                    MediaPlayback::Stopped
                };
                let same = match (&last, &playback) {
                    (Some(MediaPlayback::Playing { progress: a }), MediaPlayback::Playing { progress: b }) => a == b,
                    (Some(a), b) => a == b,
                    (None, _) => false,
                };
                if same {
                    continue;
                }
                last = Some(playback.clone());
                controls.set_playback(playback)
            }
        };
        if let Err(e) = result {
            eprintln!("[media] cannot update the media controls: {e}");
        }
    }
}

/// A `file://` URL the shell accepts: forward slashes, spaces escaped.
pub fn file_url(path: &std::path::Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    let mut out = String::from("file://");
    for ch in text.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            c => out.push(c),
        }
    }
    // `file://C:/…` → `file:///C:/…`
    if out.starts_with("file:///") {
        out
    } else {
        out.replacen("file://", "file:///", 1)
    }
}

/// Title of the window: the track while it plays, the app name otherwise.
pub fn window_title(app: &AppHandle, title: Option<&str>, artist: Option<&str>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let text = match (title, artist) {
        (Some(t), Some(a)) if !a.is_empty() => format!("{a} — {t}"),
        (Some(t), _) if !t.is_empty() => t.to_string(),
        _ => "Fasttube".to_string(),
    };
    let _ = window.set_title(&text);
}

/// The overlay has no queue of its own: this thread feeds it from the engine.
pub fn spawn_ticker(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("media-ticker".to_string())
        .spawn(move || {
            let mut session: Option<MediaSession> = None;
            let mut announced = false;
            let mut last_key = String::new();
            let mut last_state = (false, 0i64);
            loop {
                let state = app.state::<AppState>();
                let visible = app
                    .get_webview_window("main")
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false);
                let interval = if visible { 1_000 } else { 3_000 };
                std::thread::sleep(Duration::from_millis(interval));

                let snapshot = state.engine.lock().snapshot();
                let item = state.queue.lock().current_item();

                let (title, artist, album) = match (&item, &snapshot) {
                    (Some(item), _) => (item.title.clone(), item.artist.clone(), String::new()),
                    (None, Some(snap)) => match &snap.track {
                        Some(track) => (track.title.clone(), track.artist.clone(), String::new()),
                        None => (String::new(), String::new(), String::new()),
                    },
                    _ => (String::new(), String::new(), String::new()),
                };
                let playing = snapshot
                    .as_ref()
                    .map(|s| s.status == "playing")
                    .unwrap_or(false);
                let position = snapshot.as_ref().map(|s| s.elapsed).unwrap_or(0.0);
                let duration = snapshot.as_ref().map(|s| s.total).unwrap_or(0.0);

                let cover = match (&item, &snapshot) {
                    (Some(item), _) if !item.thumbnail.is_empty() => item.thumbnail.clone(),
                    (_, Some(snap)) => snap
                        .track
                        .as_ref()
                        .and_then(|t| t.cover.as_deref())
                        .map(|p| file_url(std::path::Path::new(p)))
                        .unwrap_or_default(),
                    _ => String::new(),
                };

                // The session exists from the first tick, not from the first
                // track: a media key pressed before anything plays must still
                // reach the app.
                if session.is_none() {
                    session = MediaSession::start(app.clone());
                    if !announced {
                        announced = true;
                        eprintln!(
                            "[media] session: {}",
                            if session.is_some() { "ready" } else { "unavailable" }
                        );
                    }
                }
                let key = format!("{title}\u{1}{artist}\u{1}{cover}");
                if let Some(session) = session.as_ref() {
                    if key != last_key {
                        last_key = key;
                        session.set_metadata(NowPlaying {
                            title: title.clone(),
                            artist: artist.clone(),
                            album,
                            duration,
                            cover,
                        });
                        crate::integration::notify_track_change(&app, &title, &artist);
                    }
                    let now_state = (playing, (position * 10.0) as i64);
                    if now_state != last_state {
                        last_state = now_state;
                        session.set_state(playing, position, duration);
                    }
                }
                window_title(
                    &app,
                    (!title.is_empty()).then_some(title.as_str()),
                    (!artist.is_empty()).then_some(artist.as_str()),
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_path_becomes_a_file_url() {
        assert_eq!(
            file_url(std::path::Path::new(r"C:\music\a b.mp3")),
            "file:///C:/music/a%20b.mp3"
        );
        assert_eq!(
            file_url(std::path::Path::new("/home/x/a.flac")),
            "file:///home/x/a.flac"
        );
    }
}
