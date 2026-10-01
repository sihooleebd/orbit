//! The play queue: order, current position, shuffle, repeat, and editing.
//!
//! `tracks` is the queue in display order. Playback follows a private *play order*: a permutation
//! of the indices of `tracks`, the identity with shuffle off and random with shuffle on. One pass
//! through the play order plays every track once; with repeat-all the next pass starts over (with
//! shuffle on it is freshly reshuffled and never starts with the track that just played).

use std::borrow::Cow;
use std::collections::VecDeque;

use rand::RngExt;
use rand::rngs::SmallRng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

use crate::library::TrackId;

/// How many previously played tracks `prev` can walk back through in shuffle mode.
const HISTORY_LEN: usize = 500;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn next(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }
}

/// `tracks` is the queue in display order. With shuffle on, playback follows a private shuffled
/// order over the same indices; `current` is always an index into `tracks`.
///
/// The public fields may be assigned directly (e.g. when restoring a session): the play order is
/// rebuilt as soon as it no longer fits (`tracks` changed length, `shuffle` flipped). `&mut`
/// methods do that themselves; read-only methods compute the same rebuilt order on a copy until
/// then, so call [`Queue::rebuild_order`] after such writes to spare them that work.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Queue {
    pub tracks: Vec<TrackId>,
    pub current: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Stop when the current track ends (cleared once it triggers).
    pub stop_after_current: bool,
    /// Play order bookkeeping, opaque outside this module (crate-visible only so that
    /// `Queue { shuffle, ..Queue::default() }` compiles elsewhere). Not persisted.
    #[serde(skip)]
    pub(crate) order: PlayOrder,
}

#[derive(Clone, Debug)]
pub(crate) struct PlayOrder {
    /// Permutation of `0..tracks.len()`: the order tracks play in.
    seq: Vec<usize>,
    /// Inverse of `seq`: `rank[i]` is the position of track index `i` in `seq`.
    rank: Vec<usize>,
    /// The `shuffle` value `seq` was built for (catches direct writes to `Queue::shuffle`).
    shuffled: bool,
    /// Track indices that were current before, most recent last.
    history: VecDeque<usize>,
    /// Every random choice comes from here, so a clone of the queue makes exactly the same
    /// choices: `peek_advance` is exact even when the next step reshuffles.
    rng: SmallRng,
}

impl Default for PlayOrder {
    fn default() -> Self {
        PlayOrder { seq: Vec::new(), rank: Vec::new(), shuffled: false, history: VecDeque::new(), rng: rand::make_rng() }
    }
}

impl PlayOrder {
    fn set(&mut self, seq: Vec<usize>) {
        self.seq = seq;
        self.reindex();
    }

    /// Recompute `rank` from `seq`.
    fn reindex(&mut self) {
        self.rank.resize(self.seq.len(), 0);
        for (pos, &i) in self.seq.iter().enumerate() {
            self.rank[i] = pos;
        }
    }

    /// Map every stored track index through `f` (None drops it). Call `reindex` afterwards.
    fn map_indices(&mut self, f: impl Fn(usize) -> Option<usize>) {
        self.seq = self.seq.iter().filter_map(|&i| f(i)).collect();
        self.history = self.history.iter().filter_map(|&i| f(i)).collect();
    }

    fn remember(&mut self, index: usize) {
        self.history.push_back(index);
        if self.history.len() > HISTORY_LEN {
            self.history.pop_front();
        }
    }
}

impl Queue {
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn current_track(&self) -> Option<TrackId> {
        self.current.and_then(|i| self.tracks.get(i).copied())
    }

    /// Replace the queue. `start` (index into `tracks`) becomes current; with shuffle on, it
    /// plays first and the rest is shuffled after it (everything is shuffled if `start` is None).
    pub fn set(&mut self, tracks: Vec<TrackId>, start: Option<usize>) {
        self.tracks = tracks;
        self.current = start;
        self.order.history.clear();
        self.rebuild_order();
    }

    /// Rebuild the play order from scratch: natural order, or with shuffle on the current track
    /// first and everything else in a fresh random order. Also drops an out-of-range `current`.
    pub fn rebuild_order(&mut self) {
        let n = self.tracks.len();
        self.current = self.current.filter(|&c| c < n);
        self.order.history.retain(|&h| h < n);
        let mut seq: Vec<usize> = (0..n).collect();
        if self.shuffle {
            let rest = match self.current {
                Some(c) => {
                    seq.swap(0, c);
                    &mut seq[1..]
                }
                None => &mut seq[..],
            };
            rest.shuffle(&mut self.order.rng);
        }
        self.order.shuffled = self.shuffle;
        self.order.set(seq);
    }

