//! Application state, actions, and key handling.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;

use crate::audio::{Engine, EqShared, MAX_GAIN_DB, NUM_BANDS, PRESETS};
use crate::bucket::BucketStore;
use crate::config::{self, Config};
use crate::library::{self, LibEntry, Library};
use crate::model::Track;
use crate::queue::{Queue, RepeatMode};
use crate::stats::Stats;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Library,
    Buckets,
    Queue,
}

/// Main-loop ticks per second (the loop ticks every 50ms).
const TICKS_PER_SEC: u32 = 20;

/// Sleep-timer state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sleep {
    Off,
    /// Counting down; `ticks` remaining, `mins` is the chosen preset.
    Timed { ticks: u32, mins: u32 },
    EndOfTrack,
}

impl Sleep {
    fn timed(mins: u32) -> Self {
        Sleep::Timed {
            ticks: mins * 60 * TICKS_PER_SEC,
            mins,
        }
    }

    /// Cycle: Off → 15 → 30 → 45 → 60 → End of track → Off.
    fn cycled(self) -> Self {
        match self {
            Sleep::Off => Sleep::timed(15),
            Sleep::Timed { mins, .. } => match mins {
                15 => Sleep::timed(30),
                30 => Sleep::timed(45),
                45 => Sleep::timed(60),
                _ => Sleep::EndOfTrack,
            },
            Sleep::EndOfTrack => Sleep::Off,
        }
    }

    pub fn label(self) -> String {
        match self {
            Sleep::Off => "off".to_string(),
            Sleep::EndOfTrack => "end of track".to_string(),
            Sleep::Timed { ticks, .. } => {
                let secs = ticks / TICKS_PER_SEC;
                format!("{}:{:02}", secs / 60, secs % 60)
            }
        }
    }

    pub fn is_active(self) -> bool {
        !matches!(self, Sleep::Off)
    }
}

/// The zen-mode visualizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZenViz {
    Spectrum,
    Cassette,
}

impl ZenViz {
    pub fn from_usize(v: usize) -> Self {
        match v {
            1 => ZenViz::Cassette,
            _ => ZenViz::Spectrum,
        }
    }

    pub fn as_usize(self) -> usize {
        match self {
            ZenViz::Spectrum => 0,
            ZenViz::Cassette => 1,
        }
    }

    pub fn next(self) -> Self {
        match self {
            ZenViz::Spectrum => ZenViz::Cassette,
            ZenViz::Cassette => ZenViz::Spectrum,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ZenViz::Spectrum => "spectrum",
            ZenViz::Cassette => "cassette",
        }
    }
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Focus::Library => Focus::Buckets,
            Focus::Buckets => Focus::Queue,
            Focus::Queue => Focus::Library,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Focus::Library => Focus::Queue,
            Focus::Buckets => Focus::Library,
            Focus::Queue => Focus::Buckets,
        }
    }
}

#[derive(Clone)]
pub enum InputKind {
    Search,
    NewBucket,
    /// Create a new bucket and immediately add this track.
    NewBucketForTrack(Track),
    /// Create a bucket from the current queue.
    SaveQueueAsBucket,
    /// Rename an existing user bucket (by index).
    RenameBucket(usize),
}

/// An auto-generated, read-only bucket derived from the library + play stats.
pub struct SmartBucket {
    pub name: String,
    pub icon: &'static str,
    pub color: u8,
    pub tracks: Vec<Track>,
}

/// A row in the buckets pane: either a smart bucket or a user bucket.
#[derive(Clone, Copy)]
pub enum BucketRow {
    Smart(usize),
    User(usize),
}

/// A pending destructive action awaiting confirmation.
#[derive(Clone)]
pub enum ConfirmAction {
    DeleteBucket(usize),
    ClearQueue,
    RemoveFolder(usize),
}

#[derive(Clone)]
pub struct Input {
    pub kind: InputKind,
    pub buffer: String,
}

#[derive(Clone)]
pub enum Mode {
    Normal,
    Help,
    Eq,
    Input(Input),
    PickBucket { track: Track },
    FileBrowser,
    ManageFolders,
    /// Viewing/editing the tracks inside a bucket.
    BucketView(BucketRow),
    About,
    /// A yes/no confirmation for a destructive action.
    Confirm { prompt: String, action: ConfirmAction },
    /// Live theme picker; `original` is restored on cancel.
    ThemePicker { original: usize },
    /// The unified settings hub.
    Settings,
}


/// A directory browser for picking a library folder (musikcube-style).
pub struct FileBrowser {
    pub dir: PathBuf,
    /// Sub-directories of `dir`, sorted case-insensitively.
    pub entries: Vec<PathBuf>,
    pub has_parent: bool,
    pub show_hidden: bool,
}

impl FileBrowser {
    pub fn load(dir: PathBuf, show_hidden: bool) -> Self {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| show_hidden || !is_hidden(p))
            .collect();
        entries.sort_by_key(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });
        let has_parent = dir.parent().is_some();
        Self {
            dir,
            entries,
            has_parent,
            show_hidden,
        }
    }

    /// Number of rows shown (entries plus the ".." row when applicable).
    pub fn displayed_len(&self) -> usize {
        self.entries.len() + self.has_parent as usize
    }

    pub fn is_parent_row(&self, idx: usize) -> bool {
        self.has_parent && idx == 0
    }

    /// The path a displayed row points to.
    pub fn path_at(&self, idx: usize) -> Option<PathBuf> {
        if self.has_parent {
            if idx == 0 {
                return self.dir.parent().map(|p| p.to_path_buf());
            }
            self.entries.get(idx - 1).cloned()
        } else {
            self.entries.get(idx).cloned()
        }
    }
}

