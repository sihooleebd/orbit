//! File locations, the TOML config (every field has a default, so partial configs work),
//! and the commented default template written by `orbit --init-config`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::app::Tab;
use crate::dsp::{EQ_FREQS, EQ_MAX_DB};
use crate::keymap::Keymap;
use crate::library::{BrowseMode, SortKey};
use crate::queue::Repeat;
use crate::theme::Theme;
use crate::visualizer::VisMode;

/// Where orbit keeps its files. Honors XDG_CONFIG_HOME / XDG_DATA_HOME / XDG_CACHE_HOME / XDG_RUNTIME_DIR;
/// defaults to ~/.config/orbit, ~/.local/share/orbit, ~/.cache/orbit.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_file: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// data_dir/state.json: session, play counts, favorites
    pub state_file: PathBuf,
    /// data_dir/playlists/*.m3u8
    pub playlists_dir: PathBuf,
    /// cache_dir/library.json: metadata cache
    pub library_cache: PathBuf,
    /// IPC socket for `orbit ctl`
    pub socket: PathBuf,
}

impl Paths {
    pub fn new(config_override: Option<PathBuf>) -> Paths {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(fallback))
        };
        let config_dir = xdg("XDG_CONFIG_HOME", ".config").join("orbit");
        let data_dir = xdg("XDG_DATA_HOME", ".local/share").join("orbit");
        let cache_dir = xdg("XDG_CACHE_HOME", ".cache").join("orbit");
        let socket = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir).join("orbit.sock"),
            None => std::env::temp_dir().join(format!("orbit-{}.sock", std::env::var("USER").unwrap_or_else(|_| "user".into()))),
        };
        Paths {
            config_file: config_override.unwrap_or_else(|| config_dir.join("config.toml")),
            state_file: data_dir.join("state.json"),
            playlists_dir: data_dir.join("playlists"),
            library_cache: cache_dir.join("library.json"),
            config_dir,
            data_dir,
            cache_dir,
            socket,
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.cache_dir, &self.playlists_dir] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// "~/Music" -> "/Users/me/Music"
pub fn expand_tilde(p: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    if p == "~" {
        home
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// The candidate closest to `word` (case-insensitive edit distance) if it is close enough to be a
/// likely typo: "ui.them" -> "theme", "acent" -> "accent", "volme" -> "volume".
pub fn did_you_mean<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max = (word.chars().count() / 3).max(1);
    candidates
        .into_iter()
        .map(|c| (edit_distance(word, c), c))
        .filter(|(d, _)| *d <= max)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub library: LibraryConfig,
    pub playback: PlaybackConfig,
    pub ui: UiConfig,
    pub visualizer: VisualizerConfig,
    pub lyrics: LyricsConfig,
    pub eq: EqSettings,
    pub notifications: NotificationsConfig,
    pub ipc: IpcConfig,
    /// key -> action name or ":command". `"ctrl+n" = "next"`, `"F1" = ":vol 30"`, `"x" = "none"` unbinds.
    pub keys: BTreeMap<String, String>,
    /// Color overrides applied on top of `ui.theme`, e.g. `accent = "#ff79c6"` (see theme.rs for slot names).
    /// A list value (`gradient = ["#000", "#fff"]`) is accepted and stored comma-separated.
    #[serde(deserialize_with = "color_overrides")]
    pub colors: BTreeMap<String, String>,
}

fn color_overrides<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, String>, D::Error> {
    use serde::de::Error;
    BTreeMap::<String, toml::Value>::deserialize(d)?
        .into_iter()
        .map(|(slot, value)| {
            let text = match value {
                toml::Value::String(s) => Some(s),
                toml::Value::Array(list) => {
                    list.iter().map(|v| v.as_str()).collect::<Option<Vec<&str>>>().map(|colors| colors.join(", "))
                }
                _ => None,
            };
            text.map(|t| (slot, t)).ok_or_else(|| D::Error::custom("colors must be strings like \"#ff79c6\" (or a list of them)"))
        })
        .collect()
}

/// Sections whose keys are user-defined (not checked for typos by the schema walk).
const FREE_FORM: &[&str] = &["keys", "colors"];

impl Config {
    /// Missing file -> defaults. Parse errors carry the file name and TOML line/column.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    /// Like `load`, but forgiving: only unreadable files and TOML syntax errors are errors. An option with
    /// a bad value (wrong type, unknown enum name) keeps its default and produces a warning, as do unknown
    /// options (typos like `ui.them`, with "did you mean" hints) and everything `validate` reports.
    pub fn load_with_warnings(path: &Path) -> anyhow::Result<(Config, Vec<String>)> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse_lenient(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), Vec::new())),
            Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
        }
    }

    fn parse_lenient(text: &str) -> Result<(Config, Vec<String>), toml::de::Error> {
        let mut table: toml::Table = text.parse()?;
        let mut warnings = Vec::new();
        let schema = schema();
        unknown_options(&table, &schema, &schema, "", &mut warnings);
        let explicit_bands = table.get("eq").and_then(|eq| eq.get("bands")).is_some();
        let mut cfg = match table.clone().try_into::<Config>() {
            Ok(cfg) => cfg,
            Err(_) => {
                drop_invalid(&mut table, &mut warnings);
                table.try_into().unwrap_or_else(|e: toml::de::Error| {
                    warnings.push(format!("config: {} (using the defaults)", e.message()));
                    Config::default()
                })
            }
        };
        // a named preset sets the bands; they apply as written only with preset = "custom"
        if let Some(bands) = crate::dsp::eq_preset(&cfg.eq.preset) {
            if explicit_bands && bands != cfg.eq.bands {
                warnings.push(format!("config: eq.bands is ignored with eq.preset \"{}\" (use preset = \"custom\")", cfg.eq.preset));
            }
            cfg.eq.bands = bands;
        }
        warnings.extend(cfg.validate());
        Ok((cfg, warnings))
    }

    /// Semantic problems that parse fine: out-of-range numbers, inconsistent settings, unknown theme or
    /// EQ preset, and bad `[colors]` / `[keys]` entries. The last two are exactly the warnings
    /// `Theme::from_config` and `Keymap::new` return for this config (same strings), so dedup when merging.
    pub fn validate(&self) -> Vec<String> {
        let mut w = Vec::new();
        let p = &self.playback;
        if p.volume > p.max_volume {
            w.push(format!("config: playback.volume ({}) is above playback.max_volume ({})", p.volume, p.max_volume));
        }
        if p.max_volume > 150 {
            w.push(format!("config: playback.max_volume ({}) is above 150, the loudest orbit plays", p.max_volume));
        }
        if p.volume_step == 0 {
            w.push("config: playback.volume_step is 0, so volume keys do nothing".into());
        }
        if p.speed_step.is_nan() || p.speed_step <= 0.0 {
            w.push(format!("config: playback.speed_step ({}) must be above 0", p.speed_step));
        }
        if !(0.0..=1.0).contains(&p.count_play_after) {
            w.push(format!("config: playback.count_play_after ({}) must be between 0.0 and 1.0", p.count_play_after));
        }

        let u = &self.ui;
        if u.fps == 0 {
            w.push("config: ui.fps must be at least 1".into());
        } else if u.fps > 240 {
            w.push(format!("config: ui.fps ({}) is above 240 and will be capped", u.fps));
        }
        let split: u32 = u.library_split.iter().map(|&x| u32::from(x)).sum();
        if !(98..=102).contains(&split) {
            w.push(format!("config: ui.library_split adds up to {split}; it should be 100"));
        }
        if u.columns.is_empty() {
            w.push("config: ui.columns is empty, so track lists show nothing".into());
        }

        let v = &self.visualizer;
        if !v.fft_size.is_power_of_two() || !(512..=16384).contains(&v.fft_size) {
            w.push(format!("config: visualizer.fft_size ({}) must be a power of two from 512 to 16384", v.fft_size));
        }
        if v.min_freq.is_nan() || v.min_freq <= 0.0 {
            w.push(format!("config: visualizer.min_freq ({}) must be above 0", v.min_freq));
        } else if v.max_freq.is_nan() || v.min_freq >= v.max_freq {
            w.push(format!("config: visualizer.min_freq ({}) must be below visualizer.max_freq ({})", v.min_freq, v.max_freq));
        }
        if !(0.0..1.0).contains(&v.smoothing) {
            w.push(format!("config: visualizer.smoothing ({}) must be at least 0.0 and below 1.0", v.smoothing));
        }
        if v.falloff.is_nan() || v.falloff <= 0.0 {
            w.push(format!("config: visualizer.falloff ({}) must be above 0", v.falloff));
        }
        if v.db_floor.is_nan() || v.db_floor >= 0.0 {
            w.push(format!("config: visualizer.db_floor ({}) must be below 0", v.db_floor));
        }
        if v.bar_width == 0 {
            w.push("config: visualizer.bar_width must be at least 1".into());
        }

        let e = &self.eq;
        if e.preset != "custom" && crate::dsp::eq_preset(&e.preset).is_none() {
            let presets = crate::dsp::eq_preset_names();
            let hint = did_you_mean(&e.preset, presets.iter().copied())
                .map_or_else(|| format!("use one of {} or \"custom\"", presets.join(", ")), |n| format!("did you mean \"{n}\"?"));
            w.push(format!("config: eq.preset \"{}\" is not a preset ({hint})", e.preset));
        }
        // TOML allows nan and inf, so every float check also rejects NaN
        if e.preamp_db.is_nan() || e.preamp_db.abs() > EQ_MAX_DB {
            w.push(format!("config: eq.preamp_db ({}) must be between -{EQ_MAX_DB} and {EQ_MAX_DB}", e.preamp_db));
        }
        for (gain, hz) in e.bands.iter().zip(EQ_FREQS) {
            if gain.is_nan() || gain.abs() > EQ_MAX_DB {
                w.push(format!("config: eq.bands: {gain} dB at {hz} Hz must be between -{EQ_MAX_DB} and {EQ_MAX_DB}"));
            }
        }

        if self.library.dirs.is_empty() {
            w.push("config: library.dirs is empty, so there is nothing to scan".into());
        }
        for ext in self.library.extensions.iter().filter(|x| x.starts_with('.')) {
            w.push(format!("config: library.extensions: write \"{}\", not \"{ext}\"", ext.trim_start_matches('.')));
        }

        w.extend(Theme::from_config(&u.theme, &self.colors).1);
        w.extend(Keymap::new(&self.keys).1);
        w
    }

    pub fn music_dirs(&self) -> Vec<PathBuf> {
        self.library.dirs.iter().map(|d| expand_tilde(d)).collect()
    }
}

