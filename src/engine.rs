use crate::playlist::Playlist;
use rodio::{Decoder, DeviceSinkBuilder, Player, Source, source::Zero};
use serde::Serialize;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use std::time::{Duration, Instant};

const BUFFER_FRAMES: u32 = 2048;

pub enum Cmd {
    SetPlaylist(Playlist),
    OpenPlaylist(Playlist),
    PlayTrack(usize),
    TogglePause,
    Next,
    Prev,
    Stop,
    Seek(f64),
    SetVolume(f32),
    SetSpeed(f32),
    /// Live loudness update: (path, gain factor).
    SetGain(String, f32),
    /// Drop all stored gains (used when the normalization level changes).
    ClearGains,
    /// Toggle the perceptual (exponential) volume curve.
    SetVolumeCurve(bool),
}

/// What the engine tells the rest of the app about playback.
///
/// The queue used to poll the snapshot every 250 ms to notice that a track
/// had ended; these events carry the same information the moment it happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A track started playing (engine playlist index [`Event::TrackStarted`]).
    TrackStarted,
    /// The current track played to its end on its own (not stopped by us).
    TrackEnded,
}

#[derive(Serialize, Clone)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub duration: f64,
    pub cover: Option<String>,
    pub path: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct Snapshot {
    pub playlist_name: String,
    pub playlist_len: usize,
    pub idx: usize,
    pub status: &'static str,
    pub elapsed: f64,
    pub total: f64,
    pub volume: f32,
    pub speed: f32,
    pub track: Option<TrackInfo>,
}

pub struct EngineHandle {
    tx: Sender<Cmd>,
    snapshot: Arc<RwLock<Option<Snapshot>>>,
    /// Taken once by whoever consumes [`Event`]s (the queue task).
    events: Arc<Mutex<Option<UnboundedReceiver<Event>>>>,
}

impl Clone for EngineHandle {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            snapshot: self.snapshot.clone(),
            events: self.events.clone(),
        }
    }
}

impl Default for EngineHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineHandle {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let snapshot = Arc::new(RwLock::new(None));
        let snap = snapshot.clone();
        let (events_tx, events_rx) = unbounded_channel();
        std::thread::Builder::new()
            .name("audio-engine".to_string())
            .spawn(move || run(rx, snap, events_tx))
            .expect("failed to spawn audio engine thread");
        Self {
            tx,
            snapshot,
            events: Arc::new(Mutex::new(Some(events_rx))),
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot.read().clone()
    }

    /// Hands the event stream to its single consumer. `None` when another
    /// part of the app already took it.
    pub fn take_events(&self) -> Option<UnboundedReceiver<Event>> {
        self.events.lock().take()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Playing,
    Paused,
    Stopped,
    Loading,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Playing => "playing",
            Status::Paused => "paused",
            Status::Stopped => "stopped",
            Status::Loading => "loading",
        }
    }
}

struct Engine {
    _device: rodio::MixerDeviceSink,
    player: Player,
    playlist: Playlist,
    idx: usize,
    total: Duration,
    volume: f32,
    speed: f32,
    gain: f32,
    target_gain: f32,
    curve: bool,
    status: Status,
    dirty: bool,
    /// Counter of successful starts, so the run loop can announce them.
    started: u64,
}

impl Engine {
    fn create() -> anyhow::Result<Engine> {
        let device = DeviceSinkBuilder::from_default_device()
            .and_then(|b| {
                b.with_buffer_size(cpal::BufferSize::Fixed(BUFFER_FRAMES))
                    .open_stream()
            })
            .or_else(|_| DeviceSinkBuilder::open_default_sink())
            .map_err(|e| anyhow::anyhow!("failed to open audio device: {e}"))?;

        let mut device = device;
        device.log_on_drop(false);
        let cfg = device.config();
        let mixer = device.mixer();
        mixer.add(Zero::new(cfg.channel_count(), cfg.sample_rate()));
        let player = Player::connect_new(mixer);

        let engine = Engine {
            _device: device,
            player,
            playlist: Playlist {
                name: String::new(),
                tracks: Vec::new(),
            },
            idx: 0,
            total: Duration::ZERO,
            volume: 0.8,
            speed: 1.0,
            gain: 1.0,
            target_gain: 1.0,
            curve: true,
            status: Status::Stopped,
            dirty: true,
            started: 0,
        };
        engine.apply_volume();
        engine.player.set_speed(engine.speed);
        Ok(engine)
    }

    fn set_playlist(&mut self, playlist: Playlist) {
        self.playlist = playlist;
        self.load(0);
    }

