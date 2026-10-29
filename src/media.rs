//! What Windows shows about the app: the volume overlay, the media keys and the
//! task-switcher. One thread keeps it in step with the engine.

use crate::AppState;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::ffi::c_void;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// What the overlay needs to know about the current track.
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    /// `file://…` for a local cover, `https://…` for a cached thumbnail.
    pub cover: String,
}

/// The two calls the presenter makes on the system session. A trait, so the
/// stream logic runs in a test without Windows and without a real session.
trait Controls {
    fn put_metadata(&mut self, meta: MediaMetadata<'_>) -> Result<(), souvlaki::Error>;
    fn put_playback(&mut self, playback: MediaPlayback) -> Result<(), souvlaki::Error>;
}

impl Controls for MediaControls {
    fn put_metadata(&mut self, meta: MediaMetadata<'_>) -> Result<(), souvlaki::Error> {
        self.set_metadata(meta)
    }

    fn put_playback(&mut self, playback: MediaPlayback) -> Result<(), souvlaki::Error> {
        self.set_playback(playback)
    }
}

enum Message {
    Metadata(Box<NowPlaying>),
    State {
        playing: bool,
        position: f64,
        duration: f64,
        /// Something is on screen even if it is not playing: the last track
        /// after a start, for instance. Without it the session would report
        /// Stopped and the overlay would show nothing at all.
        has_track: bool,
    },
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

    pub fn set_state(&self, playing: bool, position: f64, duration: f64, has_track: bool) {
        let _ = self.tx.send(Message::State {
            playing,
            position,
            duration,
            has_track,
        });
    }
}

/// Keeps the system session in step with the messages it is fed. The last
/// published playback state is remembered so a position update that changes
/// nothing does not reach the shell.
struct Presenter<C: Controls> {
    controls: C,
    last: Option<MediaPlayback>,
}

impl<C: Controls> Presenter<C> {
    fn new(controls: C) -> Self {
        Self {
            controls,
            last: None,
        }
    }