/// Every known option: the defaults serialized to TOML, with optional options filled in (None is skipped).
fn schema() -> toml::Table {
    let mut all = Config::default();
    all.ipc.socket = Some(String::new());
    toml::Table::try_from(all).expect("the config serializes to TOML")
}

/// Warn about options the schema doesn't know, with a hint: a close sibling ("ui.them" -> "ui.theme") or
/// the same name in another section ("theme" at the top level -> "ui.theme").
fn unknown_options(user: &toml::Table, known: &toml::Table, root: &toml::Table, path: &str, out: &mut Vec<String>) {
    for (key, value) in user {
        let full = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
        match (known.get(key), value) {
            (None, _) => {
                let sibling = did_you_mean(key, known.keys().map(String::as_str))
                    .map(|k| if path.is_empty() { k.to_string() } else { format!("{path}.{k}") });
                let elsewhere = || {
                    let found: Vec<String> = root
                        .iter()
                        .filter(|(section, _)| !FREE_FORM.contains(&section.as_str()))
                        .filter_map(|(section, v)| {
                            v.as_table()?.keys().find(|k| k.eq_ignore_ascii_case(key)).map(|k| format!("{section}.{k}"))
                        })
                        .filter(|p| *p != full)
                        .collect();
                    (!found.is_empty() && found.len() <= 3).then(|| found.join("\" or \""))
                };
                let hint = sibling.or_else(elsewhere).map(|s| format!(" (did you mean \"{s}\"?)")).unwrap_or_default();
                out.push(format!("config: unknown option \"{full}\"{hint}"));
            }
            (Some(toml::Value::Table(k)), toml::Value::Table(u)) if !FREE_FORM.contains(&full.as_str()) => {
                unknown_options(u, k, root, &full, out)
            }
            _ => {}
        }
    }
}

