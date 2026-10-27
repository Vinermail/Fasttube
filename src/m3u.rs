//! M3U, the lingo every other player speaks.
//!
//! Rows that point at a file become [`PlaylistItem::Local`]; rows that are a
//! YouTube link become [`PlaylistItem::Youtube`], so a playlist exported here can
//! be opened by anything and come back without losing which tracks were which.

use crate::playlists_store::PlaylistItem;

const HEADER: &str = "#EXTM3U";
const YT_HOSTS: [&str; 2] = ["music.youtube.com", "youtube.com"];

/// Renders a playlist. `duration` is taken from the item when we know it, -1
/// otherwise (which players treat as "unknown").
pub fn render(items: &[PlaylistItem]) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    for item in items {
        let (duration, artist, title, target) = match item {
            PlaylistItem::Local { path, title, artist } => (-1.0, artist, title, path.clone()),
            PlaylistItem::Youtube {
                video_id,
                title,
                artist,
                duration,
                ..
            } => (
                *duration,
                artist,
                title,
                format!("https://music.youtube.com/watch?v={video_id}"),
            ),
        };
        let seconds = if duration.is_finite() && duration > 0.0 {
            duration.round() as i64
        } else {
            -1
        };
        let label = match (artist.trim().is_empty(), title.trim().is_empty()) {
            (true, _) => title.trim().to_string(),
            (false, true) => artist.trim().to_string(),
            (false, false) => format!("{} - {}", artist.trim(), title.trim()),
        };
        out.push_str(&format!("#EXTINF:{seconds},{label}\n"));
        out.push_str(&target);
        out.push('\n');
    }
    out
}

/// Parses whatever a player wrote: tags are optional, blank lines and unknown
/// `#` lines are skipped.
pub fn parse(text: &str) -> Vec<PlaylistItem> {
    let mut items = Vec::new();
    let mut pending: Option<(f64, String)> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = Some(parse_extinf(rest));
            continue;
        }
        if line.starts_with('#') {
            continue; // #EXTM3U, #PLAYLIST:, comments
        }
        let (duration, label) = pending.take().unwrap_or((-1.0, String::new()));
        items.push(item_for(line, duration, &label));
    }
    items
}

fn parse_extinf(rest: &str) -> (f64, String) {
    // "#EXTINF:213,Artist - Title": the comma after the duration starts the label.
    let (seconds, label) = match rest.split_once(',') {
        Some((seconds, label)) => (seconds.trim(), label.trim()),
        None => (rest.trim(), ""),
    };
    let duration = seconds.parse::<f64>().unwrap_or(-1.0);
    (duration, label.to_string())
}

fn item_for(target: &str, duration: f64, label: &str) -> PlaylistItem {
    let (artist, title) = split_label(label);
    if let Some(video_id) = youtube_id(target) {
        let seconds = if duration.is_finite() && duration > 0.0 {
            duration
        } else {
            0.0
        };
        return PlaylistItem::youtube(
            &video_id,
            if title.is_empty() { label } else { &title },
            &artist,
            seconds,
            "",
        );
    }
    PlaylistItem::Local {
        path: target.to_string(),
        title: if title.is_empty() {
            std::path::Path::new(target)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| target.to_string())
        } else {
            title
        },
        artist,
    }
}

/// "Artist - Title" as most players write it; a label with no dash is a title.
fn split_label(label: &str) -> (String, String) {
    match label.split_once(" - ") {
        Some((artist, title)) => (artist.trim().to_string(), title.trim().to_string()),
        None => (String::new(), label.trim().to_string()),
    }
}

/// `v=` from a watch link, if this is one.
fn youtube_id(target: &str) -> Option<String> {
    if !target.starts_with("http://") && !target.starts_with("https://") {
        return None;
    }
    let host_ok = YT_HOSTS.iter().any(|host| target.contains(host));
    if !host_ok {
        return None;
    }
    let query = target.split('?').nth(1)?;
    let value = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("v="))?;
    let id = value.split(['&', '#']).next().unwrap_or("").trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Relative paths in a playlist file are relative to the file itself.
