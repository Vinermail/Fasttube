//! Pure sync logic: snapshots, diffs, the outbox journal and its coalescing.
//!
//! Nothing here knows about Tauri, gRPC or the network: the worker takes a
//! [`PlaylistApi`] so the whole thing is testable against a fake.

use serde::{Deserialize, Serialize};

/// One playlist row as the app remembers it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SnapshotTrack {
    #[serde(default)]
    pub video_id: String,
    /// YouTube's per-item id; "" for a row added optimistically.
    #[serde(default)]
    pub set_video_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub thumbnail: String,
    #[serde(default)]
    pub like_status: String,
    /// Set while the row exists only locally (an add that has not landed yet).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub temp_id: String,
}

/// A playlist as last seen from YouTube (plus optimistic edits).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Snapshot {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub owned: bool,
    #[serde(default)]
    pub privacy: String,
    #[serde(default)]
    pub total: i32,
    #[serde(default)]
    pub tracks: Vec<SnapshotTrack>,
    #[serde(default)]
    pub fetched_at: f64,
}

impl Snapshot {
    pub fn is_stale(&self, now: f64, after_secs: f64) -> bool {
        now - self.fetched_at > after_secs
    }
}

/// What changed between two reads of the same playlist.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Diff {
    pub added: usize,
    pub removed: usize,
    pub moved: usize,
}

/// Identity of a row: YouTube's per-item id when we have it, otherwise the
/// video id plus its position (the same song may legitimately appear twice).
fn row_key(track: &SnapshotTrack, index: usize) -> String {
    if !track.set_video_id.is_empty() {
        return format!("s:{}", track.set_video_id);
    }
    if !track.temp_id.is_empty() {
        return format!("t:{}", track.temp_id);
    }
    format!("v:{}#{}", track.video_id, index)
}

/// Compares two snapshots of one playlist.
///
/// Rows are matched by `set_video_id` (falling back to video id + position),
/// so a re-read that only reorders things reports moves, not removes + adds.
pub fn diff(old: &[SnapshotTrack], new: &[SnapshotTrack]) -> Diff {
    use std::collections::HashMap;

    let old_keys: Vec<String> = old.iter().enumerate().map(|(i, t)| row_key(t, i)).collect();
    let new_keys: Vec<String> = new.iter().enumerate().map(|(i, t)| row_key(t, i)).collect();

    let old_pos: HashMap<&String, usize> = old_keys.iter().enumerate().map(|(i, k)| (k, i)).collect();
    let new_set: HashMap<&String, usize> = new_keys.iter().enumerate().map(|(i, k)| (k, i)).collect();

    let added = new_keys.iter().filter(|k| !old_pos.contains_key(k)).count();
    let removed = old_keys.iter().filter(|k| !new_set.contains_key(k)).count();

    // Rows present in both: how many of them changed relative order?
    // Map each shared row to its position in the new list, in the old order,
    // then everything outside the longest increasing subsequence has moved.
    let sequence: Vec<usize> = old_keys
        .iter()
        .filter_map(|key| new_set.get(key).copied())
        .collect();
    let moved = sequence.len() - longest_increasing(&sequence);

    Diff {
        added,
        removed,
        moved,
    }
}

/// Length of the longest strictly increasing subsequence (patience sorting).
fn longest_increasing(values: &[usize]) -> usize {
    let mut tails: Vec<usize> = Vec::new();
    for &value in values {
        match tails.binary_search(&value) {
            Ok(_) => {}
            Err(pos) => {
                if pos == tails.len() {
                    tails.push(value);
                } else {
                    tails[pos] = value;
                }
            }
        }
    }
    tails.len()
}

/// Kinds of work the journal can hold.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Add,
    Remove,
    Move,
    Rename,
    Meta,
    Create,
    Delete,
    Like,
    Library,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    #[default]
    Pending,
    Running,
    Failed,
    Done,
}

