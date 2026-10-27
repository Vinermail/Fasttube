//! User-side playlist data: what the indexers cannot know.
//!
//! Folder playlists and YouTube playlists are read-only sources; this keeps the
//! things a listener decides — pin, hide, rename, cover — plus playlists they
//! build themselves out of local files.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PlaylistMeta {
    /// Empty means "use the source title".
    #[serde(default)]
    pub title: String,
    /// Empty means "use the source cover".
    #[serde(default)]
    pub cover: String,
}

/// One row of a playlist the user built: a file on this machine, or a track
/// that lives on YouTube.
///
/// Old files stored only `{"path": ...}`; that is what the hand-written
/// [`Deserialize`] below turns into a `Local` item.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaylistItem {
    Local {
        path: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        artist: String,
    },
    Youtube {
        video_id: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        artist: String,
        #[serde(default)]
        duration: f64,
        #[serde(default)]
        thumbnail: String,
    },
}

impl PlaylistItem {
    pub fn local(path: &str, title: &str, artist: &str) -> Self {
        PlaylistItem::Local {
            path: path.to_string(),
            title: if title.is_empty() { file_stem(path) } else { title.to_string() },
            artist: artist.to_string(),
        }
    }

    pub fn youtube(
        video_id: &str,
        title: &str,
        artist: &str,
        duration: f64,
        thumbnail: &str,
    ) -> Self {
        PlaylistItem::Youtube {
            video_id: video_id.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            duration,
            thumbnail: thumbnail.to_string(),
        }
    }

    /// What makes two rows "the same track".
    pub fn key(&self) -> String {
        match self {
            PlaylistItem::Local { path, .. } => format!("file:{path}"),
            PlaylistItem::Youtube { video_id, .. } => format!("yt:{video_id}"),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, PlaylistItem::Local { .. })
    }

    #[cfg(test)]
    pub fn title(&self) -> &str {
        match self {
            PlaylistItem::Local { title, .. } | PlaylistItem::Youtube { title, .. } => title,
        }
    }
}

fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