/// Remove every option whose value doesn't deserialize (checked one at a time, in context), with a warning.
fn drop_invalid(table: &mut toml::Table, warnings: &mut Vec<String>) {
    let check = |section: &str, value: toml::Value| -> Result<(), toml::de::Error> {
        toml::Table::from_iter([(section.to_string(), value)]).try_into::<Config>().map(|_| ())
    };
    let mut bad = Vec::new();
    for (section, value) in table.iter() {
        match value {
            toml::Value::Table(options) => {
                for (key, v) in options {
                    let one = toml::Table::from_iter([(key.clone(), v.clone())]);
                    if let Err(e) = check(section, toml::Value::Table(one)) {
                        bad.push((section.clone(), Some(key.clone()), e));
                    }
                }
            }
            other => {
                if let Err(e) = check(section, other.clone()) {
                    bad.push((section.clone(), None, e));
                }
            }
        }
    }
    for (section, key, e) in bad {
        let path = match &key {
            Some(k) => {
                table.get_mut(&section).and_then(toml::Value::as_table_mut).map(|t| t.remove(k));
                format!("{section}.{k}")
            }
            None => {
                table.remove(&section);
                section
            }
        };
        let msg = e.message().lines().next().unwrap_or_default().to_string();
        warnings.push(format!("config: {path}: {msg} (using the default)"));
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LibraryConfig {
    /// Folders to scan (recursively). `~` is expanded.
    pub dirs: Vec<String>,
    /// File extensions treated as music (lowercase, no dot).
    pub extensions: Vec<String>,
    pub follow_symlinks: bool,
    /// Skip files and folders whose name starts with '.'.
    pub ignore_hidden: bool,
    /// Skip any path containing one of these substrings (e.g. "/Voice Memos/").
    pub exclude: Vec<String>,
    /// Skip tracks shorter than this many seconds (0 = keep everything).
    pub min_duration_secs: u32,
    /// Cache metadata between runs for instant startup.
    pub use_cache: bool,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        LibraryConfig {
            dirs: vec!["~/Music".into()],
            extensions: ["mp3", "flac", "wav", "m4a", "mp4", "aac", "ogg", "oga"].map(String::from).to_vec(),
            follow_symlinks: false,
            ignore_hidden: true,
            // GarageBand / Logic project bundles hold loops and stems, not music to browse
            exclude: vec![".band/".into(), ".logicx/".into()],
            min_duration_secs: 0,
            use_cache: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayGainMode {
    #[default]
    Off,
    Track,
    Album,
    /// Album gain while the queue plays in order, track gain while it's shuffled.
    Auto,
}

/// What Enter on a track does in the library / playlists.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PlayContext {
    /// Replace the queue with the list the track is in and play it (like most players).
    #[default]
    List,
    /// Play just this track now, keeping the queue (inserted before the rest).
    Single,
    /// Add to the end of the queue without interrupting.
    Enqueue,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlaybackConfig {
    /// Startup volume in percent (0..=max_volume) when no session is restored.
    pub volume: u8,
    pub volume_step: u8,
    /// Upper limit; values above 100 amplify.
    pub max_volume: u8,
    pub seek_step_secs: u32,
    pub seek_step_large_secs: u32,
    pub speed_step: f32,
    /// Queue the next track in advance so there is no gap between tracks.
    pub gapless: bool,
    /// Short fade on pause / resume / stop to avoid clicks (0 = off).
    pub fade_ms: u32,
    /// Restore queue, current track, volume, EQ and modes on startup.
    pub resume_session: bool,
    /// Also restore the playback position of the current track.
    pub resume_position: bool,
    /// Start playing immediately after restoring a session.
    pub autoplay: bool,
    pub replaygain: ReplayGainMode,
    pub replaygain_preamp_db: f32,
    /// Lower the gain if the track's peak would clip.
    pub replaygain_prevent_clip: bool,
    pub on_select: PlayContext,
    /// "prev" restarts the current track if it has played longer than this.
    pub prev_restarts_after_secs: u32,
    /// Initial modes when no session is restored.
    pub repeat: Repeat,
    pub shuffle: bool,
    /// Count a play once this fraction of the track has been heard (0.0..=1.0).
    pub count_play_after: f32,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        PlaybackConfig {
            volume: 70,
            volume_step: 5,
            max_volume: 100,
            seek_step_secs: 5,
            seek_step_large_secs: 30,
            speed_step: 0.05,
            gapless: true,
            fade_ms: 120,
            resume_session: true,
            resume_position: true,
            autoplay: false,
            replaygain: ReplayGainMode::Off,
            replaygain_preamp_db: 0.0,
            replaygain_prevent_clip: true,
            on_select: PlayContext::List,
            prev_restarts_after_secs: 3,
            repeat: Repeat::Off,
            shuffle: false,
            count_play_after: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IconSet {
    /// Nerd Font glyphs (needs a patched font).
    Nerd,
    #[default]
    Unicode,
    Ascii,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BorderStyle {
    #[default]
    Rounded,
    Plain,
    Double,
    Thick,
    None,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProgressStyle {
    /// ━━━━━━●──────
    #[default]
    Line,
    /// ██████░░░░░░
    Block,
    /// ▰▰▰▰▰▱▱▱▱
    Segments,
    /// ⣿⣿⣿⣿⣀⣀⣀⣀
    Dots,
    /// line colored with the visualizer gradient
    Gradient,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TimeDisplay {
    #[default]
    Elapsed,
    Remaining,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BarPosition {
    Top,
    #[default]
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Align {
    Left,
    #[default]
    Center,
}

/// Columns of track tables (library, queue, playlists, search).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Column {
    /// Row number in the list
    Index,
    /// Track number from tags ("1", "2-03" with disc)
    Track,
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    Year,
    Duration,
    Plays,
    Format,
    Bitrate,
    /// ♥ marker
    Favorite,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Built-in theme name (orbit --list-themes); `[colors]` overrides single colors on top of it.
    pub theme: String,
    pub icons: IconSet,
    pub border: BorderStyle,
    /// Redraw rate while music plays (visualizer, lyrics). Idle redraws happen on input only.
    pub fps: u16,
    pub mouse: bool,
    pub default_tab: Tab,
    pub browse_mode: BrowseMode,
    pub sort: SortKey,
    pub sort_desc: bool,
    pub columns: Vec<Column>,
    /// Library pane widths in percent: [modes, groups, tracks].
    pub library_split: [u16; 3],
    pub player_bar: BarPosition,
    pub progress: ProgressStyle,
    pub time_display: TimeDisplay,
    pub show_art: bool,
    pub show_lyrics: bool,
    pub mini_visualizer: bool,
    pub show_tab_bar: bool,
    /// No borders or padding; more room for content.
    pub compact: bool,
    /// Tint the accent color with the album art's dominant color.
    pub dynamic_accent: bool,
    /// Set the terminal window title to the playing track.
    pub terminal_title: bool,
    /// Placeholders: {title} {artist} {album} {album_artist} {year} {genre} {track} {duration}
    /// {position} {remaining} {state} {icon} {volume} {speed} {file}
    pub title_format: String,
    pub message_timeout_secs: u32,
    /// Keep this many rows visible above/below the selection when scrolling.
    pub scroll_margin: u16,
    pub highlight_playing: bool,
    pub lyrics_align: Align,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            theme: "orbit".into(),
            icons: IconSet::Unicode,
            border: BorderStyle::Rounded,
            fps: 30,
            mouse: true,
            default_tab: Tab::Library,
            browse_mode: BrowseMode::Folders,
            sort: SortKey::Default,
            sort_desc: false,
            columns: vec![Column::Index, Column::Title, Column::Artist, Column::Album, Column::Duration],
            library_split: [16, 26, 58],
            player_bar: BarPosition::Bottom,
            progress: ProgressStyle::Line,
            time_display: TimeDisplay::Elapsed,
            show_art: true,
            show_lyrics: true,
            mini_visualizer: true,
            show_tab_bar: true,
            compact: false,
            dynamic_accent: false,
            terminal_title: true,
            title_format: "{icon} {title} - {artist}".into(),
            message_timeout_secs: 3,
            scroll_margin: 3,
            highlight_playing: true,
            lyrics_align: Align::Center,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct VisualizerConfig {
    pub mode: VisMode,
    /// Number of bars; 0 = as many as fit.
    pub bars: u16,
    pub bar_width: u16,
    pub bar_gap: u16,
    pub min_freq: f32,
    pub max_freq: f32,
    /// Level (dB) drawn as an empty bar; 0 dB is a full bar.
    pub db_floor: f32,
    /// 0 = raw, 0.9 = very smooth.
    pub smoothing: f32,
    /// How fast bars fall, in full heights per second.
    pub falloff: f32,
    pub peaks: bool,
    pub peak_hold_ms: u32,
    /// Color bars by height using the theme gradient.
    pub gradient: bool,
    /// FFT window size (power of two, 512..=16384).
    pub fft_size: usize,
    /// Mirror / VU modes show left and right separately.
    pub stereo: bool,
}

impl Default for VisualizerConfig {
    fn default() -> Self {
        VisualizerConfig {
            mode: VisMode::Bars,
            bars: 0,
            bar_width: 2,
            bar_gap: 1,
            min_freq: 30.0,
            max_freq: 16000.0,
            db_floor: -70.0,
            smoothing: 0.55,
            falloff: 1.6,
            peaks: true,
            peak_hold_ms: 700,
            gradient: true,
            fft_size: 4096,
            stereo: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LyricsConfig {
    pub enabled: bool,
    /// Look for <track>.lrc / <track>.txt next to the audio file.
    pub sidecar: bool,
    /// Read lyrics embedded in the file's tags.
    pub embedded: bool,
    /// Extra folders searched for "<artist> - <title>.lrc" or "<title>.lrc".
    pub dirs: Vec<String>,
    /// Global timing offset in milliseconds (positive = lyrics later).
    pub offset_ms: i32,
}

impl Default for LyricsConfig {
    fn default() -> Self {
        LyricsConfig { enabled: true, sidecar: true, embedded: true, dirs: Vec::new(), offset_ms: 0 }
    }
}

/// 10-band graphic equalizer. Stored in the config (defaults) and in the session (live edits).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EqSettings {
    pub enabled: bool,
    /// Name of a preset from dsp::EQ_PRESETS, or "custom".
    pub preset: String,
    pub preamp_db: f32,
    /// Gains in dB for 31, 62, 125, 250, 500 Hz, 1, 2, 4, 8, 16 kHz (each -12..=12).
    pub bands: [f32; 10],
}

impl Default for EqSettings {
    fn default() -> Self {
        EqSettings { enabled: false, preset: "flat".into(), preamp_db: 0.0, bands: [0.0; 10] }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Desktop notification on track change.
    pub enabled: bool,
    pub title: String,
    pub body: String,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        NotificationsConfig { enabled: false, title: "{title}".into(), body: "{artist} - {album}".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct IpcConfig {
    /// Listen for `orbit ctl ...` commands.
    pub enabled: bool,
    /// Override the socket path.
    pub socket: Option<String>,
}

impl Default for IpcConfig {
    fn default() -> Self {
        IpcConfig { enabled: true, socket: None }
    }
}

/// The commented default config. Must parse to exactly `Config::default()` (tested).
pub const DEFAULT_CONFIG: &str = r##"# orbit configuration
#
# Every option is optional: delete what you don't need and orbit uses the default shown here.
# Unknown options and bad values are reported at startup. Reload with ctrl+r (reload-config),
# :reload or `orbit ctl reload`. `orbit --print-config` prints this file and `orbit --init-config`
# writes it to ~/.config/orbit/config.toml.

[library]
# Folders to scan (recursively). "~" is your home folder.
dirs = ["~/Music"]
# File extensions treated as music (lowercase, without the dot).
extensions = ["mp3", "flac", "wav", "m4a", "mp4", "aac", "ogg", "oga"]
# Follow symbolic links while scanning.
follow_symlinks = false
# Skip files and folders whose name starts with ".".
ignore_hidden = true
# Skip any path containing one of these substrings, e.g. "/Voice Memos/", "/Podcasts/".
# The defaults skip GarageBand and Logic project bundles (loops and stems, not your music).
exclude = [".band/", ".logicx/"]
# Skip tracks shorter than this many seconds (0 = keep everything).
min_duration_secs = 0
# Cache metadata between runs for instant startup (`orbit --rescan` re-reads every file once).
use_cache = true

[playback]
# Startup volume in percent (0 to max_volume) when no session is restored.
volume = 70
# Volume change per volume-up / volume-down, in percent.
volume_step = 5
# Upper volume limit in percent; values above 100 amplify (up to 150).
max_volume = 100
# Seek step of seek-forward / seek-backward, in seconds.
seek_step_secs = 5
# Seek step of seek-forward-large / seek-backward-large, in seconds.
seek_step_large_secs = 30
# Speed change per speed-up / speed-down (speed ranges from 0.25 to 4.0).
speed_step = 0.05
# Queue the next track in advance so there is no gap between tracks.
gapless = true
# Short fade on pause / resume / stop to avoid clicks, in milliseconds (0 = off).
fade_ms = 120
# Restore the queue, current track, volume, EQ and modes on startup.
resume_session = true
# Also restore the playback position of the current track.
resume_position = true
# Start playing right away after restoring a session.
autoplay = false
# Loudness normalization from ReplayGain tags: "off", "track", "album", or "auto"
# (album gain while the queue plays in order, track gain while it's shuffled).
replaygain = "off"
# Extra gain on top of ReplayGain, in dB.
replaygain_preamp_db = 0.0
# Lower the gain when a track's peak would clip.
replaygain_prevent_clip = true
# What Enter on a track does: "list" (play the list it is in from that track, replacing the queue),
# "single" (play just this track now, keeping the queue) or "enqueue" (add it to the queue's end).
on_select = "list"
# "prev" restarts the current track instead when it has played longer than this many seconds.
prev_restarts_after_secs = 3
# Repeat mode when no session is restored: "off", "all" or "one".
repeat = "off"
# Shuffle when no session is restored.
shuffle = false
# Count a play once this fraction of the track has been heard (0.0 to 1.0).
count_play_after = 0.5

[ui]
# Color theme; [colors] below overrides single colors on top of it. Cycle with ctrl+t.
# Built in (orbit --list-themes): orbit, ember, default (your terminal's own colors), catppuccin-mocha,
#   catppuccin-macchiato, catppuccin-frappe, tokyo-night, dracula, nord, gruvbox-dark, rose-pine,
#   rose-pine-moon, kanagawa, everforest, one-dark, monokai, ayu-dark, github-dark, nightfox,
#   solarized-dark, synthwave, sakura, matrix, catppuccin-latte, tokyo-night-day, rose-pine-dawn,
#   gruvbox-light, solarized-light, ayu-light, github-light, mono, high-contrast.
theme = "orbit"
# Icons: "unicode", "nerd" (needs a Nerd Font) or "ascii".
icons = "unicode"
# Borders: "rounded", "plain", "double", "thick" or "none".
border = "rounded"
# Redraws per second while music plays (visualizer, lyrics), 1 to 240. Idle, orbit redraws on input.
fps = 30
# Mouse: click tabs, rows, buttons and the progress bar; scroll lists, the volume and EQ sliders.
mouse = true
# Tab shown at startup: "library", "queue", "playlists", "now-playing" or "equalizer".
default_tab = "library"
# How the library is grouped: "folders", "artists", "albums", "genres", "years" or "tracks" (flat).
browse_mode = "folders"
# Sort order of track lists: "default", "title", "artist", "album", "duration", "year", "added",
# "plays", "path" or "random". "default" is album/disc/track order for albums and artists, folder
# and file name order for folders, and title order otherwise.
sort = "default"
# Sort descending.
sort_desc = false
# Columns of track tables, in order: "index", "track", "title", "artist", "album", "album-artist",
# "genre", "year", "duration", "plays", "format", "bitrate", "favorite".
columns = ["index", "title", "artist", "album", "duration"]
# Library pane widths in percent: [browse modes, groups, tracks] (should add up to 100).
library_split = [16, 26, 58]
# Player bar position: "top" or "bottom".
player_bar = "bottom"
# Progress bar: "line" (━━━●───), "block" (███░░░), "segments" (▰▰▰▱▱▱), "dots" (⣿⣿⣿⣀⣀⣀)
# or "gradient" (a line colored with the theme's visualizer gradient).
progress = "line"
# Time shown in the player bar: "elapsed" or "remaining" (toggle with ctrl+e).
time_display = "elapsed"
# Album art in Now Playing.
show_art = true
# Lyrics panel in Now Playing.
show_lyrics = true
# Small visualizer in the player bar.
mini_visualizer = true
# Tab bar at the top.
show_tab_bar = true
# No borders or padding: more room for content.
compact = false
# Tint the accent color with the album art's dominant color.
dynamic_accent = false
# Set the terminal window title to the playing track.
terminal_title = true
# Window title. Placeholders: {title} {artist} {album} {album_artist} {year} {genre} {track}
# {duration} {position} {remaining} {state} {icon} {volume} {speed} {file}
title_format = "{icon} {title} - {artist}"
# How long status messages stay, in seconds.
message_timeout_secs = 3
# Rows kept visible above and below the selection when scrolling.
scroll_margin = 3
# Highlight the row of the playing track.
highlight_playing = true
# Lyrics alignment: "left" or "center".
lyrics_align = "center"

[visualizer]
# Style: "bars", "mirror", "blocks", "wave", "vu" or "cassette" (cycle with v).
mode = "bars"
# Number of bars (0 = as many as fit).
bars = 0
# Bar width and the gap between bars, in cells.
bar_width = 2
bar_gap = 1
# Frequency range shown, in Hz.
min_freq = 30.0
max_freq = 16000.0
# Level drawn as an empty bar, in dB (0 dB is a full bar).
db_floor = -70.0
# Smoothing between frames: 0.0 = raw, 0.9 = very smooth.
smoothing = 0.55
# How fast bars fall, in full heights per second.
falloff = 1.6
# Peak markers above the bars, and how long they hold, in milliseconds.
peaks = true
peak_hold_ms = 700
# Color bars by height with the theme's gradient.
gradient = true
# FFT window size: a power of two from 512 to 16384 (bigger = finer bass, slower response).
fft_size = 4096
# Mirror and VU modes show the left and right channels separately.
stereo = true

[lyrics]
enabled = true
# Look for <track>.lrc / <track>.txt next to the audio file.
sidecar = true
# Read lyrics embedded in the file's tags.
embedded = true
# Extra folders searched for "<artist> - <title>.lrc" or "<title>.lrc".
dirs = []
# Timing offset in milliseconds (positive = lyrics later). Adjust per track with ( and ).
offset_ms = 0

[eq]
# 10-band graphic equalizer; the Equalizer tab edits it live (live edits are kept with the session).
enabled = false
# Preset: flat, bass-boost, bass-cut, treble-boost, vocal, loudness, pop, rock, jazz, classical,
# electronic, hip-hop, acoustic, night, or "custom" to use the bands below as written.
preset = "flat"
# Gain before the bands, in dB (-12 to 12).
preamp_db = 0.0
# Gains in dB (-12 to 12) at 31, 62, 125, 250, 500 Hz, 1, 2, 4, 8, 16 kHz.
bands = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]

[notifications]
# Desktop notification when the track changes.
enabled = false
# Notification title and body (same placeholders as ui.title_format).
title = "{title}"
body = "{artist} - {album}"

[ipc]
# Accept `orbit ctl ...` commands: `orbit ctl next`, `orbit ctl vol +5`,
# `orbit ctl status "{artist} - {title}"` (any command from the list in [keys] below).
enabled = true
# Socket path (default: $XDG_RUNTIME_DIR/orbit.sock, else orbit-$USER.sock in the temp folder).
# socket = "~/.cache/orbit.sock"

[keys]
# Rebind keys: "key" = "action", "key" = ":command" (anything the : command line accepts),
# or "key" = "none" to unbind. Several keys may share an action; ? in orbit lists every binding.
# Keys: a character ("a", "G", "?", "+"), or space enter esc tab backtab backspace delete insert
# home end pageup pagedown up down left right f1-f24 plus minus comma period slash backslash colon
# semicolon, with any of the modifiers ctrl+ alt+ shift+, e.g. "ctrl+n", "alt+enter", "shift+left"
# or "ctrl++" (ctrl and plus).
#
# "alt+n" = "next"
# "alt+p" = "prev"
# "F1" = ":vol 30"
# "F2" = ":theme nord"
# "F3" = ":eq rock"
# "F4" = ":sleep 30"
# "F5" = ":view artists"
# "F6" = ":sort plays desc"
# "F7" = ":seek 50%"
# "q" = "none"                       # unbind q (ctrl+c still quits)
#
# Actions:
#   General     quit help command-palette search reload-config redraw
#   Navigation  up down left right page-up page-down top bottom select back focus-next focus-prev
#               next-tab prev-tab tab-library tab-queue tab-playlists tab-now-playing
#               tab-equalizer jump-to-current
#   Playback    toggle-pause stop next prev seek-forward seek-backward seek-forward-large
#               seek-backward-large volume-up volume-down mute speed-up speed-down speed-reset
#               toggle-shuffle cycle-repeat toggle-stop-after ab-loop sleep-timer
#   Queue       enqueue play-next enqueue-all remove move-up move-down clear-queue shuffle-queue
#               radio
#   Library     toggle-favorite add-to-playlist new-playlist rename-playlist save-queue cycle-sort
#               reverse-sort cycle-browse-mode rescan download
#   View        cycle-visualizer toggle-lyrics toggle-art lyrics-offset-up lyrics-offset-down
#               cycle-theme toggle-time-display toggle-compact toggle-mini-visualizer
#   Equalizer   eq-toggle eq-next-preset eq-prev-preset eq-reset
#
# Commands (also for the : command line and `orbit ctl`; quote names with spaces):
#   play [file|folder|search text]    Resume, or play a file/folder/best match
#   pause                             Pause
#   toggle                            Play / pause
#   stop                              Stop
#   next                              Next track
#   prev                              Previous track
#   seek <+s|-s|m:ss|n%>              Seek relative, absolute or to a percentage
#   vol <n|+n|-n|reset>               Set or change the volume (percent)
#   mute [on|off]                     Mute / unmute (toggle without argument)
#   speed <x|+x|-x|reset>             Playback speed, e.g. 1.25
#   shuffle [on|off]                  Shuffle (toggle without argument)
#   repeat [off|all|one]              Repeat mode (cycle without argument)
#   stopafter [on|off]                Stop after the current track
#   sleep <minutes|1h30m|end|off>     Sleep timer
#   loop <a|b|clear>                  A-B loop points
#   eq <on|off|toggle|reset|[preset] NAME|band N DB|preamp DB>
#                                     Equalizer (N = 1-10 or a frequency like 1k)
#   theme [name]                      Switch theme (cycle without argument)
#   vis [bars|mirror|blocks|wave|vu|cassette]
#                                     Visualizer style (cycle without argument)
#   sort <default|title|artist|album|duration|year|added|plays|path|random> [asc|desc]
#                                     Sort track lists
#   view <folders|artists|albums|genres|years|tracks>
#                                     Library browse mode
#   goto <library|queue|playlists|now|eq|1-5>
#                                     Switch tab
#   add <path>...                     Add files or folders to the queue
#   clear                             Clear the queue
#   save <name>                       Save the queue as a playlist
#   load <name>                       Replace the queue with a playlist
#   playlist new <name>               Create a playlist
#   playlist delete <name>            Delete a playlist
#   playlist rename <old> <new>       Rename a playlist
#   search <text>                     Open search with text
#   rescan                            Rescan the library
#   radio                             Play the playing track, then what sounds most like it
#   download <url> [folder]           Download audio with yt-dlp into a music folder (default Downloads)
#   lyrics offset <ms|+ms|-ms|reset>  Shift lyric timing for this track
#   fav [on|off]                      Favorite the playing track
#   status [format]                   Print status, e.g. status {artist} - {title}
#   reload                            Reload the config file
#   help                              Show help
#   quit                              Quit

[colors]
# Override single colors of the theme: slot = "color".
# Slots: bg fg dim accent accent2 border border_focus title sel_bg sel_fg playing progress
#   progress_bg error warn ok lyric_active lyric gradient (visualizer colors, low to high).
# Colors: "#rrggbb", "#rgb", "rgb(r, g, b)", a name (black red green yellow blue magenta cyan gray
#   darkgray lightred lightgreen lightyellow lightblue lightmagenta lightcyan white), a 256-color
#   index ("0" to "255"), or "reset" (the terminal's own color).
#
# accent = "#ff79c6"
# sel_bg = "rgb(68, 71, 90)"
# border = "darkgray"
# playing = "114"
# bg = "reset"                            # keep the terminal's own (e.g. transparent) background
# gradient = "#50fa7b, #f1fa8c, #ff5555"  # or a list: ["#50fa7b", "#f1fa8c", "#ff5555"]
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Action;

    fn parse(text: &str) -> (Config, Vec<String>) {
        Config::parse_lenient(text).unwrap()
    }

    #[test]
    fn default_config_is_the_default() {
        assert_eq!(toml::from_str::<Config>(DEFAULT_CONFIG).unwrap(), Config::default());
        let (cfg, warnings) = parse(DEFAULT_CONFIG);
        assert_eq!(cfg, Config::default());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(Config::default().validate().is_empty());
    }

    #[test]
    fn default_config_lists_every_option_and_value() {
        // every option appears as `key = ...` (ipc.socket commented out, since it defaults to None)
        for (section, value) in schema() {
            let Some(options) = value.as_table() else { continue };
            assert!(DEFAULT_CONFIG.contains(&format!("[{section}]")), "[{section}]");
            for key in options.keys() {
                assert!(DEFAULT_CONFIG.lines().any(|l| l.trim_start_matches("# ").starts_with(&format!("{key} = "))), "{section}.{key}");
            }
        }
        let quoted = |v: toml::Value| format!("\"{}\"", v.as_str().unwrap());
        let mut values: Vec<String> = Vec::new();
        values.extend(Tab::ALL.map(|t| quoted(toml::Value::try_from(t).unwrap())));
        values.extend(BrowseMode::ALL.map(|m| quoted(toml::Value::try_from(m).unwrap())));
        values.extend(SortKey::ALL.map(|k| quoted(toml::Value::try_from(k).unwrap())));
        values.extend(VisMode::ALL.map(|m| quoted(toml::Value::try_from(m).unwrap())));
        for v in [
            toml::Value::try_from(IconSet::Nerd),
            toml::Value::try_from(IconSet::Ascii),
            toml::Value::try_from(BorderStyle::Plain),
            toml::Value::try_from(BorderStyle::Double),
            toml::Value::try_from(BorderStyle::Thick),
            toml::Value::try_from(BorderStyle::None),
            toml::Value::try_from(ProgressStyle::Block),
            toml::Value::try_from(ProgressStyle::Segments),
            toml::Value::try_from(ProgressStyle::Dots),
            toml::Value::try_from(ProgressStyle::Gradient),
            toml::Value::try_from(TimeDisplay::Remaining),
            toml::Value::try_from(BarPosition::Top),
            toml::Value::try_from(Align::Left),
            toml::Value::try_from(ReplayGainMode::Track),
            toml::Value::try_from(ReplayGainMode::Album),
            toml::Value::try_from(ReplayGainMode::Auto),
            toml::Value::try_from(PlayContext::Single),
            toml::Value::try_from(PlayContext::Enqueue),
            toml::Value::try_from(Repeat::All),
            toml::Value::try_from(Repeat::One),
        ] {
            values.push(quoted(v.unwrap()));
        }
        for c in [
            Column::Index,
            Column::Track,
            Column::Title,
            Column::Artist,
            Column::Album,
            Column::AlbumArtist,
            Column::Genre,
            Column::Year,
            Column::Duration,
            Column::Plays,
            Column::Format,
            Column::Bitrate,
            Column::Favorite,
        ] {
            values.push(quoted(toml::Value::try_from(c).unwrap()));
        }
        for v in values {
            assert!(DEFAULT_CONFIG.contains(&v), "{v} is not documented");
        }
        let words: Vec<&str> = DEFAULT_CONFIG.split(|c: char| c.is_whitespace() || c == ',' || c == '.').collect();
        for name in crate::theme::names() {
            assert!(words.contains(&name), "theme {name}");
        }
        for a in Action::ALL {
            assert!(words.contains(&a.name()), "action {}", a.name());
        }
        for slot in crate::theme::SLOTS {
            assert!(words.contains(slot), "slot {slot}");
        }
        for (usage, _) in crate::command::COMMANDS {
            assert!(DEFAULT_CONFIG.contains(usage), "command {usage}");
        }
    }

    #[test]
    fn documented_examples_are_valid() {
        // uncomment the example lines of [keys] and [colors]: they must load without a single warning
        let mut text = String::new();
        let mut section = "";
        for line in DEFAULT_CONFIG.lines() {
            if line.starts_with('[') {
                section = line;
            }
            let example = line.strip_prefix("# ").filter(|l| {
                (section == "[keys]" && l.starts_with('"'))
                    || (section == "[colors]" && crate::theme::SLOTS.iter().any(|s| l.starts_with(&format!("{s} = "))))
            });
            text.push_str(example.unwrap_or(line));
            text.push('\n');
        }
        let (cfg, warnings) = parse(&text);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.keys.len(), 10);
        assert_eq!(cfg.colors.len(), 6);
    }

    #[test]
    fn unknown_options_get_hints() {
        let (cfg, w) = parse(
            "theme = \"nord\"\n[UI]\nfps = 1\n[ui]\nthem = \"nord\"\n[playback]\nvolum = 5\nvolume = 50\n\
             [ipc]\nsocket = \"/tmp/x.sock\"\n[keys]\n\"F9\" = \"next\"\n[colors]\naccent = \"red\"\n[bogus]\nx = 1\n",
        );
        assert_eq!(cfg.playback.volume, 50);
        assert_eq!(cfg.ipc.socket.as_deref(), Some("/tmp/x.sock"));
        let has = |needle: &str| w.iter().any(|m| m.contains(needle));
        assert!(has("unknown option \"theme\" (did you mean \"ui.theme\"?)"), "{w:#?}");
        assert!(has("unknown option \"UI\" (did you mean \"ui\"?)"), "{w:#?}");
        assert!(has("unknown option \"ui.them\" (did you mean \"ui.theme\"?)"), "{w:#?}");
        assert!(has("unknown option \"playback.volum\" (did you mean \"playback.volume\"?)"), "{w:#?}");
        assert!(has("unknown option \"bogus\""), "{w:#?}");
        assert_eq!(w.len(), 5, "{w:#?}");
    }

    #[test]
    fn bad_values_keep_their_defaults() {
        let (cfg, w) = parse(
            "[playback]\nvolume = \"loud\"\nvolume_step = 10\n[ui]\nicons = \"nerdz\"\nfps = 60\n\
             columns = [\"title\", \"artst\"]\n[visualizer]\nmode = \"wave\"\n[eq]\nbands = [1.0]\nlyrics = 5\n",
        );
        assert_eq!(cfg.playback.volume, 70);
        assert_eq!(cfg.playback.volume_step, 10);
        assert_eq!(cfg.ui.icons, IconSet::Unicode);
        assert_eq!(cfg.ui.fps, 60);
        assert_eq!(cfg.ui.columns, UiConfig::default().columns);
        assert_eq!(cfg.visualizer.mode, VisMode::Wave);
        assert_eq!(cfg.eq.bands, [0.0; 10]);
        let has = |needle: &str| w.iter().any(|m| m.contains(needle));
        assert!(has("config: playback.volume: invalid type: string \"loud\""), "{w:#?}");
        assert!(has("config: ui.icons: unknown variant `nerdz`"), "{w:#?}");
        assert!(has("config: ui.columns:"), "{w:#?}");
        assert!(has("config: eq.bands:"), "{w:#?}");
        assert!(has("unknown option \"eq.lyrics\""), "{w:#?}");

        // a whole section of the wrong type
        let (cfg, w) = parse("ui = 5\n");
        assert_eq!(cfg, Config::default());
        assert!(w.iter().any(|m| m.starts_with("config: ui: ")), "{w:#?}");
        // colors must be strings (or lists of strings)
        let (cfg, w) = parse("[colors]\naccent = 5\nfg = \"red\"\n");
        assert_eq!(cfg.colors.get("fg").map(String::as_str), Some("red"));
        assert!(!cfg.colors.contains_key("accent"));
        assert!(w.iter().any(|m| m.starts_with("config: colors.accent: colors must be strings")), "{w:#?}");
        // syntax errors are still errors, with a line number
        let err = Config::parse_lenient("[ui\nfps = 3\n").unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn color_lists_are_joined() {
        let (cfg, w) = parse("[colors]\ngradient = [\"#000000\", \"#ffffff\"]\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(cfg.colors["gradient"], "#000000, #ffffff");
        let theme = Theme::from_config("default", &cfg.colors).0;
        assert_eq!(theme.gradient.len(), 2);
        // strict loading accepts the list form too
        assert!(toml::from_str::<Config>("[colors]\ngradient = [\"red\", \"blue\"]\n").is_ok());
    }

    #[test]
    fn validate_reports_semantic_problems() {
        let mut c = Config::default();
        c.playback.volume = 250;
        c.playback.max_volume = 200;
        c.playback.speed_step = 0.0;
        c.playback.count_play_after = 1.5;
        c.ui.fps = 0;
        c.ui.library_split = [30, 30, 30];
        c.ui.columns.clear();
        c.ui.theme = "nordd".into();
        c.visualizer.fft_size = 1000;
        c.visualizer.min_freq = 20000.0;
        c.visualizer.smoothing = 1.0;
        c.visualizer.falloff = f32::NAN;
        c.visualizer.db_floor = 3.0;
        c.visualizer.bar_width = 0;
        c.eq.preset = "rok".into();
        c.eq.preamp_db = 20.0;
        c.eq.bands[9] = -13.0;
        c.library.dirs.clear();
        c.library.extensions.push(".opus".into());
        c.colors.insert("acent".into(), "#fff".into());
        c.keys.insert("hyper+x".into(), "quit".into());
        c.keys.insert("x".into(), "nxt".into());
        let w = c.validate();
        for needle in [
            "playback.volume (250) is above playback.max_volume (200)",
            "playback.max_volume (200) is above 150",
            "playback.speed_step (0) must be above 0",
            "playback.count_play_after (1.5)",
            "ui.fps must be at least 1",
            "ui.library_split adds up to 90",
            "ui.columns is empty",
            "unknown theme \"nordd\" (did you mean \"nord\"?)",
            "visualizer.fft_size (1000) must be a power of two",
            "visualizer.min_freq (20000) must be below visualizer.max_freq (16000)",
            "visualizer.smoothing (1)",
            "visualizer.falloff (NaN)",
            "visualizer.db_floor (3)",
            "visualizer.bar_width",
            "eq.preset \"rok\" is not a preset (did you mean \"rock\"?)",
            "eq.preamp_db (20)",
            "-13 dB at 16000 Hz",
            "library.dirs is empty",
            "write \"opus\", not \".opus\"",
            "[colors] unknown slot \"acent\" (did you mean \"accent\"?)",
            "[keys] can't parse key \"hyper+x\"",
            "unknown action \"nxt\"",
        ] {
            assert!(w.iter().any(|m| m.contains(needle)), "missing {needle:?} in {w:#?}");
        }
        assert_eq!(w.len(), 22, "{w:#?}");

        let mut c = Config::default();
        c.visualizer.fft_size = 32768;
        c.ui.fps = 500;
        c.eq.preset = "custom".into();
        let w = c.validate();
        assert_eq!(w.len(), 2, "{w:#?}");
    }

    #[test]
    fn load_with_warnings_reads_files() {
        let dir = std::env::temp_dir().join(format!("orbit-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing.toml");
        assert_eq!(Config::load_with_warnings(&missing).unwrap(), (Config::default(), vec![]));
        let file = dir.join("config.toml");
        std::fs::write(&file, "[ui]\ntheme = \"dracula\"\nthme = 1\n").unwrap();
        let (cfg, w) = Config::load_with_warnings(&file).unwrap();
        assert_eq!(cfg.ui.theme, "dracula");
        assert_eq!(w.len(), 1, "{w:?}");
        std::fs::write(&file, "[ui\n").unwrap();
        let err = Config::load_with_warnings(&file).unwrap_err().to_string();
        assert!(err.contains("config.toml"), "{err}");
        assert!(Config::load(&file).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn typo_suggestions() {
        assert_eq!(did_you_mean("them", ["theme", "icons"]), Some("theme"));
        assert_eq!(did_you_mean("VOLUM", ["volume", "vol"]), Some("volume"));
        assert_eq!(did_you_mean("xyz", ["theme", "icons"]), None);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }
}
