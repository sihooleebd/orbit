//! The playing queue, with shuffle and repeat modes.
//!
//! Playback follows an explicit `order` (a permutation of item indices) so that
//! shuffle gives stable next/previous behaviour rather than re-rolling each step.

use rand::seq::SliceRandom;

use crate::model::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => RepeatMode::All,
            2 => RepeatMode::One,
            _ => RepeatMode::Off,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            RepeatMode::Off => 0,
            RepeatMode::All => 1,
            RepeatMode::One => 2,
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        }
    }
}

#[derive(Default)]
pub struct Queue {
    pub items: Vec<Track>,
    /// Play order: a permutation of `0..items.len()`.
    order: Vec<usize>,
    /// Position within `order` of the currently selected track.
    order_pos: usize,
    pub repeat: RepeatMode,
    pub shuffle: bool,
}

impl Default for RepeatMode {
    fn default() -> Self {
        RepeatMode::Off
    }
}

impl Queue {
    pub fn new(repeat: RepeatMode, shuffle: bool) -> Self {
        Self {
            items: Vec::new(),
            order: Vec::new(),
            order_pos: 0,
            repeat,
            shuffle,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The item index (into `items`) currently selected for playback.
    pub fn current_index(&self) -> Option<usize> {
        self.order.get(self.order_pos).copied()
    }

    pub fn current(&self) -> Option<&Track> {
        self.current_index().and_then(|i| self.items.get(i))
    }

    fn rebuild_order(&mut self, keep_current: Option<usize>) {
        let n = self.items.len();
        self.order = (0..n).collect();
        if self.shuffle {
            let mut rng = rand::rng();
            self.order.shuffle(&mut rng);
        }
        // Restore order_pos so the same item stays current if requested.
        self.order_pos = match keep_current {
            Some(item_idx) => self
                .order
                .iter()
                .position(|&i| i == item_idx)
                .unwrap_or(0),
            None => 0,
        };
    }

    /// Append tracks to the end of the queue.
    pub fn extend(&mut self, tracks: impl IntoIterator<Item = Track>) {
        let current = self.current_index();
        self.items.extend(tracks);
        self.rebuild_order(current);
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.order_pos = 0;
    }

    /// Remove the item at the given `items` index.
    pub fn remove(&mut self, item_idx: usize) {
        if item_idx >= self.items.len() {
            return;
        }
        let current = self.current_index();
        self.items.remove(item_idx);
        // Figure out what should be current after removal.
        let keep = match current {
            Some(c) if c == item_idx => None, // removed the current one
            Some(c) if c > item_idx => Some(c - 1),
            other => other,
        };
        self.rebuild_order(keep);
    }

    /// Jump to a specific `items` index (e.g. user clicked a queue row).
    pub fn jump_to(&mut self, item_idx: usize) {
        if let Some(pos) = self.order.iter().position(|&i| i == item_idx) {
            self.order_pos = pos;
        }
    }

    /// Advance to the next track. Returns the new current track, or None if the
    /// queue has run off the end (respecting repeat mode).
    pub fn advance(&mut self) -> Option<&Track> {
        if self.items.is_empty() {
            return None;
        }
        match self.repeat {
            RepeatMode::One => { /* stay put */ }
            _ => {
                if self.order_pos + 1 < self.order.len() {
                    self.order_pos += 1;
                } else if self.repeat == RepeatMode::All {
                    // Reshuffle on wrap for variety, then start over.
                    if self.shuffle {
                        self.rebuild_order(None);
                    }
                    self.order_pos = 0;
                } else {
                    return None;
                }
            }
        }
        self.current()
    }

    /// Step back to the previous track (clamped at the start).
    pub fn previous(&mut self) -> Option<&Track> {
        if self.items.is_empty() {
            return None;
        }
        if self.order_pos > 0 {
            self.order_pos -= 1;
        } else if self.repeat == RepeatMode::All {
            self.order_pos = self.order.len().saturating_sub(1);
        }
        self.current()
    }

    pub fn set_shuffle(&mut self, shuffle: bool) {
        if shuffle == self.shuffle {
            return;
        }
        self.shuffle = shuffle;
        let current = self.current_index();
        self.rebuild_order(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn track(name: &str) -> Track {
        Track {
            path: PathBuf::from(name),
            title: name.to_string(),
            artist: String::new(),
            album: String::new(),
            duration_secs: 1,
            mtime: 0,
        }
    }

    #[test]
    fn advance_stops_at_end_when_repeat_off() {
        let mut q = Queue::new(RepeatMode::Off, false);
        q.extend([track("a"), track("b")]);
        assert_eq!(q.current().unwrap().title, "a");
        assert_eq!(q.advance().unwrap().title, "b");
        assert!(q.advance().is_none());
    }

    #[test]
    fn advance_wraps_when_repeat_all() {
        let mut q = Queue::new(RepeatMode::All, false);
        q.extend([track("a"), track("b")]);
        q.advance();
        assert_eq!(q.advance().unwrap().title, "a");
    }

    #[test]
    fn repeat_one_stays_put() {
        let mut q = Queue::new(RepeatMode::One, false);
        q.extend([track("a"), track("b")]);
        assert_eq!(q.advance().unwrap().title, "a");
    }

    #[test]
    fn remove_current_keeps_index_valid() {
        let mut q = Queue::new(RepeatMode::Off, false);
        q.extend([track("a"), track("b"), track("c")]);
        q.advance(); // now on "b"
        q.remove(1); // remove "b"
        // Current index should still point at a valid item.
        assert!(q.current().is_some());
    }

    #[test]
    fn jump_to_selects_item() {
        let mut q = Queue::new(RepeatMode::Off, false);
        q.extend([track("a"), track("b"), track("c")]);
        q.jump_to(2);
        assert_eq!(q.current().unwrap().title, "c");
    }
}