impl<'de> Deserialize<'de> for PlaylistItem {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            kind: String,
            #[serde(default)]
            path: String,
            #[serde(default)]
            video_id: String,
            #[serde(default)]
            title: String,
            #[serde(default)]
            artist: String,
            #[serde(default)]
            duration: f64,
            #[serde(default)]
            thumbnail: String,
        }

        let raw = Raw::deserialize(deserializer)?;
        // `kind` is authoritative when present; anything older is inferred from
        // which id it carries, so a file written by an older build still loads.
        match raw.kind.as_str() {
            "youtube" => Ok(PlaylistItem::Youtube {
                video_id: raw.video_id,
                title: raw.title,
                artist: raw.artist,
                duration: raw.duration,
                thumbnail: raw.thumbnail,
            }),
            "local" => {
                let title = if raw.title.is_empty() { file_stem(&raw.path) } else { raw.title };
                Ok(PlaylistItem::Local {
                    path: raw.path,
                    title,
                    artist: raw.artist,
                })
            }
            _ if !raw.video_id.is_empty() => Ok(PlaylistItem::Youtube {
                video_id: raw.video_id,
                title: raw.title,
                artist: raw.artist,
                duration: raw.duration,
                thumbnail: raw.thumbnail,
            }),
            _ => {
                let title = if raw.title.is_empty() { file_stem(&raw.path) } else { raw.title };
                Ok(PlaylistItem::Local {
                    path: raw.path,
                    title,
                    artist: raw.artist,
                })
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomPlaylist {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub cover: String,
    /// The YouTube playlist this one is linked to, once "связать" ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_id: Option<String>,
    #[serde(default, alias = "tracks")]
    pub tracks: Vec<PlaylistItem>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Store {
    /// Pinned ids in the order they were pinned.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Ids tucked into the "hidden" folder at the bottom of the sidebar.
    #[serde(default)]
    pub hidden: Vec<String>,
    #[serde(default)]
    pub meta: BTreeMap<String, PlaylistMeta>,
    #[serde(default)]
    pub custom: Vec<CustomPlaylist>,
}

impl Store {
    /// Returns the new state.
    pub fn toggle_pin(&mut self, id: &str) -> bool {
        if let Some(pos) = self.pinned.iter().position(|p| p == id) {
            self.pinned.remove(pos);
            false
        } else {
            self.pinned.push(id.to_string());
            self.hidden.retain(|p| p != id);
            true
        }
    }

    /// Returns the new state. Hiding also unpins: a playlist cannot be both at
    /// the very top and inside the hidden folder.
    pub fn toggle_hidden(&mut self, id: &str) -> bool {
        if let Some(pos) = self.hidden.iter().position(|p| p == id) {
            self.hidden.remove(pos);
            false
        } else {
            self.hidden.push(id.to_string());
            self.pinned.retain(|p| p != id);
            true
        }
    }

    pub fn set_title(&mut self, id: &str, title: &str) {
        let entry = self.meta.entry(id.to_string()).or_default();
        entry.title = title.trim().to_string();
        if entry.title.is_empty() && entry.cover.is_empty() {
            self.meta.remove(id);
        }
    }

    pub fn set_cover(&mut self, id: &str, cover: &str) {
        let entry = self.meta.entry(id.to_string()).or_default();
        entry.cover = cover.to_string();
        if entry.title.is_empty() && entry.cover.is_empty() {
            self.meta.remove(id);
        }
    }

    /// Forgets everything about an id (after a delete).
    pub fn forget(&mut self, id: &str) {
        self.pinned.retain(|p| p != id);
        self.hidden.retain(|p| p != id);
        self.meta.remove(id);
        self.custom.retain(|c| c.id != id);
    }

    pub fn custom_by_id(&self, id: &str) -> Option<&CustomPlaylist> {
        self.custom.iter().find(|c| c.id == id)
    }

    pub fn custom_by_id_mut(&mut self, id: &str) -> Option<&mut CustomPlaylist> {
        self.custom.iter_mut().find(|c| c.id == id)
    }

    /// Creates an empty playlist and returns its id.
    pub fn create_custom(&mut self, title: &str, now_ms: u128) -> String {
        let mut id = format!("custom:{now_ms}");
        let mut n = 1;
        while self.custom.iter().any(|c| c.id == id) {
            n += 1;
            id = format!("custom:{now_ms}-{n}");
        }
        let title = title.trim();
        self.custom.push(CustomPlaylist {
            id: id.clone(),
            title: if title.is_empty() {
                "Новый плейлист".to_string()
            } else {
                title.to_string()
            },
            cover: String::new(),
            linked_id: None,
            tracks: Vec::new(),
        });
        id
    }

    /// Appends items, skipping ones the playlist already has. Returns how many
    /// were actually added.
    pub fn add_tracks(&mut self, id: &str, items: Vec<PlaylistItem>) -> usize {
        let Some(list) = self.custom_by_id_mut(id) else {
            return 0;
        };
        let mut added = 0;
        for item in items {
            if item.key().ends_with(':') {
                continue;
            }
            if list.tracks.iter().any(|t| t.key() == item.key()) {
                continue;
            }
            list.tracks.push(item);
            added += 1;
        }
        added
    }

    /// Links a playlist to a YouTube one ("" unlinks).
    pub fn set_linked(&mut self, id: &str, youtube_id: Option<String>) -> bool {
        match self.custom_by_id_mut(id) {
            Some(list) => {
                list.linked_id = youtube_id.filter(|v| !v.is_empty());
                true
            }
            None => false,
        }
    }

    pub fn remove_track(&mut self, id: &str, index: usize) -> bool {
        let Some(list) = self.custom_by_id_mut(id) else {
            return false;
        };
        if index >= list.tracks.len() {
            return false;
        }
        list.tracks.remove(index);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str) -> PlaylistItem {
        PlaylistItem::local(path, path, "")
    }

    #[test]
    fn pinning_and_hiding_are_exclusive() {
        let mut store = Store::default();
        assert!(store.toggle_pin("local:/music/rock"));
        assert_eq!(store.pinned, vec!["local:/music/rock".to_string()]);

        assert!(store.toggle_hidden("local:/music/rock"));
        assert_eq!(store.hidden, vec!["local:/music/rock".to_string()]);
        assert!(store.pinned.is_empty(), "hidden wins");

        // Pin it again: it leaves the hidden folder.
        assert!(store.toggle_pin("local:/music/rock"));
        assert!(store.hidden.is_empty());
        assert_eq!(store.pinned, vec!["local:/music/rock".to_string()]);
    }

    #[test]
    fn unpinning_and_unhiding_do_not_duplicate_ids() {
        let mut store = Store::default();
        store.toggle_pin("a");
        store.toggle_pin("a");
        store.toggle_pin("a");
        assert_eq!(store.pinned, vec!["a".to_string()]);

        store.toggle_hidden("a");
        store.toggle_hidden("a");
        store.toggle_hidden("a");
        assert_eq!(store.hidden, vec!["a".to_string()]);
    }

    #[test]
    fn a_custom_playlist_rejects_duplicate_paths() {
        let mut store = Store::default();
        let id = store.create_custom("", 1);
        assert_eq!(store.custom_by_id(&id).unwrap().title, "Новый плейлист");

        assert_eq!(store.add_tracks(&id, vec![track("/a.mp3"), track("/b.mp3")]), 2);
        assert_eq!(store.add_tracks(&id, vec![track("/b.mp3"), track("/c.mp3")]), 1);
        assert_eq!(store.custom_by_id(&id).unwrap().tracks.len(), 3);
    }

    #[test]
    fn created_ids_never_collide() {
        let mut store = Store::default();
        let first = store.create_custom("one", 42);
        let second = store.create_custom("two", 42);
        assert_ne!(first, second);
        assert_eq!(store.custom.len(), 2);
    }

    #[test]
    fn removing_a_track_keeps_the_order_of_the_rest() {
        let mut store = Store::default();
        let id = store.create_custom("mix", 1);
        store.add_tracks(&id, vec![track("/a.mp3"), track("/b.mp3"), track("/c.mp3")]);

        assert!(store.remove_track(&id, 1));
        let names: Vec<String> = store
            .custom_by_id(&id)
            .unwrap()
            .tracks
            .iter()
            .map(|t| t.title().to_string())
            .collect();
        assert_eq!(names, vec!["/a.mp3".to_string(), "/c.mp3".to_string()]);
        assert!(!store.remove_track(&id, 9), "out of range is a no-op");
    }

    #[test]
    fn renaming_and_cover_edits_survive_an_empty_value() {
        let mut store = Store::default();
        store.set_title("yt:PL1", "Моё имя");
        store.set_cover("yt:PL1", "/covers/x.jpg");
        assert_eq!(store.meta["yt:PL1"].title, "Моё имя");
        assert_eq!(store.meta["yt:PL1"].cover, "/covers/x.jpg");

        store.set_title("yt:PL1", "");
        assert_eq!(store.meta["yt:PL1"].title, "", "renaming back to the source");
        assert_eq!(store.meta["yt:PL1"].cover, "/covers/x.jpg", "cover kept");

        store.set_cover("yt:PL1", "");
        assert!(!store.meta.contains_key("yt:PL1"), "empty entry is dropped");
    }

    #[test]
    fn deleting_forgets_every_trace() {
        let mut store = Store::default();
        let id = store.create_custom("temp", 7);
        store.toggle_pin(&id);
        store.set_cover(&id, "/c.jpg");
        store.forget(&id);

        assert!(store.custom.is_empty());
        assert!(store.pinned.is_empty());
        assert!(store.meta.is_empty());
    }

    #[test]
    fn an_old_playlist_file_loads_its_paths_as_local_items() {
        // Exactly what the previous build wrote: no `kind`, no `linked_id`.
        let json = r#"{
            "custom": [
                {"id": "custom:1", "title": "Старый",
                 "tracks": [{"path": "C:/music/a.mp3", "title": "A", "artist": "X"},
                            {"path": "C:/music/b.flac", "title": "", "artist": ""}]}
            ]
        }"#;

        let store: Store = serde_json::from_str(json).unwrap();
        let list = store.custom_by_id("custom:1").unwrap();

        assert_eq!(list.tracks.len(), 2, "nothing is dropped");
        match &list.tracks[0] {
            PlaylistItem::Local { path, title, artist } => {
                assert_eq!(path, "C:/music/a.mp3");
                assert_eq!(title, "A");
                assert_eq!(artist, "X");
            }
            other => panic!("expected a local item, got {other:?}"),
        }
        // An empty title falls back to the file name.
        assert_eq!(list.tracks[1].title(), "b");
        assert!(list.linked_id.is_none());
    }

    #[test]
    fn items_round_trip_with_their_kind() {
        let mut store = Store::default();
        let id = store.create_custom("mix", 1);
        store.add_tracks(
            &id,
            vec![
                PlaylistItem::youtube("dQw4w9WgXcQ", "Track", "Artist", 0.0, ""),
                track("/m/a.mp3"),
            ],
        );

        let json = serde_json::to_string(&store).unwrap();
        let back: Store = serde_json::from_str(&json).unwrap();
        let list = back.custom_by_id(&id).unwrap();

        assert!(matches!(list.tracks[0], PlaylistItem::Youtube { .. }));
        assert!(list.tracks[1].is_local());
    }

    #[test]
    fn linking_and_unlinking_is_remembered() {
        let mut store = Store::default();
        let id = store.create_custom("mix", 1);

        assert!(store.set_linked(&id, Some("PLyt".to_string())));
        assert_eq!(store.custom_by_id(&id).unwrap().linked_id.as_deref(), Some("PLyt"));

        assert!(store.set_linked(&id, None));
        assert!(store.custom_by_id(&id).unwrap().linked_id.is_none());
    }

    #[test]
    fn an_old_file_without_new_fields_still_loads() {
        let store: Store = serde_json::from_str(r#"{"pinned":["a"]}"#).unwrap();
        assert_eq!(store.pinned, vec!["a".to_string()]);
        assert!(store.hidden.is_empty() && store.custom.is_empty());
    }
}