    /// Automatic advance when a track ends: repeat-one replays, repeat-all wraps, off stops at
    /// the end; honors (and clears) stop_after_current. Returns the new current track.
    pub fn advance(&mut self) -> Option<TrackId> {
        if self.stop_after_current {
            self.stop_after_current = false;
            return None;
        }
        if self.repeat == Repeat::One && self.current_track().is_some() {
            return self.current_track();
        }
        self.next()
    }

    /// What `advance` would return, without changing anything (for gapless preloading). Exact,
    /// even across a repeat-all reshuffle.
    pub fn peek_advance(&self) -> Option<TrackId> {
        if self.stop_after_current {
            return None;
        }
        if self.repeat == Repeat::One && self.current_track().is_some() {
            return self.current_track();
        }
        let q = self.fresh();
        match q.following() {
            Some(i) => Some(q.tracks[i]),
            None if q.repeat == Repeat::Off || q.is_empty() => None,
            // a new pass may reshuffle: let a copy (same RNG state) do it
            None => q.into_owned().next(),
        }
    }

    /// User skip (in play order): never replays (repeat-one only affects `advance`); wraps unless
    /// repeat is off; None at the end otherwise, leaving `current` where it is.
    pub fn next(&mut self) -> Option<TrackId> {
        self.sync();
        if let Some(i) = self.following() {
            return self.go(i);
        }
        if self.repeat == Repeat::Off || self.is_empty() {
            return None;
        }
        self.new_pass();
        self.go(self.order.seq[0])
    }

    /// User "previous" (in play order). Wraps unless repeat is off. With shuffle on it walks back
    /// through what actually played (then `next` retraces the same path forward).
    pub fn prev(&mut self) -> Option<TrackId> {
        self.sync();
        if self.shuffle {
            while let Some(h) = self.order.history.pop_back() {
                if self.current != Some(h) {
                    self.place(h, false);
                    self.current = Some(h);
                    return self.current_track();
                }
            }
        }
        let pos = self.cur_pos()?;
        let target = match pos.checked_sub(1) {
            Some(p) => self.order.seq[p],
            None if self.repeat != Repeat::Off => self.order.seq[self.len() - 1],
            None => return None,
        };
        if self.order.history.back() == Some(&target) {
            self.order.history.pop_back();
        }
        self.current = Some(target);
        self.current_track()
    }

    /// Make `index` current (None, and no change, if it's out of range). With shuffle on, the
    /// tracks still unplayed in this pass keep playing after it.
    pub fn jump(&mut self, index: usize) -> Option<TrackId> {
        self.sync();
        if index >= self.len() {
            return None;
        }
        if self.shuffle && self.current != Some(index) {
            self.place(index, true);
        }
        self.go(index)
    }

    /// Append to the end (and to the end of the shuffled order).
    pub fn push(&mut self, ids: &[TrackId]) {
        self.sync();
        let n = self.len();
        self.tracks.extend_from_slice(ids);
        self.order.seq.extend(n..self.len());
        self.order.reindex();
    }

    /// Insert right after the current track (in play order, also when shuffled), keeping the
    /// given order. With no current track they go to the front.
    pub fn insert_next(&mut self, ids: &[TrackId]) {
        self.sync();
        let k = ids.len();
        let at = self.current.map_or(0, |c| c + 1);
        let pos = self.cur_pos().map_or(0, |p| p + 1);
        self.tracks.splice(at..at, ids.iter().copied());
        let o = &mut self.order;
        o.map_indices(|i| Some(if i >= at { i + k } else { i }));
        o.seq.splice(pos..pos, at..at + k);
        o.reindex();
    }

    /// Remove `index`; `current` keeps pointing at the same track (if the current track itself
    /// is removed, `current` moves to what would have played next and the caller decides).
    pub fn remove(&mut self, index: usize) {
        self.sync();
        if index >= self.len() {
            return;
        }
        let pos = self.order.rank[index];
        self.tracks.remove(index);
        let o = &mut self.order;
        o.map_indices(|i| match i.cmp(&index) {
            std::cmp::Ordering::Less => Some(i),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(i - 1),
        });
        o.reindex();
        self.current = match self.current {
            Some(c) if c == index => self.order.seq.get(pos).copied(),
            Some(c) if c > index => Some(c - 1),
            other => other,
        };
    }

