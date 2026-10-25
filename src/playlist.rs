use anyhow::Result;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const AUDIO_EXTS: [&str; 10] = [
    "mp3", "flac", "wav", "ogg", "oga", "m4a", "mp4", "aac", "opus", "wma",
];

const FOLDER_COVER_NAMES: [&str; 8] = [
    "cover.jpg",
    "cover.png",
    "folder.jpg",
    "folder.png",
    "front.jpg",
    "front.png",
    "albumart.jpg",
    "album.png",
];

#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    /// Album tag, used by the library search.
    pub album: String,
    pub duration: Option<f64>,
    pub cover: Option<PathBuf>,
    /// ReplayGain-style loudness normalization factor (1.0 = unchanged).
    pub gain: Option<f32>,
}

impl Track {
    pub fn new(path: PathBuf, title: String) -> Self {
        Self {
            path,
            title,
            artist: String::new(),
            album: String::new(),
            duration: None,
            cover: None,
            gain: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub name: String,
    pub tracks: Vec<Track>,
}

impl Track {
    /// Case-insensitive substring match over the fields a user types into
    /// the search box. `needle` is expected to be lowercased already.
    pub fn matches_query(&self, needle: &str) -> bool {
        [self.title.as_str(), self.artist.as_str(), self.album.as_str()]
            .iter()
            .any(|field| field.to_lowercase().contains(needle))
    }
}

/// Tracks of the local library matching `query`, at most `limit`.
///
/// Matching is a case-insensitive substring over title, artist and album.
/// One file can sit in several folder-playlists, so results are deduped by
/// path and keep the order of the library.
pub fn search_library<'a>(library: &'a [Playlist], query: &str, limit: usize) -> Vec<&'a Track> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for playlist in library {
        for track in &playlist.tracks {
            if !seen.insert(track.path.clone()) {
                continue;
            }
            if track.matches_query(&needle) {
                found.push(track);
                if found.len() >= limit {
                    return found;
                }
            }
        }
    }
    found
}

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Builds playlists from a folder structure: every directory that directly
/// contains audio files becomes a playlist named after that directory.
/// Metadata (title, artist, duration, cover art) is read from file tags;
/// embedded cover art is extracted into `cover_dir`.
pub fn collect(root: &Path, cover_dir: &Path) -> Result<Vec<Playlist>> {
    let root = root.canonicalize()?;
    std::fs::create_dir_all(cover_dir).ok();

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if is_audio(path) {
            paths.push(path.to_path_buf());
        }
    }

    let tracks: Vec<Track> = paths
        .par_iter()
        .map(|path| read_track(path, cover_dir))
        .collect();

    let mut by_dir: BTreeMap<PathBuf, Vec<Track>> = BTreeMap::new();
    for track in tracks {
        let dir = track.path.parent().unwrap().to_path_buf();
        by_dir.entry(dir).or_default().push(track);
    }

    let mut name_counts: BTreeMap<String, usize> = BTreeMap::new();
    for dir in by_dir.keys() {
        *name_counts.entry(default_name(dir, &root)).or_insert(0) += 1;
    }

    let mut playlists = Vec::new();
    for (dir, mut tracks) in by_dir {
        tracks.sort_by_key(|a| a.title.to_lowercase());
        let base = default_name(&dir, &root);
        let name = if name_counts[&base] > 1 {
            rel_path_name(&dir, &root)
        } else {
            base
        };
        playlists.push(Playlist { name, tracks });
    }

    playlists.sort_by_key(|a| a.name.to_lowercase());
    Ok(playlists)
}

fn read_track(path: &Path, cover_dir: &Path) -> Track {
    let fallback_title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let mut title = fallback_title.clone();
    let mut artist = String::new();
    let mut album = String::new();
    let mut duration = None;
    let mut cover = None;
    let mut gain = None;
    let mut peak: Option<f32> = None;

    if let Ok(tagged) = lofty::read_from_path(path) {
        if let Some(tag) = tagged.primary_tag() {
            if let Some(t) = tag.title() {
                title = t.to_string();
            }
            if let Some(a) = tag.artist() {
                artist = a.to_string();
            }
            if let Some(a) = tag.album() {
                album = a.to_string();
            }
            if let Some(pic) = tag.pictures().first() {
                cover = write_cover(cover_dir, pic.data(), pic.mime_type().map(|m| m.as_str()));
            }
            // ReplayGain always comes as a pair: gain + peak. The peak keeps
            // the boost from clipping (samples must never exceed 0 dBFS).
            if let Some(raw) = tag.get_string(lofty::tag::ItemKey::ReplayGainTrackGain) {
                gain = parse_replaygain_tag(raw);
            }
            if let Some(raw) = tag.get_string(lofty::tag::ItemKey::ReplayGainTrackPeak) {
                peak = raw
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<f32>().ok());
            }
        }
        let d = tagged.properties().duration().as_secs_f64();
        if d > 0.0 {
            duration = Some(d);
        }
    }

    if cover.is_none()
        && let Some(dir) = path.parent()
    {
        cover = folder_cover(dir, cover_dir);
    }

    Track {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        duration,
        cover,
        gain: gain.map(|g| limit_gain_by_peak(g, peak)),
    }
}

/// Parses ReplayGain tag values like "-7.23 dB" into a linear gain factor.
fn parse_replaygain_tag(raw: &str) -> Option<f32> {
    let db: f32 = raw.split_whitespace().next()?.parse().ok()?;
    Some(10f32.powf(db / 20.0).clamp(0.1, 8.0))
}

