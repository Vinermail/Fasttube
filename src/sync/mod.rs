//! Playlist sync: snapshots, the outbox and the worker that drains it.
//!
//! The pure parts (`model`, `store`, `worker`) do not know about Tauri; this
//! module is the thin layer that binds them to the app: a blocking adapter over
//! the gRPC client and the command/event surface.

pub mod api;
pub mod model;
pub mod store;
pub mod worker;

use std::sync::Arc;

pub use api::{ApiError, PlaylistApi, PlaylistRead};
pub use model::{Op, OpKind, Snapshot, SnapshotTrack};
pub use store::SnapshotStore;
pub use worker::{SyncEngine, SyncEvent, SyncStatus};

use crate::grpc;

/// Blocking adapter: the worker owns its thread, so blocking on the futures is
/// exactly what it wants.
pub struct GrpcApi;

fn classify(error: &str) -> ApiError {
    let text = error.to_lowercase();
    if text.contains("unauth")
        || text.contains("нужен вход")
        || text.contains("sign in")
        || text.contains("expired")
        || text.contains("timed out")
        || text.contains("deadline")
    {
        return ApiError::Auth(error.to_string());
    }
    if text.contains("transport")
        || text.contains("connect")
        || text.contains("timeout")
        || text.contains("unavailable")
        || text.contains("not ready")
        || text.contains("operation was cancelled")
    {
        return ApiError::Network(error.to_string());
    }
    ApiError::Permanent(error.to_string())
}

impl PlaylistApi for GrpcApi {
    fn add_items(
        &self,
        playlist_id: &str,
        video_ids: &[String],
        allow_duplicates: bool,
    ) -> Result<Vec<(String, String)>, ApiError> {
        let response = tauri::async_runtime::block_on(grpc::add_playlist_items_full(
            playlist_id,
            video_ids,
            allow_duplicates,
        ))
        .map_err(|e| classify(&e.to_string()))?;
        if !response.ok {
            // "Already there" is a success from the journal's point of view.
            if response.code == "DUPLICATE" {
                return Ok(Vec::new());
            }
            return Err(classify(&response.error));
        }
        Ok(response
            .items
            .into_iter()
            .map(|i| (i.video_id, i.set_video_id))
            .collect())
    }

