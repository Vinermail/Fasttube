//! Crash-safe JSON writes.
//!
//! `settings.json`, `history.json`, `favorites.json` and `loudness.json` hold
//! user data that must never be left half-written: a plain `File::create`
//! truncates the old content before the new one is on disk, so a crash or a
//! full disk destroys both versions. Every save goes to a sibling temp file,
//! is flushed to the device, and only then replaces the destination with a
//! rename (atomic on NTFS and POSIX).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path` atomically: readers either see the previous file
/// or the complete new one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(path);
    let written = (|| -> io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        // Without the flush a crash right after the rename can leave an empty
        // file: the rename is durable before the data is.
        file.sync_all()?;
        drop(file);
        replace(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Serializes `value` on the same code path as [`write_atomic`].
pub fn write_json_atomic<T: serde::Serialize>(
    path: &Path,
    value: &T,
    pretty: bool,
) -> io::Result<()> {
    let bytes = if pretty {
        serde_json::to_vec_pretty(value)
    } else {
        serde_json::to_vec(value)
    }
    .map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

/// Temp file next to the destination: `rename` cannot cross volumes, and a
/// per-process, per-call name keeps two concurrent saves from colliding.
fn temp_path(path: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "data".to_string());
    path.with_file_name(format!(".{name}.{}.{seq}.tmp", std::process::id()))
}

fn replace(tmp: &Path, path: &Path) -> io::Result<()> {
    // Windows can hold the destination locked for a moment (indexer, scanner);
    // a few short retries are much cheaper than losing the write.
    let mut last = None;
    for attempt in 0..4u64 {
        match std::fs::rename(tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1)));
            }
        }
    }
    Err(last.expect("at least one rename attempt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fasttube-fsutil-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn replaces_existing_content_without_leaving_temp_files() {
        let dir = Dir::new("replace");
        let path = dir.0.join("history.json");
        std::fs::write(&path, "old").unwrap();

        write_atomic(&path, b"new content").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["history.json".to_string()]);
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = Dir::new("mkdir");
        let path = dir.0.join("nested").join("deeper").join("settings.json");

        write_atomic(&path, b"{}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn failed_write_leaves_the_destination_alone_and_cleans_up() {
        let dir = Dir::new("fail");
        let path = dir.0.join("blocked");
        // A directory at the destination makes the final rename fail on every
        // platform (a file cannot replace a folder).
        std::fs::create_dir_all(&path).unwrap();

        assert!(write_atomic(&path, b"lost").is_err());

        assert!(path.is_dir(), "the destination must not be touched");
        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["blocked".to_string()], "temp file left behind");
    }

    #[test]
    fn concurrent_saves_to_one_path_do_not_share_a_temp_file() {
        let dir = Dir::new("concurrent");
        let path = dir.0.join("history.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || write_atomic(&path, format!("v{i}").as_bytes()))
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap().is_ok());
        }
        // Whichever write won, the file holds one complete value.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with('v') && text.len() <= 3, "got {text:?}");
        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["history.json".to_string()]);
    }

    #[test]
    fn json_roundtrip_keeps_pretty_and_compact_modes() {
        let dir = Dir::new("json");
        let pretty = dir.0.join("settings.json");
        let compact = dir.0.join("loudness.json");
        let value = serde_json::json!({"audio_quality": "high", "n": 1});

        write_json_atomic(&pretty, &value, true).unwrap();
        write_json_atomic(&compact, &value, false).unwrap();

        let pretty_text = std::fs::read_to_string(&pretty).unwrap();
        let compact_text = std::fs::read_to_string(&compact).unwrap();
        assert!(pretty_text.contains('\n'));
        assert!(!compact_text.contains('\n'));
        let parsed: serde_json::Value = serde_json::from_str(&compact_text).unwrap();
        assert_eq!(parsed, value);
    }
}