    /// Shows the playlist without starting playback: the track list is
    /// published and the first track's metadata is displayed, but the
    /// player stays stopped until a track is explicitly started.
    fn open_playlist(&mut self, playlist: Playlist) {
        self.playlist = playlist;
        self.idx = 0;
        self.player.stop();
        self.status = Status::Stopped;
        self.total = self
            .playlist
            .tracks
            .first()
            .and_then(|t| t.duration)
            .map(Duration::from_secs_f64)
            .unwrap_or(Duration::ZERO);
        self.dirty = true;
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            playlist_name: self.playlist.name.clone(),
            playlist_len: self.playlist.tracks.len(),
            idx: self.idx,
            status: self.status.as_str(),
            elapsed: self.player.get_pos().as_secs_f64(),
            total: self.total.as_secs_f64(),
            volume: self.volume,
            speed: self.speed,
            track: self.playlist.tracks.get(self.idx).map(|t| TrackInfo {
                title: t.title.clone(),
                artist: t.artist.clone(),
                duration: self.total.as_secs_f64(),
                cover: t.cover.as_ref().map(|p| p.to_string_lossy().into_owned()),
                path: Some(t.path.to_string_lossy().into_owned()),
            }),
        }
    }

    /// Starts streaming `tracks[idx]`: the file is decoded incrementally by
    /// the audio backend, so playback begins almost immediately and track
    /// switches no longer wait for a full in-memory decode.
    fn load(&mut self, idx: usize) {
        // Skip unreadable/corrupt tracks instead of getting stuck.
        let mut idx = idx;
        let mut skipped = 0;
        loop {
            if idx >= self.playlist.tracks.len() {
                if skipped == 0 {
                    return;
                }
                self.idx = self.playlist.tracks.len();
                self.total = Duration::ZERO;
                self.player.stop();
                self.status = Status::Stopped;
                self.dirty = true;
                return;
            }
            if self.try_start(idx) {
                return;
            }
            skipped += 1;
            idx += 1;
            if skipped > self.playlist.tracks.len() {
                return;
            }
        }
    }

    fn try_start(&mut self, idx: usize) -> bool {
        self.idx = idx;
        self.total = Duration::ZERO;
        self.player.stop();
        self.status = Status::Loading;
        self.dirty = true;

        let track = &self.playlist.tracks[idx];
        let file = match std::fs::File::open(&track.path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[eng] failed to open {}: {e}", track.path.display());
                return false;
            }
        };
        let decoder = match Decoder::try_from(file) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[eng] failed to decode {}: {e}", track.path.display());
                return false;
            }
        };
        self.total = decoder
            .total_duration()
            .or_else(|| track.duration.map(Duration::from_secs_f64))
            .unwrap_or(Duration::ZERO);
        self.gain = track.gain.unwrap_or(1.0);
        self.target_gain = self.gain;
        self.player.append(decoder);
        self.apply_volume();
        self.player.play();
        self.status = Status::Playing;
        self.started = self.started.wrapping_add(1);
        true
    }

    fn apply_volume(&self) {
        // Perceptual volume curve (exponent 3): human loudness perception is
        // logarithmic, so a linear slider feels wrong. With the curve the
        // slider position maps roughly linearly to perceived loudness.
        let user = if self.curve {
            self.volume.powf(3.0)
        } else {
            self.volume
        };
        // Safety net: gains are peak-limited elsewhere (never boosting into
        // clipping), the 4.0 cap only guards against corrupt cached values.
        self.player.set_volume((user * self.gain).clamp(0.0, 4.0));
    }

    fn set_volume_curve(&mut self, enabled: bool) {
        self.curve = enabled;
        // Force-apply the current volume under the new curve immediately.
        self.apply_volume();
        self.dirty = true;
    }

    /// Applies a freshly computed loudness gain. If it belongs to the
    /// currently playing track, the change is ramped in smoothly instead
    /// of jumping abruptly.
    fn set_gain(&mut self, path: &str, gain: f32) {
        for track in self.playlist.tracks.iter_mut() {
            if track.path.to_string_lossy() == path {
                track.gain = Some(gain);
            }
        }
        if let Some(track) = self.playlist.tracks.get(self.idx)
            && track.path.to_string_lossy() == path
        {
            self.target_gain = gain;
            self.dirty = true;
        }
    }

    fn clear_gains(&mut self) {
        for track in self.playlist.tracks.iter_mut() {
            track.gain = None;
        }
        // Ramp the current track back to unity; a fresh scan will soon
        // push the new level's gain.
        if self.idx < self.playlist.tracks.len() && !self.playlist.tracks.is_empty() {
            self.target_gain = 1.0;
            self.dirty = true;
        }
    }

    /// Smoothly eases the applied gain toward the target. Called from the
    /// run loop on every tick (~20 times per second). The ramp is fast
    /// (~250 ms) so the correction itself is barely noticeable.
    fn ramp_tick(&mut self) {
        let diff = self.target_gain - self.gain;
        if diff.abs() > 0.005 {
            self.gain += diff * 0.5;
            self.apply_volume();
            self.dirty = true;
        } else if self.gain != self.target_gain {
            self.gain = self.target_gain;
            self.apply_volume();
            self.dirty = true;
        }
    }

    fn advance(&mut self) {
        if self.idx + 1 < self.playlist.tracks.len() {
            self.load(self.idx + 1);
        } else {
            self.status = Status::Stopped;
            self.dirty = true;
        }
    }

    fn toggle_pause(&mut self) {
        match self.status {
            Status::Playing => {
                self.player.pause();
                self.status = Status::Paused;
                self.dirty = true;
            }
            Status::Paused => {
                self.player.play();
                self.status = Status::Playing;
                self.dirty = true;
            }
            Status::Stopped | Status::Loading => self.load(self.idx),
        }
    }

    fn next(&mut self) {
        let idx = if self.idx + 1 < self.playlist.tracks.len() {
            self.idx + 1
        } else {
            0
        };
        self.load(idx);
    }

    fn prev(&mut self) {
        let idx = if self.idx > 0 && self.player.get_pos() < Duration::from_secs(3) {
            self.idx - 1
        } else {
            self.idx
        };
        self.load(idx);
    }

    fn stop(&mut self) {
        self.player.stop();
        self.status = Status::Stopped;
        self.dirty = true;
    }

    fn seek(&mut self, secs: f64) {
        if self.status != Status::Stopped {
            let _ = self.player.try_seek(Duration::from_secs_f64(secs.max(0.0)));
        }
        self.dirty = true;
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply_volume();
        self.dirty = true;
    }

    fn set_speed(&mut self, speed: f32) {
        self.speed = speed.clamp(0.25, 3.0);
        self.player.set_speed(self.speed);
        self.dirty = true;
    }
}