    /// Move an item (display order); `current` follows the track it pointed at. Unshuffled, the
    /// play order is the display order; shuffled, the play sequence stays the same.
    pub fn move_item(&mut self, from: usize, to: usize) {
        self.sync();
        let n = self.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let t = self.tracks.remove(from);
        self.tracks.insert(to, t);
        let moved = |i: usize| match i {
            _ if i == from => to,
            _ if from < to && i > from && i <= to => i - 1,
            _ if to < from && i >= to && i < from => i + 1,
            _ => i,
        };
        self.current = self.current.map(moved);
        let o = &mut self.order;
        o.map_indices(|i| Some(moved(i)));
        if !self.shuffle {
            o.seq = (0..n).collect();
        }
        o.reindex();
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.current = None;
        self.order.history.clear();
        self.rebuild_order();
    }

    /// The play order (indices into `tracks`), to save a shuffled pass.
    pub fn play_order(&self) -> Vec<usize> {
        self.fresh().order.seq.clone()
    }

    /// Continue a saved shuffled pass: `seq` becomes the play order when shuffle is on and it is
    /// a permutation of the track indices; otherwise the order stays as it is.
    pub fn restore_order(&mut self, seq: Vec<usize>) {
        self.sync();
        let mut seen = vec![false; self.len()];
        if self.shuffle && seq.len() == seen.len() && seq.iter().all(|&i| i < seen.len() && !std::mem::replace(&mut seen[i], true)) {
            self.order.set(seq);
        }
    }

