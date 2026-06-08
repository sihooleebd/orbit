//! Library scanning and the in-memory track collection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::Accessor;
use walkdir::WalkDir;

use crate::config;
use crate::model::Track;

/// The merged library plus a cached, filtered view.
#[derive(Default)]
pub struct Library {
    pub tracks: Vec<Track>,
    /// Indices into `tracks` matching the current filter, in display order.
    pub view: Vec<usize>,
    filter: String,
}

impl Library {
    pub fn new(tracks: Vec<Track>) -> Self {
        let mut lib = Self {
            tracks,
            view: Vec::new(),
            filter: String::new(),
        };
        lib.rebuild_view();
        lib
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.rebuild_view();
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn rebuild_view(&mut self) {
        let needle = self.filter.to_lowercase();
        self.view = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| needle.is_empty() || t.search_haystack().contains(&needle))
            .map(|(i, _)| i)
            .collect();
    }

    /// Track at a view row, if any.
    pub fn track_at_view(&self, row: usize) -> Option<&Track> {
        self.view.get(row).and_then(|&i| self.tracks.get(i))
    }

    pub fn view_len(&self) -> usize {
        self.view.len()
    }

    /// Append a single track during an in-progress scan (cheap; no sort/save).
    pub fn push(&mut self, track: Track) {
        self.tracks.push(track);
    }

    /// Called once a scan completes: sort, rebuild the view, and cache to disk.
    pub fn finalize(&mut self) {
        sort_tracks(&mut self.tracks);
        self.rebuild_view();
        save_cache(&self.tracks);
    }
}

fn sort_tracks(tracks: &mut [Track]) {
    tracks.sort_by(|a, b| {
        a.artist
            .to_lowercase()
            .cmp(&b.artist.to_lowercase())
            .then(a.album.to_lowercase().cmp(&b.album.to_lowercase()))
            .then(a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
}

/// Load the cached library from disk, if present.
pub fn load_cache() -> Vec<Track> {
    fs::read_to_string(config::library_cache_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(tracks: &[Track]) {
    let path = config::library_cache_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    if let Ok(json) = serde_json::to_string(tracks) {
        fs::write(path, json).ok();
    }
}

/// Audio extensions Orbit can decode (rodio/symphonia) and tag (lofty).
const SUPPORTED_EXTS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "oga", "m4a", "mp4", "aac",
];

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            SUPPORTED_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

/// Read one file's tags into a Track. Falls back to the file name for the title.
pub fn read_track(path: &Path) -> Option<Track> {
    if !is_supported(path) {
        return None;
    }
    let tagged = lofty::read_from_path(path).ok();

    let (title, artist, album) = match &tagged {
        Some(t) => {
            let tag = t.primary_tag().or_else(|| t.first_tag());
            match tag {
                Some(tag) => (
                    tag.title().map(|c| c.to_string()).unwrap_or_default(),
                    tag.artist().map(|c| c.to_string()).unwrap_or_default(),
                    tag.album().map(|c| c.to_string()).unwrap_or_default(),
                ),
                None => (String::new(), String::new(), String::new()),
            }
        }
        None => (String::new(), String::new(), String::new()),
    };

    let duration_secs = tagged
        .as_ref()
        .map(|t| t.properties().duration().as_secs())
        .unwrap_or(0);

    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let title = if title.is_empty() {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string()
    } else {
        title
    };

    Some(Track {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        duration_secs,
        mtime,
    })
}

/// Spawn a background scan of all roots. Tracks stream back over the channel;
/// the channel closing signals completion.
pub fn spawn_scan(roots: Vec<PathBuf>) -> Receiver<Track> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for root in roots {
            for entry in WalkDir::new(&root)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                if let Some(track) = read_track(entry.path()) {
                    // If the receiver is gone, stop early.
                    if tx.send(track).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}