/// One unit of work for the outbox.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Op {
    pub id: String,
    /// Which account this belonged to; another account's ops are never applied.
    #[serde(default)]
    pub account_id: String,
    pub kind: OpKind,
    #[serde(default)]
    pub playlist_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub not_before: f64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub state: OpState,
    #[serde(default)]
    pub error: String,
}

impl Op {
    pub fn new(kind: OpKind, playlist_id: &str, account_id: &str, payload: serde_json::Value, now: f64) -> Op {
        Op {
            id: format!("op:{}", temp_suffix(now)),
            account_id: account_id.to_string(),
            kind,
            playlist_id: playlist_id.to_string(),
            payload,
            created_at: now,
            not_before: now,
            attempts: 0,
            state: OpState::Pending,
            error: String::new(),
        }
    }
}

/// Identifier for rows that exist only locally until YouTube answers.
pub fn temp_id(seed: f64) -> String {
    format!("pending:{}", temp_suffix(seed))
}

fn temp_suffix(seed: f64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}", (seed * 1000.0) as u64, seq)
}

/// Backoff for the n-th retry: 1, 2, 4, … capped at 60 s.
pub fn backoff_secs(attempts: u32) -> f64 {
    let step = 2f64.powi(attempts.min(6) as i32);
    step.min(60.0)
}

/// Collapses the journal so the same playlist is not asked to do redundant work.
///
/// - `add(a)` followed by `remove(a)` disappears entirely (with a temp id, of
///   course: a real removal is a separate intent).
/// - consecutive adds merge into one batched add.
pub fn coalesce(ops: Vec<Op>) -> Vec<Op> {
    let mut out: Vec<Op> = Vec::with_capacity(ops.len());
    for op in ops {
        match op.kind {
            OpKind::Add => {
                let merged = out.last_mut().filter(|last| {
                    last.kind == OpKind::Add
                        && last.playlist_id == op.playlist_id
                        && last.state == OpState::Pending
                        && last.account_id == op.account_id
                });
                match merged {
                    Some(last) => merge_add(last, op),
                    None => out.push(op),
                }
            }
            OpKind::Remove => {
                // Removing a track that was only ever added locally cancels the
                // add instead of touching YouTube.
                let cancelled = out
                    .iter()
                    .rposition(|last| {
                        last.kind == OpKind::Add
                            && last.playlist_id == op.playlist_id
                            && last.state == OpState::Pending
                    })
                    .filter(|index| add_contains_temp(&out[*index], &op));
                match cancelled {
                    Some(index) => drop_temp_from_add(&mut out[index], &op),
                    None => out.push(op),
                }
            }
            _ => out.push(op),
        }
    }
    out.retain(|op| !(op.kind == OpKind::Add && add_is_empty(op)));
    out
}

