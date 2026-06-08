//! Library scanning and the in-memory track collection.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::Accessor;
use walkdir::WalkDir;

use crate::config;
use crate::model::Track;

/// A row shown in the Library pane.
pub enum LibEntry {
    /// The ".." row that navigates up a level.
    Parent,
    /// A sub-directory (with a recursive track count).
    Folder { path: PathBuf, count: usize },
    /// A track (index into `tracks`).
    Track(usize),
}

/// The merged library, navigable by folder, with a search filter.
#[derive(Default)]
pub struct Library {
    pub tracks: Vec<Track>,
    /// Library roots (top-level folders).
    roots: Vec<PathBuf>,
    /// Current folder being browsed (None = top level showing the roots).
    cwd: Option<PathBuf>,
    filter: String,
    /// The current display rows.
    pub entries: Vec<LibEntry>,
}

impl Library {
    pub fn new(tracks: Vec<Track>) -> Self {
        let mut lib = Self {
            tracks,
            roots: Vec::new(),
            cwd: None,
            filter: String::new(),
            entries: Vec::new(),
        };
        lib.rebuild_view();
        lib
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = roots;
        // If the current folder is no longer under a root, reset to top.
        if let Some(dir) = &self.cwd {
            if !self.roots.iter().any(|r| dir.starts_with(r)) {
                self.cwd = None;
            }
        }
        self.rebuild_view();
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.rebuild_view();
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Reset folder navigation back to the top level.
    pub fn reset_nav(&mut self) {
        self.cwd = None;
        self.rebuild_view();
    }

    /// A short label for the current folder (for the panel title).
    pub fn cwd_label(&self) -> Option<String> {
        self.cwd
            .as_ref()
            .map(|d| d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| d.display().to_string()))
    }

    /// Descend into a folder.
    pub fn enter(&mut self, path: PathBuf) {
        self.cwd = Some(path);
        self.rebuild_view();
    }

    /// Go up one level (to the parent folder, or the top when leaving a root).
    pub fn go_up(&mut self) {
        if let Some(dir) = self.cwd.clone() {
            if self.roots.iter().any(|r| *r == dir) {
                self.cwd = None;
            } else {
                self.cwd = dir.parent().map(|p| p.to_path_buf());
            }
            self.rebuild_view();
        }
    }

    pub fn entry_at(&self, row: usize) -> Option<&LibEntry> {
        self.entries.get(row)
    }

    pub fn entries_len(&self) -> usize {
        self.entries.len()
    }

    pub fn track(&self, idx: usize) -> Option<&Track> {
        self.tracks.get(idx)
    }

    /// Tracks in the current scope: filtered matches, else everything under the
    /// current folder (recursively), else the whole library.
    pub fn scoped_tracks(&self) -> Vec<Track> {
        if !self.filter.is_empty() {
            let needle = self.filter.to_lowercase();
            return self
                .tracks
                .iter()
                .filter(|t| t.search_haystack().contains(&needle))
                .cloned()
                .collect();
        }
        match &self.cwd {
            Some(dir) => self
                .tracks
                .iter()
                .filter(|t| t.path.starts_with(dir))
                .cloned()
                .collect(),
            None => self.tracks.clone(),
        }
    }

    pub fn rebuild_view(&mut self) {
        self.entries.clear();

        // Search overrides folder browsing with a flat, global result list.
        if !self.filter.is_empty() {
            let needle = self.filter.to_lowercase();
            for (i, t) in self.tracks.iter().enumerate() {
                if t.search_haystack().contains(&needle) {
                    self.entries.push(LibEntry::Track(i));
                }
            }
            return;
        }

        match self.cwd.clone() {
            None => {
                // Top level: the roots, each with a recursive track count.
                let mut roots = self.roots.clone();
                roots.sort_by_key(|p| folder_sort_key(p));
                for r in roots {
                    let count = self.tracks.iter().filter(|t| t.path.starts_with(&r)).count();
                    self.entries.push(LibEntry::Folder { path: r, count });
                }
            }
            Some(dir) => {
                self.entries.push(LibEntry::Parent);
                let mut counts: HashMap<PathBuf, usize> = HashMap::new();
                let mut tracks_here: Vec<usize> = Vec::new();
                for (i, t) in self.tracks.iter().enumerate() {
                    if let Ok(rel) = t.path.strip_prefix(&dir) {
                        let mut comps = rel.components();
                        if let Some(first) = comps.next() {
                            if comps.next().is_some() {
                                *counts.entry(dir.join(first.as_os_str())).or_insert(0) += 1;
                            } else {
                                tracks_here.push(i);
                            }
                        }
                    }
                }
                let mut folders: Vec<(PathBuf, usize)> = counts.into_iter().collect();
                folders.sort_by(|a, b| folder_sort_key(&a.0).cmp(&folder_sort_key(&b.0)));
                for (path, count) in folders {
                    self.entries.push(LibEntry::Folder { path, count });
                }
                // tracks_here keeps the global (artist/album/title) sort order.
                for i in tracks_here {
                    self.entries.push(LibEntry::Track(i));
                }
            }
        }
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

fn folder_sort_key(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| p.to_string_lossy().to_lowercase())
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