/// Caps the gain so that gain * peak <= 0.95 (no digital clipping).
/// Without peak info we never boost beyond unity — only attenuate.
fn limit_gain_by_peak(gain: f32, peak: Option<f32>) -> f32 {
    match peak {
        Some(p) if p > 0.0 => gain.min(0.95 / p.max(0.0001)),
        _ => gain.min(1.0),
    }
}

fn folder_cover(dir: &Path, cover_dir: &Path) -> Option<PathBuf> {
    for name in FOLDER_COVER_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            let Ok(data) = std::fs::read(&candidate) else {
                continue;
            };
            let mime = cover_mime(name);
            if let Some(cover) = write_cover(cover_dir, &data, mime) {
                return Some(cover);
            }
        }
    }
    None
}

fn cover_mime(name: &str) -> Option<&'static str> {
    match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// Writes cover bytes to the cache dir. The file name is derived from the
/// image content, so identical covers across tracks are stored once.
fn write_cover(cover_dir: &Path, data: &[u8], mime: Option<&str>) -> Option<PathBuf> {
    if data.is_empty() {
        return None;
    }
    let ext = match mime {
        Some("image/jpeg") | Some("image/jpg") => "jpg",
        Some("image/png") => "png",
        Some("image/webp") => "webp",
        Some("image/gif") => "gif",
        Some("image/bmp") => "bmp",
        _ => "img",
    };
    let dest = cover_dir.join(format!("{:016x}.{}", fnv1a64(data), ext));
    if !dest.exists() {
        std::fs::write(&dest, data).ok()?;
    }
    Some(dest)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn default_name(dir: &Path, root: &Path) -> String {
    if dir == root {
        root.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("root")
            .to_string()
    } else {
        dir.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }
}

fn rel_path_name(dir: &Path, root: &Path) -> String {
    dir.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace(['/', '\\'], " › "))
        .unwrap_or_else(|_| default_name(dir, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, seconds: f32) {
        let sample_rate = 8000u32;
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
    fn collect_builds_playlists_from_folder() {
        let dir = std::env::temp_dir().join(format!("player_llm_scan_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_wav(&dir.join("a.wav"), 0.1);
        write_wav(&dir.join("b.wav"), 0.1);
        let sub = dir.join("album");
        std::fs::create_dir_all(&sub).unwrap();
        write_wav(&sub.join("c.wav"), 0.1);

        let covers = dir.join("covers");
        let playlists = collect(&dir, &covers).unwrap();
        assert_eq!(playlists.len(), 2);
        let root_pl = playlists.iter().find(|p| p.tracks.len() == 2).unwrap();
        assert_eq!(root_pl.tracks[0].title, "a");
        assert_eq!(root_pl.tracks[0].duration, Some(0.1));
        let album_pl = playlists.iter().find(|p| p.tracks.len() == 1).unwrap();
        assert_eq!(album_pl.tracks[0].title, "c");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cover_dedup_by_content() {
        let dir = std::env::temp_dir().join(format!("player_llm_cover_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = write_cover(&dir, &[1, 2, 3, 4], Some("image/jpeg"));
        let b = write_cover(&dir, &[1, 2, 3, 4], Some("image/jpeg"));
        let c = write_cover(&dir, &[5, 6, 7, 8], Some("image/jpeg"));
        assert!(a.is_some());
        assert_eq!(a, b);
        assert_ne!(a, c);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn lib_track(path: &str, title: &str, artist: &str, album: &str) -> Track {
        Track {
            path: std::path::PathBuf::from(path),
            title: title.to_string(),
            artist: artist.to_string(),
            album: album.to_string(),
            duration: None,
            cover: None,
            gain: None,
        }
    }

    fn library() -> Vec<Playlist> {
        vec![
            Playlist {
                name: "Root".to_string(),
                tracks: vec![
                    lib_track("a.mp3", "Wonderwall", "Oasis", "Morning Glory"),
                    lib_track("b.mp3", "Da Funk", "Daft Punk", "Homework"),
                ],
            },
            Playlist {
                name: "Homework".to_string(),
                tracks: vec![
                    // same file as in Root: must not show up twice
                    lib_track("b.mp3", "Da Funk", "Daft Punk", "Homework"),
                    lib_track("c.mp3", "Around the World", "Daft Punk", "Homework"),
                ],
            },
        ]
    }

    #[test]
    fn library_search_matches_title_artist_and_album_case_insensitively() {
        let lib = library();

        let by_title: Vec<_> = search_library(&lib, "WONDER", 10)
            .iter()
            .map(|t| t.title.clone())
            .collect();
        assert_eq!(by_title, vec!["Wonderwall"]);

        let by_artist = search_library(&lib, "daft punk", 10);
        assert_eq!(by_artist.len(), 2, "dedup keeps one entry per file");

        let by_album = search_library(&lib, "homework", 10);
        assert_eq!(by_album.len(), 2);
    }

    #[test]
    fn library_search_keeps_library_order_and_respects_the_limit() {
        let lib = library();

        let all = search_library(&lib, "daft", 10);
        assert_eq!(
            all.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            vec!["Da Funk", "Around the World"]
        );

        let limited = search_library(&lib, "daft", 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].title, "Da Funk");
    }

    #[test]
    fn library_search_ignores_blank_queries_and_zero_limit() {
        let lib = library();

        assert!(search_library(&lib, "", 10).is_empty());
        assert!(search_library(&lib, "   ", 10).is_empty());
        assert!(search_library(&lib, "daft", 0).is_empty());
    }

    #[test]
    fn library_search_reports_no_matches() {
        let lib = library();
        assert!(search_library(&lib, "nirvana", 10).is_empty());
    }

}