fn run(
    rx: Receiver<Cmd>,
    snapshot: Arc<RwLock<Option<Snapshot>>>,
    events: UnboundedSender<Event>,
) {
    let mut engine: Option<Engine> = None;
    let mut last_publish = Instant::now();
    let mut pending_curve: Option<bool> = None;
    let mut seen_starts: u64 = 0;

    loop {
        if let Some(e) = engine.as_mut()
            && e.status == Status::Playing
            && e.player.empty()
        {
            // The source ran dry: this is the natural end of the track,
            // not a stop we asked for.
            let _ = events.send(Event::TrackEnded);
            e.advance();
        }

        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(cmd) => match cmd {
                Cmd::SetPlaylist(pl) => {
                    if engine.is_none() {
                        match Engine::create() {
                            Ok(mut e) => {
                                if let Some(c) = pending_curve.take() {
                                    e.curve = c;
                                    e.apply_volume();
                                }
                                engine = Some(e);
                            }
                            Err(err) => {
                                eprintln!("[eng] audio device error: {err}");
                            }
                        }
                    }
                    if let Some(e) = engine.as_mut() {
                        e.set_playlist(pl);
                    }
                }
                Cmd::OpenPlaylist(pl) => {
                    if engine.is_none() {
                        match Engine::create() {
                            Ok(mut e) => {
                                if let Some(c) = pending_curve.take() {
                                    e.curve = c;
                                    e.apply_volume();
                                }
                                engine = Some(e);
                            }
                            Err(err) => {
                                eprintln!("[eng] audio device error: {err}");
                            }
                        }
                    }
                    if let Some(e) = engine.as_mut() {
                        e.open_playlist(pl);
                    }
                }
                Cmd::PlayTrack(i) => {
                    if let Some(e) = engine.as_mut() {
                        e.load(i);
                    }
                }
                Cmd::TogglePause => {
                    if let Some(e) = engine.as_mut() {
                        e.toggle_pause();
                    }
                }
                Cmd::Next => {
                    if let Some(e) = engine.as_mut() {
                        e.next();
                    }
                }
                Cmd::Prev => {
                    if let Some(e) = engine.as_mut() {
                        e.prev();
                    }
                }
                Cmd::Stop => {
                    if let Some(e) = engine.as_mut() {
                        e.stop();
                    }
                }
                Cmd::Seek(secs) => {
                    if let Some(e) = engine.as_mut() {
                        e.seek(secs);
                    }
                }
                Cmd::SetVolume(v) => {
                    if let Some(e) = engine.as_mut() {
                        e.set_volume(v);
                    }
                }
                Cmd::SetSpeed(s) => {
                    if let Some(e) = engine.as_mut() {
                        e.set_speed(s);
                    }
                }
                Cmd::SetGain(path, gain) => {
                    if let Some(e) = engine.as_mut() {
                        e.set_gain(&path, gain);
                    }
                }
                Cmd::ClearGains => {
                    if let Some(e) = engine.as_mut() {
                        e.clear_gains();
                    }
                }
                Cmd::SetVolumeCurve(enabled) => {
                    if let Some(e) = engine.as_mut() {
                        e.set_volume_curve(enabled);
                    } else {
                        pending_curve = Some(enabled);
                    }
                }
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some(e) = engine.as_ref() {
            if e.started != seen_starts {
                seen_starts = e.started;
                let _ = events.send(Event::TrackStarted);
            }
            let due = e.dirty || last_publish.elapsed() >= Duration::from_millis(100);
            if due {
                *snapshot.write() = Some(e.snapshot());
                last_publish = Instant::now();
            }
        }
        if let Some(e) = engine.as_mut() {
            e.dirty = false;
            e.ramp_tick();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::Track;

    fn write_wav(path: &std::path::Path, seconds: f32, sample_rate: u32) {
        let n = (sample_rate as f32 * seconds) as usize;
        let mut data = Vec::with_capacity(44 + n * 2);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&((36 + n * 2) as u32).to_le_bytes());
        data.extend_from_slice(b"WAVE");
        data.extend_from_slice(b"fmt ");
        data.extend_from_slice(&16u32.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&1u16.to_le_bytes());
        data.extend_from_slice(&sample_rate.to_le_bytes());
        data.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        data.extend_from_slice(&2u16.to_le_bytes());
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&((n * 2) as u32).to_le_bytes());
        for i in 0..n {
            let v = (i as f32 * 440.0 * std::f32::consts::TAU / sample_rate as f32).sin();
            data.extend_from_slice(&((v * 4000.0) as i16).to_le_bytes());
        }
        std::fs::write(path, data).unwrap();
    }

    #[test]
    fn engine_plays_track() {
        let dir = std::env::temp_dir().join(format!("player_llm_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        write_wav(&wav, 1.0, 8000);

        let playlist = Playlist {
            name: "test".into(),
            tracks: vec![Track::new(wav, "tone".into())],
        };
        let handle = EngineHandle::new();
        handle.send(Cmd::SetPlaylist(playlist));
        handle.send(Cmd::SetVolume(0.0));

        let mut ok = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(snap) = handle.snapshot()
                && snap.status == "playing"
                && snap.elapsed > 0.0
            {
                ok = true;
                break;
            }
        }
        handle.send(Cmd::Stop);
        std::thread::sleep(Duration::from_millis(300));
        std::fs::remove_dir_all(&dir).ok();
        assert!(ok, "engine did not start playing");
    }

    #[test]
    fn engine_next_pause_seek_mid_play() {
        let dir = std::env::temp_dir().join(format!("player_llm_test2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav1 = dir.join("one.wav");
        let wav2 = dir.join("two.wav");
        write_wav(&wav1, 4.0, 8000);
        write_wav(&wav2, 4.0, 8000);

        let playlist = Playlist {
            name: "test".into(),
            tracks: vec![
                Track::new(wav1, "one".into()),
                Track::new(wav2, "two".into()),
            ],
        };
        let handle = EngineHandle::new();
        handle.send(Cmd::SetPlaylist(playlist));
        handle.send(Cmd::SetVolume(0.0));

        let wait = |f: &dyn Fn(&Snapshot) -> bool| {
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(snap) = handle.snapshot()
                    && f(&snap)
                {
                    return true;
                }
            }
            false
        };

        assert!(
            wait(&|s| s.status == "playing" && s.elapsed > 0.0),
            "not playing"
        );
        handle.send(Cmd::Next);
        assert!(
            wait(&|s| s.idx == 1 && s.status == "playing"),
            "next did not switch track"
        );
        handle.send(Cmd::TogglePause);
        assert!(wait(&|s| s.status == "paused"), "pause did not work");
        let paused_elapsed = handle.snapshot().map(|s| s.elapsed).unwrap_or(0.0);
        std::thread::sleep(Duration::from_millis(500));
        let still = handle.snapshot().map(|s| s.elapsed).unwrap_or(0.0);
        assert!(
            (still - paused_elapsed).abs() < 0.05,
            "elapsed moved while paused"
        );
        handle.send(Cmd::TogglePause);
        assert!(
            wait(&|s| s.status == "playing" && s.elapsed >= 1.0),
            "resume did not work"
        );
        handle.send(Cmd::Seek(1.5));
        assert!(
            wait(&|s| s.elapsed >= 1.5 && s.elapsed < 2.0),
            "seek did not work"
        );

        handle.send(Cmd::Stop);
        std::thread::sleep(Duration::from_millis(300));
        std::fs::remove_dir_all(&dir).ok();
    }
}