pub fn resolve(path: &str, base: &std::path::Path) -> String {
    let candidate = std::path::Path::new(path);
    if candidate.is_absolute() || path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }
    base.join(candidate).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, title: &str, artist: &str) -> PlaylistItem {
        PlaylistItem::local(path, title, artist)
    }

    #[test]
    fn a_file_row_keeps_its_path_and_label() {
        let text = "#EXTM3U\n#EXTINF:213,Queen - Bohemian Rhapsody\nC:/music/br.flac\n";
        let items = parse(text);

        assert_eq!(items.len(), 1);
        match &items[0] {
            PlaylistItem::Local { path, title, artist } => {
                assert_eq!(path, "C:/music/br.flac");
                assert_eq!(title, "Bohemian Rhapsody");
                assert_eq!(artist, "Queen");
            }
            other => panic!("expected a local item, got {other:?}"),
        }
    }

    #[test]
    fn a_youtube_row_becomes_a_youtube_item() {
        let text = "#EXTM3U\n#EXTINF:180,Artist - Song\nhttps://music.youtube.com/watch?v=dQw4w9WgXcQ\n";
        match &parse(text)[0] {
            PlaylistItem::Youtube { video_id, title, artist, duration, .. } => {
                assert_eq!(video_id, "dQw4w9WgXcQ");
                assert_eq!(title, "Song");
                assert_eq!(artist, "Artist");
                assert_eq!(*duration, 180.0);
            }
            other => panic!("expected a youtube item, got {other:?}"),
        }
    }

    #[test]
    fn tags_without_extinf_still_produce_rows() {
        let text = "#EXTM3U\r\nC:\\m\\one.mp3\r\n\r\nC:\\m\\two.mp3\r\n";
        let items = parse(text);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title(), "one", "the file name is the fallback title");
    }

    #[test]
    fn unknown_tags_and_blank_labels_do_not_break_the_file() {
        let text = "#EXTM3U\n#PLAYLIST:Мой\n#EXTINF:-1,\n/tmp/a.mp3\n#EXT-X-ENDLIST\n";
        let items = parse(text);

        assert_eq!(items.len(), 1);
        match &items[0] {
            PlaylistItem::Local { title, .. } => assert_eq!(title, "a"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_label_without_a_dash_is_a_title() {
        let text = "#EXTM3U\n#EXTINF:10,Just A Title\n/tmp/a.mp3\n";
        match &parse(text)[0] {
            PlaylistItem::Local { title, artist, .. } => {
                assert_eq!(title, "Just A Title");
                assert!(artist.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rendering_and_parsing_a_playlist_gives_the_same_rows_back() {
        let items = vec![
            local("C:/m/a.mp3", "Первый", "Кто-то"),
            PlaylistItem::youtube("abc123", "Второй", "Другой", 0.0, ""),
        ];

        let back = parse(&render(&items));

        assert_eq!(back.len(), 2);
        assert_eq!(back[0].key(), items[0].key());
        assert_eq!(back[0].title(), "Первый");
        assert_eq!(back[1].key(), "yt:abc123");
        assert_eq!(back[1].title(), "Второй");
    }

    #[test]
    fn a_relative_path_is_resolved_against_the_playlist_file() {
        let base = std::path::Path::new("C:/m");
        // Path equality ignores the separator style, which is what matters here.
        assert_eq!(
            std::path::Path::new(&resolve("a.mp3", base)),
            base.join("a.mp3"),
            "a bare name joins the playlist's folder"
        );
        assert_eq!(
            resolve("https://music.youtube.com/watch?v=x", base),
            "https://music.youtube.com/watch?v=x"
        );
    }

    #[test]
    fn a_written_file_reads_back_into_a_playlist() {
        // The same steps the export/import commands take, without the app.
        let dir = std::env::temp_dir().join(format!("fasttube-m3u-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mix.m3u");

        let items = vec![
            local("C:/m/a.mp3", "A", "X"),
            PlaylistItem::youtube("vid1", "B", "Y", 120.0, ""),
        ];
        crate::fsutil::write_atomic(&file, render(&items).as_bytes()).unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        let mut back = parse(&text);
        for item in &mut back {
            if let PlaylistItem::Local { path, .. } = item {
                *path = resolve(path, &dir);
            }
        }

        let mut store = crate::playlists_store::Store::default();
        let id = store.create_custom("mix", 1);
        assert_eq!(store.add_tracks(&id, back), 2, "both rows land in the playlist");
        let list = store.custom_by_id(&id).unwrap();
        assert!(list.tracks[0].is_local());
        assert_eq!(list.tracks[1].key(), "yt:vid1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_watch_links_from_youtube_count() {
        assert_eq!(youtube_id("https://example.com/watch?v=x"), None);
        assert_eq!(youtube_id("https://music.youtube.com/playlist?list=PL1"), None);
        assert_eq!(
            youtube_id("https://www.youtube.com/watch?v=abc&t=10").as_deref(),
            Some("abc")
        );
    }
}
