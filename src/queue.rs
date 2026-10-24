//! Playback queue state machine.
//!
//! Pure data and transitions: no Tauri, no audio engine, no file system. The
//! module only decides *what* should happen next; `main.rs` still owns the
//! actual playback (resolve, download, engine commands) and applies the
//! returned [`Action`].
//!
//! Keeping this separate makes the tricky cases (end of queue, repeat, removing
//! the item that is playing) testable without starting an app.

use serde::{Deserialize, Serialize};

/// One entry of the playback queue, as the UI sends it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct QueueItem {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub thumbnail: String,
    #[serde(default)]
    pub duration_secs: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        }
    }
}

/// What the caller has to do after a transition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Queue state changed, nothing has to start playing.
    None,
    /// Play the item at [`Queue::current`].
    PlayCurrent,
    /// Stop playback and leave the queue idle.
    Stop,
}

#[derive(Default)]
pub struct Queue {
    items: Vec<QueueItem>,
    current: usize,
    active: bool,
    /// Set by an explicit stop: playback must not auto-advance from here.
    manual_stop: bool,
    version: u64,
}

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn manual_stop(&self) -> bool {
        self.manual_stop
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn current_item(&self) -> Option<QueueItem> {
        self.items.get(self.current).cloned()
    }

    /// Incremented on every change the UI should notice.
    pub fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Replaces the queue with `items`, starting at `start` (clamped).
    pub fn set_items(&mut self, mut items: Vec<QueueItem>, start: usize) {
        if items.is_empty() {
            self.clear();
            return;
        }
        let start = start.min(items.len() - 1);
        self.items.clear();
        self.items.append(&mut items);
        self.current = start;
        self.active = true;
        self.manual_stop = false;
        self.version = self.version.wrapping_add(1);
    }

    /// Inserts items after the current track (`play_next`) or at the end.
    /// Returns how many were inserted.
    pub fn add(&mut self, items: Vec<QueueItem>, play_next: bool) -> usize {
        if items.is_empty() {
            return 0;
        }
        let at = if play_next {
            (self.current + 1).min(self.items.len())
        } else {
            self.items.len()
        };
        let count = items.len();
        for (i, item) in items.into_iter().enumerate() {
            self.items.insert(at + i, item);
        }
        self.version = self.version.wrapping_add(1);
        count
    }

    /// Removes one entry. Removing a track before the current one keeps the
    /// same track playing; removing the current one leaves the cursor on the
    /// track that moved into its place; removing the last entry stops playback.
    pub fn remove(&mut self, index: usize) -> Result<Action, &'static str> {
        if index >= self.items.len() {
            return Err("index out of range");
        }
        if self.items.len() == 1 {
            self.active = false;
            self.items.clear();
            self.current = 0;
            self.version = self.version.wrapping_add(1);
            return Ok(Action::Stop);
        }
        self.items.remove(index);
        if self.active && index < self.current {
            self.current -= 1;
        } else if self.active && self.current >= self.items.len() {
            self.current = self.items.len() - 1;
        }
        self.version = self.version.wrapping_add(1);
        Ok(Action::None)
    }

    /// Empties the queue and marks the stop as manual.
    pub fn clear(&mut self) {
        self.active = false;
        self.items.clear();
        self.current = 0;
        self.manual_stop = true;
        self.version = self.version.wrapping_add(1);
    }

    /// The queue stops driving playback (a local playlist or file took over).
    pub fn deactivate(&mut self) {
        self.active = false;
    }

    /// Explicit stop button: keep the items, but do not advance on our own.
    pub fn mark_manual_stop(&mut self) {
        if self.active {
            self.manual_stop = true;
        }
    }

    /// "Next" button. `None` means the queue is not in charge: the caller
    /// should fall back to the engine's own playlist.
    pub fn next(&mut self) -> Option<Action> {
        if !self.active {
            return None;
        }
        if self.current + 1 >= self.items.len() {
            self.manual_stop = true;
            self.active = false;
            self.version = self.version.wrapping_add(1);
            return Some(Action::Stop);
        }
        self.current += 1;
        self.manual_stop = false;
        self.version = self.version.wrapping_add(1);
        Some(Action::PlayCurrent)
    }

    /// "Previous" button: restarts the track unless it has been playing for
    /// more than three seconds and there is something before it.
    pub fn prev(&mut self, elapsed_secs: f64) -> Option<Action> {
        if !self.active {
            return None;
        }
        if elapsed_secs > 3.0 && self.current > 0 {
            self.current -= 1;
            self.version = self.version.wrapping_add(1);
        }
        self.manual_stop = false;
        Some(Action::PlayCurrent)
    }

    /// Natural end of the current track, under the given repeat mode.
    pub fn advance(&mut self, repeat: RepeatMode) -> Action {
        if self.items.is_empty() {
            return Action::None;
        }
        match repeat {
            RepeatMode::One => Action::PlayCurrent,
            RepeatMode::All => {
                self.current = (self.current + 1) % self.items.len();
                self.version = self.version.wrapping_add(1);
                Action::PlayCurrent
            }
            RepeatMode::Off => {
                if self.current + 1 < self.items.len() {
                    self.current += 1;
                    self.version = self.version.wrapping_add(1);
                    Action::PlayCurrent
                } else {
                    self.active = false;
                    self.version = self.version.wrapping_add(1);
                    Action::Stop
                }
            }
        }
    }

    /// Up to `count` tracks after the current one (what to preload).
    /// Repeat-one never needs a neighbour; the caller decides that.
    pub fn upcoming(&self, count: usize) -> Vec<QueueItem> {
        self.items
            .iter()
            .skip(self.current + 1)
            .take(count)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> QueueItem {
        QueueItem {
            id: id.to_string(),
            title: id.to_string(),
            artist: "a".to_string(),
            thumbnail: String::new(),
            duration_secs: 0.0,
        }
    }

    fn queue(ids: &[&str], start: usize) -> Queue {
        let mut q = Queue::default();
        q.set_items(ids.iter().map(|id| item(id)).collect(), start);
        q
    }

    fn ids(q: &Queue) -> Vec<&str> {
        q.items().iter().map(|i| i.id.as_str()).collect()
    }

    #[test]
    fn set_items_starts_at_the_requested_track() {
        let q = queue(&["a", "b", "c"], 1);
        assert_eq!(q.current(), 1);
        assert_eq!(q.current_item().unwrap().id, "b");
        assert!(q.active());
        assert!(!q.manual_stop());
    }

    #[test]
    fn set_items_clamps_the_start_index() {
        let q = queue(&["a", "b"], 99);
        assert_eq!(q.current(), 1);
    }

    #[test]
    fn set_items_with_nothing_clears_the_queue() {
        let mut q = queue(&["a"], 0);
        q.set_items(Vec::new(), 0);
        assert!(q.is_empty());
        assert!(!q.active());
    }

    #[test]
    fn next_moves_forward_and_stops_at_the_end() {
        let mut q = queue(&["a", "b"], 0);
        assert_eq!(q.next(), Some(Action::PlayCurrent));
        assert_eq!(q.current(), 1);

        // Last track: the queue ends and does not wrap without repeat-all.
        assert_eq!(q.next(), Some(Action::Stop));
        assert!(!q.active());
        assert!(q.manual_stop());
    }

    #[test]
    fn next_on_an_inactive_queue_defers_to_the_engine() {
        let mut q = queue(&["a", "b"], 0);
        q.deactivate();
        assert_eq!(q.next(), None);
        assert_eq!(q.prev(10.0), None);
    }

    #[test]
    fn prev_restarts_the_track_within_three_seconds() {
        let mut q = queue(&["a", "b"], 1);
        assert_eq!(q.prev(1.0), Some(Action::PlayCurrent));
        assert_eq!(q.current(), 1, "early prev restarts the same track");
    }

    #[test]
    fn prev_goes_back_after_three_seconds_but_not_past_the_first() {
        let mut q = queue(&["a", "b", "c"], 2);
        assert_eq!(q.prev(5.0), Some(Action::PlayCurrent));
        assert_eq!(q.current(), 1);

        let mut first = queue(&["a", "b"], 0);
        assert_eq!(first.prev(30.0), Some(Action::PlayCurrent));
        assert_eq!(first.current(), 0, "there is nothing before the first track");
    }

    #[test]
    fn advance_with_repeat_one_replays_the_same_track() {
        let mut q = queue(&["a", "b"], 0);
        assert_eq!(q.advance(RepeatMode::One), Action::PlayCurrent);
        assert_eq!(q.current(), 0);
    }

    #[test]
    fn advance_with_repeat_all_wraps_around() {
        let mut q = queue(&["a", "b", "c"], 2);
        assert_eq!(q.advance(RepeatMode::All), Action::PlayCurrent);
        assert_eq!(q.current(), 0);
        assert!(q.active());
    }

    #[test]
    fn advance_without_repeat_stops_after_the_last_track() {
        let mut q = queue(&["a", "b"], 1);
        assert_eq!(q.advance(RepeatMode::Off), Action::Stop);
        assert!(!q.active());
    }

    #[test]
    fn advance_stops_past_the_end_even_with_repeat_off_and_one_item() {
        let mut q = queue(&["a"], 0);
        assert_eq!(q.advance(RepeatMode::Off), Action::Stop);
        assert!(!q.active());
    }

    #[test]
    fn advance_on_an_empty_queue_does_nothing() {
        let mut q = Queue::default();
        assert_eq!(q.advance(RepeatMode::All), Action::None);
    }

    #[test]
    fn removing_a_track_before_the_current_keeps_it_playing() {
        let mut q = queue(&["a", "b", "c"], 2);
        assert_eq!(q.remove(0), Ok(Action::None));
        assert_eq!(ids(&q), vec!["b", "c"]);
        assert_eq!(q.current(), 1, "cursor follows the playing track");
        assert_eq!(q.current_item().unwrap().id, "c");
    }

    #[test]
    fn removing_the_playing_track_points_at_its_replacement() {
        let mut q = queue(&["a", "b", "c"], 1);
        assert_eq!(q.remove(1), Ok(Action::None));
        assert_eq!(ids(&q), vec!["a", "c"]);
        assert_eq!(q.current(), 1);
        assert_eq!(q.current_item().unwrap().id, "c");
    }

    #[test]
    fn removing_after_the_current_does_not_move_the_cursor() {
        let mut q = queue(&["a", "b", "c"], 0);
        assert_eq!(q.remove(2), Ok(Action::None));
        assert_eq!(q.current(), 0);
        assert_eq!(ids(&q), vec!["a", "b"]);
    }

    #[test]
    fn removing_the_last_item_of_a_three_item_queue_clamps_the_cursor() {
        let mut q = queue(&["a", "b", "c"], 2);
        assert_eq!(q.remove(2), Ok(Action::None));
        assert_eq!(q.current(), 1);
        assert_eq!(q.current_item().unwrap().id, "b");
    }

    #[test]
    fn removing_the_only_track_stops_playback() {
        let mut q = queue(&["a"], 0);
        assert_eq!(q.remove(0), Ok(Action::Stop));
        assert!(q.is_empty());
        assert!(!q.active());
    }

    #[test]
    fn removing_out_of_range_is_an_error_and_changes_nothing_else() {
        let mut q = queue(&["a", "b"], 0);
        assert_eq!(q.remove(5), Err("index out of range"));
        assert_eq!(q.items().len(), 2);
        assert!(q.active());
    }

    #[test]
    fn clear_empties_the_queue_and_blocks_auto_advance() {
        let mut q = queue(&["a", "b"], 1);
        q.clear();
        assert!(q.is_empty());
        assert!(!q.active());
        assert!(q.manual_stop());
        assert_eq!(q.current(), 0);
    }

    #[test]
    fn add_appends_or_inserts_after_the_current_track() {
        let mut q = queue(&["a", "b"], 0);

        assert_eq!(q.add(vec![item("x")], false), 1);
        assert_eq!(ids(&q), vec!["a", "b", "x"]);

        assert_eq!(q.add(vec![item("y")], true), 1);
        assert_eq!(ids(&q), vec!["a", "y", "b", "x"]);
        assert_eq!(q.current(), 0, "inserting does not change the cursor");
    }

    #[test]
    fn add_with_nothing_is_a_noop() {
        let mut q = queue(&["a"], 0);
        let version = q.version();
        assert_eq!(q.add(Vec::new(), true), 0);
        assert_eq!(q.version(), version);
    }

    #[test]
    fn upcoming_returns_the_tracks_after_the_current_one() {
        let q = queue(&["a", "b", "c", "d"], 1);
        let next: Vec<String> = q.upcoming(2).into_iter().map(|i| i.id).collect();
        assert_eq!(next, vec!["c", "d"]);
    }

    #[test]
    fn upcoming_at_the_end_is_empty() {
        let q = queue(&["a", "b"], 1);
        assert!(q.upcoming(2).is_empty());
    }

    #[test]
    fn every_mutation_bumps_the_version_so_the_ui_refreshes() {
        let mut q = queue(&["a", "b"], 0);
        let v0 = q.version();
        q.next();
        let v1 = q.version();
        assert_ne!(v0, v1);
        q.advance(RepeatMode::Off);
        assert_ne!(v1, q.version());
    }
}