fn add_items(op: &Op) -> Vec<serde_json::Value> {
    op.payload
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn add_contains_temp(op: &Op, remove: &Op) -> bool {
    let Some(set_video_id) = remove
        .payload
        .get("set_video_id")
        .and_then(|v| v.as_str())
        .filter(|s| s.starts_with("pending:"))
    else {
        return false;
    };
    add_items(op).iter().any(|item| {
        item.get("temp_id").and_then(|v| v.as_str()) == Some(set_video_id)
    })
}

fn drop_temp_from_add(add: &mut Op, remove: &Op) {
    let Some(set_video_id) = remove.payload.get("set_video_id").and_then(|v| v.as_str()) else {
        return;
    };
    let kept: Vec<serde_json::Value> = add_items(add)
        .into_iter()
        .filter(|item| item.get("temp_id").and_then(|v| v.as_str()) != Some(set_video_id))
        .collect();
    if let Some(obj) = add.payload.as_object_mut() {
        obj.insert("items".to_string(), serde_json::Value::Array(kept));
    }
}

fn merge_add(into: &mut Op, other: Op) {
    let mut items = add_items(into);
    for item in add_items(&other) {
        if !items.contains(&item) {
            items.push(item);
        }
    }
    if let Some(obj) = into.payload.as_object_mut() {
        obj.insert("items".to_string(), serde_json::Value::Array(items));
    }
}

fn add_is_empty(op: &Op) -> bool {
    add_items(op).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(video_id: &str, set_video_id: &str) -> SnapshotTrack {
        SnapshotTrack {
            video_id: video_id.to_string(),
            set_video_id: set_video_id.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_reordered_playlist_reports_moves_not_adds() {
        let old = vec![row("a", "sa"), row("b", "sb"), row("c", "sc")];
        let new = vec![row("b", "sb"), row("a", "sa"), row("c", "sc")];

        let d = diff(&old, &new);

        assert_eq!(d.added, 0);
        assert_eq!(d.removed, 0);
        assert!(d.moved > 0, "the first two rows swapped: {d:?}");
    }

    #[test]
    fn added_and_removed_are_counted_by_set_video_id() {
        let old = vec![row("a", "sa"), row("b", "sb")];
        let new = vec![row("b", "sb"), row("c", "sc"), row("d", "sd")];

        let d = diff(&old, &new);

        assert_eq!(d.added, 2);
        assert_eq!(d.removed, 1);
    }

    #[test]
    fn rows_without_a_set_video_id_fall_back_to_video_and_position() {
        let old = vec![row("a", ""), row("a", "")];
        let new = vec![row("a", "")];

        let d = diff(&old, &new);

        assert_eq!(d.removed, 1, "two copies became one");
        assert_eq!(d.added, 0);
    }

    #[test]
    fn an_identical_snapshot_shows_nothing() {
        let rows = vec![row("a", "sa"), row("b", "sb")];
        let d = diff(&rows, &rows);
        assert_eq!((d.added, d.removed, d.moved), (0, 0, 0));
    }

    fn add_op(playlist: &str, items: &[(&str, &str)]) -> Op {
        let items: Vec<serde_json::Value> = items
            .iter()
            .map(|(video, temp)| serde_json::json!({"video_id": video, "temp_id": temp}))
            .collect();
        Op::new(
            OpKind::Add,
            playlist,
            "acc",
            serde_json::json!({"items": items}),
            1.0,
        )
    }

    fn remove_op(playlist: &str, video: &str, set_video_id: &str) -> Op {
        Op::new(
            OpKind::Remove,
            playlist,
            "acc",
            serde_json::json!({"video_id": video, "set_video_id": set_video_id}),
            2.0,
        )
    }

    #[test]
    fn consecutive_adds_merge_into_one_batch() {
        let ops = vec![
            add_op("PL", &[("a", "pending:1")]),
            add_op("PL", &[("b", "pending:2")]),
        ];

        let merged = coalesce(ops);

        assert_eq!(merged.len(), 1);
        assert_eq!(add_items(&merged[0]).len(), 2);
    }

    #[test]
    fn adding_then_removing_a_local_row_cancels_both() {
        let ops = vec![
            add_op("PL", &[("a", "pending:1")]),
            remove_op("PL", "a", "pending:1"),
        ];

        assert!(coalesce(ops).is_empty());
    }

    #[test]
    fn removing_a_real_row_keeps_the_removal() {
        let ops = vec![
            add_op("PL", &[("a", "pending:1")]),
            remove_op("PL", "b", "sb"),
        ];

        let kept = coalesce(ops);

        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].kind, OpKind::Remove);
    }

    #[test]
    fn adds_for_different_playlists_are_not_merged() {
        let ops = vec![add_op("PL1", &[("a", "pending:1")]), add_op("PL2", &[("b", "pending:2")])];
        assert_eq!(coalesce(ops).len(), 2);
    }

    #[test]
    fn backoff_grows_and_stops_at_a_minute() {
        assert_eq!(backoff_secs(0), 1.0);
        assert_eq!(backoff_secs(1), 2.0);
        assert_eq!(backoff_secs(2), 4.0);
        assert_eq!(backoff_secs(10), 60.0);
    }

    #[test]
    fn temporary_ids_do_not_collide() {
        assert_ne!(temp_id(1.0), temp_id(1.0));
        assert!(temp_id(1.0).starts_with("pending:"));
    }
}
