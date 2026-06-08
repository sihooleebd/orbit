//! Core data types shared across Orbit.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A single playable track with the metadata we care about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Duration in whole seconds (0 if unknown).
    pub duration_secs: u64,
    /// File modification time (Unix seconds) — drives "Recently Added".
    #[serde(default)]
    pub mtime: u64,
}

impl Track {
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.duration_secs)
    }

    /// Best-effort display title. The scanner always fills this from the file
    /// stem when there's no tag, so it's effectively never empty.
    pub fn display_title(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else {
            "Untitled"
        }
    }

    /// Artist, or `None` when there's no tag (so callers can omit it).
    pub fn artist_opt(&self) -> Option<&str> {
        if self.artist.is_empty() {
            None
        } else {
            Some(&self.artist)
        }
    }

    /// Album, or `None` when there's no tag.
    pub fn album_opt(&self) -> Option<&str> {
        if self.album.is_empty() {
            None
        } else {
            Some(&self.album)
        }
    }

    /// `"Title — Artist"`, or just the title when the artist is unknown.
    pub fn title_artist(&self) -> String {
        match self.artist_opt() {
            Some(artist) => format!("{} — {}", self.display_title(), artist),
            None => self.display_title().to_string(),
        }
    }

    /// `"Artist — Title"`, or just the title when the artist is unknown.
    pub fn artist_title(&self) -> String {
        match self.artist_opt() {
            Some(artist) => format!("{} — {}", artist, self.display_title()),
            None => self.display_title().to_string(),
        }
    }

    /// Lowercased haystack for fuzzy-ish filtering.
    pub fn search_haystack(&self) -> String {
        format!(
            "{} {} {}",
            self.title, self.artist, self.album
        )
        .to_lowercase()
    }
}

/// Format a duration as `M:SS` or `H:MM:SS`.
pub fn fmt_duration(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
