//! Buckets — named, self-contained playlists you can dump into the queue.

use std::fs;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::model::Track;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bucket {
    pub name: String,
    pub tracks: Vec<Track>,
    /// Accent colour index (into the theme's bucket palette).
    #[serde(default)]
    pub color: u8,
}

impl Bucket {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tracks: Vec::new(),
            color: 0,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct BucketStore {
    pub buckets: Vec<Bucket>,
}

impl BucketStore {
    pub fn load() -> Self {
        fs::read_to_string(config::buckets_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = config::buckets_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            fs::write(path, json).ok();
        }
    }

    pub fn create(&mut self, name: String) -> usize {
        let mut b = Bucket::new(name);
        // Cycle accent colours so new buckets look distinct.
        b.color = (self.buckets.len() % crate::theme::BUCKET_COLORS) as u8;
        self.buckets.push(b);
        self.buckets.len() - 1
    }

    /// Create a bucket pre-filled with tracks (e.g. "save queue as bucket").
    pub fn create_with(&mut self, name: String, tracks: Vec<Track>) -> usize {
        let idx = self.create(name);
        self.buckets[idx].tracks = tracks;
        idx
    }

    pub fn delete(&mut self, idx: usize) {
        if idx < self.buckets.len() {
            self.buckets.remove(idx);
        }
    }

    /// Add a track to a bucket, skipping exact-path duplicates.
    pub fn add_track(&mut self, bucket_idx: usize, track: Track) -> bool {
        if let Some(bucket) = self.buckets.get_mut(bucket_idx) {
            if bucket.tracks.iter().any(|t| t.path == track.path) {
                return false;
            }
            bucket.tracks.push(track);
            return true;
        }
        false
    }

    /// Remove a track from a bucket by track index.
    pub fn remove_track(&mut self, bucket_idx: usize, track_idx: usize) {
        if let Some(b) = self.buckets.get_mut(bucket_idx) {
            if track_idx < b.tracks.len() {
                b.tracks.remove(track_idx);
            }
        }
    }

    /// Move a track within a bucket by `delta` (clamped). Returns its new index.
    pub fn move_track(&mut self, bucket_idx: usize, track_idx: usize, delta: i32) -> usize {
        if let Some(b) = self.buckets.get_mut(bucket_idx) {
            let len = b.tracks.len();
            if len == 0 || track_idx >= len {
                return track_idx;
            }
            let target = (track_idx as i32 + delta).clamp(0, len as i32 - 1) as usize;
            if target != track_idx {
                let t = b.tracks.remove(track_idx);
                b.tracks.insert(target, t);
            }
            return target;
        }
        track_idx
    }

    pub fn rename(&mut self, bucket_idx: usize, name: String) {
        if let Some(b) = self.buckets.get_mut(bucket_idx) {
            b.name = name;
        }
    }

    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}
