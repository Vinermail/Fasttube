//! The outbox worker: one thread, one queue, per-playlist order.
//!
//! Every user action lands in the snapshot immediately and in the journal; the
//! worker is the only thing that talks to YouTube, retries with backoff and
//! rolls the snapshot back when YouTube refuses for good.

use super::api::{ApiError, PlaylistApi};
use super::model::*;
use super::store::SnapshotStore;
use serde_json::Value;
use std::collections::HashMap;

/// How long a snapshot is trusted before a background re-read.
pub const STALE_AFTER_SECS: f64 = 120.0;

/// YouTube does not like a burst of writes: at most one request in this window,
/// whichever playlist it belongs to.
pub const MIN_WRITE_GAP_SECS: f64 = 0.3;

/// How many times an operation may wait for the row it points at to get a real
/// per-item id from YouTube before it is called a failure. The waits are backed
/// off like network retries, so this is a few minutes, not a few seconds.
const MAX_ID_WAITS: u32 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    Synced,
    Syncing,
    Pending,
    Error,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SyncStatus {
    pub state: SyncState,
    pub pending: u32,
    pub last_synced_at: f64,
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncEvent {
    /// YouTube changed something: `diff` says what.
    Synced { id: String, diff: Diff },
    Status { id: String, status: SyncStatus },
    /// A permanent refusal: the snapshot was rolled back.
    Failed { id: String, error: String },
    /// Operations of another account are in the journal.
    ForeignOps { count: usize },
}

pub struct SyncEngine<A: PlaylistApi> {
    api: A,
    store: SnapshotStore,
    account: String,
    ops: Vec<Op>,
    paused_auth: bool,
    last_synced: HashMap<String, f64>,
    errors: HashMap<String, String>,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    /// When the last write to YouTube started (0 = never).
    last_write_at: f64,
}

impl<A: PlaylistApi> SyncEngine<A> {
    pub fn new(api: A, store: SnapshotStore, account: String, clock: Box<dyn Fn() -> f64 + Send + Sync>) -> Self {
        Self {
            api,
            store,
            account,
            ops: Vec::new(),
            paused_auth: false,
            last_synced: HashMap::new(),
            errors: HashMap::new(),
            clock,
            last_write_at: 0.0,
        }
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    #[cfg(test)]
    pub fn api(&self) -> &A {
        &self.api
    }

    #[cfg(test)]
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Loads the journal left by the previous run. Anything that was `running`
    /// when the app died is pending again: the operation is idempotent.
    pub fn replay(&mut self) -> Vec<SyncEvent> {
        self.ops = self.store.load_journal();
        for op in &mut self.ops {
            if op.state == OpState::Running {
                op.state = OpState::Pending;
            }
        }
        self.coalesce_and_save();
        let foreign = self.ops.iter().filter(|o| o.account_id != self.account).count();
        if foreign > 0 {
            return vec![SyncEvent::ForeignOps { count: foreign }];
        }
        Vec::new()
    }

    /// A different account: its snapshots are gone, and the previous account's
    /// operations are never applied on its behalf.
    pub fn set_account(&mut self, account: String) {
        if account == self.account {
            return;
        }
        // "" means "not identified yet" (the start of a run): adopting the name
        // the session reports is not a switch and must not wipe the snapshots.
        let first_identification = self.account.is_empty();
        let switching = !first_identification && !account.is_empty();
        self.account = account;
        if switching {
            self.store.clear();
            self.last_synced.clear();
            self.errors.clear();
        }
        self.paused_auth = false;
        self.coalesce_and_save();
    }

    pub fn set_auth_ok(&mut self, ok: bool) {
        self.paused_auth = !ok;
        if ok {
            let now = self.now();
            for op in &mut self.ops {
                if op.state == OpState::Pending {
                    op.not_before = now;
                }
            }
            self.save();
        }
    }

    /// How much work is still waiting (the acceptance test asks).
    #[cfg(test)]
    pub fn pending_ops(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| op.state == OpState::Pending || op.state == OpState::Running)
            .count()
    }

    pub fn status(&self, playlist_id: &str) -> SyncStatus {
        let pending = self
            .ops
            .iter()
            .filter(|o| o.playlist_id == playlist_id && matches!(o.state, OpState::Pending | OpState::Running))
            .count() as u32;
        let error = self.errors.get(playlist_id).cloned().unwrap_or_default();
        let running = self
            .ops
            .iter()
            .any(|o| o.playlist_id == playlist_id && o.state == OpState::Running);
        let state = if !error.is_empty() {
            SyncState::Error
        } else if running {
            SyncState::Syncing
        } else if pending > 0 {
            SyncState::Pending
        } else {
            SyncState::Synced
        };
        SyncStatus {
            state,
            pending,
            last_synced_at: self.last_synced.get(playlist_id).copied().unwrap_or(0.0),
            error,
        }
    }

    pub fn snapshot(&self, playlist_id: &str) -> Option<Snapshot> {
        self.store.load(playlist_id)
    }

    /// Stale-while-revalidate: snapshots older than two minutes are read again,
    /// and never more often than once every two minutes per playlist.
    pub fn should_revalidate(&self, playlist_id: &str) -> bool {
        let now = self.now();
        if let Some(last) = self.last_synced.get(playlist_id)
            && now - last < STALE_AFTER_SECS
        {
            return false;
        }
        match self.store.load(playlist_id) {
            Some(snapshot) => snapshot.is_stale(now, STALE_AFTER_SECS),
            None => true,
        }
    }

    /// Re-reads one playlist and reports what changed without touching the queue.
    pub fn revalidate(&mut self, playlist_id: &str) -> Vec<SyncEvent> {
        let before = self.store.load(playlist_id);
        let read = match self.api.fetch_playlist(playlist_id) {
            Ok(read) => read,
            Err(ApiError::Auth(message)) => {
                self.paused_auth = true;
                return vec![SyncEvent::Failed {
                    id: playlist_id.to_string(),
                    error: message,
                }];
            }
            Err(e) => {
                return vec![SyncEvent::Failed {
                    id: playlist_id.to_string(),
                    error: e.message().to_string(),
                }];
            }
        };
        let diff = match &before {
            Some(old) => model_diff(&old.tracks, &read.tracks),
            None => Diff {
                added: read.tracks.len(),
                ..Default::default()
            },
        };
        let snapshot = Snapshot {
            title: if read.title.is_empty() {
                before.as_ref().map(|s| s.title.clone()).unwrap_or_default()
            } else {
                read.title
            },
            owned: read.owned,
            privacy: read.privacy,
            total: read.total,
            tracks: read.tracks,
            fetched_at: self.now(),
        };
        let _ = self.store.save(playlist_id, &snapshot);
        self.last_synced.insert(playlist_id.to_string(), self.now());
        self.errors.remove(playlist_id);
        vec![SyncEvent::Synced {
            id: playlist_id.to_string(),
            diff,
        }]
    }

    /// Takes a pending operation back out of the journal (the undo of a
    /// deferred removal) and returns it, so the caller can restore the row.
    pub fn cancel(&mut self, op_id: &str) -> Option<Op> {
        let position = self.ops.iter().position(|op| op.id == op_id)?;
        if self.ops[position].state != OpState::Pending {
            return None;
        }
        let op = self.ops.remove(position);
        self.save();
        Some(op)
    }

    /// Puts a row back into a playlist snapshot (undo of a removal).
    pub fn restore_row(&mut self, playlist_id: &str, index: usize, track: SnapshotTrack) -> bool {
        let Some(mut snapshot) = self.store.load(playlist_id) else {
            return false;
        };
        let already_there = if track.set_video_id.is_empty() {
            snapshot
                .tracks
                .iter()
                .any(|t| t.video_id == track.video_id && t.set_video_id.is_empty())
        } else {
            snapshot
                .tracks
                .iter()
                .any(|t| t.set_video_id == track.set_video_id)
        };
        if already_there {
            return false;
        }
        let at = index.min(snapshot.tracks.len());
        snapshot.tracks.insert(at, track);
        let _ = self.store.save(playlist_id, &snapshot);
        true
    }

    /// The user's action: applied to the snapshot now, queued for YouTube.
    pub fn enqueue(&mut self, mut op: Op) -> Vec<SyncEvent> {
        op.account_id = self.account.clone();
        self.apply_optimistically(&op);
        self.ops.push(op);
        self.coalesce_and_save();
        let id = self.ops.last().map(|o| o.playlist_id.clone()).unwrap_or_default();
        vec![SyncEvent::Status {
            id: id.clone(),
            status: self.status(&id),
        }]
    }

    /// Runs everything that is due. Stops at the first network failure per
    /// playlist so the order inside a playlist is never scrambled.
    pub fn tick(&mut self) -> Vec<SyncEvent> {
        if self.paused_auth {
            return Vec::new();
        }
        self.coalesce_and_save();
        let now = self.now();
        let mut events = Vec::new();
        let mut blocked: Vec<String> = Vec::new();
        // Every playlist that saw work gets a status line at the end, so the UI
        // can drop its "pending" badge even when nothing failed.
        let mut touched: Vec<String> = Vec::new();

        let count = self.ops.len();
        for index in 0..count {
            let (kind, playlist_id, payload, due, account, state) = {
                let op = &self.ops[index];
                (
                    op.kind,
                    op.playlist_id.clone(),
                    op.payload.clone(),
                    op.not_before,
                    op.account_id.clone(),
                    op.state,
                )
            };
            if state != OpState::Pending || due > now {
                continue;
            }
            if account != self.account {
                // Not ours: it does not even get a slot in the write window.
                blocked.push(playlist_id);
                continue;
            }
            if blocked.contains(&playlist_id) {
                continue;
            }
            // A move or a removal that names a row YouTube has not named yet
            // cannot be sent: it would either fail or hit the wrong row.
            if let Some(pending) = blocking_temp_id(kind, &payload) {
                let attempts = self.ops[index].attempts + 1;
                self.ops[index].attempts = attempts;
                if attempts > MAX_ID_WAITS {
                    self.ops[index].state = OpState::Failed;
                    let message = format!(
                        "строка {pending} так и не получила id от YouTube"
                    );
                    self.ops[index].error = message.clone();
                    self.errors.insert(playlist_id.clone(), message.clone());
                    events.push(SyncEvent::Failed {
                        id: playlist_id,
                        error: message,
                    });
                } else {
                    self.ops[index].not_before = now + backoff_secs(attempts);
                    blocked.push(playlist_id);
                }
                continue;
            }
            if !touched.contains(&playlist_id) {
                touched.push(playlist_id.clone());
            }
            // Everything the journal holds is a write, so the gap covers all of
            // it: stop here and let the next tick take the rest.
            if self.last_write_at > 0.0 && now - self.last_write_at < MIN_WRITE_GAP_SECS {
                break;
            }
            self.last_write_at = now;
            self.ops[index].state = OpState::Running;
            match self.execute(kind, &playlist_id, &payload) {
                Ok(()) => {
                    self.ops[index].state = OpState::Done;
                    self.errors.remove(&playlist_id);
                }
                Err(ApiError::Network(message)) => {
                    let attempts = self.ops[index].attempts + 1;
                    self.ops[index].attempts = attempts;
                    self.ops[index].state = OpState::Pending;
                    self.ops[index].not_before = now + backoff_secs(attempts);
                    self.ops[index].error = message;
                    blocked.push(playlist_id.clone());
                    events.push(SyncEvent::Status {
                        id: playlist_id,
                        status: self.status(&self.ops[index].playlist_id),
                    });
                }
                Err(ApiError::Auth(message)) => {
                    self.ops[index].state = OpState::Pending;
                    self.ops[index].error = message;
                    self.paused_auth = true;
                    blocked.push(playlist_id);
                }
                Err(ApiError::Permanent(message)) => {
                    self.ops[index].state = OpState::Failed;
                    self.ops[index].error = message.clone();
                    // The optimistic edit was a lie: drop the snapshot so the
                    // next open shows what YouTube actually has.
                    self.store.delete(&playlist_id);
                    self.errors.insert(playlist_id.clone(), message.clone());
                    blocked.push(playlist_id.clone());
                    events.push(SyncEvent::Failed {
                        id: playlist_id,
                        error: message,
                    });
                }
            }
        }

        self.ops.retain(|op| op.state != OpState::Done);
        self.save();
        for id in touched.iter().chain(self.errors.keys()) {
            let status = self.status(id);
            let already = events.iter().any(|e| matches!(
                e,
                SyncEvent::Status { id: seen, .. } if seen == id
            ));
            if !already {
                events.push(SyncEvent::Status {
                    id: id.clone(),
                    status,
                });
            }
        }
        events
    }

    fn execute(&mut self, kind: OpKind, playlist_id: &str, payload: &serde_json::Value) -> Result<(), ApiError> {
        match kind {
            OpKind::Add => {
                let items = payload_items(payload);
                let video_ids: Vec<String> = items
                    .iter()
                    .map(|i| i.0.clone())
                    .filter(|v| !v.is_empty())
                    .collect();
                if video_ids.is_empty() {
                    return Ok(());
                }
                let mut created = self.api.add_items(playlist_id, &video_ids, false)?;
                if created.is_empty() {
                    // YouTube said it worked but did not name the rows. Read the
                    // playlist back: the journal cannot stay on a temporary id.
                    if let Ok(read) = self.api.fetch_playlist(playlist_id) {
                        created = read
                            .tracks
                            .iter()
                            .filter(|row| {
                                !row.set_video_id.is_empty()
                                    && video_ids.iter().any(|v| v == &row.video_id)
                            })
                            .map(|row| (row.video_id.clone(), row.set_video_id.clone()))
                            .collect();
                    }
                }
                self.reconcile_add(playlist_id, &items, &created);
                Ok(())
            }
            OpKind::Remove => {
                let video_id = payload_str(payload, "video_id");
                let set_video_id = payload_str(payload, "set_video_id");
                if set_video_id.is_empty() && video_id.is_empty() {
                    return Ok(());
                }
                // The snapshot already shows the row as gone (that is the whole
                // point of the optimistic edit), so "is it still there" is not a
                // question the snapshot can answer: ask YouTube, and treat a
                // "no such row" refusal as the work being done.
                match self
                    .api
                    .remove_items(playlist_id, &[(video_id.clone(), set_video_id.clone())])
                {
                    Ok(()) => {}
                    Err(ApiError::Permanent(message)) if looks_missing(&message) => {}
                    Err(e) => return Err(e),
                }
                self.forget_row(playlist_id, &video_id, &set_video_id);
                Ok(())
            }
            OpKind::Move => {
                let moved = payload_str(payload, "set_video_id");
                let before = payload_str(payload, "before_set_video_id");
                self.api.move_item(playlist_id, &moved, &before)
            }
            OpKind::Rename => {
                let title = payload_str(payload, "title");
                self.api.edit_meta(playlist_id, &title, "", "")
            }
            OpKind::Meta => {
                let title = payload_str(payload, "title");
                let description = payload_str(payload, "description");
                let privacy = payload_str(payload, "privacy");
                self.api.edit_meta(playlist_id, &title, &description, &privacy)
            }
            OpKind::Create => {
                let title = payload_str(payload, "title");
                let temp = payload_str(payload, "temp_id");
                let real = self.api.create_playlist(&title)?;
                if !temp.is_empty() {
                    for op in &mut self.ops {
                        if op.playlist_id == temp {
                            op.playlist_id = real.clone();
                        }
                    }
                }
                Ok(())
            }
            OpKind::Delete => self.api.delete_playlist(playlist_id),
            OpKind::Like => {
                let video_id = payload_str(payload, "video_id");
                let rating = payload_str(payload, "rating");
                self.api
                    .rate_song(&video_id, if rating.is_empty() { "LIKE" } else { &rating })
            }
            OpKind::Library => {
                let save = payload
                    .get("save")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                self.api.library(playlist_id, save)
            }
        }
    }

    fn reconcile_add(&mut self, playlist_id: &str, items: &[(String, String)], created: &[(String, String)]) {
        let Some(mut snapshot) = self.store.load(playlist_id) else {
            return;
        };
        let mut rewrites: Vec<(String, String, String)> = Vec::new();
        for (video_id, set_video_id) in created {
            if let Some(row) = snapshot
                .tracks
                .iter_mut()
                .find(|t| t.video_id == *video_id && t.set_video_id.is_empty())
            {
                let temp = row.temp_id.clone();
                row.set_video_id = set_video_id.clone();
                row.temp_id.clear();
                if temp.starts_with("pending:") {
                    rewrites.push((temp, set_video_id.clone(), video_id.clone()));
                }
            }
        }
        snapshot.total = snapshot.tracks.len() as i32;
        let _ = self.store.save(playlist_id, &snapshot);
        // Anything queued behind this row was written against the temporary id:
        // the move that follows the add, the removal of a track added a second
        // ago. They only ever reach YouTube with the real per-item id.
        self.rewrite_temp_ids(&rewrites);
        let _ = items;
    }

    /// `pending:<…>` → the id YouTube gave. Applied to every operation that has
    /// not run yet: the row itself, whatever it must be moved in front of, and
    /// the temporary ids of batched adds.
    fn rewrite_temp_ids(&mut self, rewrites: &[(String, String, String)]) {
        if rewrites.is_empty() {
            return;
        }
        let mut touched = false;
        for op in self.ops.iter_mut() {
            if op.state != OpState::Pending {
                continue;
            }
            for (temp, real, video_id) in rewrites {
                touched |= rewrite_field(&mut op.payload, "set_video_id", temp, real);
                touched |= rewrite_field(&mut op.payload, "before_set_video_id", temp, real);
                touched |= rewrite_items_temp_id(&mut op.payload, temp, real, video_id);
            }
        }
        if touched {
            self.save();
        }
    }

    fn forget_row(&mut self, playlist_id: &str, video_id: &str, set_video_id: &str) {
        let Some(mut snapshot) = self.store.load(playlist_id) else {
            return;
        };
        snapshot.tracks.retain(|t| {
            if !set_video_id.is_empty() {
                t.set_video_id != set_video_id
            } else {
                t.video_id != video_id
            }
        });
        snapshot.total = snapshot.tracks.len() as i32;
        let _ = self.store.save(playlist_id, &snapshot);
    }

    /// The snapshot the user sees right after tapping: the network catches up.
    fn apply_optimistically(&mut self, op: &Op) {
        let Some(mut snapshot) = self.store.load(&op.playlist_id) else {
            return;
        };
        match op.kind {
            OpKind::Add => {
                for (video_id, temp_id) in payload_items(&op.payload) {
                    if snapshot.tracks.iter().any(|t| t.video_id == video_id && t.temp_id.is_empty()) {
                        continue;
                    }
                    snapshot.tracks.push(SnapshotTrack {
                        video_id,
                        temp_id,
                        title: payload_str(&op.payload, "title"),
                        ..Default::default()
                    });
                }
                snapshot.total = snapshot.tracks.len() as i32;
            }
            OpKind::Remove => {
                let video_id = payload_str(&op.payload, "video_id");
                let set_video_id = payload_str(&op.payload, "set_video_id");
                snapshot.tracks.retain(|t| {
                    if set_video_id.starts_with("pending:") {
                        t.temp_id != set_video_id
                    } else if !set_video_id.is_empty() {
                        t.set_video_id != set_video_id
                    } else {
                        t.video_id != video_id
                    }
                });
                snapshot.total = snapshot.tracks.len() as i32;
            }
            OpKind::Move => {
                let moved = payload_str(&op.payload, "set_video_id");
                let before = payload_str(&op.payload, "before_set_video_id");
                if let Some(index) = snapshot.tracks.iter().position(|t| t.set_video_id == moved) {
                    let row = snapshot.tracks.remove(index);
                    let target = if before.is_empty() {
                        snapshot.tracks.len()
                    } else {
                        snapshot
                            .tracks
                            .iter()
                            .position(|t| t.set_video_id == before)
                            .unwrap_or(snapshot.tracks.len())
                    };
                    snapshot.tracks.insert(target, row);
                }
            }
            _ => return,
        }
        let _ = self.store.save(&op.playlist_id, &snapshot);
    }

    fn coalesce_and_save(&mut self) {
        let merged = coalesce(std::mem::take(&mut self.ops));
        self.ops = merged;
        self.save();
    }

    fn save(&self) {
        if let Err(e) = self.store.save_journal(&self.ops) {
            eprintln!("[sync] cannot save the outbox: {e}");
        }
    }
}

/// YouTube says "this row is not in the playlist any more" in a few ways; for
/// the journal that is a success, not a failure.
fn looks_missing(message: &str) -> bool {
    let text = message.to_lowercase();
    text.contains("не найден")
        || text.contains("нет id")
        || text.contains("not found")
        || text.contains("missing")
        || text.contains("no longer")
}

/// The temporary id an operation is still pointing at, if any.
fn blocking_temp_id(kind: OpKind, payload: &serde_json::Value) -> Option<String> {
    if !matches!(kind, OpKind::Move | OpKind::Remove) {
        return None;
    }
    let mut candidates = vec![payload_str(payload, "set_video_id")];
    if kind == OpKind::Move {
        candidates.push(payload_str(payload, "before_set_video_id"));
    }
    candidates
        .into_iter()
        .find(|id| id.starts_with("pending:") && id.len() > "pending:".len())
}

fn rewrite_field(payload: &mut serde_json::Value, key: &str, from: &str, to: &str) -> bool {
    let Some(Value::String(current)) = payload.get_mut(key) else {
        return false;
    };
    if current != from {
        return false;
    }
    *current = to.to_string();
    true
}

fn rewrite_items_temp_id(
    payload: &mut serde_json::Value,
    from: &str,
    to: &str,
    video_id: &str,
) -> bool {
    let Some(items) = payload.get_mut("items").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut touched = false;
    for item in items {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        let id_matches = obj
            .get("video_id")
            .and_then(|v| v.as_str())
            .map(|v| v == video_id)
            .unwrap_or(false);
        let temp_matches = obj
            .get("temp_id")
            .and_then(|v| v.as_str())
            .map(|v| v == from)
            .unwrap_or(false);
        if temp_matches && id_matches {
            obj.insert("temp_id".to_string(), Value::String(to.to_string()));
            touched = true;
        }
    }
    touched
}

fn payload_str(payload: &serde_json::Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `(video_id, temp_id)` pairs of an add operation.
fn payload_items(payload: &serde_json::Value) -> Vec<(String, String)> {
    payload
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    (
                        item.get("video_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        item.get("temp_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn model_diff(old: &[SnapshotTrack], new: &[SnapshotTrack]) -> Diff {
    crate::sync::model::diff(old, new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::api::fake::FakeApi;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn store(tag: &str) -> SnapshotStore {
        let base = std::env::temp_dir().join(format!(
            "ft-worker-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        SnapshotStore::new(base)
    }

    struct Clock(Arc<AtomicU64>);

    impl Clock {
        fn new(start: u64) -> (Self, Arc<AtomicU64>) {
            let value = Arc::new(AtomicU64::new(start));
            (Self(value.clone()), value)
        }
    }

    impl Clock {
        fn closure(&self) -> Box<dyn Fn() -> f64 + Send + Sync> {
            let value = self.0.clone();
            Box::new(move || value.load(Ordering::Relaxed) as f64)
        }

    }

    fn engine(tag: &str, api: FakeApi) -> (SyncEngine<FakeApi>, Arc<AtomicU64>, SnapshotStore) {
        let store = store(tag);
        let (clock, value) = Clock::new(1000);
        let engine = SyncEngine::new(api, store.clone(), "acc".to_string(), clock.closure());
        (engine, value, store)
    }

    fn snapshot(tracks: &[(&str, &str)]) -> Snapshot {
        Snapshot {
            title: "P".to_string(),
            owned: true,
            total: tracks.len() as i32,
            tracks: tracks
                .iter()
                .map(|(video, set)| SnapshotTrack {
                    video_id: video.to_string(),
                    set_video_id: set.to_string(),
                    title: video.to_string(),
                    ..Default::default()
                })
                .collect(),
            fetched_at: 1000.0,
            ..Default::default()
        }
    }

    fn add_op(playlist: &str, items: &[(&str, &str)]) -> Op {
        let items: Vec<serde_json::Value> = items
            .iter()
            .map(|(video, temp)| serde_json::json!({"video_id": video, "temp_id": temp}))
            .collect();
        Op::new(OpKind::Add, playlist, "acc", serde_json::json!({"items": items}), 1000.0)
    }

    #[test]
    fn operations_run_in_the_order_they_were_queued() {
        let api = FakeApi::default();
        let (mut engine, clock, store) = engine("order", api);
        store.save("PL", &snapshot(&[("a", "sa")])).unwrap();

        engine.enqueue(add_op("PL", &[("x", "pending:1")]));
        let mut remove = Op::new(
            OpKind::Remove,
            "PL",
            "acc",
            serde_json::json!({"video_id": "a", "set_video_id": "sa"}),
            1000.0,
        );
        remove.kind = OpKind::Remove;
        engine.enqueue(remove);

        let events = engine.tick();
        // One write per window: the removal waits for the next tick.
        assert_eq!(engine.api().calls(), vec!["add:PL:x:dup=false"]);
        assert!(!events.is_empty());

        clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        engine.tick();

        assert_eq!(engine.api().calls(), vec!["add:PL:x:dup=false", "remove:PL:a"]);
        assert!(engine.ops().is_empty(), "both finished");
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn a_network_failure_backs_off_and_is_retried_later() {
        let api = FakeApi::default();
        api.remove.lock().unwrap().push_back(Err(ApiError::Network("offline".into())));
        let (mut engine, clock, store) = engine("backoff", api);
        store.save("PL", &snapshot(&[("a", "sa")])).unwrap();

        engine.enqueue(Op::new(
            OpKind::Remove,
            "PL",
            "acc",
            serde_json::json!({"video_id": "a", "set_video_id": "sa"}),
            1000.0,
        ));
        engine.tick();

        assert_eq!(engine.ops().len(), 1);
        assert_eq!(engine.ops()[0].attempts, 1);
        assert!(engine.ops()[0].not_before > 1000.0, "not before a backoff");

        // Too early: nothing is sent.
        engine.tick();
        assert_eq!(engine.api().calls().len(), 1);

        // After the pause it goes out again and succeeds.
        clock.store(1000 + 5, Ordering::Relaxed);
        engine.tick();
        assert_eq!(engine.api().calls().len(), 2);
        assert!(engine.ops().is_empty());
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn two_adds_become_one_batched_request() {
        let api = FakeApi::default();
        let (mut engine, _clock, store) = engine("coalesce", api);
        store.save("PL", &snapshot(&[])).unwrap();

        engine.enqueue(add_op("PL", &[("a", "pending:1")]));
        engine.enqueue(add_op("PL", &[("b", "pending:2")]));
        assert_eq!(engine.ops().len(), 1, "merged before any tick");

        engine.tick();

        assert_eq!(engine.api().calls(), vec!["add:PL:a,b:dup=false"]);
        let saved = engine.snapshot("PL").unwrap();
        assert_eq!(saved.tracks.len(), 2);
        assert!(saved.tracks.iter().all(|t| t.temp_id.is_empty()), "{saved:?}");
        assert_eq!(saved.tracks[0].set_video_id, "sv-a");
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn a_permanent_refusal_rolls_the_snapshot_back() {
        let api = FakeApi::default();
        api.remove
            .lock()
            .unwrap()
            .push_back(Err(ApiError::Permanent("чужой плейлист".into())));
        let (mut engine, _clock, store) = engine("rollback", api);
        store.save("PL", &snapshot(&[("a", "sa")])).unwrap();

        engine.enqueue(Op::new(
            OpKind::Remove,
            "PL",
            "acc",
            serde_json::json!({"video_id": "a", "set_video_id": "sa"}),
            1000.0,
        ));
        let events = engine.tick();

        assert!(engine.snapshot("PL").is_none(), "the snapshot must be dropped");
        assert_eq!(engine.ops()[0].state, OpState::Failed);
        assert!(events.iter().any(|e| matches!(e, SyncEvent::Failed { .. })));
        assert_eq!(engine.status("PL").state, SyncState::Error);
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn a_crash_leaves_the_operation_pending_and_replay_runs_it() {
        let api = FakeApi::default();
        let (mut engine, _clock, store) = engine("crash", api);
        store.save("PL", &snapshot(&[("a", "sa")])).unwrap();
        let mut op = Op::new(
            OpKind::Remove,
            "PL",
            "acc",
            serde_json::json!({"video_id": "a", "set_video_id": "sa"}),
            1000.0,
        );
        op.state = OpState::Running; // died mid-flight
        store.save_journal(&[op]).unwrap();

        engine.replay();
        assert_eq!(engine.ops()[0].state, OpState::Pending);

        engine.tick();

        assert_eq!(engine.api().calls(), vec!["remove:PL:a"]);
        assert!(engine.ops().is_empty());
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn another_accounts_operations_are_not_applied() {
        let api = FakeApi::default();
        let (mut engine, _clock, store) = engine("account", api);
        store.save("PL", &snapshot(&[("a", "sa")])).unwrap();
        let mut op = Op::new(
            OpKind::Remove,
            "PL",
            "other",
            serde_json::json!({"video_id": "a", "set_video_id": "sa"}),
            1000.0,
        );
        op.account_id = "other".to_string();
        store.save_journal(&[op]).unwrap();

        let events = engine.replay();
        assert!(events.iter().any(|e| matches!(e, SyncEvent::ForeignOps { count: 1 })));

        engine.tick();
        assert!(engine.api().calls().is_empty(), "nothing for another account");
        assert_eq!(engine.ops().len(), 1);

        engine.set_account("other".to_string());
        engine.tick();
        assert_eq!(engine.api().calls(), vec!["remove:PL:a"]);
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn removing_something_youtube_no_longer_has_is_done() {
        let api = FakeApi::default();
        api.remove
            .lock()
            .unwrap()
            .push_back(Err(ApiError::Permanent("строка не найдена в плейлисте".into())));
        let (mut engine, _clock, store) = engine("idempotent", api);
        store.save("PL", &snapshot(&[("b", "sb")])).unwrap();

        engine.enqueue(Op::new(
            OpKind::Remove,
            "PL",
            "acc",
            serde_json::json!({"video_id": "a", "set_video_id": "sa"}),
            1000.0,
        ));
        engine.tick();

        assert_eq!(engine.api().calls(), vec!["remove:PL:a"]);
        assert!(engine.ops().is_empty(), "not a failure: the row is gone");
        assert_eq!(engine.status("PL").state, SyncState::Synced);
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn adopting_the_account_at_startup_keeps_the_snapshots() {
        let api = FakeApi::default();
        let store = store("identify");
        let (clock, _value) = Clock::new(1000);
        let mut engine = SyncEngine::new(api, store.clone(), String::new(), clock.closure());
        store.save("PL", &snapshot(&[("a", "sa")])).unwrap();

        engine.set_account("acc".to_string());
        assert!(engine.snapshot("PL").is_some(), "not a switch: nothing to clear");

        engine.set_account("other".to_string());
        assert!(
            engine.snapshot("PL").is_none(),
            "a different account must not see the previous snapshots"
        );
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn writes_keep_a_gap_between_them() {
        let api = FakeApi::default();
        let store = store("pace");
        store.save("PL", &snapshot(&[("a", "sa"), ("b", "sb"), ("c", "sc")])).unwrap();

        // A clock that can express fractions of a second.
        let millis = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1_000_000));
        let clock = {
            let value = millis.clone();
            Box::new(move || value.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0)
        };
        let mut engine = SyncEngine::new(api, store.clone(), "acc".to_string(), clock);

        for id in ["a", "b", "c"] {
            engine.enqueue(Op::new(
                OpKind::Remove,
                "PL",
                "acc",
                serde_json::json!({"video_id": id, "set_video_id": format!("s{id}")}),
                1000.0,
            ));
        }

        engine.tick();
        assert_eq!(engine.api().calls().len(), 1, "one write per window");

        millis.store(1_000_100, std::sync::atomic::Ordering::Relaxed);
        engine.tick();
        assert_eq!(engine.api().calls().len(), 1, "still inside the window");

        millis.store(1_000_400, std::sync::atomic::Ordering::Relaxed);
        engine.tick();
        assert_eq!(engine.api().calls().len(), 2);

        millis.store(1_000_800, std::sync::atomic::Ordering::Relaxed);
        engine.tick();
        assert_eq!(engine.api().calls().len(), 3);
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn an_offline_add_then_move_sends_the_real_id() {
        let api = FakeApi::default();
        api.add
            .lock()
            .unwrap()
            .push_back(Err(ApiError::Network("offline".into())));
        let (mut engine, clock, store) = engine("temp-move", api);
        store.save("PL", &snapshot(&[])).unwrap();

        engine.enqueue(add_op("PL", &[("x", "pending:p1")]));
        engine.enqueue(Op::new(
            OpKind::Move,
            "PL",
            "acc",
            serde_json::json!({"set_video_id": "pending:p1", "before_set_video_id": ""}),
            1000.0,
        ));
        engine.tick();
        assert!(
            engine.api().calls().iter().all(|c| !c.starts_with("move:")),
            "the move must not be sent while the row has no id: {:?}",
            engine.api().calls()
        );

        // The network is back: the add lands, the journal learns the real id and
        // the move goes out with it.
        clock.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        engine.tick();
        clock.fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        engine.tick();

        let calls = engine.api().calls();
        assert!(
            calls.contains(&"move:PL:sv-x->".to_string()),
            "the move was not rewritten: {calls:?}"
        );
        assert!(
            calls.iter().all(|c| !c.contains("pending:")),
            "a temporary id reached YouTube: {calls:?}"
        );
        assert_eq!(engine.status("PL").state, SyncState::Synced);
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn an_add_without_ids_is_reconciled_by_reading_the_playlist() {
        let api = FakeApi::default();
        // The answer names no rows; the playlist read does.
        api.add.lock().unwrap().push_back(Ok(Vec::new()));
        api.read.lock().unwrap().push_back(Ok(crate::sync::api::PlaylistRead {
            tracks: vec![SnapshotTrack {
                video_id: "x".to_string(),
                set_video_id: "sv-x".to_string(),
                title: "X".to_string(),
                ..Default::default()
            }],
            total: 1,
            owned: true,
            title: "P".to_string(),
            privacy: String::new(),
        }));
        let (mut engine, clock, store) = engine("reconcile-read", api);
        store.save("PL", &snapshot(&[])).unwrap();

        engine.enqueue(add_op("PL", &[("x", "pending:p2")]));
        engine.enqueue(Op::new(
            OpKind::Move,
            "PL",
            "acc",
            serde_json::json!({"set_video_id": "pending:p2", "before_set_video_id": ""}),
            1000.0,
        ));

        engine.tick();
        // The write window is 300 ms wide: the next request needs a later tick.
        clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        engine.tick();

        let calls = engine.api().calls();
        assert!(
            calls.iter().any(|c| c == "read:PL"),
            "the add had to be reconciled by reading the playlist: {calls:?}"
        );
        let row = engine
            .snapshot("PL")
            .expect("snapshot")
            .tracks
            .into_iter()
            .find(|t| t.video_id == "x")
            .expect("row");
        assert_eq!(row.set_video_id, "sv-x", "the snapshot learned the id");
        assert!(row.temp_id.is_empty());
        assert!(
            calls.contains(&"move:PL:sv-x->".to_string()),
            "the queued move did not get the real id: {calls:?}"
        );
        assert!(
            calls.iter().all(|c| !c.contains("pending:")),
            "a temporary id reached YouTube: {calls:?}"
        );
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn a_move_for_a_row_that_never_gets_an_id_gives_up() {
        let api = FakeApi::default();
        let (mut engine, clock, store) = engine("temp-never", api);
        store.save("PL", &snapshot(&[])).unwrap();
        engine.enqueue(Op::new(
            OpKind::Move,
            "PL",
            "acc",
            serde_json::json!({"set_video_id": "pending:lost", "before_set_video_id": ""}),
            1000.0,
        ));

        for _ in 0..=MAX_ID_WAITS {
            engine.tick();
            clock.fetch_add(65, std::sync::atomic::Ordering::Relaxed);
        }

        let status = engine.status("PL");
        assert_eq!(status.state, SyncState::Error, "{status:?}");
        assert!(
            status.error.contains("pending:lost"),
            "the message names the row: {}",
            status.error
        );
        assert!(
            engine.api().calls().iter().all(|c| !c.starts_with("move:")),
            "a temporary id was sent: {:?}",
            engine.api().calls()
        );
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn a_deferred_removal_waits_and_can_be_taken_back() {
        let api = FakeApi::default();
        let (mut engine, clock, store) = engine("undo", api);
        store.save("PL", &snapshot(&[("a", "sa"), ("b", "sb")])).unwrap();

        let mut op = Op::new(
            OpKind::Remove,
            "PL",
            "acc",
            serde_json::json!({
                "video_id": "a",
                "set_video_id": "sa",
                "title": "A",
                "index": 0,
            }),
            1000.0,
        );
        op.not_before = 1008.0;
        let id = op.id.clone();
        engine.enqueue(op);

        engine.tick();
        assert!(
            engine.api().calls().is_empty(),
            "a deferred removal must not touch the network yet"
        );
        let taken = engine.cancel(&id).expect("still pending");
        assert_eq!(taken.playlist_id, "PL");
        assert!(engine.restore_row(
            "PL",
            0,
            SnapshotTrack {
                video_id: "a".into(),
                set_video_id: "sa".into(),
                title: "A".into(),
                ..Default::default()
            },
        ));

        let snapshot = engine.snapshot("PL").unwrap();
        assert_eq!(snapshot.tracks.len(), 2, "the row is back where it was");
        assert_eq!(snapshot.tracks[0].video_id, "a");
        assert!(engine.api().calls().is_empty(), "undo is local");
        assert!(engine.cancel(&id).is_none(), "a second undo is a no-op");
        let _ = clock;
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn revalidation_reports_what_moved() {
        use crate::sync::api::PlaylistRead;

        let api = FakeApi::default();
        api.read.lock().unwrap().push_back(Ok(PlaylistRead {
            tracks: vec![
                SnapshotTrack {
                    video_id: "b".to_string(),
                    set_video_id: "sb".to_string(),
                    ..Default::default()
                },
                SnapshotTrack {
                    video_id: "a".to_string(),
                    set_video_id: "sa".to_string(),
                    ..Default::default()
                },
            ],
            total: 2,
            owned: true,
            title: "P".to_string(),
            privacy: String::new(),
        }));
        let (mut engine, _clock, store) = engine("revalidate", api);
        store.save("PL", &snapshot(&[("a", "sa"), ("b", "sb")])).unwrap();

        let events = engine.revalidate("PL");

        let diff = events
            .iter()
            .find_map(|e| match e {
                SyncEvent::Synced { diff, .. } => Some(diff.clone()),
                _ => None,
            })
            .expect("a synced event");
        assert_eq!(diff.added, 0);
        assert_eq!(diff.removed, 0);
        assert!(diff.moved > 0);
        assert_eq!(engine.snapshot("PL").unwrap().tracks[0].video_id, "b");
        let _ = std::fs::remove_dir_all(store.base());
    }

    #[test]
    fn a_fresh_snapshot_is_not_read_again_immediately() {
        let api = FakeApi::default();
        let (engine, _clock, store) = engine("fresh", api);
        store.save("PL", &snapshot(&[("a", "sa")])).unwrap();

        // fetched_at is 1000 in the fixture and the clock reads 1000.
        assert!(!engine.should_revalidate("PL"));
        assert!(engine.should_revalidate("PL-unknown"));
        let _ = std::fs::remove_dir_all(store.base());
    }
}
