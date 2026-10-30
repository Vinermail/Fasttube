use crate::playlist::Playlist;
use rodio::{Decoder, DeviceSinkBuilder, Player, Source, source::Zero};
use serde::Serialize;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use std::time::{Duration, Instant};

/// One fade step: 150 ms covered in ten ticks.
const FADE_STEP: f32 = 0.1;

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
    /// Mute the whole app without losing the slider position.
    SetMuted(bool),
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
    /// Output muted: `volume` keeps the level the user picked.
    pub muted: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// The level handed to the mixer: the perceptual curve first, mute last, so
/// unmuting restores exactly the level the slider is still showing.
fn output_level(volume: f32, muted: bool, curve: bool) -> f32 {
    let volume = volume.clamp(0.0, 1.0);
    if muted {
        return 0.0;
    }
    // Human loudness perception is logarithmic: with the exponent the slider
    // position maps roughly linearly to perceived loudness.
    if curve { volume.powf(3.0) } else { volume }
}

struct Engine {
    _device: rodio::MixerDeviceSink,
    player: Player,
    playlist: Playlist,
    idx: usize,
    total: Duration,
    volume: f32,
    /// Silence without touching `volume`.
    muted: bool,
    speed: f32,
    gain: f32,
    target_gain: f32,
    /// Multiplier for the short fades on pause, resume, stop and seek. 1.0 is
    /// "no fade in progress".
    fade: f32,
    fade_target: f32,
    /// What to do once the fade reaches its target.
    fade_after: Option<FadeAfter>,
    curve: bool,
    status: Status,
    dirty: bool,
    /// Counter of successful starts, so the run loop can announce them.
    started: u64,
}

