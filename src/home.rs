//! Home page shelves that need no network: "continue", recent history,
//! favourites and freshly added local files.
//!
//! Pure data and layout: no Tauri, no gRPC, no file system. `main.rs` collects
//! the inputs (they live under the app's locks there) and this module decides
//! what the shelves look like, which keeps the rules testable.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const SHELF_CONTINUE: &str = "Продолжить";
pub const SHELF_RECENT: &str = "Недавно слушали";
pub const SHELF_FAVORITES: &str = "Избранное";
pub const SHELF_ADDED: &str = "Недавно добавленное";

/// Shelf kinds shared with the UI. "personal"/"explore" mark network shelves,
/// "local" marks rows that play a file instead of a video id.
pub const KIND_LOCAL: &str = "local";
pub const KIND_SONG: &str = "song";
pub const KIND_PERSONAL: &str = "personal";

/// A track that just started playing is not worth offering to continue.
pub const RESUME_MIN_SECS: f64 = 5.0;

/// One card of a shelf. The field names match `SearchItemDto` on purpose: the
/// UI renders home shelves with the same code as search results.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Item {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumbnail: String,
    pub duration_secs: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub video_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub browse_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub playlist_id: String,
    /// Set for local rows: the file to play instead of a video id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Item {
    pub fn song(
        id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        thumbnail: impl Into<String>,
        duration_secs: f64,
    ) -> Self {
        Self {
            kind: KIND_SONG.to_string(),
            id: id.into(),
            title: title.into(),
            subtitle: artist.into(),
            thumbnail: thumbnail.into(),
            duration_secs,
            ..Default::default()
        }
    }

    pub fn local(
        path: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        cover: impl Into<String>,
    ) -> Self {
        let path = path.into();
        Self {
            kind: KIND_LOCAL.to_string(),
            id: path.clone(),
            title: title.into(),
            subtitle: artist.into(),
            thumbnail: cover.into(),
            path: Some(path),
            ..Default::default()
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Shelf {
    pub title: String,
    pub kind: String,
    pub items: Vec<Item>,
}

impl Shelf {
    pub fn new(title: impl Into<String>, kind: impl Into<String>, items: Vec<Item>) -> Self {
        Self {
            title: title.into(),
            kind: kind.into(),
            items,
        }
    }
}

/// The last played track and where the listener stopped.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Resume {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub thumbnail: String,
    pub position_secs: f64,
    /// Local files resume by path, network tracks by video id.
    pub is_local: bool,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct HistoryEntry {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub thumbnail: String,
    pub played_at: f64,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct FavoriteEntry {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub cover: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct FileEntry {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub cover: String,
    pub mtime: f64,
}

#[derive(Clone, Debug, Default)]
pub struct LocalInput {
    pub resume: Option<Resume>,
    pub history: Vec<HistoryEntry>,
    pub favorites: Vec<FavoriteEntry>,
    /// Local files, newest first.
    pub recent_files: Vec<FileEntry>,
}

/// "1:23" / "1:02:03", rounded down like every player does.
pub fn format_position(secs: f64) -> String {
    let total = if secs.is_finite() && secs > 0.0 {
        secs.floor() as u64
    } else {
        0
    };
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn continue_subtitle(artist: &str, position_secs: f64) -> String {
    let artist = artist.trim();
    let left = format!("остановились на {}", format_position(position_secs));
    if artist.is_empty() {
        left
    } else {
        format!("{artist} • {left}")
    }
}

/// Shelves that work offline, in the order they should appear.
pub fn local_shelves(input: &LocalInput, limit: usize) -> Vec<Shelf> {
    let limit = limit.max(1);
    let mut shelves = Vec::new();

    let resume_id = input
        .resume
        .as_ref()
        .filter(|r| !r.id.is_empty() && r.position_secs >= RESUME_MIN_SECS)
        .map(|r| r.id.clone());

    if let Some(resume) = input
        .resume
        .as_ref()
        .filter(|r| !r.id.is_empty() && r.position_secs >= RESUME_MIN_SECS)
    {
        let item = if resume.is_local || !resume.path.is_empty() {
            let mut item = Item::local(
                resume.path.clone(),
                resume.title.clone(),
                String::new(),
                resume.thumbnail.clone(),
            );
            item.id = resume.id.clone();
            item
        } else {
            Item::song(
                resume.id.clone(),
                resume.title.clone(),
                String::new(),
                resume.thumbnail.clone(),
                0.0,
            )
        };
        let mut item = item;
        item.subtitle = continue_subtitle(&resume.artist, resume.position_secs);
        shelves.push(Shelf::new(SHELF_CONTINUE, KIND_LOCAL, vec![item]));
    }

    let mut history = input.history.clone();
    // Newest first; the sort is stable so equal (missing) stamps keep the
    // order the file had.
    history.sort_by(|a, b| {
        b.played_at
            .partial_cmp(&a.played_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen: HashSet<String> = HashSet::new();
    let mut recent = Vec::new();
    for entry in &history {
        if entry.id.is_empty() || resume_id.as_deref() == Some(entry.id.as_str()) {
            continue;
        }
        if !seen.insert(entry.id.clone()) {
            continue;
        }
        recent.push(Item::song(
            entry.id.clone(),
            entry.title.clone(),
            entry.artist.clone(),
            entry.thumbnail.clone(),
            0.0,
        ));
        if recent.len() >= limit {
            break;
        }
    }
    if !recent.is_empty() {
        shelves.push(Shelf::new(SHELF_RECENT, KIND_LOCAL, recent));
    }

    let mut seen_paths: HashSet<&str> = HashSet::new();
    let favorites: Vec<Item> = input
        .favorites
        .iter()
        .filter(|f| seen_paths.insert(f.path.as_str()))
        .take(limit)
        .map(|f| Item::local(f.path.clone(), f.title.clone(), f.artist.clone(), f.cover.clone()))
        .collect();
    if !favorites.is_empty() {
        shelves.push(Shelf::new(SHELF_FAVORITES, KIND_LOCAL, favorites));
    }

    let added: Vec<Item> = input
        .recent_files
        .iter()
        .take(limit)
        .map(|f| Item::local(f.path.clone(), f.title.clone(), f.artist.clone(), f.cover.clone()))
        .collect();
    if !added.is_empty() {
        shelves.push(Shelf::new(SHELF_ADDED, KIND_LOCAL, added));
    }

    shelves
}

/// Network shelves are kept in the snapshot only while they still belong to the
/// listener: an anonymous page hides a snapshot that was built with an account.
pub fn visible_network_shelves(shelves: &[Shelf], signed_in: bool) -> Vec<Shelf> {
    shelves
        .iter()
        .filter(|shelf| {
            if shelf.items.is_empty() {
                return false;
            }
            signed_in || shelf.kind != KIND_PERSONAL
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(id: &str, played_at: f64) -> HistoryEntry {
        HistoryEntry {
            id: id.to_string(),
            title: format!("Track {id}"),
            artist: "Artist".to_string(),
            thumbnail: String::new(),
            played_at,
        }
    }

    fn favorite(path: &str) -> FavoriteEntry {
        FavoriteEntry {
            path: path.to_string(),
            title: format!("File {path}"),
            artist: "Someone".to_string(),
            cover: String::new(),
        }
    }

    #[test]
    fn position_is_formatted_like_a_player() {
        assert_eq!(format_position(0.0), "0:00");
        assert_eq!(format_position(9.7), "0:09");
        assert_eq!(format_position(83.0), "1:23");
        assert_eq!(format_position(3600.0), "1:00:00");
        assert_eq!(format_position(3723.9), "1:02:03");
        assert_eq!(format_position(-5.0), "0:00");
        assert_eq!(format_position(f64::NAN), "0:00");
    }

    #[test]
    fn continue_comes_first_and_keeps_the_position() {
        let input = LocalInput {
            resume: Some(Resume {
                id: "vid1".to_string(),
                title: "Halfway".to_string(),
                artist: "Band".to_string(),
                thumbnail: "cover".to_string(),
                position_secs: 83.0,
                is_local: false,
                path: String::new(),
            }),
            history: vec![history("vid1", 100.0), history("vid2", 90.0)],
            ..Default::default()
        };

        let shelves = local_shelves(&input, 20);

        assert_eq!(shelves[0].title, SHELF_CONTINUE);
        assert_eq!(shelves[0].items.len(), 1);
        assert_eq!(shelves[0].items[0].subtitle, "Band • остановились на 1:23");
        assert_eq!(shelves[0].items[0].id, "vid1");
        // The resumed track is not repeated in "recent".
        assert_eq!(
            shelves[1].items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["vid2"]
        );
    }

    #[test]
    fn a_track_that_barely_started_is_not_offered_to_continue() {
        let input = LocalInput {
            resume: Some(Resume {
                id: "vid1".to_string(),
                title: "Just started".to_string(),
                position_secs: 1.0,
                ..Default::default()
            }),
            ..Default::default()
        };

        assert!(local_shelves(&input, 20).is_empty());
    }

    #[test]
    fn resume_of_a_local_file_points_at_the_path() {
        let input = LocalInput {
            resume: Some(Resume {
                id: r"C:\music\a.mp3".to_string(),
                title: "A".to_string(),
                artist: "Me".to_string(),
                position_secs: 30.0,
                is_local: true,
                path: r"C:\music\a.mp3".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let item = &local_shelves(&input, 20)[0].items[0];

        assert_eq!(item.kind, KIND_LOCAL);
        assert_eq!(item.path.as_deref(), Some(r"C:\music\a.mp3"));
    }

    #[test]
    fn recent_history_is_deduped_sorted_and_capped() {
        let input = LocalInput {
            history: vec![
                history("b", 10.0),
                history("a", 30.0),
                history("b", 20.0),
                history("c", 5.0),
                history("d", 1.0),
            ],
            ..Default::default()
        };

        let shelves = local_shelves(&input, 3);

        assert_eq!(shelves.len(), 1);
        assert_eq!(
            shelves[0].items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn favorites_and_added_files_come_after_history() {
        let input = LocalInput {
            history: vec![history("vid1", 10.0)],
            favorites: vec![favorite("/m/fav.mp3"), favorite("/m/fav.mp3"), favorite("/m/2.mp3")],
            recent_files: vec![FileEntry {
                path: "/m/new.mp3".to_string(),
                title: "New".to_string(),
                artist: "Artist".to_string(),
                cover: String::new(),
                mtime: 500.0,
            }],
            ..Default::default()
        };

        let shelves = local_shelves(&input, 20);

        assert_eq!(
            shelves.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            vec![SHELF_RECENT, SHELF_FAVORITES, SHELF_ADDED]
        );
        assert_eq!(shelves[1].items.len(), 2, "duplicate favourite path dropped");
        assert_eq!(shelves[2].items[0].kind, KIND_LOCAL);
        assert_eq!(shelves[2].items[0].path.as_deref(), Some("/m/new.mp3"));
    }

    #[test]
    fn added_files_are_capped() {
        let input = LocalInput {
            recent_files: (0..30)
                .map(|i| FileEntry {
                    path: format!("/m/{i}.mp3"),
                    title: format!("T{i}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };

        assert_eq!(local_shelves(&input, 20)[0].items.len(), 20);
    }

    #[test]
    fn an_empty_library_has_no_shelves() {
        assert!(local_shelves(&LocalInput::default(), 20).is_empty());
    }

    #[test]
    fn personal_shelves_are_hidden_without_a_session() {
        let shelves = vec![
            Shelf::new(SHELF_CONTINUE, KIND_LOCAL, vec![Item::song("a", "A", "", "", 0.0)]),
            Shelf::new("Quick picks", KIND_PERSONAL, vec![Item::song("b", "B", "", "", 0.0)]),
            Shelf::new("Новые релизы", "explore", vec![Item::song("c", "C", "", "", 0.0)]),
        ];

        let signed_out = visible_network_shelves(&shelves[1..], false);
        assert_eq!(
            signed_out.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(),
            vec!["Новые релизы"]
        );

        let signed_in = visible_network_shelves(&shelves[1..], true);
        assert_eq!(signed_in.len(), 2);
    }
}