    fn apply(&mut self, message: Message) {
        let result = match message {
            Message::Metadata(meta) => {
                let cover = if meta.cover.is_empty() {
                    None
                } else {
                    Some(meta.cover.as_str())
                };
                self.controls.put_metadata(MediaMetadata {
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
                has_track,
            } => {
                let progress = (position > 0.0)
                    .then(|| MediaPosition(Duration::from_secs_f64(position)));
                let playback = if playing {
                    MediaPlayback::Playing { progress }
                } else if has_track || duration > 0.0 || position > 0.0 {
                    MediaPlayback::Paused { progress }
                } else {
                    MediaPlayback::Stopped
                };
                let same = match (&self.last, &playback) {
                    (
                        Some(MediaPlayback::Playing { progress: a }),
                        MediaPlayback::Playing { progress: b },
                    ) => a == b,
                    (Some(a), b) => a == b,
                    (None, _) => false,
                };
                if same {
                    return;
                }
                self.last = Some(playback.clone());
                self.controls.put_playback(playback)
            }
        };
        if let Err(e) = result {
            eprintln!("[media] cannot update the media controls: {e}");
        }
    }
}

/// Reads the queue until the channel closes. An idle period is not the end:
/// this thread is the app's only link to the media keys and lives as long as
/// the app does, so a timeout simply returns to waiting.
fn pump<C: Controls>(rx: &Receiver<Message>, presenter: &mut Presenter<C>, idle: Duration) {
    loop {
        match rx.recv_timeout(idle) {
            Ok(message) => presenter.apply(message),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
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
            // Straight into the same functions the commands call: the queue
            // and the play state live in Rust, and a key must not depend on
            // the page being awake.
            crate::media_action(&emitter, &action);
        }
    });
    if let Err(e) = attached {
        eprintln!("[media] cannot attach to the media keys: {e}");
        return;
    }

    let mut presenter = Presenter::new(controls);
    pump(&rx, &mut presenter, Duration::from_secs(5));
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
            let mut last_title = String::new();
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
                // Nothing loaded yet (a fresh start): show what would continue,
                // so the overlay is not empty and the session is not "Stopped".
                let at_rest = state.settings.lock().resume.clone();

                let mut title = String::new();
                let mut artist = String::new();
                let mut cover = String::new();
                let mut position = snapshot.as_ref().map(|s| s.elapsed).unwrap_or(0.0);
                let mut duration = snapshot.as_ref().map(|s| s.total).unwrap_or(0.0);
                let playing = snapshot
                    .as_ref()
                    .map(|s| s.status == "playing")
                    .unwrap_or(false);

                if let Some(item) = &item {
                    title = item.title.clone();
                    artist = item.artist.clone();
                    cover = item.thumbnail.clone();
                } else if let Some(snap) = &snapshot {
                    if let Some(track) = &snap.track {
                        title = track.title.clone();
                        artist = track.artist.clone();
                        cover = track
                            .cover
                            .as_deref()
                            .map(|p| file_url(std::path::Path::new(p)))
                            .unwrap_or_default();
                    }
                }
                if title.is_empty() {
                    if let Some(resume) = &at_rest {
                        title = resume.title.clone();
                        artist = resume.artist.clone();
                        cover = resume.thumbnail.clone();
                        position = resume.position_secs;
                        duration = 0.0;
                    }
                }

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
                // The duration belongs in the key: the timeline end is part of
                // the metadata, and a track that starts before its length is
                // known would otherwise never publish it.
                let key = format!(
                    "{title}\u{1}{artist}\u{1}{cover}\u{1}{}",
                    duration.round() as i64
                );
                if let Some(session) = session.as_ref() {
                    if key != last_key {
                        last_key = key;
                        session.set_metadata(NowPlaying {
                            title: title.clone(),
                            artist: artist.clone(),
                            album: String::new(),
                            duration,
                            cover,
                        });
                        crate::integration::notify_track_change(&app, &title, &artist);
                    }
                    let now_state = (playing, (position * 10.0) as i64);
                    if now_state != last_state {
                        last_state = now_state;
                        session.set_state(playing, position, duration, !title.is_empty());
                    }
                }
                // A window title is a Win32 call per tick for nothing: only a
                // real change is worth it. The separator keeps "a" + "bc" from
                // looking like "ab" + "c".
                let wanted = format!("{artist}\u{1}{title}");
                if wanted != last_title {
                    last_title = wanted;
                    window_title(
                        &app,
                        (!title.is_empty()).then_some(title.as_str()),
                        (!artist.is_empty()).then_some(artist.as_str()),
                    );
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the presenter pushed, so a test can see which messages
    /// survived an idle period.
    #[derive(Default)]
    struct Recorder {
        titles: Vec<String>,
        playback: Vec<String>,
    }

    impl Controls for Recorder {
        fn put_metadata(&mut self, meta: MediaMetadata<'_>) -> Result<(), souvlaki::Error> {
            self.titles.push(meta.title.unwrap_or_default().to_string());
            Ok(())
        }

        fn put_playback(&mut self, playback: MediaPlayback) -> Result<(), souvlaki::Error> {
            self.playback.push(match playback {
                MediaPlayback::Playing { .. } => "playing".to_string(),
                MediaPlayback::Paused { .. } => "paused".to_string(),
                MediaPlayback::Stopped => "stopped".to_string(),
            });
            Ok(())
        }
    }

    fn track(title: &str) -> Message {
        Message::Metadata(Box::new(NowPlaying {
            title: title.to_string(),
            artist: "tone".to_string(),
            album: String::new(),
            duration: 2.0,
            cover: String::new(),
        }))
    }

    #[test]
    fn an_idle_period_does_not_end_the_stream() {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut presenter = Presenter::new(Recorder::default());
            pump(&rx, &mut presenter, Duration::from_millis(20));
            presenter.controls
        });

        // Several idle periods go by with nothing to publish.
        std::thread::sleep(Duration::from_millis(90));
        tx.send(track("first")).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        tx.send(Message::State {
            playing: true,
            position: 1.0,
            duration: 2.0,
            has_track: true,
        })
        .unwrap();
        drop(tx);

        let recorder = worker.join().unwrap();
        assert_eq!(
            recorder.titles,
            vec!["first".to_string()],
            "the message after the idle periods was dropped"
        );
        assert_eq!(recorder.playback, vec!["playing".to_string()]);
    }

    #[test]
    fn the_same_playback_state_is_not_pushed_twice() {
        let mut presenter = Presenter::new(Recorder::default());
        presenter.apply(Message::State {
            playing: true,
            position: 1.0,
            duration: 2.0,
            has_track: true,
        });
        presenter.apply(Message::State {
            playing: true,
            position: 1.0,
            duration: 2.0,
            has_track: true,
        });
        presenter.apply(Message::State {
            playing: false,
            position: 1.0,
            duration: 2.0,
            has_track: true,
        });
        assert_eq!(
            presenter.controls.playback,
            vec!["playing".to_string(), "paused".to_string()]
        );
    }

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
