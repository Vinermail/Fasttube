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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CustomTrack {
    pub path: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CustomPlaylist {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub cover: String,
    #[serde(default)]
    pub tracks: Vec<CustomTrack>,
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
            tracks: Vec::new(),
        });
        id
    }

    /// Appends tracks, skipping ones the playlist already has. Returns how many
    /// were actually added.
    pub fn add_tracks(&mut self, id: &str, tracks: Vec<CustomTrack>) -> usize {
        let Some(list) = self.custom_by_id_mut(id) else {
            return 0;
        };
        let mut added = 0;
        for track in tracks {
            if track.path.is_empty() || list.tracks.iter().any(|t| t.path == track.path) {
                continue;
            }
            list.tracks.push(track);
            added += 1;
        }
        added
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

    fn track(path: &str) -> CustomTrack {
        CustomTrack {
            path: path.to_string(),
            title: path.to_string(),
            artist: String::new(),
        }
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
            .map(|t| t.title.clone())
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
    fn an_old_file_without_new_fields_still_loads() {
        let store: Store = serde_json::from_str(r#"{"pinned":["a"]}"#).unwrap();
        assert_eq!(store.pinned, vec!["a".to_string()]);
        assert!(store.hidden.is_empty() && store.custom.is_empty());
    }
}