/// The work a fade is covering. Pausing and stopping are held back until the
/// level is down, a seek is dipped through so the jump is inaudible.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FadeAfter {
    Pause,
    Stop,
    Seek(f64),
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
            muted: false,
            speed: 1.0,
            gain: 1.0,
            target_gain: 1.0,
            fade: 1.0,
            fade_target: 1.0,
            fade_after: None,
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
        self.reset_fade();
        self.apply_volume();
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
            muted: self.muted,
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
                self.reset_fade();
                self.apply_volume();
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
        self.reset_fade();
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
        let level = output_level(self.volume, self.muted, self.curve);
        // Safety net: gains are peak-limited elsewhere (never boosting into
        // clipping), the 4.0 cap only guards against corrupt cached values.
        self.player
            .set_volume((level * self.gain * self.fade).clamp(0.0, 4.0));
    }

    /// Starts a fade in the given direction; `after` runs once it lands.
    fn begin_fade(&mut self, to: f32, after: Option<FadeAfter>) {
        self.fade_target = to.clamp(0.0, 1.0);
        self.fade_after = after;
        self.dirty = true;
    }

    /// One step of a running fade, called from the run loop. Steps are sized for
    /// a 150 ms fade at the tick interval the loop uses while a fade is active.
    /// Returns whether the level moved.
    fn fade_tick(&mut self, step: f32) -> bool {
        if (self.fade - self.fade_target).abs() < f32::EPSILON {
            return false;
        }
        if self.fade < self.fade_target {
            self.fade = (self.fade + step).min(self.fade_target);
        } else {
            self.fade = (self.fade - step).max(self.fade_target);
        }
        self.apply_volume();
        self.dirty = true;
        if (self.fade - self.fade_target).abs() < f32::EPSILON {
            self.finish_fade();
        }
        true
    }

    /// Runs the work the fade was covering. Only once: a second tick after the
    /// landing has nothing left to do.
    fn finish_fade(&mut self) {
        match self.fade_after.take() {
            Some(FadeAfter::Pause) => self.player.pause(),
            Some(FadeAfter::Stop) => self.player.stop(),
            Some(FadeAfter::Seek(secs)) => {
                let _ = self.player.try_seek(Duration::from_secs_f64(secs.max(0.0)));
                self.fade_target = 1.0;
            }
            None => {}
        }
    }

    fn fading(&self) -> bool {
        (self.fade - self.fade_target).abs() > f32::EPSILON
    }

    /// A fresh source starts at full level with nothing pending: a fade or a
    /// deferred pause/stop/seek belonged to the track that just went away, and
    /// carrying it over silenced the next one (or paused it, or seeked it).
    fn reset_fade(&mut self) {
        self.fade = 1.0;
        self.fade_target = 1.0;
        self.fade_after = None;
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
                // The state flips at once so the button and the overlay answer
                // the click; the audio is held for the 150 ms of the fade.
                self.begin_fade(0.0, Some(FadeAfter::Pause));
                self.status = Status::Paused;
                self.dirty = true;
            }
            Status::Paused => {
                // Resume from the level the fade stopped at: a pause that was
                // interrupted in the middle must not jump to silence first.
                self.player.play();
                self.begin_fade(1.0, None);
                self.apply_volume();
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
        // The sink runs on until the level is down; `stop` follows the fade.
        self.begin_fade(0.0, Some(FadeAfter::Stop));
        self.status = Status::Stopped;
        self.dirty = true;
    }

    fn seek(&mut self, secs: f64) {
        if self.status == Status::Playing {
            // Dip through silence: a hard jump in the middle of a sample clicks.
            self.begin_fade(0.0, Some(FadeAfter::Seek(secs.max(0.0))));
        } else if self.status != Status::Stopped {
            let _ = self.player.try_seek(Duration::from_secs_f64(secs.max(0.0)));
        }
        self.dirty = true;
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply_volume();
        self.dirty = true;
    }

    fn set_muted(&mut self, muted: bool) {
        if self.muted == muted {
            return;
        }
        self.muted = muted;
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
    let mut pending_volume: Option<f32> = None;
    let mut pending_muted: Option<bool> = None;
    let mut seen_starts: u64 = 0;

    /// The engine is created on the first playlist: volume, mute and the curve
    /// can arrive before that, so they are kept until the device exists.
    fn apply_pending(
        engine: &mut Engine,
        curve: &mut Option<bool>,
        volume: &mut Option<f32>,
        muted: &mut Option<bool>,
    ) {
        if let Some(c) = curve.take() {
            engine.curve = c;
        }
        if let Some(v) = volume.take() {
            engine.volume = v.clamp(0.0, 1.0);
        }
        if let Some(m) = muted.take() {
            engine.muted = m;
        }
        engine.apply_volume();
    }

    loop {
        // While playing the loop has to look at the decoder (end of track) and
        // refresh the position a few times a second; once playback is over
        // there is nothing to watch, so it sleeps on the channel instead of
        // waking up twenty times a second for nothing.
        let playing = matches!(
            engine.as_ref().map(|e| e.status),
            Some(Status::Playing)
        );
        let wait = if engine.as_ref().map(|e| e.fading()).unwrap_or(false) {
            // A fade is 150 ms long: stepping it at 15 ms keeps it smooth
            // without keeping the loop this busy the rest of the time.
            Duration::from_millis(15)
        } else if playing {
            Duration::from_millis(150)
        } else {
            Duration::from_millis(500)
        };

        if let Some(e) = engine.as_mut()
            && e.status == Status::Playing
            && e.player.empty()
        {
            // The source ran dry: this is the natural end of the track,
            // not a stop we asked for.
            let _ = events.send(Event::TrackEnded);
            e.advance();
        }

        match rx.recv_timeout(wait) {
            Ok(cmd) => match cmd {
                Cmd::SetPlaylist(pl) => {
                    if engine.is_none() {
                        match Engine::create() {
                            Ok(mut e) => {
                                apply_pending(
                                    &mut e,
                                    &mut pending_curve,
                                    &mut pending_volume,
                                    &mut pending_muted,
                                );
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
                                apply_pending(
                                    &mut e,
                                    &mut pending_curve,
                                    &mut pending_volume,
                                    &mut pending_muted,
                                );
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
                    } else {
                        pending_volume = Some(v);
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
                Cmd::SetMuted(muted) => {
                    if let Some(e) = engine.as_mut() {
                        e.set_muted(muted);
                    } else {
                        pending_muted = Some(muted);
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
            // Only a running engine has a position worth republishing; a
            // paused or stopped one changes solely through commands, which set
            // `dirty` themselves.
            let running = e.status == Status::Playing;
            let due = e.dirty || (running && last_publish.elapsed() >= Duration::from_millis(150));
            if due {
                *snapshot.write() = Some(e.snapshot());
                last_publish = Instant::now();
            }
        }
        if let Some(e) = engine.as_mut() {
            e.dirty = false;
            e.ramp_tick();
            if e.fading() {
                e.fade_tick(FADE_STEP);
            }
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
        if audio_device().is_none() {
            return;
        }
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
        if audio_device().is_none() {
            return;
        }
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
        // The pause is covered by a 150 ms fade: the decoder keeps running
        // until the level is down, so measure the stillness after it lands.
        std::thread::sleep(Duration::from_millis(300));
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

    use super::output_level;

    /// An engine on a two second tone, created straight instead of through the
    /// handle: the fades are internal, so the test needs the struct.
    fn fading_engine(tag: &str) -> Option<Engine> {
        let mut engine = audio_device()?;
        engine.set_playlist(tone_playlist(tag, 1));
        engine.volume = 0.0;
        Some(engine)
    }

    /// A playlist of `count` tones, one file each.
    fn tone_playlist(tag: &str, count: usize) -> Playlist {
        let dir = std::env::temp_dir().join(format!("player_llm_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut tracks = Vec::new();
        for i in 0..count {
            let wav = dir.join(format!("tone{i}.wav"));
            write_wav(&wav, 2.0, 8000);
            tracks.push(Track::new(wav, format!("tone{i}")));
        }
        Playlist {
            name: "tone".into(),
            tracks,
        }
    }

    /// A CI runner has no sound card. Tests that need an output device return
    /// quietly instead of failing for a reason that says nothing about the code.
    fn audio_device() -> Option<Engine> {
        match Engine::create() {
            Ok(engine) => Some(engine),
            Err(e) => {
                eprintln!("[test] no audio device ({e}), skipping");
                None
            }
        }
    }

    /// Runs the fade to its landing point.
    fn settle_fades(engine: &mut Engine) {
        let mut ticks = 0;
        while engine.fading() {
            engine.fade_tick(FADE_STEP);
            ticks += 1;
            assert!(ticks < 100, "the fade never landed");
        }
    }

    #[test]
    fn a_pause_fades_out_before_the_sink_stops() {
        let Some(mut engine) = fading_engine("fade_pause") else {
            return;
        };
        assert_eq!(engine.status, Status::Playing);

        engine.toggle_pause();
        // The state answers the click at once, the level does not jump.
        assert_eq!(engine.status, Status::Paused);
        assert!(engine.fading(), "the pause must be covered by a fade");
        assert_eq!(engine.fade, 1.0);

        let mut ticks = 0;
        while engine.fading() {
            engine.fade_tick(FADE_STEP);
            ticks += 1;
            assert!(ticks < 50, "the fade never landed");
        }
        assert_eq!(engine.fade, 0.0);
        assert!(engine.fade_after.is_none(), "the sink was paused at the bottom");
        // A 150 ms fade at the loop's 15 ms tick.
        assert!(ticks <= 12, "the fade took {ticks} ticks");
    }

    #[test]
    fn a_resume_climbs_from_silence() {
        let Some(mut engine) = fading_engine("fade_resume") else {
            return;
        };
        engine.toggle_pause();
        while engine.fading() {
            engine.fade_tick(FADE_STEP);
        }
        engine.toggle_pause();
        assert_eq!(engine.status, Status::Playing);
        assert_eq!(engine.fade, 0.0, "a resume starts from nothing");
        while engine.fading() {
            engine.fade_tick(FADE_STEP);
        }
        assert_eq!(engine.fade, 1.0);
    }

    #[test]
    fn a_seek_dips_through_silence_and_comes_back() {
        let Some(mut engine) = fading_engine("fade_seek") else {
            return;
        };
        engine.seek(1.2);
        assert!(engine.fading());
        while engine.fading() {
            engine.fade_tick(FADE_STEP);
        }
        // The jump happened at the bottom and the level climbed back up.
        assert_eq!(engine.fade, 1.0);
        assert!(engine.fade_after.is_none());
        assert!(engine.player.get_pos().as_secs_f64() >= 1.0);
    }

    #[test]
    fn a_finished_pause_does_not_silence_the_next_track() {
        let Some(mut engine) = fading_engine("fade_leak_pause") else {
            return;
        };
        engine.toggle_pause();
        settle_fades(&mut engine);
        assert_eq!(engine.fade, 0.0);

        // The next track starts from full level, not from the faded silence.
        engine.load(0);
        assert_eq!(engine.status, Status::Playing);
        assert_eq!(engine.fade, 1.0);
        assert!(engine.fade_after.is_none());
        assert!(!engine.player.is_paused());
    }

    #[test]
    fn a_stop_then_play_has_sound() {
        let Some(mut engine) = fading_engine("fade_leak_stop") else {
            return;
        };
        engine.stop();
        settle_fades(&mut engine);
        assert_eq!(engine.status, Status::Stopped);
        assert_eq!(engine.fade, 0.0);

        engine.toggle_pause();
        assert_eq!(engine.status, Status::Playing);
        assert_eq!(engine.fade, 1.0, "the fade of the stop must not survive");
        assert!(!engine.player.is_paused());
    }

    #[test]
    fn a_next_in_the_middle_of_a_pause_is_not_paused() {
        let Some(mut engine) = fading_engine("fade_leak_next") else {
            return;
        };
        engine.toggle_pause();
        engine.fade_tick(FADE_STEP);
        engine.fade_tick(FADE_STEP);
        assert!(engine.fade < 1.0, "the fade is still running");

        engine.next();
        assert_eq!(engine.status, Status::Playing);
        assert_eq!(engine.fade, 1.0);
        assert!(
            engine.fade_after.is_none(),
            "the pending pause applied itself to the new track"
        );
        settle_fades(&mut engine);
        assert!(!engine.player.is_paused(), "the new track was paused by the old fade");
    }

    #[test]
    fn a_seek_in_flight_does_not_move_the_next_track() {
        let Some(mut engine) = fading_engine("fade_leak_seek") else {
            return;
        };
        engine.seek(1.2);
        assert!(engine.fading());

        // Another track arrives before the dip reached the bottom.
        engine.next();
        settle_fades(&mut engine);
        assert_eq!(engine.status, Status::Playing);
        assert!(engine.fade_after.is_none());
        let pos = engine.player.get_pos().as_secs_f64();
        assert!(pos < 0.5, "the new track was seeked to {pos}s by the old fade");
    }

    #[test]
    fn mute_is_silence_whatever_the_slider_says() {
        assert_eq!(output_level(1.0, true, true), 0.0);
        assert_eq!(output_level(0.42, true, false), 0.0);
    }

    #[test]
    fn the_curve_only_shapes_the_level() {
        assert!((output_level(0.5, false, true) - 0.125).abs() < 1e-6);
        assert!((output_level(0.5, false, false) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn unmuting_restores_the_same_level() {
        let level = output_level(0.7, false, true);
        assert!((output_level(0.7, false, true) - level).abs() < 1e-6);
        assert!(level > 0.0);
    }

}