fn is_hidden(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

pub struct App {
    pub config: Config,
    pub library: Library,
    pub store: BucketStore,
    pub queue: Queue,
    pub engine: Engine,
    pub stats: Stats,
    /// Auto-generated buckets, recomputed when the library or stats change.
    pub smart: Vec<SmartBucket>,

    pub focus: Focus,
    pub mode: Mode,

    pub lib_state: ListState,
    pub bucket_state: ListState,
    pub queue_state: ListState,
    pub pick_state: ListState,
    pub eq_sel: usize,

    pub now_playing: Option<Track>,
    expect_playing: bool,
    seen_progress: bool,
    /// Watchdog: detect a stalled output device (e.g. unplugged headphones).
    last_pos: Duration,
    stall_ticks: u32,

    scan_rx: Option<Receiver<Track>>,
    pub scanning: bool,
    pub scan_count: usize,

    pub status: String,
    pub status_is_error: bool,
    /// Ticks since the status was last set (for auto-clearing to a default).
    status_age: u32,
    pub spinner_frame: usize,
    pub should_quit: bool,

    /// Sleep-timer state.
    pub sleep: Sleep,

    /// Zen mode: hide all panels and show only the full-screen player.
    pub zen: bool,
    /// Which zen visualizer is active.
    pub zen_viz: ZenViz,

    /// Active directory browser (when in `Mode::FileBrowser`).
    pub browser: Option<FileBrowser>,
    pub fs_state: ListState,
    /// Selection state for the Manage Folders overlay.
    pub folders_state: ListState,
    /// Selection state for the bucket detail view.
    pub bucket_view_state: ListState,
    /// Selection state for the theme picker.
    pub theme_state: ListState,
    /// Scroll offset (rows) for the help overlay.
    pub help_scroll: u16,
    /// Selection state for the settings panel.
    pub settings_state: ListState,
    /// True when the EQ / theme picker was opened from the settings panel, so
    /// closing it returns there instead of to Normal.
    from_settings: bool,

    /// Synced lyrics for the current track, if a .lrc sidecar exists.
    pub lyrics: Option<crate::media::Lyrics>,

    /// OS media controls (media keys + Now Playing), if available.
    remote: Option<crate::remote::Remote>,
}

impl App {
    pub fn new() -> Result<Self> {
        let mut config = Config::load();
        crate::theme::set_palette(config.palette);

        let eq = EqShared::new(config.eq_enabled, config.eq_preamp, &config.eq_gains);
        let engine = Engine::new(config.volume, eq)?;

        // First-run convenience: adopt ~/Music if no roots configured.
        if config.roots.is_empty() {
            if let Some(music) = config::default_music_dir() {
                config.roots.push(music);
                config.save().ok();
            }
        }

        let mut library = Library::new(library::load_cache());
        library.set_roots(config.roots.clone());
        let store = BucketStore::load();
        let queue = Queue::new(RepeatMode::from_u8(config.repeat), config.shuffle);
        let stats = Stats::load();

        let mut lib_state = ListState::default();
        if library.entries_len() > 0 {
            lib_state.select(Some(0));
        }

        let config_zen_viz = config.zen_viz;

        let mut app = Self {
            config,
            library,
            store,
            queue,
            engine,
            stats,
            smart: Vec::new(),
            focus: Focus::Library,
            mode: Mode::Normal,
            lib_state,
            bucket_state: ListState::default(),
            queue_state: ListState::default(),
            pick_state: ListState::default(),
            eq_sel: 0,
            now_playing: None,
            expect_playing: false,
            seen_progress: false,
            last_pos: Duration::ZERO,
            stall_ticks: 0,
            scan_rx: None,
            scanning: false,
            scan_count: 0,
            status: String::new(),
            status_is_error: false,
            status_age: 0,
            spinner_frame: 0,
            should_quit: false,
            sleep: Sleep::Off,
            zen: false,
            zen_viz: ZenViz::from_usize(config_zen_viz),
            browser: None,
            fs_state: ListState::default(),
            folders_state: ListState::default(),
            bucket_view_state: ListState::default(),
            theme_state: ListState::default(),
            help_scroll: 0,
            settings_state: ListState::default(),
            from_settings: false,
            lyrics: None,
            remote: crate::remote::Remote::new(),
        };

        app.recompute_smart();
        if app.bucket_rows_len() > 0 {
            app.bucket_state.select(Some(0));
        }

        if app.config.roots.is_empty() {
            app.set_status("No music folders yet — press 'A' to add one.");
        } else {
            app.start_scan();
        }
        Ok(app)
    }

    // -- smart buckets -----------------------------------------------------

    /// Rebuild the auto-generated buckets from the library and play stats.
    pub fn recompute_smart(&mut self) {
        let tracks = &self.library.tracks;
        let mut smart = Vec::new();

        // Recently Added — by file mtime.
        let mut by_added: Vec<&Track> = tracks.iter().filter(|t| t.mtime > 0).collect();
        by_added.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        let added: Vec<Track> = by_added.into_iter().take(50).cloned().collect();
        if !added.is_empty() {
            smart.push(SmartBucket {
                name: "Recently Added".into(),
                icon: "↻",
                color: 4,
                tracks: added,
            });
        }

        // Most Played — by play count.
        let mut by_count: Vec<&Track> = tracks
            .iter()
            .filter(|t| self.stats.count(&t.path) > 0)
            .collect();
        by_count.sort_by(|a, b| {
            self.stats
                .count(&b.path)
                .cmp(&self.stats.count(&a.path))
                .then(self.stats.last_played(&b.path).cmp(&self.stats.last_played(&a.path)))
        });
        let most: Vec<Track> = by_count.into_iter().take(50).cloned().collect();
        if !most.is_empty() {
            smart.push(SmartBucket {
                name: "Most Played".into(),
                icon: "★",
                color: 3,
                tracks: most,
            });
        }

        // Recently Played — by last-played time.
        let mut by_recent: Vec<&Track> = tracks
            .iter()
            .filter(|t| self.stats.last_played(&t.path) > 0)
            .collect();
        by_recent.sort_by(|a, b| {
            self.stats
                .last_played(&b.path)
                .cmp(&self.stats.last_played(&a.path))
        });
        let recent: Vec<Track> = by_recent.into_iter().take(50).cloned().collect();
        if !recent.is_empty() {
            smart.push(SmartBucket {
                name: "Recently Played".into(),
                icon: "◷",
                color: 0,
                tracks: recent,
            });
        }

        self.smart = smart;
    }

    pub fn smart_len(&self) -> usize {
        self.smart.len()
    }

    pub fn bucket_rows_len(&self) -> usize {
        self.smart.len() + self.store.len()
    }

    /// Resolve a buckets-pane row index into a smart or user bucket.
    pub fn resolve_bucket_row(&self, idx: usize) -> Option<BucketRow> {
        if idx < self.smart.len() {
            Some(BucketRow::Smart(idx))
        } else if idx - self.smart.len() < self.store.len() {
            Some(BucketRow::User(idx - self.smart.len()))
        } else {
            None
        }
    }

    // -- status helpers ----------------------------------------------------

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = false;
        self.status_age = 0;
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = true;
        self.status_age = 0;
    }

    /// The idle footer text once a transient status has expired.
    fn default_status() -> String {
        format!("◈ Orbit v{}", env!("CARGO_PKG_VERSION"))
    }

    // -- scanning ----------------------------------------------------------

    pub fn start_scan(&mut self) {
        if self.config.roots.is_empty() {
            self.set_status("No music folders to scan — press 'A' to add one.");
            return;
        }
        self.library.tracks.clear();
        self.library.set_roots(self.config.roots.clone());
        self.library.reset_nav();
        self.scan_count = 0;
        self.scanning = true;
        self.scan_rx = Some(library::spawn_scan(self.config.roots.clone()));
        self.set_status("Scanning library…");
    }

    /// Called every loop iteration: drain scan results, advance spinner,
    /// handle track-finished transitions.
    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);

        // Drain a batch of freshly-scanned tracks.
        if self.scanning {
            let mut done = false;
            if let Some(rx) = &self.scan_rx {
                for _ in 0..512 {
                    match rx.try_recv() {
                        Ok(track) => {
                            self.library.push(track);
                            self.scan_count += 1;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            done = true;
                            break;
                        }
                    }
                }
            }
            self.library.rebuild_view();
            if self.lib_state.selected().is_none() && self.library.entries_len() > 0 {
                self.lib_state.select(Some(0));
            }
            if done {
                self.scanning = false;
                self.scan_rx = None;
                self.library.finalize();
                if self.lib_state.selected().is_none() && self.library.entries_len() > 0 {
                    self.lib_state.select(Some(0));
                }
                self.recompute_smart();
                if self.bucket_state.selected().is_none() && self.bucket_rows_len() > 0 {
                    self.bucket_state.select(Some(0));
                }
                self.set_status(format!("Library ready — {} tracks.", self.library.tracks.len()));
            }
        }

        // Track playback progress / completion.
        if self.expect_playing {
            if self.engine.position() > Duration::from_millis(150) {
                self.seen_progress = true;
            }
            if self.seen_progress && self.engine.is_finished() {
                self.on_track_finished();
            }
        }

        // Watchdog: if playback is running but the position has been frozen for
        // ~1.2s, the output device likely changed (e.g. headphones unplugged).
        // Rebuild onto the current default device and resume where we were.
        const STALL_LIMIT: u32 = 24; // ~24 * 50ms
        if self.expect_playing
            && self.seen_progress
            && !self.engine.is_paused()
            && !self.engine.is_finished()
        {
            let pos = self.engine.position();
            if pos == self.last_pos {
                self.stall_ticks += 1;
            } else {
                self.stall_ticks = 0;
                self.last_pos = pos;
            }
            if self.stall_ticks >= STALL_LIMIT {
                self.stall_ticks = 0;
                self.recover_audio_device();
            }
        } else {
            self.stall_ticks = 0;
            self.last_pos = self.engine.position();
        }

        // Let the spectrum fall when nothing is actively playing.
        if self.now_playing.is_none() || self.engine.is_paused() {
            self.engine.eq().decay_levels(0.80);
        }

        // Sleep-timer countdown (timed presets only; end-of-track is handled on finish).
        if let Sleep::Timed { ticks, mins } = self.sleep {
            let rem = ticks.saturating_sub(1);
            // Fade the output over the final 5 seconds.
            const FADE: u32 = 5 * TICKS_PER_SEC;
            if rem <= FADE {
                let f = rem as f32 / FADE as f32;
                self.engine.set_output_volume(self.engine.volume() * f);
            }
            if rem == 0 {
                self.engine.restore_volume();
                if !self.engine.is_paused() {
                    self.toggle_pause();
                }
                self.sleep = Sleep::Off;
                self.set_status("Sleep timer — paused playback. 🌙");
            } else {
                self.sleep = Sleep::Timed { ticks: rem, mins };
            }
        }

        // Auto-clear a transient status back to the idle default.
        const STATUS_TTL: u32 = 5 * TICKS_PER_SEC;
        self.status_age = self.status_age.saturating_add(1);
        if self.status_age == STATUS_TTL {
            self.status = Self::default_status();
            self.status_is_error = false;
        }

        // Apply any OS media-control commands (media keys / Now Playing).
        self.process_remote();
    }

    /// The output device stalled (likely changed). Reopen it and resume.
    fn recover_audio_device(&mut self) {
        let resume_at = self.last_pos;
        if !self.engine.rebuild_output() {
            self.set_error("Lost the audio output device.");
            self.expect_playing = false;
            return;
        }
        if let Some(track) = self.now_playing.clone() {
            if self.engine.play_path(&track.path).is_ok() {
                self.engine.seek(resume_at);
                self.seen_progress = false;
                self.last_pos = resume_at;
                self.update_remote();
                self.set_status("Audio device changed — resumed playback.");
            }
        }
    }

    fn on_track_finished(&mut self) {
        self.seen_progress = false;

        // Sleep timer set to "end of track" — stop here instead of advancing.
        if self.sleep == Sleep::EndOfTrack {
            self.sleep = Sleep::Off;
            self.expect_playing = false;
            self.now_playing = None;
            self.update_media();
            self.update_remote();
            self.engine.stop();
            self.set_status("Sleep timer — stopped at end of track. 🌙");
            return;
        }

        match self.queue.advance() {
            Some(track) => {
                let track = track.clone();
                self.play_track(track);
            }
            None => {
                self.expect_playing = false;
                self.now_playing = None;
                self.update_media();
                self.update_remote();
                self.engine.stop();
                self.set_status("Queue finished.");
            }
        }
    }

    // -- playback ----------------------------------------------------------

    /// Refresh lyrics for whatever is in `now_playing`.
    fn update_media(&mut self) {
        self.lyrics = match &self.now_playing {
            Some(t) => crate::media::Lyrics::load(&t.path),
            None => None,
        };
    }

    /// Push the current track + playback state to the OS Now Playing panel.
    fn update_remote(&mut self) {
        if self.remote.is_none() {
            return;
        }
        let info = self.now_playing.as_ref().map(|t| {
            (
                t.display_title().to_string(),
                t.artist_opt().unwrap_or("").to_string(),
                t.album_opt().map(|s| s.to_string()),
                self.engine.is_paused(),
                self.engine.position(),
                self.engine.total(),
            )
        });
        let remote = self.remote.as_mut().unwrap();
        match info {
            Some((title, artist, album, paused, pos, dur)) => {
                remote.set_now_playing(&title, &artist, album.as_deref(), paused, pos, dur)
            }
            None => remote.set_stopped(),
        }
    }

    /// Toggle play/pause and reflect it in the OS controls.
    pub fn toggle_pause(&mut self) {
        self.engine.toggle_pause();
        self.update_remote();
    }

    /// Apply queued OS media-control commands (media keys, Now Playing buttons).
    fn process_remote(&mut self) {
        let cmds = match self.remote.as_mut() {
            Some(r) => r.poll(),
            None => return,
        };
        for cmd in cmds {
            use crate::remote::RemoteCmd::*;
            match cmd {
                PlayPause => self.toggle_pause(),
                Play => {
                    if self.engine.is_paused() {
                        self.toggle_pause();
                    }
                }
                Pause => {
                    if !self.engine.is_paused() {
                        self.toggle_pause();
                    }
                }
                Next => self.next_track(),
                Prev => self.prev_track(),
                Stop => self.clear_queue(),
                SeekForward => self.engine.seek_relative(5),
                SeekBackward => self.engine.seek_relative(-5),
            }
        }
    }

    fn play_track(&mut self, track: Track) {
        match self.engine.play_path(&track.path) {
            Ok(()) => {
                self.set_status(format!("▶ {}", track.artist_title()));
                self.stats.record_play(&track.path);
                self.stats.save();
                self.recompute_smart();
                self.now_playing = Some(track);
                self.update_media();
                self.expect_playing = true;
                self.seen_progress = false;
                self.last_pos = Duration::ZERO;
                self.stall_ticks = 0;
                self.sync_queue_selection();
                self.update_remote();
            }
            Err(e) => {
                self.set_error(format!("Playback error: {e}"));
                self.expect_playing = false;
            }
        }
    }

    fn play_current_in_queue(&mut self) {
        if let Some(track) = self.queue.current().cloned() {
            self.play_track(track);
        }
    }

    pub fn next_track(&mut self) {
        if let Some(track) = self.queue.advance().cloned() {
            self.play_track(track);
        } else {
            self.set_status("End of queue.");
        }
    }

    pub fn prev_track(&mut self) {
        // Restart current track if we're more than 3s in.
        if self.engine.position() > Duration::from_secs(3) {
            self.engine.seek(Duration::ZERO);
            return;
        }
        if let Some(track) = self.queue.previous().cloned() {
            self.play_track(track);
        }
    }

    fn sync_queue_selection(&mut self) {
        if let Some(idx) = self.queue.current_index() {
            self.queue_state.select(Some(idx));
        }
    }

    // -- list navigation ---------------------------------------------------

    fn focused_len(&self) -> usize {
        match self.focus {
            Focus::Library => self.library.entries_len(),
            Focus::Buckets => self.bucket_rows_len(),
            Focus::Queue => self.queue.len(),
        }
    }

    fn focused_state(&mut self) -> &mut ListState {
        match self.focus {
            Focus::Library => &mut self.lib_state,
            Focus::Buckets => &mut self.bucket_state,
            Focus::Queue => &mut self.queue_state,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.focused_len();
        if len == 0 {
            return;
        }
        let state = self.focused_state();
        let cur = state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, len as i32 - 1);
        state.select(Some(next as usize));
    }

    fn move_to_edge(&mut self, start: bool) {
        let len = self.focused_len();
        if len == 0 {
            return;
        }
        let target = if start { 0 } else { len - 1 };
        self.focused_state().select(Some(target));
    }

    // -- selections --------------------------------------------------------

    fn selected_library_track(&self) -> Option<Track> {
        let row = self.lib_state.selected()?;
        match self.library.entry_at(row)? {
            LibEntry::Track(i) => self.library.track(*i).cloned(),
            _ => None,
        }
    }

    fn selected_bucket_row(&self) -> Option<BucketRow> {
        let idx = self.bucket_state.selected()?;
        self.resolve_bucket_row(idx)
    }

    fn selected_queue_index(&self) -> Option<usize> {
        let idx = self.queue_state.selected()?;
        if idx < self.queue.len() {
            Some(idx)
        } else {
            None
        }
    }

    // -- enter / context actions ------------------------------------------

    fn activate(&mut self) {
        match self.focus {
            Focus::Library => self.activate_library(),
            Focus::Buckets => self.dump_selected_bucket(),
            Focus::Queue => {
                if let Some(idx) = self.selected_queue_index() {
                    self.queue.jump_to(idx);
                    self.play_current_in_queue();
                }
            }
        }
    }

    /// Enter on a library row: descend folders, go up via "..", or play a track.
    fn activate_library(&mut self) {
        let Some(row) = self.lib_state.selected() else {
            return;
        };
        enum Act {
            Up,
            Enter(PathBuf),
            Play(Track),
            None,
        }
        let act = match self.library.entry_at(row) {
            Some(LibEntry::Parent) => Act::Up,
            Some(LibEntry::Folder { path, .. }) => Act::Enter(path.clone()),
            Some(LibEntry::Track(i)) => self
                .library
                .track(*i)
                .cloned()
                .map(Act::Play)
                .unwrap_or(Act::None),
            None => Act::None,
        };
        match act {
            Act::Up => {
                self.library.go_up();
                self.lib_state.select(Some(0));
            }
            Act::Enter(path) => {
                self.library.enter(path);
                self.lib_state
                    .select(if self.library.entries_len() > 0 { Some(0) } else { None });
            }
            Act::Play(track) => self.enqueue_and_play(track),
            Act::None => {}
        }
    }

    fn dump_selected_bucket(&mut self) {
        let Some(row) = self.selected_bucket_row() else {
            return;
        };
        let was_empty = self.queue.is_empty();
        let (name, tracks) = match row {
            BucketRow::Smart(i) => {
                let b = &self.smart[i];
                (b.name.clone(), b.tracks.clone())
            }
            BucketRow::User(i) => {
                let b = &self.store.buckets[i];
                (b.name.clone(), b.tracks.clone())
            }
        };
        if tracks.is_empty() {
            self.set_status(format!("Bucket “{name}” is empty."));
            return;
        }
        let count = tracks.len();
        self.queue.extend(tracks);
        if was_empty {
            // Start playing the first track of the freshly-filled queue.
            self.queue.jump_to(0);
            self.play_current_in_queue();
        }
        self.set_status(format!("Dumped {count} tracks from “{name}” into the queue."));
    }

    /// Dump the current library scope (folder/filter, or everything) into the queue.
    fn dump_library(&mut self) {
        let tracks = self.library.scoped_tracks();
        if tracks.is_empty() {
            self.set_status("Nothing here to dump.");
            return;
        }
        let was_empty = self.queue.is_empty();
        let count = tracks.len();
        self.queue.extend(tracks);
        if was_empty {
            self.queue.jump_to(0);
            self.play_current_in_queue();
        }
        let scope = if !self.library.filter().is_empty() {
            "the filtered results".to_string()
        } else if let Some(name) = self.library.cwd_label() {
            format!("“{name}”")
        } else {
            "the library".to_string()
        };
        self.set_status(format!("Dumped {count} tracks from {scope} into the queue."));
    }

    /// Append one track to the queue and start playing it immediately.
    fn enqueue_and_play(&mut self, track: Track) {
        self.queue.extend([track.clone()]);
        let idx = self.queue.len() - 1;
        self.queue.jump_to(idx);
        self.play_track(track);
    }

    // -- bucket detail view ------------------------------------------------

    /// Tracks for a bucket row (smart or user).
    fn row_tracks(&self, row: BucketRow) -> &[Track] {
        match row {
            BucketRow::Smart(i) => self.smart.get(i).map(|b| b.tracks.as_slice()).unwrap_or(&[]),
            BucketRow::User(i) => self
                .store
                .buckets
                .get(i)
                .map(|b| b.tracks.as_slice())
                .unwrap_or(&[]),
        }
    }

    fn open_bucket_view(&mut self) {
        let Some(row) = self.selected_bucket_row() else {
            return;
        };
        let len = self.row_tracks(row).len();
        self.bucket_view_state
            .select(if len > 0 { Some(0) } else { None });
        self.mode = Mode::BucketView(row);
    }

    fn handle_bucket_view_key(&mut self, key: KeyEvent) {
        let row = match &self.mode {
            Mode::BucketView(r) => *r,
            _ => return,
        };
        let len = self.row_tracks(row).len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                if len > 0 {
                    let c = self.bucket_view_state.selected().unwrap_or(0);
                    self.bucket_view_state.select(Some(c.saturating_sub(1)));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 {
                    let c = self.bucket_view_state.selected().unwrap_or(0);
                    self.bucket_view_state.select(Some((c + 1).min(len - 1)));
                }
            }
            KeyCode::Enter => {
                if let Some(i) = self.bucket_view_state.selected() {
                    if let Some(t) = self.row_tracks(row).get(i).cloned() {
                        self.enqueue_and_play(t);
                    }
                }
            }
            // Editing — user buckets only.
            _ => {
                let BucketRow::User(bidx) = row else { return };
                match key.code {
                    KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete => {
                        if let Some(i) = self.bucket_view_state.selected() {
                            self.store.remove_track(bidx, i);
                            self.store.save();
                            let nlen = self.store.buckets[bidx].tracks.len();
                            self.bucket_view_state
                                .select(if nlen == 0 { None } else { Some(i.min(nlen - 1)) });
                        }
                    }
                    KeyCode::Char('K') => {
                        if let Some(i) = self.bucket_view_state.selected() {
                            let ni = self.store.move_track(bidx, i, -1);
                            self.store.save();
                            self.bucket_view_state.select(Some(ni));
                        }
                    }
                    KeyCode::Char('J') => {
                        if let Some(i) = self.bucket_view_state.selected() {
                            let ni = self.store.move_track(bidx, i, 1);
                            self.store.save();
                            self.bucket_view_state.select(Some(ni));
                        }
                    }
                    KeyCode::Char('r') => {
                        self.mode = Mode::Input(Input {
                            kind: InputKind::RenameBucket(bidx),
                            buffer: self.store.buckets[bidx].name.clone(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    // -- buckets -----------------------------------------------------------

    fn add_selected_to_bucket(&mut self) {
        let Some(track) = self.selected_library_track() else {
            self.set_status("No track selected.");
            return;
        };
        if self.store.is_empty() {
            // No buckets yet — go straight to creating one for this track.
            self.mode = Mode::Input(Input {
                kind: InputKind::NewBucketForTrack(track),
                buffer: String::new(),
            });
            return;
        }
        self.pick_state.select(Some(0));
        self.mode = Mode::PickBucket { track };
    }

    fn commit_pick_bucket(&mut self, track: Track) {
        if let Some(idx) = self.pick_state.selected() {
            if idx < self.store.len() {
                let added = self.store.add_track(idx, track.clone());
                let name = self.store.buckets[idx].name.clone();
                self.store.save();
                if added {
                    self.set_status(format!("Added to “{name}”."));
                } else {
                    self.set_status(format!("Already in “{name}”."));
                }
            }
        }
        self.mode = Mode::Normal;
    }

    /// Ask before deleting the selected user bucket.
    fn delete_selected_bucket(&mut self) {
        match self.selected_bucket_row() {
            Some(BucketRow::User(i)) => {
                let name = self.store.buckets[i].name.clone();
                self.confirm(
                    format!("Delete bucket “{name}”?"),
                    ConfirmAction::DeleteBucket(i),
                );
            }
            Some(BucketRow::Smart(_)) => {
                self.set_status("Smart buckets are automatic — they can't be deleted.");
            }
            None => {}
        }
    }

    fn do_delete_bucket(&mut self, i: usize) {
        if i >= self.store.len() {
            return;
        }
        let name = self.store.buckets[i].name.clone();
        self.store.delete(i);
        self.store.save();
        let rows = self.bucket_rows_len();
        if rows == 0 {
            self.bucket_state.select(None);
        } else {
            let cur = self.bucket_state.selected().unwrap_or(0);
            self.bucket_state.select(Some(cur.min(rows - 1)));
        }
        self.set_status(format!("Deleted bucket “{name}”."));
    }

    // -- queue edits -------------------------------------------------------

    fn remove_from_queue(&mut self) {
        if let Some(idx) = self.selected_queue_index() {
            let current = self.queue.current_index();
            self.queue.remove(idx);
            // If we removed the playing track, advance playback.
            if Some(idx) == current {
                if self.queue.is_empty() {
                    self.engine.stop();
                    self.now_playing = None;
                    self.update_media();
                    self.update_remote();
                    self.expect_playing = false;
                } else {
                    self.play_current_in_queue();
                }
            }
            let len = self.queue.len();
            if len == 0 {
                self.queue_state.select(None);
            } else {
                self.queue_state.select(Some(idx.min(len - 1)));
            }
        }
    }

    fn request_clear_queue(&mut self) {
        if self.queue.is_empty() {
            self.set_status("Queue is already empty.");
        } else {
            self.confirm("Clear the entire queue?".to_string(), ConfirmAction::ClearQueue);
        }
    }

    fn clear_queue(&mut self) {
        self.queue.clear();
        self.queue_state.select(None);
        self.engine.stop();
        self.now_playing = None;
        self.update_media();
        self.update_remote();
        self.expect_playing = false;
        self.set_status("Queue cleared.");
    }

    // -- theme picker ------------------------------------------------------

    fn open_theme_picker(&mut self) {
        let original = crate::theme::active_index();
        self.theme_state.select(Some(original));
        self.mode = Mode::ThemePicker { original };
    }

    fn handle_theme_picker_key(&mut self, key: KeyEvent) {
        let original = match &self.mode {
            Mode::ThemePicker { original } => *original,
            _ => return,
        };
        let count = crate::theme::palette_count();
        let cur = self.theme_state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                let next = if cur == 0 { count - 1 } else { cur - 1 };
                self.theme_state.select(Some(next));
                crate::theme::set_palette(next); // live preview
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let next = (cur + 1) % count;
                self.theme_state.select(Some(next));
                crate::theme::set_palette(next); // live preview
            }
            KeyCode::Enter => {
                crate::theme::set_palette(cur);
                self.config.palette = cur;
                self.config.save().ok();
                self.set_status(format!("Theme: {}", crate::theme::palette_name()));
                self.close_overlay();
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                crate::theme::set_palette(original); // revert
                self.close_overlay();
            }
            _ => {}
        }
    }

    // -- confirmation ------------------------------------------------------

    fn confirm(&mut self, prompt: String, action: ConfirmAction) {
        self.mode = Mode::Confirm { prompt, action };
    }

    /// Where to return after a confirm dialog resolves.
    fn confirm_return_mode(action: &ConfirmAction) -> Mode {
        match action {
            ConfirmAction::RemoveFolder(_) => Mode::ManageFolders,
            _ => Mode::Normal,
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        let action = match &self.mode {
            Mode::Confirm { action, .. } => action.clone(),
            _ => return,
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.mode = Self::confirm_return_mode(&action);
                match action {
                    ConfirmAction::DeleteBucket(i) => self.do_delete_bucket(i),
                    ConfirmAction::ClearQueue => self.clear_queue(),
                    ConfirmAction::RemoveFolder(i) => self.do_remove_root(i),
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Self::confirm_return_mode(&action);
                self.set_status("Cancelled.");
            }
            _ => {}
        }
    }

    // -- modes / playback toggles -----------------------------------------

    fn toggle_shuffle(&mut self) {
        let new = !self.queue.shuffle;
        self.queue.set_shuffle(new);
        self.config.shuffle = new;
        self.config.save().ok();
        self.set_status(format!("Shuffle {}.", if new { "on" } else { "off" }));
    }

    fn cycle_repeat(&mut self) {
        let new = self.queue.repeat.cycle();
        self.queue.repeat = new;
        self.config.repeat = new.as_u8();
        self.config.save().ok();
        self.set_status(format!("Repeat: {}.", new.label()));
    }

    fn change_volume(&mut self, delta: f32) {
        self.engine.nudge_volume(delta);
        self.config.volume = self.engine.volume();
        self.config.save().ok();
    }

    // -- EQ ----------------------------------------------------------------

    fn save_eq(&mut self) {
        let eq = self.engine.eq();
        self.config.eq_enabled = eq.enabled();
        self.config.eq_preamp = eq.preamp_db();
        self.config.eq_gains = eq.all_gains_db().to_vec();
        self.config.save().ok();
    }

    fn eq_adjust(&mut self, delta_db: f32) {
        // Tweaking the EQ implies you want it on.
        if !self.engine.eq().enabled() {
            self.engine.eq().set_enabled(true);
        }
        // eq_sel == NUM_BANDS means the preamp slider.
        if self.eq_sel >= NUM_BANDS {
            let eq = self.engine.eq();
            eq.set_preamp_db(eq.preamp_db() + delta_db);
        } else {
            self.engine.eq().nudge_gain(self.eq_sel, delta_db);
        }
        self.save_eq();
    }

    fn eq_reset(&mut self) {
        let eq = self.engine.eq();
        eq.apply_preset(&PRESETS[0]);
        eq.set_preamp_db(0.0);
        self.save_eq();
        self.set_status("EQ reset to flat.");
    }

    fn eq_apply_preset(&mut self, idx: usize) {
        if let Some(p) = PRESETS.get(idx) {
            self.engine.eq().apply_preset(p);
            self.engine.eq().set_enabled(true);
            self.save_eq();
            self.set_status(format!("EQ on · preset: {}.", p.name));
        }
    }

    fn toggle_eq(&mut self) {
        self.engine.eq().toggle_enabled();
        self.save_eq();
        let on = self.engine.eq().enabled();
        self.set_status(format!("EQ {}.", if on { "enabled" } else { "bypassed" }));
    }

    // -- folders / rescan --------------------------------------------------

    /// Add a directory to the library roots and kick off a rescan.
    fn add_root(&mut self, path: PathBuf) {
        if !path.is_dir() {
            self.set_error(format!("Not a directory: {}", path.display()));
            return;
        }
        if self.config.roots.contains(&path) {
            self.set_status("Folder already in library.");
            return;
        }
        self.config.roots.push(path.clone());
        self.config.save().ok();
        self.set_status(format!("Added {} — rescanning…", path.display()));
        self.start_scan();
    }

    // -- file browser ------------------------------------------------------

    fn open_file_browser(&mut self) {
        // Start from the most recently added root, else the home directory.
        let home = config::home_dir();
        let start = self.config.roots.last().cloned().unwrap_or_else(|| home.clone());
        let start = if start.is_dir() { start } else { home };
        self.browser = Some(FileBrowser::load(start, false));
        self.fs_state.select(Some(0));
        self.mode = Mode::FileBrowser;
        self.set_status("Browse to a folder, then press 'a' to add it.");
    }

    fn browser_navigate_to(&mut self, dir: PathBuf, select_child: Option<PathBuf>) {
        let show_hidden = self.browser.as_ref().map(|b| b.show_hidden).unwrap_or(false);
        let browser = FileBrowser::load(dir, show_hidden);
        // Try to re-select the directory we came from (when going up).
        let sel = match &select_child {
            Some(child) => browser
                .entries
                .iter()
                .position(|p| p == child)
                .map(|i| i + browser.has_parent as usize)
                .unwrap_or(0),
            None => 0,
        };
        self.fs_state
            .select(Some(if browser.displayed_len() == 0 { 0 } else { sel }));
        self.browser = Some(browser);
    }

    fn browser_move(&mut self, delta: i32) {
        let Some(b) = &self.browser else { return };
        let len = b.displayed_len();
        if len == 0 {
            return;
        }
        let cur = self.fs_state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, len as i32 - 1);
        self.fs_state.select(Some(next as usize));
    }

    fn browser_open_selected(&mut self) {
        let Some(b) = &self.browser else { return };
        let idx = self.fs_state.selected().unwrap_or(0);
        if let Some(path) = b.path_at(idx) {
            if path.is_dir() {
                self.browser_navigate_to(path, None);
            }
        }
    }

    fn browser_up(&mut self) {
        let Some(b) = &self.browser else { return };
        if let Some(parent) = b.dir.parent().map(|p| p.to_path_buf()) {
            let came_from = b.dir.clone();
            self.browser_navigate_to(parent, Some(came_from));
        }
    }

    fn browser_toggle_hidden(&mut self) {
        if let Some(b) = &self.browser {
            let dir = b.dir.clone();
            let show = !b.show_hidden;
            self.browser = Some(FileBrowser::load(dir, show));
            self.fs_state.select(Some(0));
        }
    }

    fn browser_add(&mut self) {
        let Some(b) = &self.browser else { return };
        let idx = self.fs_state.selected().unwrap_or(0);
        // ".." adds the current directory; otherwise add the highlighted folder.
        let path = if b.is_parent_row(idx) {
            b.dir.clone()
        } else {
            match b.path_at(idx) {
                Some(p) => p,
                None => b.dir.clone(),
            }
        };
        self.browser = None;
        self.add_root(path.clone());
        // Return to the folder manager with the (new) root selected.
        self.mode = Mode::ManageFolders;
        if !self.config.roots.is_empty() {
            let sel = self
                .config
                .roots
                .iter()
                .position(|r| *r == path)
                .unwrap_or(self.config.roots.len() - 1);
            self.folders_state.select(Some(sel));
        }
    }

    // -- manage folders ----------------------------------------------------

    fn open_manage_folders(&mut self) {
        if self.config.roots.is_empty() {
            self.folders_state.select(None);
        } else {
            let sel = self
                .folders_state
                .selected()
                .unwrap_or(0)
                .min(self.config.roots.len() - 1);
            self.folders_state.select(Some(sel));
        }
        self.mode = Mode::ManageFolders;
        self.set_status("Manage folders — a add · x remove · r rescan · Esc close.");
    }

    fn request_remove_root(&mut self) {
        let Some(idx) = self.folders_state.selected() else {
            return;
        };
        if idx >= self.config.roots.len() {
            return;
        }
        let path = self.config.roots[idx].display().to_string();
        self.confirm(
            format!("Remove “{path}” from the library?"),
            ConfirmAction::RemoveFolder(idx),
        );
    }

    fn do_remove_root(&mut self, idx: usize) {
        if idx >= self.config.roots.len() {
            return;
        }
        let removed = self.config.roots.remove(idx);
        self.config.save().ok();
        let len = self.config.roots.len();
        if len == 0 {
            self.folders_state.select(None);
        } else {
            self.folders_state.select(Some(idx.min(len - 1)));
        }
        if self.config.roots.is_empty() {
            // Nothing left to scan — clear the library.
            self.scan_rx = None;
            self.scanning = false;
            self.library.tracks.clear();
            self.library.set_roots(self.config.roots.clone());
            self.library.finalize();
            self.lib_state.select(None);
            self.recompute_smart();
            self.set_status(format!("Removed {} — library cleared.", removed.display()));
        } else {
            self.set_status(format!("Removed {} — rescanning…", removed.display()));
            self.start_scan();
        }
    }

    fn handle_manage_folders_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                let len = self.config.roots.len();
                if len > 0 {
                    let cur = self.folders_state.selected().unwrap_or(0);
                    self.folders_state.select(Some(cur.saturating_sub(1)));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.config.roots.len();
                if len > 0 {
                    let cur = self.folders_state.selected().unwrap_or(0);
                    self.folders_state.select(Some((cur + 1).min(len - 1)));
                }
            }
            KeyCode::Char('a') => self.open_file_browser(),
            KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete => {
                self.request_remove_root()
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.start_scan();
            }
            _ => {}
        }
    }

    // -- key handling ------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Global quit on Ctrl-C regardless of mode.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit();
            return;
        }

        // Global playback controls work from any screen — except while typing in
        // an Input, or the About card (which closes on any key).
        if !matches!(self.mode, Mode::Input(_) | Mode::About) {
            match key.code {
                KeyCode::Char(' ') => return self.toggle_pause(),
                KeyCode::Char('E') => return self.toggle_eq(),
                KeyCode::Char('+') | KeyCode::Char('=') => return self.change_volume(0.05),
                KeyCode::Char('-') | KeyCode::Char('_') => return self.change_volume(-0.05),
                _ => {}
            }
        }
        // next/prev are global too, except where the `n` key is meaningful.
        if !matches!(
            self.mode,
            Mode::Input(_) | Mode::About | Mode::PickBucket { .. } | Mode::Confirm { .. }
        ) {
            match key.code {
                KeyCode::Char('n') => return self.next_track(),
                KeyCode::Char('p') => return self.prev_track(),
                _ => {}
            }
        }

        match &self.mode {
            Mode::Input(_) => self.handle_input_key(key),
            Mode::PickBucket { .. } => self.handle_pick_key(key),
            Mode::Help => self.handle_help_key(key),
            Mode::Eq => self.handle_eq_key(key),
            Mode::FileBrowser => self.handle_browser_key(key),
            Mode::ManageFolders => self.handle_manage_folders_key(key),
            Mode::BucketView(_) => self.handle_bucket_view_key(key),
            Mode::About => self.mode = Mode::Normal,
            Mode::Confirm { .. } => self.handle_confirm_key(key),
            Mode::ThemePicker { .. } => self.handle_theme_picker_key(key),
            Mode::Settings => self.handle_settings_key(key),
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        let Mode::Input(input) = &mut self.mode else {
            return;
        };
        match key.code {
            
            KeyCode::Esc => {
                // Cancel; if it was a live search, clear the filter.
                if matches!(input.kind, InputKind::Search) {
                    self.library.set_filter(String::new());
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let kind = input.kind.clone();
                let buffer = input.buffer.trim().to_string();
                self.mode = Mode::Normal;
                self.commit_input(kind, buffer);
            }
            KeyCode::Backspace => {
                input.buffer.pop();
                if matches!(input.kind, InputKind::Search) {
                    let f = input.buffer.clone();
                    self.library.set_filter(f);
                    self.lib_state.select(if self.library.entries_len() > 0 {
                        Some(0)
                    } else {
                        None
                    });
                }
            }
            KeyCode::Char(c) => {
                input.buffer.push(c);
                if matches!(input.kind, InputKind::Search) {
                    let f = input.buffer.clone();
                    self.library.set_filter(f);
                    self.lib_state.select(if self.library.entries_len() > 0 {
                        Some(0)
                    } else {
                        None
                    });
                }
            }
            _ => {}
        }
    }

    fn commit_input(&mut self, kind: InputKind, buffer: String) {
        match kind {
            InputKind::Search => {
                // Filter already applied live; just keep it.
                if self.library.filter().is_empty() {
                    self.set_status("Search cleared.");
                }
            }
            InputKind::NewBucket => {
                if buffer.is_empty() {
                    self.set_status("Bucket name cannot be empty.");
                    return;
                }
                let idx = self.store.create(buffer.clone());
                self.store.save();
                self.bucket_state.select(Some(self.smart_len() + idx));
                self.set_status(format!("Created bucket “{buffer}”."));
            }
            InputKind::NewBucketForTrack(track) => {
                if buffer.is_empty() {
                    self.set_status("Bucket name cannot be empty.");
                    return;
                }
                let idx = self.store.create(buffer.clone());
                self.store.add_track(idx, track);
                self.store.save();
                self.bucket_state.select(Some(self.smart_len() + idx));
                self.set_status(format!("Created “{buffer}” and added the track."));
            }
            InputKind::SaveQueueAsBucket => {
                if buffer.is_empty() {
                    self.set_status("Bucket name cannot be empty.");
                    return;
                }
                if self.queue.is_empty() {
                    self.set_status("Queue is empty — nothing to save.");
                    return;
                }
                let tracks = self.queue.items.clone();
                let count = tracks.len();
                let idx = self.store.create_with(buffer.clone(), tracks);
                self.store.save();
                self.bucket_state.select(Some(self.smart_len() + idx));
                self.set_status(format!("Saved {count} tracks to “{buffer}”."));
            }
            InputKind::RenameBucket(idx) => {
                if buffer.is_empty() {
                    self.set_status("Bucket name cannot be empty.");
                    return;
                }
                self.store.rename(idx, buffer.clone());
                self.store.save();
                self.set_status(format!("Renamed to “{buffer}”."));
            }
        }
    }

    fn handle_browser_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.browser = None;
                self.mode = Mode::ManageFolders;
            }
            KeyCode::Up | KeyCode::Char('k') => self.browser_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.browser_move(1),
            KeyCode::PageUp => self.browser_move(-10),
            KeyCode::PageDown => self.browser_move(10),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.browser_open_selected(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.browser_up(),
            KeyCode::Char('a') => self.browser_add(),
            KeyCode::Char('.') => self.browser_toggle_hidden(),
            _ => {}
        }
    }

    fn handle_pick_key(&mut self, key: KeyEvent) {
        let track = match &self.mode {
            Mode::PickBucket { track } => track.clone(),
            _ => return,
        };
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                let len = self.store.len();
                if len > 0 {
                    let cur = self.pick_state.selected().unwrap_or(0);
                    self.pick_state.select(Some(cur.saturating_sub(1)));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.store.len();
                if len > 0 {
                    let cur = self.pick_state.selected().unwrap_or(0);
                    self.pick_state.select(Some((cur + 1).min(len - 1)));
                }
            }
            KeyCode::Char('n') => {
                self.mode = Mode::Input(Input {
                    kind: InputKind::NewBucketForTrack(track),
                    buffer: String::new(),
                });
            }
            KeyCode::Enter => self.commit_pick_bucket(track),
            _ => {}
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => self.mode = Mode::Normal,
            KeyCode::Down | KeyCode::Char('j') => self.help_scroll = self.help_scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.help_scroll = self.help_scroll.saturating_sub(1),
            KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(8),
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(8),
            KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
            _ => {}
        }
    }

    fn open_help(&mut self) {
        self.help_scroll = 0;
        self.mode = Mode::Help;
    }

    // -- settings ----------------------------------------------------------

    /// Number of rows in the settings panel.
    pub const SETTINGS_ROWS: usize = 5;

    fn cycle_sleep(&mut self) {
        self.engine.restore_volume(); // undo any in-progress fade
        self.sleep = self.sleep.cycled();
        self.set_status(match self.sleep {
            Sleep::Off => "Sleep timer off.".to_string(),
            Sleep::EndOfTrack => "Sleep timer: stop at end of track.".to_string(),
            Sleep::Timed { mins, .. } => format!("Sleep timer: {mins} min."),
        });
    }

    fn open_settings(&mut self) {
        self.settings_state.select(Some(0));
        self.mode = Mode::Settings;
    }

    fn handle_settings_key(&mut self, key: KeyEvent) {
        let cur = self.settings_state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(',') => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_state.select(Some(cur.saturating_sub(1)));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_state
                    .select(Some((cur + 1).min(Self::SETTINGS_ROWS - 1)));
            }
            KeyCode::Enter | KeyCode::Left | KeyCode::Right => {
                self.activate_setting(cur);
            }
            _ => {}
        }
    }

    /// Close a settings sub-overlay (EQ / theme), returning to Settings if it
    /// was opened from there, otherwise to Normal.
    fn close_overlay(&mut self) {
        self.mode = if self.from_settings {
            Mode::Settings
        } else {
            Mode::Normal
        };
        self.from_settings = false;
    }

    fn activate_setting(&mut self, idx: usize) {
        match idx {
            0 => {
                // Equalizer.
                self.from_settings = true;
                self.mode = Mode::Eq;
            }
            1 => {
                self.from_settings = true;
                self.open_theme_picker();
            }
            2 => {
                self.zen_viz = self.zen_viz.next();
                self.config.zen_viz = self.zen_viz.as_usize();
                self.config.save().ok();
            }
            3 => {
                self.config.footer_hints = !self.config.footer_hints;
                self.config.save().ok();
            }
            4 => self.cycle_sleep(),
            _ => {}
        }
    }

    fn handle_eq_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('e') | KeyCode::Char('q') => self.close_overlay(),
            KeyCode::Left | KeyCode::Char('h') => {
                if self.eq_sel == 0 {
                    self.eq_sel = NUM_BANDS; // wrap to preamp
                } else {
                    self.eq_sel -= 1;
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.eq_sel = if self.eq_sel >= NUM_BANDS {
                    0
                } else {
                    self.eq_sel + 1
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.eq_adjust(1.0),
            KeyCode::Down | KeyCode::Char('j') => self.eq_adjust(-1.0),
            KeyCode::Char('x') | KeyCode::Char('E') => {
                self.engine.eq().toggle_enabled();
                self.save_eq();
                let on = self.engine.eq().enabled();
                self.set_status(format!("EQ {}.", if on { "enabled" } else { "bypassed" }));
            }
            KeyCode::Char('f') => self.eq_reset(),
            KeyCode::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                self.eq_apply_preset(idx);
            }
            KeyCode::Char(' ') => self.toggle_pause(),
            _ => {}
        }
    }

    /// Zen mode accepts only the keys that make sense with the panels hidden.
    fn handle_zen_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('z') | KeyCode::Esc => self.zen = false,
            KeyCode::Char('q') => self.quit(),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('n') => self.next_track(),
            KeyCode::Char('p') => self.prev_track(),
            KeyCode::Left | KeyCode::Char('h') => self.engine.seek_relative(-5),
            KeyCode::Right | KeyCode::Char('l') => self.engine.seek_relative(5),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(0.05),
            KeyCode::Char('-') | KeyCode::Char('_') => self.change_volume(-0.05),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('r') => self.cycle_repeat(),
            KeyCode::Char('e') => {
                self.from_settings = false;
                self.mode = Mode::Eq;
            }
            KeyCode::Char('E') => self.toggle_eq(),
            KeyCode::Char('v') => {
                self.zen_viz = self.zen_viz.next();
                self.config.zen_viz = self.zen_viz.as_usize();
                self.config.save().ok();
                self.set_status(format!("Zen visualizer: {}", self.zen_viz.label()));
            }
            KeyCode::Char(',') => self.open_settings(),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('i') => self.mode = Mode::About,
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        // In zen mode only playback-relevant keys apply (panels are hidden).
        if self.zen {
            return self.handle_zen_key(key);
        }
        match key.code {
            KeyCode::Char('q') => self.quit(),
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.prev(),

            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Home | KeyCode::Char('g') => self.move_to_edge(true),
            KeyCode::End | KeyCode::Char('G') => self.move_to_edge(false),

            KeyCode::Enter => self.activate(),
            KeyCode::Backspace => {
                if self.focus == Focus::Library {
                    self.library.go_up();
                    self.lib_state
                        .select((self.library.entries_len() > 0).then_some(0));
                }
            }
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Char('n') => self.next_track(),
            KeyCode::Char('p') => self.prev_track(),

            KeyCode::Left | KeyCode::Char('h') => self.engine.seek_relative(-5),
            KeyCode::Right | KeyCode::Char('l') => self.engine.seek_relative(5),

            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(0.05),
            KeyCode::Char('-') | KeyCode::Char('_') => self.change_volume(-0.05),

            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('r') => self.cycle_repeat(),

            KeyCode::Char('e') => {
                self.from_settings = false;
                self.mode = Mode::Eq;
            }
            KeyCode::Char('E') => self.toggle_eq(),
            KeyCode::Char('z') => {
                self.zen = !self.zen;
                self.set_status(if self.zen {
                    "Zen mode — press z or Esc to exit."
                } else {
                    "Zen mode off."
                });
            }
            KeyCode::Char(',') => self.open_settings(),
            KeyCode::Char('v') => {
                self.zen_viz = self.zen_viz.next();
                self.config.zen_viz = self.zen_viz.as_usize();
                self.config.save().ok();
                self.set_status(format!("Zen visualizer: {}", self.zen_viz.label()));
            }
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('i') => self.mode = Mode::About,

            KeyCode::Char('b') => {
                self.mode = Mode::Input(Input {
                    kind: InputKind::NewBucket,
                    buffer: String::new(),
                });
            }
            KeyCode::Char('S') => {
                if self.queue.is_empty() {
                    self.set_status("Queue is empty — nothing to save.");
                } else {
                    self.mode = Mode::Input(Input {
                        kind: InputKind::SaveQueueAsBucket,
                        buffer: String::new(),
                    });
                }
            }
            KeyCode::Char('a') => self.add_selected_to_bucket(),
            KeyCode::Char('o') => {
                if self.focus == Focus::Buckets {
                    self.open_bucket_view();
                }
            }
            KeyCode::Char('A') => self.open_manage_folders(),
            KeyCode::Char('/') => {
                self.mode = Mode::Input(Input {
                    kind: InputKind::Search,
                    buffer: self.library.filter().to_string(),
                });
                self.focus = Focus::Library;
            }
            KeyCode::Char('x') | KeyCode::Delete => match self.focus {
                Focus::Buckets => self.delete_selected_bucket(),
                Focus::Queue => self.remove_from_queue(),
                Focus::Library => {}
            },
            KeyCode::Char('c') => self.request_clear_queue(),
            KeyCode::Char('d') => match self.focus {
                Focus::Buckets => self.dump_selected_bucket(),
                Focus::Library => self.dump_library(),
                Focus::Queue => {}
            },
            KeyCode::Char('R') => self.start_scan(),
            KeyCode::Esc => {
                if self.zen {
                    self.zen = false;
                } else if !self.library.filter().is_empty() {
                    self.library.set_filter(String::new());
                    self.set_status("Search cleared.");
                }
            }
            _ => {}
        }
    }

    pub fn quit(&mut self) {
        self.config.volume = self.engine.volume();
        self.save_eq();
        self.config.save().ok();
        self.should_quit = true;
    }

    // -- accessors for the UI ---------------------------------------------

    pub fn eq(&self) -> &EqShared {
        self.engine.eq()
    }

    pub fn eq_max_db(&self) -> f32 {
        MAX_GAIN_DB
    }
}
