//! Where the snapshots and the outbox live on disk.
//!
//! The base directory is a parameter, not a global: the tests get their own.

use super::model::{Op, Snapshot};
use std::path::PathBuf;

#[derive(Clone)]
pub struct SnapshotStore {
    base: PathBuf,
}

fn safe_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl SnapshotStore {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    fn snapshot_path(&self, playlist_id: &str) -> PathBuf {
        self.base
            .join("playlists")
            .join("yt")
            .join(format!("{}.json", safe_id(playlist_id)))
    }

    fn journal_path(&self) -> PathBuf {
        self.base.join("sync").join("outbox.json")
    }

    pub fn load(&self, playlist_id: &str) -> Option<Snapshot> {
        let raw = std::fs::read_to_string(self.snapshot_path(playlist_id)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, playlist_id: &str, snapshot: &Snapshot) -> std::io::Result<()> {
        let path = self.snapshot_path(playlist_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::fsutil::write_json_atomic(&path, snapshot, false)
    }

    pub fn delete(&self, playlist_id: &str) {
        let path = self.snapshot_path(playlist_id);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("[sync] cannot drop a snapshot: {e}");
        }
    }

    /// Signing out or switching accounts: none of these belong to the next user.
    pub fn clear(&self) {
        let dir = self.base.join("playlists").join("yt");
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("[sync] cannot clear the snapshots: {e}");
        }
    }

    pub fn load_journal(&self) -> Vec<Op> {
        let Ok(raw) = std::fs::read_to_string(self.journal_path()) else {
            return Vec::new();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn save_journal(&self, ops: &[Op]) -> std::io::Result<()> {
        let path = self.journal_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::fsutil::write_json_atomic(&path, &ops, false)
    }

    #[cfg(test)]
    pub fn base(&self) -> &std::path::Path {
        &self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::SnapshotTrack;

    fn temp_store(tag: &str) -> SnapshotStore {
        let base = std::env::temp_dir().join(format!(
            "ft-sync-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        SnapshotStore::new(base)
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            title: "Мой".to_string(),
            owned: true,
            total: 1,
            tracks: vec![SnapshotTrack {
                video_id: "v1".to_string(),
                set_video_id: "sv1".to_string(),
                title: "Track".to_string(),
                ..Default::default()
            }],
            fetched_at: 10.0,
            ..Default::default()
        }
    }

    #[test]
    fn a_snapshot_survives_a_round_trip() {
        let store = temp_store("roundtrip");
        store.save("PL1", &snapshot()).unwrap();

        let back = store.load("PL1").unwrap();

        assert_eq!(back.title, "Мой");
        assert_eq!(back.tracks.len(), 1);
        assert_eq!(back.tracks[0].set_video_id, "sv1");
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn ids_with_slashes_do_not_escape_the_directory() {
        let store = temp_store("safe");
        store.save("a/../../etc", &snapshot()).unwrap();

        let files: Vec<_> = std::fs::read_dir(store.base().join("playlists").join("yt"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        assert_eq!(files.len(), 1);
        assert!(!files[0].contains('/'));
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn deleting_one_snapshot_keeps_the_others() {
        let store = temp_store("delete");
        store.save("PL1", &snapshot()).unwrap();
        store.save("PL2", &snapshot()).unwrap();

        store.delete("PL1");

        assert!(store.load("PL1").is_none());
        assert!(store.load("PL2").is_some());
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn clearing_removes_every_snapshot_but_not_the_journal() {
        let store = temp_store("clear");
        store.save("PL1", &snapshot()).unwrap();
        let op = crate::sync::model::Op::new(
            crate::sync::model::OpKind::Add,
            "PL1",
            "acc",
            serde_json::json!({}),
            1.0,
        );
        store.save_journal(&[op]).unwrap();

        store.clear();

        assert!(store.load("PL1").is_none());
        assert_eq!(store.load_journal().len(), 1);
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn the_journal_round_trips_and_a_broken_file_reads_as_empty() {
        let store = temp_store("journal");
        let op = crate::sync::model::Op::new(
            crate::sync::model::OpKind::Remove,
            "PL9",
            "acc",
            serde_json::json!({"video_id": "v"}),
            5.0,
        );
        store.save_journal(&[op.clone()]).unwrap();
        let back = store.load_journal();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].playlist_id, "PL9");

        std::fs::write(store.journal_path(), b"{not json").unwrap();
        assert!(store.load_journal().is_empty());
        let _ = std::fs::remove_dir_all(store.base());
    }
}
