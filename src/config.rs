//! Persistent configuration: library roots, last volume, EQ settings, modes.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::audio::NUM_BANDS;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Library roots to scan, merged into one library.
    pub roots: Vec<PathBuf>,
    /// Last playback volume, 0.0..=1.25.
    pub volume: f32,
    pub eq_enabled: bool,
    /// Pre-amp in dB applied after the band filters.
    pub eq_preamp: f32,
    /// Per-band gains in dB (length == NUM_BANDS).
    pub eq_gains: Vec<f32>,
    /// 0 = off, 1 = all, 2 = one.
    pub repeat: u8,
    pub shuffle: bool,
    /// Index of the active colour palette.
    pub palette: usize,
    /// Zen visualizer: 0 = spectrum, 1 = cassette.
    pub zen_viz: usize,
    /// Show the key-hint strip in the footer.
    pub footer_hints: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            volume: 0.8,
            eq_enabled: false,
            eq_preamp: 0.0,
            eq_gains: vec![0.0; NUM_BANDS],
            repeat: 0,
            shuffle: false,
            palette: 0,
            zen_viz: 0,
            footer_hints: true,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_file();
        let mut cfg: Config = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Defend against a corrupt/short gains vector.
        if cfg.eq_gains.len() != NUM_BANDS {
            cfg.eq_gains.resize(NUM_BANDS, 0.0);
        }
        cfg
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json).with_context(|| format!("writing config to {path:?}"))?;
        Ok(())
    }
}

fn project_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "orbit") {
        dirs.data_dir().to_path_buf()
    } else {
        home_dir().join(".orbit")
    }
}

/// The user's home directory, cross-platform (`%USERPROFILE%` on Windows).
pub fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_file() -> PathBuf {
    project_dir().join("config.json")
}

pub fn buckets_file() -> PathBuf {
    project_dir().join("buckets.json")
}

pub fn library_cache_file() -> PathBuf {
    project_dir().join("library.json")
}

pub fn stats_file() -> PathBuf {
    project_dir().join("stats.json")
}

/// Default music directory guess for first-run convenience. Uses the OS's
/// standard "Music" location (cross-platform), falling back to `~/Music`.
pub fn default_music_dir() -> Option<PathBuf> {
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(audio) = dirs.audio_dir() {
            if audio.is_dir() {
                return Some(audio.to_path_buf());
            }
        }
    }
    let candidate = home_dir().join("Music");
    candidate.is_dir().then_some(candidate)
}