    fn remove_items(&self, playlist_id: &str, items: &[(String, String)]) -> Result<(), ApiError> {
        let refs: Vec<grpc::PlaylistItemRef> = items
            .iter()
            .map(|(video_id, set_video_id)| grpc::PlaylistItemRef {
                video_id: video_id.clone(),
                set_video_id: set_video_id.clone(),
            })
            .collect();
        let response = tauri::async_runtime::block_on(grpc::remove_playlist_items(playlist_id, &refs))
            .map_err(|e| classify(&e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(classify(&response.error))
        }
    }

    fn move_item(
        &self,
        playlist_id: &str,
        set_video_id: &str,
        before_set_video_id: &str,
    ) -> Result<(), ApiError> {
        let response = tauri::async_runtime::block_on(grpc::move_playlist_item(
            playlist_id,
            set_video_id,
            before_set_video_id,
        ))
        .map_err(|e| classify(&e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(classify(&response.error))
        }
    }

    fn edit_meta(
        &self,
        playlist_id: &str,
        title: &str,
        description: &str,
        privacy: &str,
    ) -> Result<(), ApiError> {
        let response = tauri::async_runtime::block_on(grpc::edit_playlist_meta(
            playlist_id,
            title,
            description,
            privacy,
        ))
        .map_err(|e| classify(&e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(classify(&response.error))
        }
    }

    fn create_playlist(&self, title: &str) -> Result<String, ApiError> {
        let response = tauri::async_runtime::block_on(grpc::create_account_playlist(title))
            .map_err(|e| classify(&e.to_string()))?;
        if response.ok && !response.id.is_empty() {
            Ok(response.id)
        } else {
            Err(classify(&response.error))
        }
    }

    fn delete_playlist(&self, playlist_id: &str) -> Result<(), ApiError> {
        let response = tauri::async_runtime::block_on(grpc::delete_account_playlist(playlist_id))
            .map_err(|e| classify(&e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(classify(&response.error))
        }
    }

    fn rate_song(&self, video_id: &str, rating: &str) -> Result<(), ApiError> {
        let response = tauri::async_runtime::block_on(grpc::rate_song(video_id, rating))
            .map_err(|e| classify(&e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(classify(&response.error))
        }
    }

    fn library(&self, playlist_id: &str, save: bool) -> Result<(), ApiError> {
        let response = if save {
            tauri::async_runtime::block_on(grpc::save_playlist_to_library(playlist_id))
        } else {
            tauri::async_runtime::block_on(grpc::remove_from_library(playlist_id))
        }
        .map_err(|e| classify(&e.to_string()))?;
        if response.ok {
            Ok(())
        } else {
            Err(classify(&response.error))
        }
    }

    fn fetch_playlist(&self, playlist_id: &str) -> Result<PlaylistRead, ApiError> {
        let mut tracks = Vec::new();
        let mut offset = 0;
        let (total, owned) = loop {
            let page = tauri::async_runtime::block_on(grpc::get_playlist_tracks(
                playlist_id, offset, 200,
            ))
            .map_err(|e| classify(&e.to_string()))?;
            if !page.error.is_empty() {
                return Err(classify(&page.error));
            }
            let (page_total, page_owned) = (page.total, page.owned);
            let fetched = page.tracks.len() as i32;
            tracks.extend(page.tracks.into_iter().map(|t| SnapshotTrack {
                video_id: t.video_id,
                set_video_id: t.set_video_id,
                title: t.title,
                artist: t.artist,
                duration: t.duration_secs,
                thumbnail: t.thumbnail,
                like_status: t.like_status,
                temp_id: String::new(),
            }));
            offset += fetched;
            if fetched == 0 || (page_total > 0 && offset >= page_total) {
                break (page_total, page_owned);
            }
        };
        Ok(PlaylistRead {
            tracks,
            total,
            owned,
            title: String::new(),
            privacy: String::new(),
        })
    }
}

/// What the commands and the ticker thread work with: the engine plus the app
/// handle it reports through.
pub struct SyncService {
    engine: parking_lot::Mutex<SyncEngine<GrpcApi>>,
    app: tauri::AppHandle,
    /// The playlist that is on screen: it gets the ten-minute background refresh.
    open: parking_lot::Mutex<Option<String>>,
}

impl SyncService {
    pub fn new(app: tauri::AppHandle, account: String, base: std::path::PathBuf) -> Arc<Self> {
        let engine = SyncEngine::new(
            GrpcApi,
            SnapshotStore::new(base),
            account,
            Box::new(crate::now_secs),
        );
        Arc::new(Self {
            engine: parking_lot::Mutex::new(engine),
            app,
            open: parking_lot::Mutex::new(None),
        })
    }

    pub fn attach(&self) -> Vec<SyncEvent> {
        let events = self.engine.lock().replay();
        self.report(events)
    }

    pub fn set_account(&self, account: String) {
        let events = {
            let mut engine = self.engine.lock();
            engine.set_account(account);
            engine.tick()
        };
        self.report(events);
    }

    /// Signing in resumes the queue; an expired session parks it.
    pub fn set_auth_ok(&self, ok: bool) {
        let events = {
            let mut engine = self.engine.lock();
            engine.set_auth_ok(ok);
            if ok { engine.tick() } else { Vec::new() }
        };
        self.report(events);
    }

    /// Which playlist the UI is showing (None when it is closed).
    pub fn set_open(&self, playlist_id: Option<String>) {
        *self.open.lock() = playlist_id.filter(|id| !id.is_empty());
    }

    /// Called by the ticker: the open playlist is refreshed every ten minutes even
    /// when nothing the user did made it stale.
    pub fn periodic(&self) {
        const OPEN_REFRESH_SECS: f64 = 600.0;
        let open = self.open.lock().clone();
        let Some(playlist_id) = open else {
            return;
        };
        let now = crate::now_secs();
        let last = self.engine.lock().status(&playlist_id).last_synced_at;
        if last > 0.0 && now - last < OPEN_REFRESH_SECS {
            return;
        }
        self.revalidate(&playlist_id);
    }

    pub fn status(&self, playlist_id: &str) -> SyncStatus {
        self.engine.lock().status(playlist_id)
    }

    /// Snapshot first, network later: the caller already has the list on screen.
    pub fn revalidate(&self, playlist_id: &str) {
        let events = {
            let mut engine = self.engine.lock();
            if !engine.should_revalidate(playlist_id) {
                return;
            }
            engine.revalidate(playlist_id)
        };
        self.report(events);
    }

    /// Forces a re-read whatever the snapshot age is (the ↻ button).
    pub fn refresh_now(&self, playlist_id: &str) {
        let events = self.engine.lock().revalidate(playlist_id);
        self.report(events);
    }

    pub fn snapshot(&self, playlist_id: &str) -> Option<Snapshot> {
        self.engine.lock().snapshot(playlist_id)
    }

    /// Undo of a deferred removal: the operation leaves the journal and the row
    /// goes back into the snapshot without a single request to YouTube.
    pub fn undo_remove(&self, op_id: &str) -> bool {
        let events = {
            let mut engine = self.engine.lock();
            let Some(op) = engine.cancel(op_id) else {
                return false;
            };
            if op.kind == OpKind::Remove {
                let payload = &op.payload;
                let text = |key: &str| {
                    payload
                        .get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let number = |key: &str| payload.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let index = payload
                    .get("index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let track = SnapshotTrack {
                    video_id: text("video_id"),
                    set_video_id: text("set_video_id"),
                    title: text("title"),
                    artist: text("artist"),
                    duration: number("duration"),
                    thumbnail: text("thumbnail"),
                    like_status: text("like_status"),
                    temp_id: String::new(),
                };
                engine.restore_row(&op.playlist_id, index, track);
            }
            vec![SyncEvent::Status {
                id: op.playlist_id.clone(),
                status: engine.status(&op.playlist_id),
            }]
        };
        self.report(events);
        true
    }

    pub fn enqueue(&self, op: Op) {
        let events = self.engine.lock().enqueue(op);
        self.report(events);
    }

    pub fn tick(&self) {
        let events = self.engine.lock().tick();
        self.report(events);
    }

    fn report(&self, events: Vec<SyncEvent>) -> Vec<SyncEvent> {
        use tauri::Emitter;

        for event in &events {
            let (name, payload) = match event {
                SyncEvent::Synced { id, diff } => (
                    "playlist-synced",
                    serde_json::json!({
                        "id": id,
                        "added": diff.added,
                        "removed": diff.removed,
                        "moved": diff.moved,
                    }),
                ),
                SyncEvent::Status { id, status } => (
                    "sync-status",
                    serde_json::json!({
                        "id": id,
                        "state": status.state,
                        "pending": status.pending,
                        "last_synced_at": status.last_synced_at,
                        "error": status.error,
                    }),
                ),
                SyncEvent::Failed { id, error } => (
                    "playlist-sync-failed",
                    serde_json::json!({ "id": id, "error": error }),
                ),
                SyncEvent::ForeignOps { count } => (
                    "sync-foreign-ops",
                    serde_json::json!({ "count": count }),
                ),
            };
            let _ = self.app.emit(name, payload);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_and_permanent_errors_are_told_apart() {
        assert!(matches!(classify("transport error: connection refused"), ApiError::Network(_)));
        assert!(matches!(classify("operation was cancelled"), ApiError::Network(_)));
        assert!(matches!(classify("нужен вход в аккаунт YouTube"), ApiError::Auth(_)));
        assert!(matches!(classify("это сохранённый чужой плейлист"), ApiError::Permanent(_)));
        assert!(matches!(classify("YouTube отказал: STATUS_FAILED"), ApiError::Permanent(_)));
    }
}
