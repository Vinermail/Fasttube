//! The seam between the sync engine and the network.
//!
//! Methods are synchronous on purpose: the worker runs on its own thread, the
//! real implementation blocks on the gRPC futures there, and the tests get a
//! fake with no runtime at all.

use super::model::SnapshotTrack;

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// Worth retrying later: the network, the proxy or the sidecar.
    Network(String),
    /// Nothing to do until the user signs in again.
    Auth(String),
    /// Retrying is pointless: not owned, refused by YouTube, bad request.
    Permanent(String),
}

impl ApiError {
    pub fn message(&self) -> &str {
        match self {
            ApiError::Network(m) | ApiError::Auth(m) | ApiError::Permanent(m) => m,
        }
    }
}

/// What a playlist read gives back.
pub struct PlaylistRead {
    pub tracks: Vec<SnapshotTrack>,
    pub total: i32,
    pub owned: bool,
    pub title: String,
    pub privacy: String,
}

/// Everything the journal needs to talk to YouTube.
pub trait PlaylistApi: Send + Sync {
    /// Returns the created rows as `(video_id, set_video_id)`.
    fn add_items(
        &self,
        playlist_id: &str,
        video_ids: &[String],
        allow_duplicates: bool,
    ) -> Result<Vec<(String, String)>, ApiError>;
    fn remove_items(&self, playlist_id: &str, items: &[(String, String)]) -> Result<(), ApiError>;
    fn move_item(
        &self,
        playlist_id: &str,
        set_video_id: &str,
        before_set_video_id: &str,
    ) -> Result<(), ApiError>;
    fn edit_meta(
        &self,
        playlist_id: &str,
        title: &str,
        description: &str,
        privacy: &str,
    ) -> Result<(), ApiError>;
    fn create_playlist(&self, title: &str) -> Result<String, ApiError>;
    fn delete_playlist(&self, playlist_id: &str) -> Result<(), ApiError>;
    fn rate_song(&self, video_id: &str, rating: &str) -> Result<(), ApiError>;
    fn library(&self, playlist_id: &str, save: bool) -> Result<(), ApiError>;
    fn fetch_playlist(&self, playlist_id: &str) -> Result<PlaylistRead, ApiError>;
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted stand-in for the sidecar: every verb answers from a deque, then
    /// falls back to success. Every call is recorded in order.
    #[derive(Default)]
    pub struct FakeApi {
        pub calls: Mutex<Vec<String>>,
        pub add: Mutex<VecDeque<Result<Vec<(String, String)>, ApiError>>>,
        pub remove: Mutex<VecDeque<Result<(), ApiError>>>,
        pub move_: Mutex<VecDeque<Result<(), ApiError>>>,
        pub meta: Mutex<VecDeque<Result<(), ApiError>>>,
        pub create: Mutex<VecDeque<Result<String, ApiError>>>,
        pub delete: Mutex<VecDeque<Result<(), ApiError>>>,
        pub rate: Mutex<VecDeque<Result<(), ApiError>>>,
        pub library: Mutex<VecDeque<Result<(), ApiError>>>,
        pub read: Mutex<VecDeque<Result<PlaylistRead, ApiError>>>,
    }

    fn pop<T: Clone>(slot: &Mutex<VecDeque<Result<T, ApiError>>>, fallback: T) -> Result<T, ApiError> {
        let mut guard = slot.lock().expect("poisoned");
        guard.pop_front().unwrap_or(Ok(fallback))
    }

    impl FakeApi {
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("poisoned").clone()
        }
    }

    impl PlaylistApi for FakeApi {
        fn add_items(
            &self,
            playlist_id: &str,
            video_ids: &[String],
            allow_duplicates: bool,
        ) -> Result<Vec<(String, String)>, ApiError> {
            self.calls.lock().expect("poisoned").push(format!(
                "add:{playlist_id}:{}:dup={allow_duplicates}",
                video_ids.join(",")
            ));
            let fallback: Vec<(String, String)> = video_ids
                .iter()
                .map(|v| (v.clone(), format!("sv-{v}")))
                .collect();
            pop(&self.add, fallback)
        }

        fn remove_items(
            &self,
            playlist_id: &str,
            items: &[(String, String)],
        ) -> Result<(), ApiError> {
            self.calls.lock().expect("poisoned").push(format!(
                "remove:{playlist_id}:{}",
                items
                    .iter()
                    .map(|(v, _)| v.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            pop(&self.remove, ())
        }

        fn move_item(
            &self,
            playlist_id: &str,
            set_video_id: &str,
            before: &str,
        ) -> Result<(), ApiError> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("move:{playlist_id}:{set_video_id}->{before}"));
            pop(&self.move_, ())
        }

        fn edit_meta(
            &self,
            playlist_id: &str,
            title: &str,
            _description: &str,
            _privacy: &str,
        ) -> Result<(), ApiError> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("meta:{playlist_id}:{title}"));
            pop(&self.meta, ())
        }

        fn create_playlist(&self, title: &str) -> Result<String, ApiError> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("create:{title}"));
            pop(&self.create, "PLnew".to_string())
        }

        fn delete_playlist(&self, playlist_id: &str) -> Result<(), ApiError> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("delete:{playlist_id}"));
            pop(&self.delete, ())
        }

        fn rate_song(&self, video_id: &str, rating: &str) -> Result<(), ApiError> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("rate:{video_id}:{rating}"));
            pop(&self.rate, ())
        }

        fn library(&self, playlist_id: &str, save: bool) -> Result<(), ApiError> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("library:{playlist_id}:{save}"));
            pop(&self.library, ())
        }

        fn fetch_playlist(&self, playlist_id: &str) -> Result<PlaylistRead, ApiError> {
            self.calls
                .lock()
                .expect("poisoned")
                .push(format!("read:{playlist_id}"));
            let mut guard = self.read.lock().expect("poisoned");
            match guard.pop_front() {
                Some(answer) => answer,
                None => Ok(PlaylistRead {
                    tracks: Vec::new(),
                    total: 0,
                    owned: true,
                    title: String::new(),
                    privacy: String::new(),
                }),
            }
        }
    }
}