    /// Turn shuffle on/off; the current track stays current. On: everything else gets a fresh
    /// random order after it. Off: the natural order continues from the current track.
    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        self.rebuild_order();
    }

    /// Physically shuffle `tracks` (display order); the current track moves to the top. The play
    /// order is rebuilt (natural, or freshly shuffled).
    pub fn shuffle_now(&mut self) {
        self.sync();
        let n = self.len();
        // perm[new index] = old index
        let mut perm: Vec<usize> = (0..n).collect();
        let rest = match self.current {
            Some(c) => {
                perm.swap(0, c);
                &mut perm[1..]
            }
            None => &mut perm[..],
        };
        rest.shuffle(&mut self.order.rng);
        let mut new_index = vec![0; n];
        for (new, &old) in perm.iter().enumerate() {
            new_index[old] = new;
        }
        self.tracks = perm.iter().map(|&old| self.tracks[old]).collect();
        self.current = self.current.map(|c| new_index[c]);
        for h in &mut self.order.history {
            *h = new_index[*h];
        }
        self.rebuild_order();
    }

    /// Indices into `tracks` that will play after the current one, in play order (for "up next").
    /// Covers the rest of the current pass; `peek_advance` also knows about wraps and repeat-one.
    pub fn up_next(&self, n: usize) -> Vec<usize> {
        let q = self.fresh();
        let start = q.cur_pos().map_or(0, |p| p + 1);
        q.order.seq.iter().skip(start).take(n).copied().collect()
    }

    /// Position of `index` in play order relative to current (0 = current, 1 = next, ...), if upcoming
    /// in the current pass. With nothing current, everything is upcoming (the first is 1).
    pub fn play_position(&self, index: usize) -> Option<usize> {
        let q = self.fresh();
        let pos = *q.order.rank.get(index)?;
        match q.cur_pos() {
            Some(c) => pos.checked_sub(c),
            None => Some(pos + 1),
        }
    }

    /// After a rescan: map old ids to new ones; unmapped tracks are dropped. If the current track
    /// is dropped, `current` moves to what would have played next (like `remove`).
    pub fn remap(&mut self, f: &dyn Fn(TrackId) -> Option<TrackId>) {
        self.sync();
        let mapped: Vec<Option<TrackId>> = self.tracks.iter().map(|&t| f(t)).collect();
        let mut kept = 0;
        let new_index: Vec<Option<usize>> = mapped
            .iter()
            .map(|m| {
                m.map(|_| {
                    kept += 1;
                    kept - 1
                })
            })
            .collect();
        let current = self.cur_pos().and_then(|p| self.order.seq[p..].iter().find_map(|&i| new_index[i]));
        self.tracks = mapped.into_iter().flatten().collect();
        self.order.map_indices(|i| new_index[i]);
        self.order.reindex();
        self.current = current;
    }

    // ---- internals ----

    fn is_stale(&self) -> bool {
        self.order.seq.len() != self.tracks.len()
            || self.order.shuffled != self.shuffle
            || self.current.is_some_and(|c| c >= self.tracks.len())
    }

    fn sync(&mut self) {
        if self.is_stale() {
            self.rebuild_order();
        }
    }

    /// `self` with an up-to-date play order: borrowed when it is, else a rebuilt copy (the same
    /// order the next `&mut` call will build, since the copy carries the same RNG state).
    fn fresh(&self) -> Cow<'_, Queue> {
        if self.is_stale() {
            let mut q = self.clone();
            q.rebuild_order();
            Cow::Owned(q)
        } else {
            Cow::Borrowed(self)
        }
    }

    /// Position of the current track in the play order (on a fresh queue).
    fn cur_pos(&self) -> Option<usize> {
        self.current.map(|c| self.order.rank[c])
    }

    /// The track index `next` moves to within the current pass (None at its end).
    fn following(&self) -> Option<usize> {
        self.order.seq.get(self.cur_pos().map_or(0, |p| p + 1)).copied()
    }

    /// Make `index` current, remembering the previous current track for `prev`.
    fn go(&mut self, index: usize) -> Option<TrackId> {
        if let Some(c) = self.current.filter(|&c| c != index) {
            self.order.remember(c);
        }
        self.current = Some(index);
        self.current_track()
    }

    /// Start the next pass (repeat-all). Shuffled: a fresh random order that doesn't begin with
    /// the track that just played (uniform over all such orders). Unshuffled: nothing to do.
    fn new_pass(&mut self) {
        if !self.shuffle {
            return;
        }
        let n = self.len();
        let mut seq: Vec<usize> = (0..n).collect();
        seq.shuffle(&mut self.order.rng);
        if n > 1 && Some(seq[0]) == self.current {
            let j = self.order.rng.random_range(1..n);
            seq.swap(0, j);
        }
        self.order.set(seq);
    }

    /// Move track index `i` (not the current one) right before or `after` the current track in
    /// the play order; to the front when nothing is current.
    fn place(&mut self, i: usize, after: bool) {
        let o = &mut self.order;
        let from = o.rank[i];
        o.seq.remove(from);
        let at = match self.current {
            Some(c) => o.rank[c] - usize::from(from < o.rank[c]) + usize::from(after),
            None => 0,
        };
        o.seq.insert(at, i);
        o.reindex();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    /// A queue of `n` tracks (ids 100.., so ids and indices differ), first one current.
    fn queue(n: usize, shuffle: bool, seed: u64) -> Queue {
        let mut q = Queue { shuffle, ..Queue::default() };
        q.order.rng = SmallRng::seed_from_u64(seed);
        q.set(ids(n), Some(0));
        q
    }

    fn ids(n: usize) -> Vec<TrackId> {
        (100..100 + n).collect()
    }

    fn sorted(mut v: Vec<TrackId>) -> Vec<TrackId> {
        v.sort_unstable();
        v
    }

    #[test]
    fn a_saved_shuffle_pass_continues() {
        let mut q = queue(5, true, 1);
        q.jump(2);
        q.restore_order(vec![4, 2, 0, 1, 3]);
        check(&q);
        assert_eq!((q.next(), q.next(), q.next()), (Some(100), Some(101), Some(103)));
        assert_eq!(q.next(), None, "the pass ends: 4 and 2 played before the restart");
        // not a permutation (or shuffle off): ignored
        let mut q = queue(3, true, 1);
        let before = q.play_order();
        q.restore_order(vec![0, 0, 1]);
        assert_eq!(q.play_order(), before);
        let mut q = queue(3, false, 1);
        q.restore_order(vec![2, 1, 0]);
        assert_eq!(q.play_order(), vec![0, 1, 2]);
    }

    /// The structural invariants, checked on the order the queue will use.
    fn check(q: &Queue) {
        let q = q.fresh();
        let n = q.len();
        let o = &q.order;
        assert_eq!(o.seq.len(), n, "order length");
        assert_eq!(sorted(o.seq.clone()), (0..n).collect::<Vec<_>>(), "order is a permutation");
        for (pos, &i) in o.seq.iter().enumerate() {
            assert_eq!(o.rank[i], pos, "rank is the inverse");
        }
        assert!(q.current.is_none_or(|c| c < n), "current in range");
        assert!(o.history.iter().all(|&h| h < n), "history in range");
        assert!(o.history.len() <= HISTORY_LEN);
        if !q.shuffle {
            assert!(o.seq.iter().enumerate().all(|(p, &i)| p == i), "unshuffled order is natural");
        }
        for (k, &i) in q.up_next(n).iter().enumerate() {
            assert_eq!(q.play_position(i), Some(k + 1), "up_next agrees with play_position");
        }
    }

    #[test]
    fn natural_order_next_prev_and_wrapping() {
        let mut q = queue(4, false, 1);
        assert_eq!(q.current_track(), Some(100));
        assert_eq!((q.next(), q.next(), q.next()), (Some(101), Some(102), Some(103)));
        assert_eq!(q.next(), None, "repeat off stops at the end");
        assert_eq!(q.current, Some(3), "current stays on the last track");
        q.repeat = Repeat::All;
        assert_eq!(q.next(), Some(100), "repeat-all wraps");
        assert_eq!(q.prev(), Some(103), "prev wraps back");
        q.repeat = Repeat::Off;
        q.jump(0);
        assert_eq!(q.prev(), None);
        assert_eq!(q.jump(9), None, "out-of-range jump changes nothing");
        assert_eq!(q.current, Some(0));
        check(&q);
    }

    #[test]
    fn repeat_one_replays_only_on_advance() {
        let mut q = queue(3, false, 1);
        q.repeat = Repeat::One;
        assert_eq!(q.advance(), Some(100));
        assert_eq!(q.peek_advance(), Some(100));
        assert_eq!(q.next(), Some(101), "a user skip ignores repeat-one");
        q.jump(2);
        assert_eq!(q.next(), Some(100), "and wraps");
    }

    #[test]
    fn stop_after_current_triggers_once() {
        let mut q = queue(3, false, 1);
        q.stop_after_current = true;
        assert_eq!(q.peek_advance(), None);
        assert_eq!(q.advance(), None);
        assert!(!q.stop_after_current);
        assert_eq!(q.current, Some(0), "stays on the track that just ended");
        assert_eq!(q.advance(), Some(101));
    }

    #[test]
    fn shuffled_set_plays_start_first_then_everything_once() {
        let mut q = Queue { shuffle: true, ..Queue::default() };
        q.order.rng = SmallRng::seed_from_u64(7);
        q.set(ids(30), Some(12));
        assert_eq!(q.current_track(), Some(112));
        let mut played = vec![112];
        while let Some(t) = q.next() {
            played.push(t);
        }
        assert_eq!(sorted(played.clone()), ids(30));
        assert_ne!(played, sorted(played.clone()), "actually shuffled");
        check(&q);
    }

    #[test]
    fn shuffled_set_without_start_shuffles_everything() {
        let mut q = Queue { shuffle: true, ..Queue::default() };
        q.set(ids(10), None);
        assert_eq!(q.current, None);
        let mut played = Vec::new();
        while let Some(t) = q.next() {
            played.push(t);
        }
        assert_eq!(sorted(played), ids(10));
    }

    #[test]
    fn repeat_all_reshuffles_every_pass_without_back_to_back_repeats() {
        for seed in 0..50 {
            let n = 2 + (seed as usize % 7);
            let mut q = queue(n, true, seed);
            q.repeat = Repeat::All;
            let mut played = vec![q.current_track().unwrap()];
            for _ in 1..n * 8 {
                played.push(q.next().unwrap());
            }
            for pass in played.chunks(n) {
                assert_eq!(sorted(pass.to_vec()), ids(n), "each pass plays every track once");
            }
            assert!(played.windows(2).all(|w| w[0] != w[1]), "no immediate repeat: {played:?}");
            check(&q);
        }
    }

    #[test]
    fn reshuffled_passes_vary() {
        let mut q = queue(6, true, 3);
        q.repeat = Repeat::All;
        let mut firsts = std::collections::HashSet::new();
        for _ in 0..600 {
            if q.next().is_some() && q.order.rank[q.current.unwrap()] == 0 {
                firsts.insert(q.current_track().unwrap());
            }
        }
        assert_eq!(firsts.len(), 6, "every track gets to open a pass");
    }

    #[test]
    fn toggling_shuffle_keeps_current() {
        let mut q = queue(20, false, 5);
        q.jump(7);
        q.set_shuffle(true);
        assert_eq!(q.current, Some(7));
        assert_eq!(q.play_position(7), Some(0));
        let rest: Vec<_> = q.up_next(100).into_iter().map(|i| q.tracks[i]).collect();
        assert_eq!(rest.len(), 19, "everything else is upcoming");
        assert!(!rest.contains(&107));
        q.next();
        q.next();
        let now = q.current.unwrap();
        q.set_shuffle(false);
        assert_eq!(q.current, Some(now));
        assert_eq!(q.up_next(3), (now + 1..20).take(3).collect::<Vec<_>>(), "natural order continues from current");
        check(&q);
    }

    #[test]
    fn insert_next_plays_right_after_current() {
        for shuffle in [false, true] {
            let mut q = queue(10, shuffle, 11);
            q.next();
            let c = q.current.unwrap();
            q.insert_next(&[1, 2]);
            assert_eq!(q.tracks[c + 1..c + 3], [1, 2], "right after current in display order");
            assert_eq!(q.next(), Some(1));
            assert_eq!(q.next(), Some(2));
            let mut rest = vec![q.tracks[0], q.tracks[c], 1, 2];
            while let Some(t) = q.next() {
                rest.push(t);
            }
            assert_eq!(rest.len(), 12, "nothing lost or doubled (shuffle {shuffle})");
            check(&q);
        }
        let mut q = Queue::default();
        q.push(&[5, 6]);
        q.insert_next(&[7]);
        assert_eq!(q.tracks, [7, 5, 6], "nothing current: goes first");
        assert_eq!(q.next(), Some(7));
    }

    #[test]
    fn push_appends_to_the_play_order() {
        let mut q = queue(5, true, 2);
        q.push(&[1, 2, 3]);
        assert_eq!(&q.tracks[5..], [1, 2, 3]);
        let mut played = Vec::new();
        while let Some(t) = q.next() {
            played.push(t);
        }
        assert_eq!(played.len(), 7);
        assert_eq!(&played[4..], [1, 2, 3], "appended tracks play last, in order");
    }

    #[test]
    fn remove_keeps_current_on_its_track() {
        let mut q = queue(6, false, 1);
        q.jump(3);
        q.remove(1);
        assert_eq!(q.current_track(), Some(103));
        q.remove(4);
        assert_eq!(q.current_track(), Some(103));
        q.remove(q.current.unwrap());
        assert_eq!(q.current_track(), Some(104), "removing current moves to what plays next");
        q.remove(q.current.unwrap());
        assert_eq!(q.current, None, "nothing after it");
        q.remove(99);
        check(&q);

        let mut q = queue(8, true, 9);
        q.next();
        let upcoming = q.up_next(1)[0];
        let expected = q.tracks[upcoming];
        q.remove(q.current.unwrap());
        assert_eq!(q.current_track(), Some(expected), "shuffled: the next in play order");
        check(&q);
    }

    #[test]
    fn move_item_keeps_current_and_play_sequence() {
        let mut q = queue(8, true, 4);
        q.next();
        let cur = q.current_track();
        let before: Vec<TrackId> = q.up_next(8).into_iter().map(|i| q.tracks[i]).collect();
        q.move_item(0, 7);
        q.move_item(6, 1);
        q.move_item(3, 3);
        assert_eq!(q.current_track(), cur);
        let after: Vec<TrackId> = q.up_next(8).into_iter().map(|i| q.tracks[i]).collect();
        assert_eq!(before, after, "shuffled play sequence unchanged");
        check(&q);

        let mut q = queue(5, false, 4);
        q.move_item(4, 1);
        assert_eq!(q.tracks, [100, 104, 101, 102, 103]);
        assert_eq!(q.current_track(), Some(100));
        assert_eq!(q.next(), Some(104), "unshuffled: play order follows the display");
        q.move_item(1, 0);
        assert_eq!(q.current, Some(0), "current follows its track");
        assert_eq!(q.current_track(), Some(104));
        check(&q);
    }

    #[test]
    fn shuffle_now_moves_current_to_the_top() {
        let mut q = queue(12, false, 8);
        q.jump(5);
        q.shuffle_now();
        assert_eq!(q.current, Some(0));
        assert_eq!(q.current_track(), Some(105));
        assert_eq!(sorted(q.tracks.clone()), ids(12));
        assert_ne!(q.tracks, sorted(q.tracks.clone()));
        assert_eq!(q.next(), Some(q.tracks[1]), "plays on in the new display order");
        check(&q);
    }

    #[test]
    fn shuffled_prev_walks_back_through_what_played() {
        let mut q = queue(10, true, 21);
        let mut path = vec![q.current_track().unwrap()];
        for _ in 0..4 {
            path.push(q.next().unwrap());
        }
        q.jump(q.tracks.iter().position(|t| !path.contains(t)).unwrap());
        path.push(q.current_track().unwrap());
        for expected in path.iter().rev().skip(1) {
            assert_eq!(q.prev(), Some(*expected));
        }
        for expected in &path[1..] {
            assert_eq!(q.next(), Some(*expected), "next retraces the same path");
        }
        check(&q);
    }

    #[test]
    fn shuffled_prev_across_a_reshuffle() {
        let mut q = queue(5, true, 13);
        q.repeat = Repeat::All;
        let mut path = vec![q.current_track().unwrap()];
        for _ in 0..7 {
            path.push(q.next().unwrap());
        }
        for expected in path.iter().rev().skip(1).take(5) {
            assert_eq!(q.prev(), Some(*expected));
        }
        check(&q);
    }

    #[test]
    fn history_is_bounded() {
        let mut q = queue(7, true, 5);
        q.repeat = Repeat::All;
        let mut path = vec![q.current_track().unwrap()];
        for _ in 0..HISTORY_LEN * 2 {
            path.push(q.next().unwrap());
        }
        assert_eq!(q.order.history.len(), HISTORY_LEN);
        for expected in path.iter().rev().skip(1).take(HISTORY_LEN) {
            assert_eq!(q.prev(), Some(*expected), "the most recent {HISTORY_LEN} are remembered");
        }
        assert!(q.order.history.is_empty());
        check(&q);
    }

    #[test]
    fn shuffled_jump_keeps_the_rest_of_the_pass() {
        let mut q = queue(10, true, 17);
        let mut played = vec![q.current_track().unwrap(), q.next().unwrap()];
        let target = q.up_next(10)[5];
        played.push(q.jump(target).unwrap());
        while let Some(t) = q.next() {
            played.push(t);
        }
        assert_eq!(sorted(played), ids(10), "every track still plays exactly once");
    }

    #[test]
    fn peek_matches_advance_including_reshuffles() {
        for seed in 0..30 {
            let mut q = queue(4, true, seed);
            q.repeat = Repeat::All;
            for _ in 0..20 {
                let peeked = q.peek_advance();
                assert_eq!(peeked, q.advance());
            }
        }
    }

    #[test]
    fn stale_order_is_tolerated() {
        let mut q = queue(6, true, 3);
        q.jump(2);
        let json = serde_json::to_string(&q).unwrap();
        assert!(!json.contains("order"), "play order isn't persisted");
        let mut back: Queue = serde_json::from_str(&json).unwrap();
        assert_eq!(back.current, Some(2));
        let upcoming = back.up_next(10);
        assert_eq!(upcoming.len(), 5, "read-only methods rebuild on a copy");
        assert_eq!(back.play_position(upcoming[0]), Some(1));
        let peeked = back.peek_advance();
        assert_eq!(back.advance(), peeked, "and agree with the real rebuild");
        assert_eq!(back.current, Some(upcoming[0]));
        check(&back);

        // direct writes: more tracks, flipped shuffle, bogus current
        back.tracks.push(7);
        check(&back);
        back.shuffle = false;
        assert!(back.up_next(10).windows(2).all(|w| w[1] == w[0] + 1));
        back.current = Some(99);
        assert_eq!(back.current_track(), None);
        assert_eq!(back.next(), Some(back.tracks[0]));
        check(&back);
    }

    #[test]
    fn remap_drops_missing_tracks() {
        let mut q = queue(6, false, 1);
        q.jump(2);
        q.remap(&|t| (t != 101).then_some(t + 1000));
        assert_eq!(q.tracks, [1100, 1102, 1103, 1104, 1105]);
        assert_eq!(q.current_track(), Some(1102));
        q.remap(&|t| (t != 1102).then_some(t));
        assert_eq!(q.current_track(), Some(1103), "current gone: what would have played next");
        check(&q);
    }

    #[test]
    fn up_next_and_play_position_follow_the_play_order() {
        let q = queue(5, false, 1);
        assert_eq!(q.up_next(2), vec![1, 2]);
        assert_eq!(q.play_position(0), Some(0));
        assert_eq!(q.play_position(3), Some(3));
        assert_eq!(q.play_position(9), None);
        let mut q = queue(9, true, 6);
        for _ in 0..3 {
            q.next();
        }
        let upcoming = q.up_next(100);
        assert_eq!(upcoming.len(), 5);
        let played: Vec<usize> = (0..9).filter(|i| q.play_position(*i).is_none()).collect();
        assert_eq!(played.len(), 3, "the tracks already played this pass");
        let empty = Queue::default();
        assert!(empty.up_next(5).is_empty());
        assert_eq!(empty.play_position(0), None);
    }

    #[test]
    fn randomized_operations_keep_invariants() {
        let mut rng = SmallRng::seed_from_u64(0xDEC4);
        for round in 0..60 {
            let mut q = Queue::default();
            q.order.rng = SmallRng::seed_from_u64(round);
            for _ in 0..800 {
                let n = q.len();
                let pick = |rng: &mut SmallRng| rng.random_range(0..n.max(1));
                match rng.random_range(0..20) {
                    0 => {
                        let len = rng.random_range(0..15);
                        let start = rng.random_bool(0.8).then(|| rng.random_range(0..len.max(1)));
                        q.set((0..len).map(|_| rng.random_range(0..8)).collect(), start);
                    }
                    1..=3 => {
                        let peeked = q.peek_advance();
                        assert_eq!(q.advance(), peeked, "peek_advance == advance");
                    }
                    4..=5 => {
                        q.next();
                    }
                    6 => {
                        q.prev();
                    }
                    7 => {
                        let i = pick(&mut rng);
                        q.jump(i);
                    }
                    8 => q.push(&vec![rng.random_range(0..8); rng.random_range(0..4)]),
                    9 => q.insert_next(&vec![rng.random_range(0..8); rng.random_range(0..4)]),
                    10 => {
                        let i = pick(&mut rng);
                        q.remove(i);
                    }
                    11 => {
                        let (a, b) = (pick(&mut rng), pick(&mut rng));
                        q.move_item(a, b);
                    }
                    12 => q.set_shuffle(rng.random_bool(0.6)),
                    13 => q.shuffle_now(),
                    14 => q.repeat = q.repeat.next(),
                    15 => q.stop_after_current = rng.random_bool(0.2),
                    16 => {
                        let drop = rng.random_range(0..8);
                        q.remap(&|t| (t != drop).then_some(t));
                    }
                    17 => q.tracks.push(rng.random_range(0..8)),
                    18 => q.shuffle = !q.shuffle,
                    _ if rng.random_bool(0.1) => q.clear(),
                    _ => q.current = Some(rng.random_range(0..n + 2)),
                }
                check(&q);
                assert_full_pass(&q);
            }
        }
    }

    /// Starting a shuffled pass from the current state visits every track exactly once.
    fn assert_full_pass(q: &Queue) {
        let mut c = q.clone();
        c.repeat = Repeat::Off;
        c.stop_after_current = false;
        c.set_shuffle(true);
        let mut seen: Vec<usize> = c.current.into_iter().collect();
        while c.next().is_some() {
            seen.push(c.current.unwrap());
        }
        assert_eq!(sorted(seen), (0..c.len()).collect::<Vec<_>>(), "a shuffled pass plays every track once");
    }

    #[test]
    fn shuffled_passes_visit_every_track_once_under_random_edits() {
        let mut rng = SmallRng::seed_from_u64(42);
        for round in 0..200 {
            let n = rng.random_range(1..25);
            let mut q = queue(n, true, round);
            // random edits that don't add or remove tracks
            for _ in 0..rng.random_range(0..6) {
                let (a, b) = (rng.random_range(0..n), rng.random_range(0..n));
                match rng.random_range(0..3) {
                    0 => q.move_item(a, b),
                    1 => q.set_shuffle(true),
                    _ => {
                        q.jump(a);
                    }
                }
            }
            let mut seen = vec![q.current.unwrap()];
            while q.next().is_some() {
                seen.push(q.current.unwrap());
            }
            let upcoming_at_start = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), upcoming_at_start, "no track twice in a pass");
            q.insert_next(&[1, 2]);
            q.push(&[3]);
            q.remove(rng.random_range(0..q.len()));
            assert_full_pass(&q);
        }
    }
}
