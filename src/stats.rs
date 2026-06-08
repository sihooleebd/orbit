//! Play statistics — counts and last-played times that feed the smart buckets.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayStat {
    pub count: u32,
    /// Unix seconds of the last play (0 if never).
    pub last_played: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Stats {
    pub plays: HashMap<PathBuf, PlayStat>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Stats {
    pub fn load() -> Self {
        fs::read_to_string(stats_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = stats_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        if let Ok(json) = serde_json::to_string(self) {
            fs::write(path, json).ok();
        }
    }

    /// Record one play of `path`: bump the count and stamp the time.
    pub fn record_play(&mut self, path: &Path) {
        let entry = self.plays.entry(path.to_path_buf()).or_default();
        entry.count = entry.count.saturating_add(1);
        entry.last_played = now_secs();
    }

    pub fn count(&self, path: &Path) -> u32 {
        self.plays.get(path).map(|s| s.count).unwrap_or(0)
    }

    pub fn last_played(&self, path: &Path) -> u64 {
        self.plays.get(path).map(|s| s.last_played).unwrap_or(0)
    }
}

fn stats_file() -> PathBuf {
    config::stats_file()
}
