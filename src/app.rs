//! Application state and behavior: input -> actions/commands -> library/queue/engine updates.
//!
//! Contract with `ui/`: `ui::draw(frame, &mut app)` only READS the fields below (plus the helper
//! methods at the bottom), except that it writes `app.hit` (clickable regions of this frame),
//! `app.vis_bands` (how many visualizer bars fit), and scroll offsets inside ListState/TableState.
//!
//! Lyrics panel: when `app.lyrics_following()` is true the UI keeps the current synced line in
//! view; otherwise it shows the lyrics scrolled to `app.lyrics_scroll`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::widgets::{ListState, TableState};
use serde::{Deserialize, Serialize};

use crate::art::ArtCache;
use crate::command::{self, Command, EqArg, Level, LoopArg, SeekTarget, SleepArg};
use crate::config::{self, Config, EqSettings, Paths, PlayContext, ReplayGainMode, TimeDisplay};
use crate::download::{self, DownloadMsg};
use crate::dsp::{EQ_FREQS, EQ_MAX_DB, EQ_PRESETS};
use crate::ipc::IpcServer;
use crate::keymap::{Action, Binding, Keymap};
use crate::library::{self, BrowseMode, Group, Library, ScanMsg, SortKey, Track, TrackId, fmt_duration};
use crate::lyrics::{self, Lyrics};
use crate::media::Media;
use crate::notify;
use crate::player::{Engine, EngineEvent, PlayState};
use crate::playlist::{self, Playlists, Smart};
use crate::queue::Queue;
use crate::radio::{self, Features};
use crate::search::Searcher;
use crate::state::State;
use crate::theme::{self, Icons, Theme};
use crate::visualizer::{Analyzer, VisMode};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Tab {
    #[default]
    Library,
    Queue,
    Playlists,
    NowPlaying,
    Equalizer,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Library, Tab::Queue, Tab::Playlists, Tab::NowPlaying, Tab::Equalizer];
    pub fn label(self) -> &'static str {
        match self {
            Tab::Library => "Library",
            Tab::Queue => "Queue",
            Tab::Playlists => "Playlists",
            Tab::NowPlaying => "Now Playing",
            Tab::Equalizer => "Equalizer",
        }
    }
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
    pub fn from_index(i: usize) -> Tab {
        Self::ALL[i % Self::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LibPane {
    Modes,
    Groups,
    #[default]
    Tracks,
}

/// Library tab: [browse modes] [groups of the mode] [tracks of the group].
#[derive(Debug, Default)]
pub struct LibraryView {
    pub mode: BrowseMode,
    pub pane: LibPane,
    /// Selection over `BrowseMode::ALL`.
    pub mode_state: ListState,
    pub groups: Vec<Group>,
    pub group_state: ListState,
    /// Tracks of the selected group, sorted by `sort` / `sort_desc`.
    pub tracks: Vec<TrackId>,
    pub track_state: TableState,
    pub sort: SortKey,
    pub sort_desc: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlPane {
    #[default]
    Lists,
    Tracks,
}

/// An entry of the playlists pane: smart playlists first, then user playlists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistEntry {
    Smart(Smart),
    /// Index into `app.playlists.lists`.
    User(usize),
}

#[derive(Debug, Default)]
pub struct PlaylistView {
    pub pane: PlPane,
    /// Selection over `app.playlist_entries()`.
    pub list_state: ListState,
    /// Tracks of the selected entry that exist in the library.
    pub tracks: Vec<TrackId>,
    /// Paths in the selected playlist that aren't in the library.
    pub missing: usize,
    pub track_state: TableState,
    /// For each entry of `tracks`: its index in the playlist file (differs when paths are missing).
    rows: Vec<usize>,
}

#[derive(Debug, Default)]
pub struct EqView {
    /// 0 = preamp, 1..=10 = bands 0..=9.
    pub selected: usize,
}

/// Single-line text input with a cursor (in chars).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Input {
    pub text: String,
    pub cursor: usize,
}

impl Input {
    pub fn with(text: &str) -> Input {
        Input { text: text.to_string(), cursor: text.chars().count() }
    }

    /// Insert text at the cursor (e.g. a paste); control characters become spaces.
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            let at = self.byte(self.cursor);
            self.text.insert(at, if c.is_control() { ' ' } else { c });
            self.cursor += 1;
        }
    }

    /// Editing keys: chars, backspace, delete, left/right, home/end, ctrl+a/e/u/w/k
    /// (alt/ctrl+backspace also delete a word). Returns true if the key was consumed.
    pub fn handle_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let len = self.text.chars().count();
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char('u') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => {
                let at = self.byte(self.cursor);
                self.text.truncate(at);
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if ctrl || alt => self.delete_word(),
            KeyCode::Char(c) if !ctrl && !alt => {
                let at = self.byte(self.cursor);
                self.text.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte(self.cursor);
                self.text.remove(at);
            }
            KeyCode::Delete if self.cursor < len => {
                let at = self.byte(self.cursor);
                self.text.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            _ => return false,
        }
        true
    }

    /// Byte offset of char index `c`.
    fn byte(&self, c: usize) -> usize {
        self.text.char_indices().nth(c).map_or(self.text.len(), |(i, _)| i)
    }

    /// Delete the word before the cursor (and the spaces after it).
    fn delete_word(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let end = self.cursor.min(chars.len());
        let mut start = end;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.text = chars[..start].iter().chain(&chars[end..]).collect();
        self.cursor = start;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PromptPurpose {
    /// Create a playlist, then add these tracks to it.
    NewPlaylist(Vec<TrackId>),
    /// Rename the user playlist saved in this file (not an index: IPC can reorder the list
    /// while the prompt is open).
    RenamePlaylist(PathBuf),
    SaveQueue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConfirmAction {
    /// Delete the user playlist saved in this file.
    DeletePlaylist(PathBuf),
    ClearQueue,
    /// Save the queue over the existing playlist of this name.
    SaveQueueAs(String),
}

#[derive(Debug)]
pub enum Overlay {
    Help { scroll: u16 },
    Search { input: Input, results: Vec<crate::search::Hit>, state: ListState },
    Command { input: Input, completions: Vec<String>, selected: Option<usize>, history_pos: Option<usize> },
    /// Choose a user playlist to add `tracks` to; the list shows "+ New playlist…" first.
    PickPlaylist { tracks: Vec<TrackId>, state: ListState },
    Prompt { title: String, input: Input, purpose: PromptPurpose },
    Confirm { message: String, action: ConfirmAction },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    Info,
    Ok,
    Warn,
    Error,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub text: String,
    pub kind: MsgKind,
    pub at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sleep {
    At(Instant),
    EndOfTrack,
}

/// Which list a click landed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListTarget {
    LibModes,
    LibGroups,
    LibTracks,
    Queue,
    PlLists,
    PlTracks,
    SearchResults,
    PickPlaylist,
    Completions,
}

impl ListTarget {
    fn in_overlay(self) -> bool {
        matches!(self, ListTarget::SearchResults | ListTarget::PickPlaylist | ListTarget::Completions)
    }
}

/// A list/table body on screen: row `i` of the body is item `offset + i`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListHit {
    pub area: Rect,
    pub target: ListTarget,
    pub offset: usize,
    pub len: usize,
}

/// Clickable regions recorded by `ui::draw` every frame (cleared at the start of each draw).
#[derive(Clone, Debug, Default)]
pub struct Hit {
    pub tabs: Vec<(Rect, Tab)>,
    /// The progress bar track (x within it maps to a seek position).
    pub progress: Option<Rect>,
    /// The volume indicator (click/scroll changes volume).
    pub volume: Option<Rect>,
    pub lists: Vec<ListHit>,
    /// EQ sliders: (area, EqView index: 0 = preamp, 1..=10 = bands).
    pub eq_sliders: Vec<(Rect, usize)>,
    /// Clickable icons (play/pause, next, prev, shuffle, repeat …).
    pub buttons: Vec<(Rect, Action)>,
    /// The open overlay's area; clicks outside it close the overlay.
    pub overlay: Option<Rect>,
    pub lyrics: Option<Rect>,
    /// The help overlay's real scroll limit (its lines minus one page), once drawn.
    pub help_max: Option<usize>,
}

/// What a left-button drag adjusts (set on press, cleared on release).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    Progress,
    Volume,
    Eq(usize),
}

/// Consecutive unplayable tracks before playback gives up.
const MAX_FAILURES: u32 = 5;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Synced lyrics follow playback again this long after the last manual scroll.
const LYRICS_FOLLOW_AFTER: Duration = Duration::from_secs(4);
const AUTOSAVE_EVERY: Duration = Duration::from_secs(30);
/// Loop wake-up interval when nothing animates (IPC, scan progress, timers still get serviced).
const IDLE_TICK: Duration = Duration::from_millis(250);
/// Messages animate for this long after appearing and before expiring.
const MESSAGE_ANIM: Duration = Duration::from_millis(400);
const SLEEP_STEPS: [u32; 5] = [15, 30, 45, 60, 90];
const HISTORY_MAX: usize = 100;
const SEARCH_LIMIT: usize = 300;
const LYRICS_OFFSET_STEP: f32 = 250.0;
/// Lines scrolled per wheel notch in text views (lyrics, help).
const WHEEL_LINES: isize = 3;
/// Tracks a radio queues after its seed.
const RADIO_LEN: usize = 50;
/// Radio fingerprints, in the cache folder.
const RADIO_FILE: &str = "radio.json";
const DEFAULT_STATUS: &str = "{state} {artist} - {title} [{position}/{duration}] vol {volume}%";

pub struct App {
    pub cfg: Config,
    pub paths: Paths,
    pub theme: Theme,
    pub icons: Icons,
    pub keymap: Keymap,

    pub lib: Library,
    pub scan: Option<Receiver<ScanMsg>>,
    /// (done, total) while scanning.
    pub scan_progress: Option<(usize, usize)>,

    /// None when no audio device could be opened (the UI still works).
    pub engine: Option<Engine>,
    pub queue: Queue,
    pub playlists: Playlists,
    pub state: State,
    pub searcher: Searcher,
    pub analyzer: Analyzer,
    pub art: ArtCache,
    pub lyrics: Option<Lyrics>,
    /// Manual scroll of the lyrics panel (unsynced lyrics, or after the user scrolls).
    pub lyrics_scroll: u16,

    pub tab: Tab,
    pub library_view: LibraryView,
    pub queue_state: TableState,
    pub playlist_view: PlaylistView,
    pub eq_view: EqView,
    pub overlay: Option<Overlay>,
    pub message: Option<Message>,

    /// Percent, 0..=cfg.playback.max_volume.
    pub volume: u8,
    pub muted: bool,
    pub speed: f32,
    pub eq: EqSettings,
    pub vis_mode: VisMode,
    /// Bars the visualizer area fits (written by the UI, read by `tick`).
    pub vis_bands: usize,
    pub show_lyrics: bool,
    pub show_art: bool,
    pub compact: bool,
    pub mini_vis: bool,
    pub time_remaining: bool,
    pub sleep: Option<Sleep>,
    pub ab: (Option<Duration>, Option<Duration>),
    /// Effective lyric offset for the playing track (config + per-track), ms.
    pub lyrics_offset_ms: i32,
    pub command_history: Vec<String>,
    /// Dominant cover color when ui.dynamic_accent is on.
    pub cover_accent: Option<Color>,

    pub hit: Hit,
    pub ipc: Option<IpcServer>,
    /// Media keys and the system Now Playing panel (None in tests, or without the OS service).
    pub media: Option<Media>,
    /// Radio fingerprints (None in tests: nothing gets analysed).
    pub features: Option<Features>,
    /// (done, total) while the library is analysed for the radio.
    pub analysis_progress: Option<(usize, usize)>,
    /// (item, of) while a download runs; (0, 0) until yt-dlp reports a playlist.
    pub download_progress: Option<(u32, u32)>,
    pub should_quit: bool,
    /// Something changed; redraw on the next loop iteration.
    pub dirty: bool,
    /// Set by the `redraw` action: the main loop should clear the terminal before the next draw
    /// (repainting every cell), then reset this.
    pub force_clear: bool,

    /// Files/folders from the command line, played once the first scan is done.
    cli_paths: Vec<PathBuf>,
    /// Restore the saved session (resume_session && !--no-resume).
    resume: bool,
    /// The first library scan has finished.
    scanned: bool,
    /// Theme chosen with `theme`/cycle-theme (persisted in the session).
    theme_override: Option<String>,
    /// Command-line text typed before browsing history.
    cmd_draft: String,
    /// Track whose start-up work (art, lyrics, notification) last ran.
    setup_track: Option<TrackId>,
    /// The current play was already recorded (as a play or a skip).
    counted: bool,
    /// Where the last `Engine::load` started its track (past zero when a session resumes).
    load_start: Duration,
    /// Consecutive tracks that failed to play.
    failures: u32,
    /// Last track whose gapless preload failed (not retried every tick).
    preload_failed: Option<TrackId>,
    /// Length of the running sleep timer, for cycling.
    sleep_minutes: u32,
    /// Whole seconds left on the sleep timer at the last redraw.
    sleep_shown: Option<u64>,
    /// When the user last scrolled synced lyrics by hand.
    lyrics_manual: Option<Instant>,
    last_click: Option<(Instant, ListTarget, usize)>,
    drag: Option<Drag>,
    last_title: Option<String>,
    last_save: Instant,
    /// Stats/session changed since the last save.
    state_dirty: bool,
    /// Theme/keymap/IPC/config warnings collected at startup.
    startup_warnings: Vec<String>,
    /// What the Now Playing panel was last told: (track, state, position, when).
    media_sent: Option<(Option<TrackId>, PlayState, Duration, Instant)>,
    analysis: Option<Receiver<(PathBuf, Option<Vec<f32>>)>>,
    download: Option<Receiver<DownloadMsg>>,
}

impl App {
    pub fn new(cfg: Config, paths: Paths, cli: &crate::Cli) -> anyhow::Result<App> {
        let engine = Engine::new(&cfg.playback);
        let err = engine.as_ref().err().map(|e| format!("no audio output: {e}"));
        let mut app = App::build(cfg, paths, cli, engine.ok());
        app.media = Media::new(Duration::from_secs(app.cfg.playback.seek_step_secs as u64));
        app.features = Some(Features::load(&app.paths.cache_dir.join(RADIO_FILE)));
        if let Some(e) = err {
            app.flash(e, MsgKind::Error);
        }
        Ok(app)
    }

    fn build(cfg: Config, paths: Paths, cli: &crate::Cli, engine: Option<Engine>) -> App {
        let state = State::load(&paths.state_file);
        let resume = cfg.playback.resume_session && !cli.no_resume;
        // a theme chosen in orbit is kept until the config names a different one
        let (saved_theme, base) = (&state.session.theme, &state.session.theme_base);
        let theme_override = saved_theme.clone().filter(|_| base.as_ref().is_none_or(|b| *b == cfg.ui.theme));
        let theme_name =
            cli.theme.clone().or_else(|| theme_override.clone().filter(|_| resume)).unwrap_or_else(|| cfg.ui.theme.clone());
        let (theme, mut warnings) = Theme::from_config(&theme_name, &cfg.colors);
        let (keymap, key_warnings) = Keymap::new(&cfg.keys);
        warnings.extend(key_warnings);
        let ipc = bind_ipc(&cfg, &paths).unwrap_or_else(|e| {
            warnings.push(e);
            None
        });
        let scan = Some(library::spawn_scan(cfg.library.clone(), paths.library_cache.clone(), cli.rescan));
        let playlists = Playlists::load(&paths.playlists_dir, &cfg.music_dirs());
        let mut queue = Queue::default();
        queue.set_shuffle(cfg.playback.shuffle);
        queue.repeat = cfg.playback.repeat;
        let mut app = App {
            theme,
            icons: theme::icons(cfg.ui.icons),
            keymap,
            lib: Library::empty(),
            scan,
            scan_progress: Some((0, 0)),
            engine,
            queue,
            playlists,
            state,
            searcher: Searcher::new(),
            analyzer: Analyzer::new(),
            art: ArtCache::new(),
            lyrics: None,
            lyrics_scroll: 0,
            tab: cfg.ui.default_tab,
            library_view: LibraryView {
                mode: cfg.ui.browse_mode,
                sort: cfg.ui.sort,
                sort_desc: cfg.ui.sort_desc,
                ..LibraryView::default()
            },
            queue_state: TableState::default(),
            playlist_view: PlaylistView::default(),
            eq_view: EqView::default(),
            overlay: None,
            message: None,
            volume: cli.volume.unwrap_or(cfg.playback.volume).min(max_volume(&cfg)),
            muted: false,
            speed: 1.0,
            eq: config_eq(&cfg),
            vis_mode: cfg.visualizer.mode,
            vis_bands: 32,
            show_lyrics: cfg.ui.show_lyrics,
            show_art: cfg.ui.show_art,
            compact: cfg.ui.compact,
            mini_vis: cfg.ui.mini_visualizer,
            time_remaining: cfg.ui.time_display == TimeDisplay::Remaining,
            sleep: None,
            ab: (None, None),
            lyrics_offset_ms: cfg.lyrics.offset_ms,
            command_history: Vec::new(),
            cover_accent: None,
            hit: Hit::default(),
            ipc,
            media: None,
            features: None,
            analysis_progress: None,
            download_progress: None,
            should_quit: false,
            dirty: true,
            force_clear: false,
            cli_paths: cli.paths.clone(),
            resume,
            scanned: false,
            theme_override,
            cmd_draft: String::new(),
            setup_track: None,
            counted: false,
            load_start: Duration::ZERO,
            failures: 0,
            preload_failed: None,
            sleep_minutes: 0,
            sleep_shown: None,
            lyrics_manual: None,
            last_click: None,
            drag: None,
            last_title: None,
            last_save: Instant::now(),
            state_dirty: false,
            startup_warnings: Vec::new(),
            media_sent: None,
            analysis: None,
            download: None,
            cfg,
            paths,
        };
        // nothing saved yet (first run): the config's shuffle, repeat etc. apply
        if resume && app.paths.state_file.exists() {
            app.apply_session_settings(cli.volume.is_some());
        }
        app.command_history = app.state.session.command_history.clone();
        app.rebuild_library_view(None);
        app.load_playlist_view(None);
        app.apply_engine_settings();
        app.startup_warnings = warnings;
        app.flash_startup_warnings();
        app
    }

    /// Merge config-file warnings (unknown keys, bad values from `Config::load_with_warnings`) into
    /// the startup warnings and show the first one. An audio error already on screen wins.
    pub fn add_config_warnings(&mut self, warnings: Vec<String>) {
        for w in warnings {
            if !self.startup_warnings.contains(&w) {
                self.startup_warnings.push(w);
            }
        }
        if !self.message.as_ref().is_some_and(|m| m.kind == MsgKind::Error) {
            self.flash_startup_warnings();
        }
    }

    fn flash_startup_warnings(&mut self) {
        let Some(first) = self.startup_warnings.first() else { return };
        let more = match self.startup_warnings.len() {
            1 => String::new(),
            n => format!(" (+{} more, run `orbit check`)", n - 1),
        };
        let text = format!("{first}{more}");
        self.flash(text, MsgKind::Warn);
    }

    /// Volume, modes, EQ, tab, browse mode and visualizer from the saved session (the queue is
    /// restored once the library is scanned).
    fn apply_session_settings(&mut self, keep_volume: bool) {
        let s = &self.state.session;
        if let Some(on) = s.shuffle {
            self.queue.set_shuffle(on);
        }
        self.queue.repeat = s.repeat.unwrap_or(self.queue.repeat);
        if let Some(v) = s.volume.filter(|_| !keep_volume) {
            self.volume = v.min(max_volume(&self.cfg));
        }
        self.muted = s.muted;
        self.speed = if (0.25..=4.0).contains(&s.speed) { s.speed } else { 1.0 };
        if let Some(eq) = &s.eq {
            self.eq = eq.clone();
        }
        if let Some(tab) = s.tab {
            self.tab = tab;
        }
        if let Some(mode) = s.browse_mode {
            self.library_view.mode = mode;
        }
        if let Some(mode) = s.vis_mode {
            self.vis_mode = mode;
        }
        self.compact = s.compact.unwrap_or(self.compact);
        self.show_art = s.show_art.unwrap_or(self.show_art);
        self.show_lyrics = s.show_lyrics.unwrap_or(self.show_lyrics);
        self.mini_vis = s.mini_visualizer.unwrap_or(self.mini_vis);
        self.time_remaining = s.time_remaining.unwrap_or(self.time_remaining);
    }

    fn apply_engine_settings(&mut self) {
        let Some(e) = self.engine.as_mut() else { return };
        let p = &self.cfg.playback;
        e.set_volume(self.volume, self.muted);
        e.set_speed(self.speed);
        e.set_eq(&self.eq);
        // "auto": album gain while the queue plays in order, track gain while it's shuffled
        let rg = match p.replaygain {
            ReplayGainMode::Auto if self.queue.shuffle => ReplayGainMode::Track,
            ReplayGainMode::Auto => ReplayGainMode::Album,
            mode => mode,
        };
        e.set_replaygain(rg, p.replaygain_preamp_db, p.replaygain_prevent_clip);
        e.set_fade(p.fade_ms);
    }

    // ---- the loop ----

    /// Called every loop iteration before drawing.
    pub fn tick(&mut self) {
        self.drain_scan();
        self.drain_analysis();
        self.drain_download();
        self.serve_ipc();
        self.serve_media();
        let events = self.engine.as_mut().map(|e| e.poll()).unwrap_or_default();
        for ev in events {
            self.on_engine_event(ev);
        }
        self.track_progress();
        if self.art.poll() {
            // the cover decoded off-thread has arrived; its accent is made readable on this theme
            self.refresh_accent();
            self.dirty = true;
        }
        self.check_sleep();
        if self.lyrics_manual.is_some_and(|t| t.elapsed() >= LYRICS_FOLLOW_AFTER) {
            self.lyrics_manual = None;
            self.lyrics_scroll = 0;
            self.dirty = true;
        }
        self.sync_preload();
        let playing = self.play_state() == PlayState::Playing;
        if playing || self.analyzer.active() {
            let tap = self.engine.as_ref().map(|e| e.tap());
            self.analyzer.update(tap.as_deref(), &self.cfg.visualizer, self.vis_bands.max(1), playing);
        }
        self.update_title();
        self.sync_media();
        if self.last_save.elapsed() >= AUTOSAVE_EVERY {
            if self.state_dirty || playing {
                self.save_state();
            }
            self.last_save = Instant::now();
        }
        if self.message.as_ref().is_some_and(|m| m.at.elapsed() >= self.message_timeout()) {
            self.message = None;
            self.dirty = true;
        }
        self.fix_selections();
    }

    fn drain_scan(&mut self) {
        loop {
            let Some(msg) = self.scan.as_ref().map(|rx| rx.try_recv()) else { return };
            match msg {
                Ok(ScanMsg::Progress { done, total }) => self.scan_progress = Some((done, total)),
                Ok(ScanMsg::Done(lib)) => self.on_scan_done(lib),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.scan = None;
                    self.scan_progress = None;
                    self.flash("library scan failed", MsgKind::Error);
                }
            }
            self.dirty = true;
        }
    }

    fn serve_ipc(&mut self) {
        let Some(requests) = self.ipc.as_mut().map(|s| s.poll()) else { return };
        for req in requests {
            let reply = match command::parse(&req.line) {
                // a remote `help` lists the commands instead of opening the help screen
                Ok(Command::Help) => command::COMMANDS.iter().map(|(usage, desc)| format!("{usage:<44} {desc}")).collect::<Vec<_>>().join("\n"),
                Ok(cmd) => self.exec(cmd).unwrap_or_else(|e| format!("error: {e}")),
                Err(e) => format!("error: {e}"),
            };
            req.reply(&reply);
        }
    }

    fn on_engine_event(&mut self, ev: EngineEvent) {
        self.dirty = true;
        match ev {
            // several loads in one frame (fast skipping): only the last one still plays
            EngineEvent::Started(id) if self.is_current(id) => self.on_started(id),
            EngineEvent::Started(_) => {}
            EngineEvent::Advanced { from, to } => {
                self.count_ended(from);
                if self.queue.advance() != Some(to) {
                    if let Some(i) = self.queue.tracks.iter().position(|t| *t == to) {
                        self.queue.jump(i);
                    }
                }
                self.on_started(to);
            }
            EngineEvent::Finished(from) => {
                self.count_ended(from);
                self.on_finished();
            }
            EngineEvent::Error { track, message } => {
                self.flash(message, MsgKind::Error);
                if track.is_some() && self.play_state() == PlayState::Stopped {
                    self.skip_failed(false);
                }
            }
        }
    }

    /// A track began: art, lyrics, accent, notification, title; restart play counting.
    fn on_started(&mut self, id: TrackId) {
        let Some(track) = self.lib.get(id).cloned() else { return };
        // resumed past the count point (session restore, rescan): it was counted getting there
        let start = std::mem::take(&mut self.load_start);
        let at = self.cfg.playback.count_play_after.clamp(0.0, 1.0);
        self.counted = !start.is_zero() && self.duration().is_some_and(|d| start.as_secs_f32() >= at * d.as_secs_f32());
        self.set_ab(None, None);
        if self.setup_track != Some(id) {
            self.setup_track = Some(id);
            self.art.set_track(Some(&track)); // its cover arrives in `tick`
            self.lyrics = lyrics::load(&track, &self.cfg.lyrics);
            self.lyrics_offset_ms = self.effective_lyrics_offset(&track);
            self.lyrics_scroll = 0;
            self.lyrics_manual = None;
            if self.cfg.notifications.enabled && self.play_state() == PlayState::Playing {
                let extras = self.status_extras();
                let n = &self.cfg.notifications;
                notify::notify(&notify::format(&n.title, Some(&track), &extras), &notify::format(&n.body, Some(&track), &extras));
            }
        }
        self.update_title();
    }

    fn on_finished(&mut self) {
        if self.sleep == Some(Sleep::EndOfTrack) {
            self.sleep = None;
            self.clear_track_view();
            self.flash("sleep timer: stopped", MsgKind::Info);
            return;
        }
        let stop_after = self.queue.stop_after_current;
        if self.queue.advance().is_some() {
            self.play_queue_current(Duration::ZERO, false);
        } else {
            self.clear_track_view();
            if stop_after {
                self.flash("stopped after current track", MsgKind::Info);
            }
        }
    }

    /// Count a play once enough of the track was heard; forget old failures once a track plays.
    fn track_progress(&mut self) {
        if self.play_state() != PlayState::Playing {
            return;
        }
        let pos = self.position();
        if self.failures > 0 && pos >= Duration::from_secs(1) {
            self.failures = 0;
        }
        if self.counted {
            return;
        }
        let Some(dur) = self.duration().filter(|d| !d.is_zero()) else { return };
        if pos.as_secs_f32() >= self.cfg.playback.count_play_after.clamp(0.0, 1.0) * dur.as_secs_f32() {
            if let Some(id) = self.now_playing().map(|t| t.id) {
                self.count_play(id);
            }
            self.counted = true;
        }
    }

    /// Record a play of `id`; the smart playlists (Most Played …) follow if they're on screen.
    fn count_play(&mut self, id: TrackId) {
        self.counted = true;
        let Some(path) = self.lib.get(id).map(|t| t.path.clone()) else { return };
        self.state.record_play(&path);
        self.state_dirty = true;
        if self.tab == Tab::Playlists {
            let keep = self.selected_pl_track();
            self.load_playlist_view(keep);
        }
    }

    /// A track that played to its end counts even when no tick saw it past `count_play_after`
    /// (1.0, or the last moments of a short track before the gapless switch).
    fn count_ended(&mut self, id: TrackId) {
        if !self.counted && self.setup_track == Some(id) {
            self.count_play(id);
        }
    }

    fn check_sleep(&mut self) {
        let Some(Sleep::At(at)) = self.sleep else {
            self.sleep_shown = None;
            return;
        };
        let left = at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            self.sleep = None;
            self.sleep_shown = None;
            if let Some(e) = self.engine.as_mut() {
                e.pause();
            }
            self.flash("sleep timer: paused", MsgKind::Info);
        } else if self.sleep_shown != Some(left.as_secs()) {
            self.sleep_shown = Some(left.as_secs());
            self.dirty = true;
        }
    }

    /// Gapless: keep the engine's preload equal to what the queue will play next.
    fn sync_preload(&mut self) {
        let Some(engine) = self.engine.as_mut() else { return };
        let want = if self.cfg.playback.gapless && engine.state() != PlayState::Stopped && self.sleep != Some(Sleep::EndOfTrack)
        {
            self.queue.peek_advance()
        } else {
            None
        };
        if want == engine.preloaded() || (want.is_some() && want == self.preload_failed) {
            return;
        }
        match want.and_then(|id| self.lib.get(id)) {
            Some(track) => self.preload_failed = engine.preload(track).err().and(want),
            None => engine.cancel_preload(),
        }
    }

    fn update_title(&mut self) {
        let title = if self.cfg.ui.terminal_title {
            self.now_playing().map(|t| notify::format(&self.cfg.ui.title_format, Some(t), &self.status_extras()))
        } else {
            None
        };
        if title != self.last_title {
            notify::set_terminal_title(title.as_deref().unwrap_or(""));
            self.last_title = title;
        }
    }

    fn message_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.ui.message_timeout_secs.max(1) as u64)
    }

    fn message_animating(&self) -> bool {
        let timeout = self.message_timeout();
        self.message.as_ref().is_some_and(|m| {
            let age = m.at.elapsed();
            age < MESSAGE_ANIM || age + MESSAGE_ANIM >= timeout
        })
    }

    fn animating(&self) -> bool {
        self.play_state() == PlayState::Playing || self.analyzer.active() || self.message_animating()
    }

    /// True if the next loop iteration should draw.
    pub fn needs_redraw(&self) -> bool {
        self.dirty || self.animating()
    }

    /// How long the main loop may wait for input before the next tick.
    pub fn frame_timeout(&self) -> Duration {
        if self.animating() {
            Duration::from_millis(1000 / self.cfg.ui.fps.clamp(1, 240) as u64)
        } else {
            IDLE_TICK
        }
    }

    /// Save state before exit.
    pub fn shutdown(&mut self) {
        self.save_state();
        if let Some(f) = self.features.as_ref().filter(|_| self.analysis.is_some()) {
            let _ = f.save(&self.paths.cache_dir.join(RADIO_FILE));
        }
        if self.last_title.take().is_some() {
            notify::set_terminal_title("");
        }
    }

    /// Show a transient message in the status line.
    pub fn flash(&mut self, text: impl Into<String>, kind: MsgKind) {
        self.message = Some(Message { text: text.into(), kind, at: Instant::now() });
        self.dirty = true;
    }

    // ---- input ----

    pub fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.dirty = true;
                self.handle_key(&key);
            }
            Event::Mouse(m) if self.cfg.ui.mouse && m.kind != MouseEventKind::Moved => {
                self.dirty = true;
                self.handle_mouse(m);
            }
            Event::Paste(text) => {
                self.dirty = true;
                if let Some(Overlay::Search { input, .. } | Overlay::Command { input, .. } | Overlay::Prompt { input, .. }) =
                    &mut self.overlay
                {
                    input.insert_str(&text);
                    self.refresh_overlay();
                }
            }
            Event::Resize(..) | Event::FocusGained => self.dirty = true,
            _ => return,
        }
        self.fix_selections();
    }

    fn handle_key(&mut self, key: &KeyEvent) {
        if self.overlay.is_some() {
            if self.overlay_key(key) {
                return;
            }
            // keys the overlay doesn't use: playback / view keys and :command bindings still work
            match self.keymap.get(key).cloned() {
                Some(Binding::Action(a)) if matches!(a.category(), "Playback" | "View") => self.handle_action(a),
                Some(Binding::Command(c)) => self.run_line(&c),
                _ => {}
            }
            return;
        }
        match self.keymap.get(key).cloned() {
            Some(Binding::Action(a)) => self.handle_action(a),
            Some(Binding::Command(c)) => self.run_line(&c),
            None => {}
        }
    }

    /// Keys for the open overlay. Returns false for keys it doesn't use.
    fn overlay_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.overlay = None;
            return true;
        }
        let action = match self.keymap.get(key) {
            Some(Binding::Action(a)) => Some(*a),
            _ => None,
        };
        match self.overlay {
            Some(Overlay::Help { .. }) => self.help_key(key, action),
            Some(Overlay::Search { .. }) => self.search_key(key, ctrl),
            Some(Overlay::Command { .. }) => self.command_key(key, ctrl),
            Some(Overlay::Prompt { .. }) => self.prompt_key(key),
            Some(Overlay::Confirm { .. }) => self.confirm_key(key),
            Some(Overlay::PickPlaylist { .. }) => self.pick_key(key, action),
            None => false,
        }
    }

    fn help_key(&mut self, key: &KeyEvent, action: Option<Action>) -> bool {
        let page = self.overlay_page();
        let delta = match (key.code, action) {
            (KeyCode::Esc | KeyCode::Char('q' | '?'), _) | (_, Some(Action::Back | Action::Help | Action::Quit)) => {
                self.overlay = None;
                return true;
            }
            (KeyCode::Up, _) => -1,
            (KeyCode::Down, _) => 1,
            (KeyCode::PageUp, _) => -page,
            (KeyCode::PageDown, _) => page,
            (KeyCode::Home, _) => isize::MIN,
            (KeyCode::End, _) => isize::MAX,
            (_, Some(a @ (Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Top | Action::Bottom))) => {
                nav_delta(a, page)
            }
            _ => return false,
        };
        self.scroll_help(delta);
        true
    }

    /// The help renderer clamps `scroll` to the real (wrapped) line count on every draw.
    fn scroll_help(&mut self, delta: isize) {
        if let Some(Overlay::Help { scroll }) = &mut self.overlay {
            *scroll = (*scroll as isize).saturating_add(delta).clamp(0, u16::MAX as isize) as u16;
        }
    }

    /// Enter plays now, Tab / ctrl+a enqueue; alt+Enter plays next, alt+a enqueues every result,
    /// alt+f favorites and alt+p adds the result to a playlist.
    fn search_key(&mut self, key: &KeyEvent, ctrl: bool) -> bool {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter if alt => {
                if let Some(id) = self.search_selected() {
                    self.queue.insert_next(&[id]);
                    let what = self.describe(&[id]);
                    self.flash(format!("next: {what}"), MsgKind::Info);
                    self.move_in(ListTarget::SearchResults, 1);
                }
            }
            KeyCode::Enter => self.search_play(),
            KeyCode::Tab => self.search_enqueue(),
            KeyCode::Char('a') if ctrl => self.search_enqueue(),
            KeyCode::Char('a') if alt => {
                if let Some(Overlay::Search { results, .. }) = &self.overlay {
                    let ids = results.iter().map(|h| h.id).collect();
                    self.enqueue(ids, false);
                }
            }
            KeyCode::Char('f') if alt => {
                if let Some(id) = self.search_selected() {
                    self.toggle_favorites(&[id]);
                }
            }
            KeyCode::Char('p') if alt => {
                let tracks = self.search_selected().into_iter().collect();
                self.open_picker(tracks);
            }
            KeyCode::Up => self.move_in(ListTarget::SearchResults, -1),
            KeyCode::Char('p') if ctrl => self.move_in(ListTarget::SearchResults, -1),
            KeyCode::Down => self.move_in(ListTarget::SearchResults, 1),
            KeyCode::Char('n') if ctrl => self.move_in(ListTarget::SearchResults, 1),
            KeyCode::PageUp => self.move_in(ListTarget::SearchResults, -self.page(ListTarget::SearchResults)),
            KeyCode::PageDown => self.move_in(ListTarget::SearchResults, self.page(ListTarget::SearchResults)),
            _ => return self.edit_overlay_input(key),
        }
        true
    }

    fn command_key(&mut self, key: &KeyEvent, ctrl: bool) -> bool {
        let empty = matches!(&self.overlay, Some(Overlay::Command { input, .. }) if input.text.is_empty());
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Backspace if empty => self.overlay = None,
            KeyCode::Enter => self.run_command_input(),
            KeyCode::Tab => self.cycle_completion(false),
            KeyCode::BackTab => self.cycle_completion(true),
            KeyCode::Up => self.history_step(true),
            KeyCode::Char('p') if ctrl => self.history_step(true),
            KeyCode::Down => self.history_step(false),
            KeyCode::Char('n') if ctrl => self.history_step(false),
            _ => return self.edit_overlay_input(key),
        }
        true
    }

    fn prompt_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter => self.submit_prompt(),
            _ => return self.edit_overlay_input(key),
        }
        true
    }

    /// y / Enter confirms, n / Esc cancels; every other key is swallowed.
    fn confirm_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                if let Some(Overlay::Confirm { action, .. }) = self.overlay.take() {
                    self.confirm(action);
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => self.overlay = None,
            _ => {}
        }
        true
    }

    fn pick_key(&mut self, key: &KeyEvent, action: Option<Action>) -> bool {
        let page = self.page(ListTarget::PickPlaylist);
        match (key.code, action) {
            (KeyCode::Esc, _) | (_, Some(Action::Back | Action::Quit)) => self.overlay = None,
            (KeyCode::Enter, _) | (_, Some(Action::Select)) => self.pick_playlist(),
            (KeyCode::Up, _) => self.move_in(ListTarget::PickPlaylist, -1),
            (KeyCode::Down, _) => self.move_in(ListTarget::PickPlaylist, 1),
            (_, Some(a @ (Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Top | Action::Bottom))) => {
                self.move_in(ListTarget::PickPlaylist, nav_delta(a, page))
            }
            _ => return false,
        }
        true
    }

    /// Text editing for the overlay's input; refreshes results/completions when the text changes.
    fn edit_overlay_input(&mut self, key: &KeyEvent) -> bool {
        let Some(Overlay::Search { input, .. } | Overlay::Command { input, .. } | Overlay::Prompt { input, .. }) =
            &mut self.overlay
        else {
            return false;
        };
        let before = input.text.clone();
        if !input.handle_key(key) {
            return false;
        }
        if input.text != before {
            self.refresh_overlay();
        }
        true
    }

    /// Recompute what depends on the overlay's text: search results, command completions.
    fn refresh_overlay(&mut self) {
        let text = match &mut self.overlay {
            Some(Overlay::Search { input, results, state }) => {
                *results = self.searcher.search(&self.lib, &input.text, SEARCH_LIMIT);
                state.select((!results.is_empty()).then_some(0));
                return;
            }
            Some(Overlay::Command { input, .. }) => input.text.clone(),
            _ => return,
        };
        let fresh = self.completions_for(&text);
        if let Some(Overlay::Command { completions, selected, history_pos, .. }) = &mut self.overlay {
            *completions = fresh;
            *selected = None;
            *history_pos = None;
        }
    }

    fn open_search(&mut self, text: &str) {
        self.overlay = Some(Overlay::Search { input: Input::with(text), results: Vec::new(), state: ListState::default() });
        self.refresh_overlay();
    }

    fn search_selected(&self) -> Option<TrackId> {
        match &self.overlay {
            Some(Overlay::Search { results, state, .. }) => state.selected().and_then(|i| results.get(i)).map(|h| h.id),
            _ => None,
        }
    }

    /// Play the selected result now (keeping the queue) and close search.
    fn search_play(&mut self) {
        let Some(id) = self.search_selected() else { return };
        self.overlay = None;
        self.play_now(&[id]);
    }

    /// Enqueue the selected result and move to the next one (search stays open).
    fn search_enqueue(&mut self) {
        let Some(id) = self.search_selected() else { return };
        self.enqueue(vec![id], false);
        self.move_in(ListTarget::SearchResults, 1);
    }

    fn open_command(&mut self, text: &str) {
        self.cmd_draft.clear();
        let completions = self.completions_for(text);
        self.overlay = Some(Overlay::Command { input: Input::with(text), completions, selected: None, history_pos: None });
    }

    /// Candidate lines for a partial command line, including playlist names for commands that take one.
    fn completions_for(&self, text: &str) -> Vec<String> {
        let names: Vec<String> = self.playlists.lists.iter().map(|p| p.name.clone()).collect();
        command::complete_with(text, &names)
    }

    fn cycle_completion(&mut self, back: bool) {
        let stale = match &self.overlay {
            Some(Overlay::Command { input, completions, .. }) if completions.is_empty() => Some(input.text.clone()),
            _ => None,
        };
        let fresh = stale.map(|t| self.completions_for(&t));
        let Some(Overlay::Command { input, completions, selected, .. }) = &mut self.overlay else { return };
        if let Some(f) = fresh {
            *completions = f;
        }
        let n = completions.len();
        if n == 0 {
            return;
        }
        let i = match (*selected, back) {
            (None, false) => 0,
            (None, true) => n - 1,
            (Some(i), false) => (i + 1) % n,
            (Some(i), true) => (i + n - 1) % n,
        };
        *selected = Some(i);
        *input = Input::with(&completions[i]);
    }

    /// Up (older) / Down (newer) through the command history; past the newest returns to the draft.
    fn history_step(&mut self, older: bool) {
        let len = self.command_history.len();
        let Some(Overlay::Command { input, history_pos, .. }) = &mut self.overlay else { return };
        let pos = match (*history_pos, older) {
            _ if len == 0 => return,
            (None, true) => {
                self.cmd_draft = input.text.clone();
                Some(len - 1)
            }
            (None, false) => return,
            (Some(p), true) => Some(p.saturating_sub(1)),
            (Some(p), false) if p + 1 < len => Some(p + 1),
            (Some(_), false) => None,
        };
        *history_pos = pos;
        *input = Input::with(pos.map_or(self.cmd_draft.as_str(), |p| self.command_history[p].as_str()));
        let text = input.text.clone();
        let fresh = self.completions_for(&text);
        if let Some(Overlay::Command { completions, selected, .. }) = &mut self.overlay {
            *completions = fresh;
            *selected = None;
        }
    }

    fn run_command_input(&mut self) {
        let Some(Overlay::Command { input, .. }) = self.overlay.take() else { return };
        let line = input.text.trim().to_string();
        if line.is_empty() {
            return;
        }
        self.command_history.retain(|h| *h != line);
        self.command_history.push(line.clone());
        let excess = self.command_history.len().saturating_sub(HISTORY_MAX);
        self.command_history.drain(..excess);
        self.state_dirty = true;
        // a typed `save` asks before replacing a playlist (key bindings and `orbit ctl` replace)
        if let Ok(Command::Save(name)) = command::parse(&line)
            && self.confirm_replace(&name)
        {
            return;
        }
        self.run_line(&line);
    }

    /// The playlist picker for `tracks`, on the first playlist (or "+ New playlist…" if none).
    fn open_picker(&mut self, tracks: Vec<TrackId>) {
        if tracks.is_empty() {
            self.flash("nothing selected", MsgKind::Warn);
            return;
        }
        let row = if self.playlists.lists.is_empty() { 0 } else { 1 };
        self.overlay = Some(Overlay::PickPlaylist { tracks, state: ListState::default().with_selected(Some(row)) });
    }

    fn open_prompt(&mut self, title: &str, text: &str, purpose: PromptPurpose) {
        self.overlay = Some(Overlay::Prompt { title: title.to_string(), input: Input::with(text), purpose });
    }

    /// Enter in a prompt: on success close it, on error keep it open so the name can be fixed.
    fn submit_prompt(&mut self) {
        let Some(Overlay::Prompt { input, purpose, .. }) = &self.overlay else { return };
        let (name, purpose) = (input.text.trim().to_string(), purpose.clone());
        if name.is_empty() {
            self.flash("the name can't be empty", MsgKind::Warn);
            return;
        }
        let result = match purpose {
            PromptPurpose::NewPlaylist(ids) => self.create_playlist(&name, &ids),
            PromptPurpose::RenamePlaylist(path) => self.playlist_at(&path).and_then(|i| self.rename_playlist(i, &name)),
            PromptPurpose::SaveQueue => {
                if self.confirm_replace(&name) {
                    return;
                }
                self.save_queue_as(&name)
            }
        };
        match result {
            Ok(msg) => {
                self.overlay = None;
                self.flash(msg, MsgKind::Ok);
            }
            Err(e) => self.flash(e, MsgKind::Error),
        }
    }

    fn confirm(&mut self, action: ConfirmAction) {
        let result = match action {
            ConfirmAction::ClearQueue => self.exec(Command::Clear),
            ConfirmAction::DeletePlaylist(path) => self.playlist_at(&path).and_then(|i| self.delete_playlist(i)),
            ConfirmAction::SaveQueueAs(name) => self.save_queue_as(&name),
        };
        match result {
            Ok(msg) => self.flash(msg, MsgKind::Ok),
            Err(e) => self.flash(e, MsgKind::Error),
        }
    }

    /// Enter in the playlist picker: row 0 creates a new playlist, the others add to that playlist.
    fn pick_playlist(&mut self) {
        let Some(Overlay::PickPlaylist { tracks, state }) = self.overlay.take() else { return };
        let Some(idx) = state.selected().and_then(|row| row.checked_sub(1)) else {
            self.open_prompt("New playlist", "", PromptPurpose::NewPlaylist(tracks));
            return;
        };
        let paths = self.paths_of(&tracks);
        match self.playlists.add(idx, &paths, &self.lib) {
            Ok(n) => {
                let name = self.pl_name(idx);
                let what = if n == 1 { self.describe(&tracks) } else { format!("{n} tracks") };
                self.after_playlists_changed(None);
                self.flash(format!("added {what} to \"{name}\""), MsgKind::Ok);
            }
            Err(e) => self.flash(e, MsgKind::Error),
        }
    }

    // ---- mouse ----

    fn handle_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        let dir = match m.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => 0,
        };
        if let MouseEventKind::Up(_) = m.kind {
            self.drag = None;
        }
        if self.overlay.is_some() {
            let inside = self.hit.overlay.is_none_or(|r| r.contains(pos));
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) if inside => self.click_list(pos),
                MouseEventKind::Down(_) if !inside => self.overlay = None,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if inside => {
                    if matches!(self.overlay, Some(Overlay::Help { .. })) {
                        self.scroll_help(dir * WHEEL_LINES);
                    } else if let Some((target, _)) = self.list_at(pos) {
                        self.move_in(target, dir);
                    }
                }
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(pos),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(pos),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => self.wheel(pos, dir),
            _ => {}
        }
    }

    fn click(&mut self, pos: Position) {
        if let Some(&(_, tab)) = self.hit.tabs.iter().find(|(r, _)| r.contains(pos)) {
            self.set_tab(tab);
        } else if let Some(&(_, action)) = self.hit.buttons.iter().find(|(r, _)| r.contains(pos)) {
            self.handle_action(action);
        } else if self.hit.progress.is_some_and(|r| r.contains(pos)) {
            self.drag = Some(Drag::Progress);
            self.drag_to(pos);
        } else if self.hit.volume.is_some_and(|r| r.contains(pos)) {
            self.drag = Some(Drag::Volume);
            self.drag_to(pos);
        } else if let Some(&(_, i)) = self.hit.eq_sliders.iter().find(|(r, _)| r.contains(pos)) {
            self.drag = Some(Drag::Eq(i));
            self.drag_to(pos);
        } else if self.hit.lyrics.is_some_and(|r| r.contains(pos)) {
            // back to following the song
            self.lyrics_manual = None;
            self.lyrics_scroll = 0;
        } else {
            self.click_list(pos);
        }
    }

    /// Continue a press on the progress bar, volume or an EQ slider (x/y clamped to the widget).
    fn drag_to(&mut self, pos: Position) {
        match self.drag {
            Some(Drag::Progress) => {
                if let Some(r) = self.hit.progress {
                    let x = pos.x.clamp(r.x, r.right().saturating_sub(1)) - r.x;
                    let _ = self.seek(SeekTarget::Percent(x as f32 * 100.0 / r.width.max(1) as f32));
                }
            }
            Some(Drag::Volume) => {
                if let Some(r) = self.hit.volume {
                    let x = pos.x.clamp(r.x, r.right().saturating_sub(1)) - r.x;
                    let frac = x as f32 / r.width.saturating_sub(1).max(1) as f32;
                    let reply = self.set_volume(Level::Set(frac * max_volume(&self.cfg) as f32));
                    self.flash(reply, MsgKind::Info);
                }
            }
            Some(Drag::Eq(i)) => {
                if let Some(&(r, _)) = self.hit.eq_sliders.iter().find(|(_, j)| *j == i) {
                    let y = pos.y.clamp(r.y, r.bottom().saturating_sub(1)) - r.y;
                    let db = EQ_MAX_DB - 2.0 * EQ_MAX_DB * y as f32 / r.height.saturating_sub(1).max(1) as f32;
                    self.eq_view.selected = i;
                    self.eq_set(i, (db * 2.0).round() / 2.0);
                }
            }
            None => {}
        }
    }

    fn wheel(&mut self, pos: Position, dir: isize) {
        let up = dir < 0;
        if self.hit.tabs.iter().any(|(r, _)| r.contains(pos)) {
            self.set_tab(Tab::from_index(self.tab.index() + if up { Tab::ALL.len() - 1 } else { 1 }));
        } else if self.hit.volume.is_some_and(|r| r.contains(pos)) {
            let step = self.cfg.playback.volume_step as f32;
            self.run(Command::Volume(if up { Level::Up(step) } else { Level::Down(step) }));
        } else if self.hit.progress.is_some_and(|r| r.contains(pos)) {
            let d = Duration::from_secs(self.cfg.playback.seek_step_secs as u64);
            self.run_quiet(Command::Seek(if up { SeekTarget::Forward(d) } else { SeekTarget::Backward(d) }));
        } else if let Some(&(_, i)) = self.hit.eq_sliders.iter().find(|(r, _)| r.contains(pos)) {
            self.eq_view.selected = i;
            self.eq_nudge(i, if up { 1.0 } else { -1.0 });
        } else if self.hit.lyrics.is_some_and(|r| r.contains(pos)) {
            self.scroll_lyrics(dir * WHEEL_LINES);
        } else if let Some((target, _)) = self.list_at(pos) {
            self.move_in(target, dir);
        }
    }

    /// The topmost list under `pos` (overlay lists while an overlay is open) and the item there.
    fn list_at(&self, pos: Position) -> Option<(ListTarget, Option<usize>)> {
        let overlay = self.overlay.is_some();
        self.hit.lists.iter().rev().filter(|l| l.target.in_overlay() == overlay).find(|l| l.area.contains(pos)).map(|l| {
            let i = l.offset + (pos.y - l.area.y) as usize;
            (l.target, (i < l.len).then_some(i))
        })
    }

    /// Click in a list: focus it and select the row; a second click on the same row activates it.
    fn click_list(&mut self, pos: Position) {
        let Some((target, item)) = self.list_at(pos) else { return };
        self.focus(target);
        let Some(i) = item else { return };
        self.select_row(target, i);
        let now = Instant::now();
        let double = self.last_click.is_some_and(|(at, t, j)| t == target && j == i && now.duration_since(at) <= DOUBLE_CLICK);
        self.last_click = if double { None } else { Some((now, target, i)) };
        if double {
            match target {
                ListTarget::SearchResults => self.search_play(),
                ListTarget::PickPlaylist => self.pick_playlist(),
                ListTarget::Completions => self.run_command_input(),
                _ => self.select(),
            }
        }
    }

    // ---- lists and navigation ----

    fn focus(&mut self, target: ListTarget) {
        match target {
            ListTarget::LibModes | ListTarget::LibGroups | ListTarget::LibTracks => {
                self.tab = Tab::Library;
                self.library_view.pane = match target {
                    ListTarget::LibModes => LibPane::Modes,
                    ListTarget::LibGroups => LibPane::Groups,
                    _ => LibPane::Tracks,
                };
            }
            ListTarget::Queue => self.tab = Tab::Queue,
            ListTarget::PlLists | ListTarget::PlTracks => {
                self.tab = Tab::Playlists;
                self.playlist_view.pane = if target == ListTarget::PlLists { PlPane::Lists } else { PlPane::Tracks };
            }
            ListTarget::SearchResults | ListTarget::PickPlaylist | ListTarget::Completions => {}
        }
    }

    /// The list that keyboard navigation moves in on the current tab.
    fn focused_list(&self) -> Option<ListTarget> {
        match self.tab {
            Tab::Library => Some(match self.library_view.pane {
                LibPane::Modes => ListTarget::LibModes,
                LibPane::Groups => ListTarget::LibGroups,
                LibPane::Tracks => ListTarget::LibTracks,
            }),
            Tab::Queue => Some(ListTarget::Queue),
            Tab::Playlists => Some(match self.playlist_view.pane {
                PlPane::Lists => ListTarget::PlLists,
                PlPane::Tracks => ListTarget::PlTracks,
            }),
            Tab::NowPlaying | Tab::Equalizer => None,
        }
    }

    fn list_len(&self, target: ListTarget) -> usize {
        match (target, &self.overlay) {
            (ListTarget::LibModes, _) => BrowseMode::ALL.len(),
            (ListTarget::LibGroups, _) => self.library_view.groups.len(),
            (ListTarget::LibTracks, _) => self.library_view.tracks.len(),
            (ListTarget::Queue, _) => self.queue.len(),
            (ListTarget::PlLists, _) => Smart::ALL.len() + self.playlists.lists.len(),
            (ListTarget::PlTracks, _) => self.playlist_view.tracks.len(),
            (ListTarget::SearchResults, Some(Overlay::Search { results, .. })) => results.len(),
            (ListTarget::PickPlaylist, Some(Overlay::PickPlaylist { .. })) => self.playlists.lists.len() + 1,
            (ListTarget::Completions, Some(Overlay::Command { completions, .. })) => completions.len(),
            _ => 0,
        }
    }

    fn list_selected(&self, target: ListTarget) -> Option<usize> {
        match (target, &self.overlay) {
            (ListTarget::LibModes, _) => self.library_view.mode_state.selected(),
            (ListTarget::LibGroups, _) => self.library_view.group_state.selected(),
            (ListTarget::LibTracks, _) => self.library_view.track_state.selected(),
            (ListTarget::Queue, _) => self.queue_state.selected(),
            (ListTarget::PlLists, _) => self.playlist_view.list_state.selected(),
            (ListTarget::PlTracks, _) => self.playlist_view.track_state.selected(),
            (ListTarget::SearchResults, Some(Overlay::Search { state, .. }))
            | (ListTarget::PickPlaylist, Some(Overlay::PickPlaylist { state, .. })) => state.selected(),
            (ListTarget::Completions, Some(Overlay::Command { selected, .. })) => *selected,
            _ => None,
        }
    }

    /// Select row `i` of a list, with the side effects of moving there (mode/group/playlist reloads).
    fn select_row(&mut self, target: ListTarget, i: usize) {
        match target {
            ListTarget::LibModes => {
                if let Some(&mode) = BrowseMode::ALL.get(i).filter(|m| **m != self.library_view.mode) {
                    self.set_browse_mode(mode);
                }
            }
            ListTarget::LibGroups => {
                if self.library_view.group_state.selected() != Some(i) {
                    self.library_view.group_state.select(Some(i));
                    self.load_group_tracks(None);
                }
            }
            ListTarget::LibTracks => self.library_view.track_state.select(Some(i)),
            ListTarget::Queue => self.queue_state.select(Some(i)),
            ListTarget::PlLists => {
                if self.playlist_view.list_state.selected() != Some(i) {
                    self.playlist_view.list_state.select(Some(i));
                    self.playlist_view.track_state = TableState::default();
                    self.load_playlist_view(None);
                }
            }
            ListTarget::PlTracks => self.playlist_view.track_state.select(Some(i)),
            ListTarget::SearchResults | ListTarget::PickPlaylist | ListTarget::Completions => match &mut self.overlay {
                Some(Overlay::Search { state, .. } | Overlay::PickPlaylist { state, .. }) => state.select(Some(i)),
                Some(Overlay::Command { input, completions, selected, .. }) => {
                    if let Some(c) = completions.get(i) {
                        *input = Input::with(c);
                        *selected = Some(i);
                    }
                }
                _ => {}
            },
        }
    }

    fn move_in(&mut self, target: ListTarget, delta: isize) {
        if let Some(i) = moved(self.list_selected(target), self.list_len(target), delta) {
            self.select_row(target, i);
        }
    }

    /// Visible rows of a list in the last frame (page size for PageUp/PageDown).
    fn page(&self, target: ListTarget) -> isize {
        self.hit.lists.iter().find(|l| l.target == target).map_or(10, |l| l.area.height.max(1) as isize)
    }

    fn overlay_page(&self) -> isize {
        self.hit.overlay.map_or(10, |r| r.height.saturating_sub(2).max(1) as isize)
    }

    /// Upper bound for the help scroll: measured by the last frame that drew the help, else
    /// roughly one line per action, command and custom binding plus headers, minus one page.
    #[cfg(test)]
    fn help_max_scroll(&self) -> isize {
        if let Some(max) = self.hit.help_max {
            return max as isize;
        }
        let categories = Action::ALL.iter().map(|a| a.category()).collect::<std::collections::BTreeSet<_>>().len();
        let lines = Action::ALL.len() + 2 * categories + command::COMMANDS.len() + self.keymap.command_bindings().len() + 6;
        (lines as isize - self.overlay_page()).max(0)
    }

    fn navigate(&mut self, a: Action) {
        match self.tab {
            Tab::NowPlaying => {
                let page = self.hit.lyrics.map_or(10, |r| r.height.max(1) as isize);
                self.scroll_lyrics(nav_delta(a, page));
            }
            Tab::Equalizer => {
                let delta = match a {
                    Action::Up => 1.0,
                    Action::Down => -1.0,
                    Action::PageUp => 3.0,
                    Action::PageDown => -3.0,
                    Action::Top => 2.0 * EQ_MAX_DB,
                    Action::Bottom => -2.0 * EQ_MAX_DB,
                    _ => return,
                };
                self.eq_nudge(self.eq_view.selected, delta);
            }
            _ => {
                if let Some(target) = self.focused_list() {
                    self.move_in(target, nav_delta(a, self.page(target)));
                }
            }
        }
    }

    /// Left/Right/Back move between panes (clamped); FocusNext/FocusPrev cycle. On the EQ tab they
    /// pick the band; on Now Playing Left/Right seek.
    fn focus_move(&mut self, a: Action) {
        let (delta, wrap) = match a {
            Action::Left | Action::Back => (-1, false),
            Action::Right => (1, false),
            Action::FocusNext => (1, true),
            Action::FocusPrev => (-1, true),
            _ => return,
        };
        match self.tab {
            Tab::Library => {
                const PANES: [LibPane; 3] = [LibPane::Modes, LibPane::Groups, LibPane::Tracks];
                let i = PANES.iter().position(|p| *p == self.library_view.pane).unwrap_or(2);
                self.library_view.pane = PANES[step(i, PANES.len(), delta, wrap)];
            }
            Tab::Playlists => {
                const PANES: [PlPane; 2] = [PlPane::Lists, PlPane::Tracks];
                let i = PANES.iter().position(|p| *p == self.playlist_view.pane).unwrap_or(0);
                self.playlist_view.pane = PANES[step(i, PANES.len(), delta, wrap)];
            }
            Tab::Equalizer if a != Action::Back => self.eq_view.selected = step(self.eq_view.selected, 11, delta, wrap),
            Tab::NowPlaying if matches!(a, Action::Left | Action::Right) => {
                let d = Duration::from_secs(self.cfg.playback.seek_step_secs as u64);
                self.run_quiet(Command::Seek(if a == Action::Right { SeekTarget::Forward(d) } else { SeekTarget::Backward(d) }));
            }
            _ => {}
        }
    }

    fn select(&mut self) {
        match self.tab {
            Tab::Library => {
                let v = &self.library_view;
                match v.pane {
                    LibPane::Modes => self.library_view.pane = LibPane::Groups,
                    LibPane::Groups => self.activate(v.tracks.clone(), None),
                    LibPane::Tracks => {
                        if let Some(i) = v.track_state.selected() {
                            self.activate(v.tracks.clone(), Some(i));
                        }
                    }
                }
            }
            Tab::Queue => {
                if let Some(i) = self.queue_state.selected().filter(|i| *i < self.queue.len()) {
                    self.queue.jump(i);
                    self.note_skip();
                    self.play_queue_current(Duration::ZERO, false);
                }
            }
            Tab::Playlists => {
                let pv = &self.playlist_view;
                match pv.pane {
                    PlPane::Lists => self.activate(pv.tracks.clone(), None),
                    PlPane::Tracks => {
                        if let Some(i) = pv.track_state.selected() {
                            self.activate(pv.tracks.clone(), Some(i));
                        }
                    }
                }
            }
            Tab::Equalizer => self.eq_set(self.eq_view.selected, 0.0),
            Tab::NowPlaying => {}
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        if tab == Tab::Playlists && self.tab != Tab::Playlists {
            let keep = self.selected_pl_track();
            self.load_playlist_view(keep);
        }
        self.tab = tab;
    }

    fn jump_to_current(&mut self) {
        if self.tab == Tab::NowPlaying {
            self.lyrics_manual = None;
            self.lyrics_scroll = 0;
            return;
        }
        let playing = self.engine.as_ref().and_then(|e| e.current()).or_else(|| self.queue.current_track());
        let Some(id) = playing else {
            self.flash("nothing playing", MsgKind::Warn);
            return;
        };
        match self.tab {
            Tab::Library => {
                match self.library_view.tracks.iter().position(|t| *t == id) {
                    Some(i) => self.library_view.track_state.select(Some(i)),
                    None => self.rebuild_library_view(Some(id)),
                }
                self.library_view.pane = LibPane::Tracks;
            }
            Tab::Queue => self.queue_state.select(self.queue.current),
            Tab::Playlists => match self.playlist_view.tracks.iter().position(|t| *t == id) {
                Some(i) => {
                    self.playlist_view.track_state.select(Some(i));
                    self.playlist_view.pane = PlPane::Tracks;
                }
                None => self.flash("the playing track isn't in this playlist", MsgKind::Info),
            },
            Tab::NowPlaying | Tab::Equalizer => {}
        }
    }

    /// Clamp every selection to its list (and select something in non-empty lists).
    fn fix_selections(&mut self) {
        if self.queue_state.selected().is_none() {
            self.queue_state.select(self.queue.current);
        }
        clamp_selection(self.queue_state.selected_mut(), self.queue.len());
        let v = &mut self.library_view;
        clamp_selection(v.group_state.selected_mut(), v.groups.len());
        clamp_selection(v.track_state.selected_mut(), v.tracks.len());
        let entries = Smart::ALL.len() + self.playlists.lists.len();
        let pv = &mut self.playlist_view;
        clamp_selection(pv.list_state.selected_mut(), entries);
        clamp_selection(pv.track_state.selected_mut(), pv.tracks.len());
    }

    /// What queue/favorite/playlist actions apply to on the current tab.
    fn selection(&self) -> Vec<TrackId> {
        let pick = |sel: Option<usize>, ids: &[TrackId]| sel.and_then(|i| ids.get(i).copied()).into_iter().collect();
        let (v, pv) = (&self.library_view, &self.playlist_view);
        match self.tab {
            Tab::Library => match v.pane {
                LibPane::Modes => Vec::new(),
                LibPane::Groups => v.tracks.clone(),
                LibPane::Tracks => pick(v.track_state.selected(), &v.tracks),
            },
            Tab::Queue => pick(self.queue_state.selected(), &self.queue.tracks),
            Tab::Playlists => match pv.pane {
                PlPane::Lists => pv.tracks.clone(),
                PlPane::Tracks => pick(pv.track_state.selected(), &pv.tracks),
            },
            Tab::NowPlaying | Tab::Equalizer => self.engine.as_ref().and_then(|e| e.current()).into_iter().collect(),
        }
    }

    /// The whole list on screen (enqueue-all).
    fn current_list(&self) -> Vec<TrackId> {
        match self.tab {
            Tab::Library => self.library_view.tracks.clone(),
            Tab::Playlists => self.playlist_view.tracks.clone(),
            _ => Vec::new(),
        }
    }

    // ---- actions ----

    pub fn handle_action(&mut self, a: Action) {
        self.dirty = true;
        let p = &self.cfg.playback;
        let seek = Duration::from_secs(p.seek_step_secs as u64);
        let seek_large = Duration::from_secs(p.seek_step_large_secs as u64);
        let (volume_step, speed_step) = (p.volume_step as f32, p.speed_step);
        match a {
            Action::Quit => self.should_quit = true,
            Action::Help => self.overlay = Some(Overlay::Help { scroll: 0 }),
            Action::CommandPalette => self.open_command(""),
            Action::Search => self.open_search(""),
            Action::ReloadConfig => match self.reload_config() {
                Ok((msg, warned)) => self.flash(msg, if warned { MsgKind::Warn } else { MsgKind::Ok }),
                Err(e) => self.flash(e, MsgKind::Error),
            },
            Action::Redraw => self.force_clear = true,
            Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Top | Action::Bottom => self.navigate(a),
            Action::Left | Action::Right | Action::FocusNext | Action::FocusPrev | Action::Back => self.focus_move(a),
            Action::Select => self.select(),
            Action::NextTab => self.set_tab(Tab::from_index(self.tab.index() + 1)),
            Action::PrevTab => self.set_tab(Tab::from_index(self.tab.index() + Tab::ALL.len() - 1)),
            Action::TabLibrary => self.set_tab(Tab::Library),
            Action::TabQueue => self.set_tab(Tab::Queue),
            Action::TabPlaylists => self.set_tab(Tab::Playlists),
            Action::TabNowPlaying => self.set_tab(Tab::NowPlaying),
            Action::TabEqualizer => self.set_tab(Tab::Equalizer),
            Action::JumpToCurrent => self.jump_to_current(),
            Action::TogglePause => self.run_quiet(Command::Toggle),
            Action::Stop => self.run_quiet(Command::Stop),
            Action::Next => self.run_quiet(Command::Next),
            Action::Prev => self.run_quiet(Command::Prev),
            Action::SeekForward => self.run_quiet(Command::Seek(SeekTarget::Forward(seek))),
            Action::SeekBackward => self.run_quiet(Command::Seek(SeekTarget::Backward(seek))),
            Action::SeekForwardLarge => self.run_quiet(Command::Seek(SeekTarget::Forward(seek_large))),
            Action::SeekBackwardLarge => self.run_quiet(Command::Seek(SeekTarget::Backward(seek_large))),
            Action::VolumeUp => self.run(Command::Volume(Level::Up(volume_step))),
            Action::VolumeDown => self.run(Command::Volume(Level::Down(volume_step))),
            Action::Mute => self.run(Command::Mute(None)),
            Action::SpeedUp => self.run(Command::Speed(Level::Up(speed_step))),
            Action::SpeedDown => self.run(Command::Speed(Level::Down(speed_step))),
            Action::SpeedReset => self.run(Command::Speed(Level::Reset)),
            Action::ToggleShuffle => self.run(Command::Shuffle(None)),
            Action::CycleRepeat => self.run(Command::Repeat(None)),
            Action::ToggleStopAfter => self.run(Command::StopAfter(None)),
            Action::AbLoop => {
                let arg = match self.ab {
                    (None, _) => LoopArg::A,
                    // back before A there's no B to set: clear rather than fail on every press
                    (Some(a), None) if self.position() <= a => LoopArg::Clear,
                    (Some(_), None) => LoopArg::B,
                    _ => LoopArg::Clear,
                };
                self.run(Command::Loop(arg));
            }
            Action::SleepTimer => {
                let arg = self.next_sleep();
                self.run(Command::Sleep(arg));
            }
            Action::Enqueue => {
                let ids = self.selection();
                self.enqueue(ids, false);
            }
            Action::PlayNext => self.play_next(),
            Action::EnqueueAll => {
                let ids = self.current_list();
                self.enqueue(ids, false);
            }
            Action::Remove => match self.tab {
                Tab::Queue => self.remove_from_queue(),
                Tab::Playlists => self.remove_from_playlist(),
                _ => {}
            },
            Action::MoveUp => self.move_selected(-1),
            Action::MoveDown => self.move_selected(1),
            Action::ClearQueue => {
                if self.queue.is_empty() {
                    self.flash("the queue is already empty", MsgKind::Info);
                } else {
                    let message = format!("Clear the queue ({})?", library::plural(self.queue.len(), "track"));
                    self.overlay = Some(Overlay::Confirm { message, action: ConfirmAction::ClearQueue });
                }
            }
            Action::Radio => {
                let mut seeds = self.selection();
                if seeds.is_empty() {
                    seeds.extend(self.engine.as_ref().and_then(Engine::current));
                }
                match self.start_radio(seeds) {
                    Ok(reply) => self.flash(reply, MsgKind::Info),
                    Err(e) => self.flash(e, MsgKind::Warn),
                }
            }
            Action::ShuffleQueue => {
                if self.queue.len() < 2 {
                    self.flash("nothing to shuffle", MsgKind::Info);
                } else {
                    self.queue.shuffle_now();
                    self.queue_state.select(self.queue.current.or(Some(0)));
                    self.flash("queue shuffled", MsgKind::Info);
                }
            }
            Action::ToggleFavorite if self.tab == Tab::Playlists && self.playlist_view.pane == PlPane::Lists => {
                self.flash("open the playlist to favorite its tracks", MsgKind::Warn);
            }
            Action::ToggleFavorite => {
                let ids = self.selection();
                self.toggle_favorites(&ids);
            }
            Action::AddToPlaylist => {
                let tracks = self.selection();
                self.open_picker(tracks);
            }
            Action::NewPlaylist => self.open_prompt("New playlist", "", PromptPurpose::NewPlaylist(Vec::new())),
            Action::RenamePlaylist => match self.selected_entry() {
                Some(PlaylistEntry::User(i)) => {
                    let name = self.pl_name(i);
                    let path = self.playlists.lists[i].path.clone();
                    self.open_prompt("Rename playlist", &name, PromptPurpose::RenamePlaylist(path));
                }
                _ => self.flash("select one of your playlists first", MsgKind::Warn),
            },
            Action::SaveQueue => {
                if self.queue.is_empty() {
                    self.flash("the queue is empty", MsgKind::Warn);
                } else {
                    self.open_prompt("Save queue as", "", PromptPurpose::SaveQueue);
                }
            }
            Action::CycleSort => {
                let cmd = Command::Sort(self.library_view.sort.next(), Some(self.library_view.sort_desc));
                self.run(cmd);
            }
            Action::ReverseSort => {
                let cmd = Command::Sort(self.library_view.sort, Some(!self.library_view.sort_desc));
                self.run(cmd);
            }
            Action::CycleBrowseMode => self.set_browse_mode(self.library_view.mode.next()),
            Action::Rescan => self.run(Command::Rescan),
            Action::Download => self.open_command("download "),
            Action::CycleVisualizer => self.run(Command::Vis(None)),
            Action::ToggleLyrics => {
                self.show_lyrics = !self.show_lyrics;
                self.flash(format!("lyrics {}", on_off(self.show_lyrics)), MsgKind::Info);
            }
            Action::ToggleArt => {
                self.show_art = !self.show_art;
                self.flash(format!("album art {}", on_off(self.show_art)), MsgKind::Info);
            }
            Action::LyricsOffsetUp => self.run(Command::LyricsOffset(Level::Up(LYRICS_OFFSET_STEP))),
            Action::LyricsOffsetDown => self.run(Command::LyricsOffset(Level::Down(LYRICS_OFFSET_STEP))),
            Action::CycleTheme => self.run(Command::Theme(None)),
            Action::ToggleTimeDisplay => self.time_remaining = !self.time_remaining,
            Action::ToggleCompact => self.compact = !self.compact,
            Action::ToggleMiniVisualizer => {
                self.mini_vis = !self.mini_vis;
                self.flash(format!("mini visualizer {}", on_off(self.mini_vis)), MsgKind::Info);
            }
            Action::EqToggle => self.run(Command::Eq(EqArg::Toggle)),
            Action::EqNextPreset => {
                let name = self.eq_cycle(1);
                self.run(Command::Eq(EqArg::Preset(name)));
            }
            Action::EqPrevPreset => {
                let name = self.eq_cycle(-1);
                self.run(Command::Eq(EqArg::Preset(name)));
            }
            Action::EqReset => self.run(Command::Eq(EqArg::Reset)),
        }
        self.fix_selections();
    }

    /// Run a command line and flash its reply.
    fn run_line(&mut self, line: &str) {
        match command::parse(line) {
            Ok(cmd) => self.run(cmd),
            Err(e) => self.flash(e, MsgKind::Error),
        }
    }

    fn run(&mut self, cmd: Command) {
        match self.exec(cmd) {
            Ok(reply) if !reply.is_empty() => self.flash(reply, MsgKind::Info),
            Ok(_) => {}
            Err(e) => self.flash(e, MsgKind::Error),
        }
    }

    /// For actions whose effect is visible anyway (play/pause, skip, seek): only report problems.
    fn run_quiet(&mut self, cmd: Command) {
        if let Err(e) = self.exec(cmd) {
            self.flash(e, MsgKind::Warn);
        }
    }

    /// Play `ids[start]` (or the whole list) as `playback.on_select` says.
    fn activate(&mut self, ids: Vec<TrackId>, start: Option<usize>) {
        if ids.is_empty() {
            self.flash("nothing to play", MsgKind::Warn);
            return;
        }
        let start = start.filter(|i| *i < ids.len());
        let picked = match start {
            Some(i) => vec![ids[i]],
            None => ids.clone(),
        };
        match self.cfg.playback.on_select {
            PlayContext::List => {
                self.queue.set(ids, start);
                if start.is_none() {
                    self.queue.next();
                }
                self.note_skip();
                self.play_queue_current(Duration::ZERO, false);
            }
            PlayContext::Single => self.play_now(&picked),
            PlayContext::Enqueue => self.enqueue(picked, true),
        }
    }

    /// Play `ids` right now, keeping the queue: they go in after the current track, first one plays.
    fn play_now(&mut self, ids: &[TrackId]) {
        let Some(&first) = ids.first() else { return };
        self.queue.insert_next(ids);
        if self.queue.next() != Some(first) {
            if let Some(i) = self.queue.tracks.iter().position(|t| *t == first) {
                self.queue.jump(i);
            }
        }
        self.note_skip();
        self.play_queue_current(Duration::ZERO, false);
    }

    /// Append to the queue; with `autoplay`, start the first of them if nothing is playing.
    fn enqueue(&mut self, ids: Vec<TrackId>, autoplay: bool) {
        if ids.is_empty() {
            self.flash("nothing selected", MsgKind::Warn);
            return;
        }
        let at = self.queue.len();
        self.queue.push(&ids);
        let what = self.describe(&ids);
        self.flash(format!("queued {what}"), MsgKind::Info);
        if autoplay && self.play_state() == PlayState::Stopped {
            self.queue.jump(at);
            self.play_queue_current(Duration::ZERO, false);
        }
    }

    /// Insert the selection after the current track; on the Queue tab, move the selected row there.
    fn play_next(&mut self) {
        if self.tab == Tab::Queue {
            let Some(i) = self.queue_state.selected().filter(|i| *i < self.queue.len()) else { return };
            if self.queue.current == Some(i) {
                return;
            }
            let id = self.queue.tracks[i];
            self.queue.remove(i);
            self.queue.insert_next(&[id]);
            self.queue_state.select(Some(self.queue.current.map_or(0, |c| c + 1)));
            let what = self.describe(&[id]);
            self.flash(format!("next: {what}"), MsgKind::Info);
            return;
        }
        let ids = self.selection();
        if ids.is_empty() {
            self.flash("nothing selected", MsgKind::Warn);
            return;
        }
        self.queue.insert_next(&ids);
        let what = self.describe(&ids);
        self.flash(format!("next: {what}"), MsgKind::Info);
    }

    fn remove_from_queue(&mut self) {
        let Some(i) = self.queue_state.selected().filter(|i| *i < self.queue.len()) else { return };
        let id = self.queue.tracks[i];
        let state = self.play_state();
        let playing_it = self.queue.current == Some(i) && state != PlayState::Stopped && self.is_current(id);
        let what = self.describe(&[id]);
        self.queue.remove(i);
        if playing_it {
            if self.queue.current_track().is_some() {
                self.play_queue_current(Duration::ZERO, state == PlayState::Paused);
            } else {
                self.stop_playback();
            }
        }
        self.flash(format!("removed {what}"), MsgKind::Info);
    }

    fn remove_from_playlist(&mut self) {
        let Some(entry) = self.selected_entry() else { return };
        let idx = match entry {
            PlaylistEntry::User(idx) => idx,
            PlaylistEntry::Smart(_) => return self.flash("smart playlists are read-only", MsgKind::Warn),
        };
        if self.playlist_view.pane == PlPane::Lists {
            let message = format!("Delete playlist \"{}\"?", self.pl_name(idx));
            let path = self.playlists.lists[idx].path.clone();
            self.overlay = Some(Overlay::Confirm { message, action: ConfirmAction::DeletePlaylist(path) });
            return;
        }
        let pv = &self.playlist_view;
        let Some(sel) = pv.track_state.selected().filter(|s| *s < pv.tracks.len()) else { return };
        let what = self.describe(&[pv.tracks[sel]]);
        let row = pv.rows[sel];
        match self.playlists.remove_track(idx, row) {
            Ok(()) => {
                self.after_playlists_changed(None);
                self.flash(format!("removed {what}"), MsgKind::Info);
            }
            Err(e) => self.flash(e, MsgKind::Error),
        }
    }

    fn move_selected(&mut self, dir: isize) {
        match self.tab {
            Tab::Queue => {
                let Some(i) = self.queue_state.selected() else { return };
                let Some(j) = i.checked_add_signed(dir).filter(|j| *j < self.queue.len()) else { return };
                self.queue.move_item(i, j);
                self.queue_state.select(Some(j));
            }
            Tab::Playlists if self.playlist_view.pane == PlPane::Tracks => {
                let Some(PlaylistEntry::User(idx)) = self.selected_entry() else {
                    return self.flash("smart playlists are read-only", MsgKind::Warn);
                };
                let pv = &self.playlist_view;
                let Some(i) = pv.track_state.selected() else { return };
                let Some(j) = i.checked_add_signed(dir).filter(|j| *j < pv.tracks.len()) else { return };
                let (from, to) = (pv.rows[i], pv.rows[j]);
                match self.playlists.move_track(idx, from, to) {
                    Ok(()) => {
                        self.after_playlists_changed(None);
                        self.playlist_view.track_state.select(Some(j));
                    }
                    Err(e) => self.flash(e, MsgKind::Error),
                }
            }
            _ => {}
        }
    }

    /// Favorite the tracks (or unfavorite them when all already are).
    fn toggle_favorites(&mut self, ids: &[TrackId]) {
        let paths = self.paths_of(ids);
        if paths.is_empty() {
            self.flash("nothing selected", MsgKind::Warn);
            return;
        }
        let on = !paths.iter().all(|p| self.state.is_favorite(p));
        for p in &paths {
            self.state.set_favorite(p, on);
        }
        let what = self.describe(ids);
        self.favorites_changed();
        let msg = if on { format!("{} {what}", self.icons.favorite) } else { format!("unfavorited {what}") };
        self.flash(msg, MsgKind::Ok);
    }

    fn favorites_changed(&mut self) {
        let keep = self.selected_pl_track();
        self.load_playlist_view(keep);
        self.save_state();
    }

    // ---- playback ----

    /// Load the queue's current track. A failure is reported and skips ahead (giving up after
    /// MAX_FAILURES in a row).
    fn play_queue_current(&mut self, start: Duration, paused: bool) {
        let Some(id) = self.queue.current_track() else { return };
        if self.engine.is_none() {
            self.flash("no audio output", MsgKind::Error);
            return;
        }
        self.load_start = start;
        let result = match (self.engine.as_mut(), self.lib.get(id)) {
            (Some(e), Some(t)) => e.load(t, start, paused).map_err(|err| format!("can't play {}: {err}", t.title)),
            _ => Err("track is not in the library".to_string()),
        };
        if let Err(msg) = result {
            self.flash(msg, MsgKind::Error);
            self.skip_failed(paused);
        }
    }

    fn skip_failed(&mut self, paused: bool) {
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.failures = 0;
            self.stop_playback();
            self.flash(format!("stopped: {MAX_FAILURES} tracks in a row failed to play"), MsgKind::Error);
        } else if self.queue.next().is_some() {
            self.play_queue_current(Duration::ZERO, paused);
        } else {
            self.stop_playback();
        }
    }

    fn stop_playback(&mut self) {
        if let Some(e) = self.engine.as_mut() {
            e.stop();
        }
        self.clear_track_view();
    }

    /// Forget the per-track display state (nothing is playing any more).
    fn clear_track_view(&mut self) {
        self.set_ab(None, None);
        self.setup_track = None;
        self.lyrics = None;
        self.lyrics_scroll = 0;
        self.lyrics_manual = None;
        self.art.set_track(None);
        self.cover_accent = None;
    }

    /// The user leaves the playing track before it counted as played: record a skip.
    fn note_skip(&mut self) {
        if self.counted || self.play_state() == PlayState::Stopped {
            return;
        }
        if let Some(path) = self.now_playing().map(|t| t.path.clone()) {
            self.state.record_skip(&path);
            self.state_dirty = true;
        }
        self.counted = true;
    }

    fn need_engine(&self) -> Result<(), String> {
        self.engine.as_ref().map(|_| ()).ok_or_else(|| "no audio output".to_string())
    }

    fn start_playback(&mut self) -> Result<String, String> {
        self.need_engine()?;
        if self.queue.is_empty() {
            return Err("the queue is empty".into());
        }
        if self.queue.current_track().is_none() {
            self.queue.next();
        }
        self.play_queue_current(Duration::ZERO, false);
        Ok(self.playing_reply())
    }

    fn skip_next(&mut self) -> Result<String, String> {
        self.need_engine()?;
        if self.queue.next().is_none() {
            return Err("end of the queue".into());
        }
        self.note_skip();
        self.play_queue_current(Duration::ZERO, false);
        Ok(self.playing_reply())
    }

    fn skip_prev(&mut self) -> Result<String, String> {
        self.need_engine()?;
        let loaded = self.play_state() != PlayState::Stopped;
        if loaded && self.position() > Duration::from_secs(self.cfg.playback.prev_restarts_after_secs as u64) {
            return self.restart();
        }
        if self.queue.prev().is_none() {
            return if loaded { self.restart() } else { Err("start of the queue".into()) };
        }
        self.note_skip();
        self.play_queue_current(Duration::ZERO, false);
        Ok(self.playing_reply())
    }

    fn restart(&mut self) -> Result<String, String> {
        self.seek(SeekTarget::Absolute(Duration::ZERO)).map(|_| "restarted".to_string())
    }

    /// Replace the queue with `ids` and start it (from a random track when shuffling).
    fn replace_queue_and_play(&mut self, ids: Vec<TrackId>) {
        self.queue.set(ids, None);
        self.queue.next();
        self.note_skip();
        self.play_queue_current(Duration::ZERO, false);
    }

    fn seek(&mut self, target: SeekTarget) -> Result<String, String> {
        if self.play_state() == PlayState::Stopped {
            return Err("nothing playing".into());
        }
        let (pos, dur) = (self.position(), self.duration().unwrap_or_default());
        let mut to = match target {
            SeekTarget::Absolute(d) => d,
            SeekTarget::Forward(d) => pos.saturating_add(d),
            SeekTarget::Backward(d) => pos.saturating_sub(d),
            SeekTarget::Percent(p) => dur.mul_f64(if p.is_finite() { (p as f64 / 100.0).clamp(0.0, 1.0) } else { 0.0 }),
        };
        if !dur.is_zero() {
            to = to.min(dur);
        }
        if let Some(e) = self.engine.as_mut() {
            e.seek(to).map_err(|e| format!("can't seek: {e}"))?;
        }
        Ok(format!("{} / {}", fmt_duration(to), fmt_duration(dur)))
    }

    fn set_volume(&mut self, level: Level) -> String {
        let v = match level {
            Level::Set(x) => x,
            Level::Up(x) => self.volume as f32 + x,
            Level::Down(x) => self.volume as f32 - x,
            Level::Reset => self.cfg.playback.volume as f32,
        };
        self.volume = v.round().clamp(0.0, max_volume(&self.cfg) as f32) as u8;
        self.muted = false;
        self.push_volume();
        format!("volume {}%", self.volume)
    }

    fn push_volume(&mut self) {
        if let Some(e) = self.engine.as_mut() {
            e.set_volume(self.volume, self.muted);
        }
    }

    fn set_speed(&mut self, level: Level) -> Result<String, String> {
        let s = match level {
            Level::Set(x) => x,
            Level::Up(x) => self.speed + x,
            Level::Down(x) => self.speed - x,
            Level::Reset => 1.0,
        };
        if !s.is_finite() {
            return Err("invalid speed".into());
        }
        self.speed = (s.clamp(0.25, 4.0) * 100.0).round() / 100.0;
        if let Some(e) = self.engine.as_mut() {
            e.set_speed(self.speed);
        }
        Ok(format!("speed {:.2}x", self.speed))
    }

    fn next_sleep(&self) -> SleepArg {
        match self.sleep {
            None => SleepArg::Minutes(SLEEP_STEPS[0]),
            Some(Sleep::EndOfTrack) => SleepArg::Off,
            Some(Sleep::At(_)) => {
                SLEEP_STEPS.iter().find(|m| **m > self.sleep_minutes).map_or(SleepArg::EndOfTrack, |m| SleepArg::Minutes(*m))
            }
        }
    }

    fn set_sleep(&mut self, arg: SleepArg) -> Result<String, String> {
        self.sleep_minutes = 0;
        match arg {
            SleepArg::Minutes(0) | SleepArg::Off => {
                self.sleep = None;
                Ok("sleep timer off".into())
            }
            SleepArg::Minutes(m) => {
                let at = Instant::now().checked_add(Duration::from_secs(m as u64 * 60)).ok_or("that's too long")?;
                self.sleep = Some(Sleep::At(at));
                self.sleep_minutes = m;
                Ok(format!("sleep in {m} min"))
            }
            SleepArg::EndOfTrack => {
                self.sleep = Some(Sleep::EndOfTrack);
                Ok("sleep at end of track".into())
            }
        }
    }

    fn set_loop(&mut self, arg: LoopArg) -> Result<String, String> {
        if arg != LoopArg::Clear && self.play_state() == PlayState::Stopped {
            return Err("nothing playing".into());
        }
        let pos = self.position();
        match arg {
            LoopArg::A => {
                let b = self.ab.1.filter(|b| *b > pos);
                self.set_ab(Some(pos), b);
                Ok(format!("loop A {}", fmt_duration(pos)))
            }
            LoopArg::B => {
                let a = self.ab.0.unwrap_or(Duration::ZERO);
                if pos <= a {
                    return Err(format!("B must be after A ({})", fmt_duration(a)));
                }
                self.set_ab(Some(a), Some(pos));
                Ok(format!("loop {} - {}", fmt_duration(a), fmt_duration(pos)))
            }
            LoopArg::Clear => {
                self.set_ab(None, None);
                Ok("loop off".into())
            }
        }
    }

    fn set_ab(&mut self, a: Option<Duration>, b: Option<Duration>) {
        if self.ab == (a, b) {
            return;
        }
        self.ab = (a, b);
        if let Some(e) = self.engine.as_mut() {
            e.set_ab_loop(a, b);
        }
    }

    fn effective_lyrics_offset(&self, track: &Track) -> i32 {
        let own = self.state.session.lyrics_offsets.get(&*track.path.to_string_lossy()).copied().unwrap_or(0);
        self.cfg.lyrics.offset_ms.saturating_add(own)
    }

    fn scroll_lyrics(&mut self, delta: isize) {
        let Some(lyrics) = &self.lyrics else { return };
        let height = self.hit.lyrics.map_or(10, |r| r.height.max(1) as usize);
        let base = if self.lyrics_following() {
            lyrics.current(self.position(), self.lyrics_offset_ms).unwrap_or(0).saturating_sub(height / 2)
        } else {
            self.lyrics_scroll as usize
        };
        let max = lyrics.lines.len().saturating_sub(1).min(u16::MAX as usize) as isize;
        let synced = lyrics.synced;
        self.lyrics_scroll = (base as isize).saturating_add(delta).clamp(0, max) as u16;
        if synced {
            self.lyrics_manual = Some(Instant::now());
        }
    }

    // ---- equalizer ----

    fn eq_value(&self, idx: usize) -> f32 {
        if idx == 0 { self.eq.preamp_db } else { self.eq.bands.get(idx - 1).copied().unwrap_or(0.0) }
    }

    /// Set preamp (0) or band (1..=10) and switch the EQ on. Bands matching a preset take its name.
    fn eq_set(&mut self, idx: usize, db: f32) {
        let db = if db.is_finite() { db.clamp(-EQ_MAX_DB, EQ_MAX_DB) } else { 0.0 };
        match idx {
            0 => self.eq.preamp_db = db,
            1..=10 => {
                self.eq.bands[idx - 1] = db;
                self.eq.preset = EQ_PRESETS.iter().find(|(_, b)| *b == self.eq.bands).map_or("custom", |(n, _)| n).to_string();
            }
            _ => return,
        }
        self.eq.enabled = true;
        self.apply_eq();
    }

    fn eq_nudge(&mut self, idx: usize, delta: f32) {
        self.eq_set(idx, self.eq_value(idx) + delta);
    }

    fn apply_eq(&mut self) {
        if let Some(e) = self.engine.as_mut() {
            e.set_eq(&self.eq);
        }
    }

    /// The preset `dir` steps from the current one (custom -> first / last).
    fn eq_cycle(&self, dir: isize) -> String {
        let n = EQ_PRESETS.len() as isize;
        let next = match EQ_PRESETS.iter().position(|(name, _)| *name == self.eq.preset) {
            Some(i) => (i as isize + dir).rem_euclid(n),
            None if dir > 0 => 0,
            None => n - 1,
        };
        EQ_PRESETS[next as usize].0.to_string()
    }

    fn eq_command(&mut self, arg: EqArg) -> Result<String, String> {
        match arg {
            EqArg::On | EqArg::Off | EqArg::Toggle => {
                self.eq.enabled = match arg {
                    EqArg::On => true,
                    EqArg::Off => false,
                    _ => !self.eq.enabled,
                };
                self.apply_eq();
                Ok(format!("eq {}", on_off(self.eq.enabled)))
            }
            EqArg::Preset(name) => {
                let Some((canon, bands)) = EQ_PRESETS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name.trim())) else {
                    let names: Vec<&str> = EQ_PRESETS.iter().map(|(n, _)| *n).collect();
                    return Err(format!("unknown preset \"{name}\" ({})", names.join(", ")));
                };
                self.eq.preset = canon.to_string();
                self.eq.bands = *bands;
                self.eq.enabled = true;
                self.apply_eq();
                Ok(format!("eq preset {canon}"))
            }
            EqArg::Band(n, db) => {
                if !(1..=10).contains(&n) {
                    return Err("band must be 1-10".into());
                }
                self.eq_set(n, db);
                Ok(format!("eq {} {:+.1} dB", band_label(n), self.eq.bands[n - 1]))
            }
            EqArg::Preamp(db) => {
                self.eq_set(0, db);
                Ok(format!("eq preamp {:+.1} dB", self.eq.preamp_db))
            }
            EqArg::Reset => {
                self.eq.bands = [0.0; 10];
                self.eq.preamp_db = 0.0;
                self.eq.preset = "flat".into();
                self.apply_eq();
                Ok("eq reset".into())
            }
        }
    }

    // ---- commands ----

    /// Run a command (palette, key binding, IPC). Ok(reply) is shown/sent; Err(message) is an error.
    pub fn exec(&mut self, cmd: Command) -> Result<String, String> {
        self.dirty = true;
        match cmd {
            Command::Play(None) | Command::Toggle => match (self.play_state(), &cmd) {
                (PlayState::Paused, _) => {
                    self.engine_do(Engine::resume);
                    Ok(self.playing_reply())
                }
                (PlayState::Playing, Command::Toggle) => {
                    self.engine_do(Engine::pause);
                    Ok("paused".into())
                }
                (PlayState::Playing, _) => Ok(self.playing_reply()),
                (PlayState::Stopped, _) => self.start_playback(),
            },
            Command::Play(Some(what)) => self.play_target(&what),
            Command::Pause => {
                if self.play_state() == PlayState::Stopped {
                    return Err("nothing playing".into());
                }
                self.engine_do(Engine::pause);
                Ok("paused".into())
            }
            Command::Stop => {
                self.stop_playback();
                Ok("stopped".into())
            }
            Command::Next => self.skip_next(),
            Command::Prev => self.skip_prev(),
            Command::Seek(target) => self.seek(target),
            Command::Volume(level) => Ok(self.set_volume(level)),
            Command::Mute(on) => {
                self.muted = on.unwrap_or(!self.muted);
                self.push_volume();
                Ok(if self.muted { "muted".into() } else { format!("unmuted, volume {}%", self.volume) })
            }
            Command::Speed(level) => self.set_speed(level),
            Command::Shuffle(on) => {
                let on = on.unwrap_or(!self.queue.shuffle);
                self.queue.set_shuffle(on);
                self.apply_engine_settings(); // replaygain "auto" follows shuffle
                Ok(format!("shuffle {}", on_off(on)))
            }
            Command::Repeat(mode) => {
                self.queue.repeat = mode.unwrap_or(self.queue.repeat.next());
                Ok(format!("repeat {}", self.queue.repeat.label()))
            }
            Command::StopAfter(on) => {
                self.queue.stop_after_current = on.unwrap_or(!self.queue.stop_after_current);
                Ok(format!("stop after current {}", on_off(self.queue.stop_after_current)))
            }
            Command::Sleep(arg) => self.set_sleep(arg),
            Command::Loop(arg) => self.set_loop(arg),
            Command::Eq(arg) => self.eq_command(arg),
            Command::Theme(name) => self.set_theme(name),
            Command::Vis(mode) => {
                self.vis_mode = mode.unwrap_or(self.vis_mode.next());
                Ok(format!("visualizer {}", self.vis_mode.label()))
            }
            Command::Sort(key, desc) => {
                let desc = desc.unwrap_or(self.library_view.sort_desc);
                self.set_sort(key, desc);
                Ok(format!("sort {}{}", key.label(), if desc { " (desc)" } else { "" }))
            }
            Command::View(mode) => {
                self.set_browse_mode(mode);
                Ok(format!("view {}", mode.label().to_lowercase()))
            }
            Command::Goto(tab) => {
                self.set_tab(tab);
                Ok(String::new())
            }
            Command::Add(paths) => self.enqueue_paths(&paths),
            Command::Clear => {
                self.queue.clear();
                self.stop_playback();
                Ok("queue cleared".into())
            }
            Command::Save(name) => self.save_queue_as(&name),
            Command::Load(name) => self.load_playlist(&name),
            Command::PlaylistNew(name) => self.create_playlist(&name, &[]),
            Command::PlaylistDelete(name) => {
                let idx = self.find_playlist(&name)?;
                self.delete_playlist(idx)
            }
            Command::PlaylistRename(old, new) => {
                let idx = self.find_playlist(&old)?;
                self.rename_playlist(idx, &new)
            }
            Command::Search(text) => {
                self.open_search(&text);
                Ok(String::new())
            }
            Command::Rescan => {
                if self.scan.is_some() {
                    return Err("already scanning".into());
                }
                self.scan = Some(library::spawn_scan(self.cfg.library.clone(), self.paths.library_cache.clone(), false));
                self.scan_progress = Some((0, 0));
                Ok("rescanning…".into())
            }
            Command::Radio => {
                let seeds = self.engine.as_ref().and_then(Engine::current).into_iter().collect();
                self.start_radio(seeds)
            }
            Command::Download(url, folder) => self.start_download(&url, folder),
            Command::LyricsOffset(level) => self.set_lyrics_offset(level),
            Command::Favorite(on) => {
                let Some((path, title)) = self.now_playing().map(|t| (t.path.clone(), t.title.clone())) else {
                    return Err("nothing playing".into());
                };
                let on = on.unwrap_or(!self.state.is_favorite(&path));
                self.state.set_favorite(&path, on);
                self.favorites_changed();
                Ok(if on { format!("{} {title}", self.icons.favorite) } else { format!("unfavorited {title}") })
            }
            Command::Status(format) => Ok(self.status(format.as_deref())),
            Command::ReloadConfig => self.reload_config().map(|(msg, _)| msg),
            Command::Help => {
                self.overlay = Some(Overlay::Help { scroll: 0 });
                Ok(String::new())
            }
            Command::Quit => {
                self.should_quit = true;
                Ok(String::new())
            }
        }
    }

    fn engine_do(&mut self, f: fn(&mut Engine)) {
        if let Some(e) = self.engine.as_mut() {
            f(e);
        }
    }

    fn playing_reply(&self) -> String {
        self.now_playing().map_or_else(|| "stopped".into(), |t| format!("playing {} - {}", t.title, t.artist))
    }

    /// "Title - Artist" for one track, "N tracks" otherwise.
    fn describe(&self, ids: &[TrackId]) -> String {
        match ids {
            [id] => self.lib.get(*id).map_or_else(|| "1 track".into(), |t| format!("{} - {}", t.title, t.artist)),
            _ => format!("{} tracks", ids.len()),
        }
    }

    /// `play <x>`: an existing file/folder replaces the queue; otherwise play the best search match.
    fn play_target(&mut self, what: &str) -> Result<String, String> {
        self.need_engine()?;
        let path = absolute(config::expand_tilde(what));
        if path.exists() {
            let ids = self.lib.add_paths(std::slice::from_ref(&path), &self.cfg.library);
            if ids.is_empty() {
                return Err(format!("no playable files in {what}"));
            }
            self.library_changed();
            let what = self.describe(&ids);
            self.replace_queue_and_play(ids);
            if self.play_state() == PlayState::Stopped {
                return Err(format!("couldn't play {what}"));
            }
            return Ok(format!("playing {what}"));
        }
        let hit = self.searcher.search(&self.lib, what, 1).into_iter().next();
        let id = hit.map(|h| h.id).ok_or_else(|| format!("no match for \"{what}\""))?;
        self.play_now(&[id]);
        Ok(self.playing_reply())
    }

    fn enqueue_paths(&mut self, paths: &[String]) -> Result<String, String> {
        let (found, missing): (Vec<PathBuf>, Vec<PathBuf>) =
            paths.iter().map(|p| absolute(config::expand_tilde(p))).partition(|p| p.exists());
        let ids = self.lib.add_paths(&found, &self.cfg.library);
        if ids.is_empty() {
            return Err(match missing.first() {
                Some(p) => format!("no such file or folder: {}", p.display()),
                None => "no playable files found".into(),
            });
        }
        self.library_changed();
        self.queue.push(&ids);
        Ok(format!("added {}", self.describe(&ids)))
    }

    fn set_theme(&mut self, name: Option<String>) -> Result<String, String> {
        let names = theme::names();
        let name = match name.map(|n| n.trim().to_string()) {
            Some(n) if theme::builtin(&n.to_lowercase()).is_some() => n.to_lowercase(),
            Some(n) if n.eq_ignore_ascii_case(&self.cfg.ui.theme) => self.cfg.ui.theme.clone(),
            Some(n) => return Err(format!("unknown theme \"{n}\" ({})", names.join(", "))),
            None => {
                let i = names.iter().position(|n| *n == self.theme.name).map_or(0, |i| i + 1);
                names.get(i % names.len().max(1)).map_or_else(|| self.cfg.ui.theme.clone(), |n| n.to_string())
            }
        };
        let (theme, warnings) = Theme::from_config(&name, &self.cfg.colors);
        self.theme = theme;
        self.refresh_accent();
        self.theme_override = Some(name.clone());
        self.state_dirty = true;
        Ok(match warnings.first() {
            Some(w) => format!("theme {name} ({w})"),
            None => format!("theme {name}"),
        })
    }

    fn set_lyrics_offset(&mut self, level: Level) -> Result<String, String> {
        let Some(track) = self.now_playing().cloned() else { return Err("nothing playing".into()) };
        let key = track.path.to_string_lossy().to_string();
        let offsets = &mut self.state.session.lyrics_offsets;
        let cur = offsets.get(&key).copied().unwrap_or(0) as f32;
        let new = match level {
            Level::Set(v) => v,
            Level::Up(v) => cur + v,
            Level::Down(v) => cur - v,
            Level::Reset => 0.0,
        };
        let new = if new.is_finite() { new.round().clamp(-600_000.0, 600_000.0) as i32 } else { 0 };
        if new == 0 {
            offsets.remove(&key);
        } else {
            offsets.insert(key, new);
        }
        self.lyrics_offset_ms = self.effective_lyrics_offset(&track);
        self.state_dirty = true;
        Ok(format!("lyrics offset {new:+} ms"))
    }

    fn status(&self, format: Option<&str>) -> String {
        let track = self.now_playing();
        match (format, track) {
            (None, None) => format!("stopped, volume {}%", self.volume),
            (format, track) => notify::format(format.unwrap_or(DEFAULT_STATUS), track, &self.status_extras()),
        }
    }

    /// Placeholder values for status / title / notification formats.
    fn status_extras(&self) -> Vec<(&'static str, String)> {
        let state = self.play_state();
        let (pos, dur) = (self.position(), self.duration().unwrap_or_default());
        let (label, icon) = match state {
            PlayState::Playing => ("playing", self.icons.play),
            PlayState::Paused => ("paused", self.icons.pause),
            PlayState::Stopped => ("stopped", self.icons.stop),
        };
        let mut extras = vec![
            ("state", label.to_string()),
            ("icon", icon.to_string()),
            ("position", fmt_duration(pos)),
            ("remaining", fmt_duration(dur.saturating_sub(pos))),
            ("duration", fmt_duration(dur)),
            ("volume", self.volume.to_string()),
            ("muted", on_off(self.muted).to_string()),
            ("speed", format!("{:.2}", self.speed)),
            ("shuffle", on_off(self.queue.shuffle).to_string()),
            ("repeat", self.queue.repeat.label().to_string()),
            ("queue", format!("{}/{}", self.queue.current.map_or(0, |c| c + 1), self.queue.len())),
        ];
        if self.now_playing().is_none() {
            for key in ["title", "artist", "album", "album_artist", "year", "genre", "track", "file"] {
                extras.push((key, String::new()));
            }
        }
        extras
    }

    /// Re-read the config file and apply it live. Ok((reply, had_warnings)).
    fn reload_config(&mut self) -> Result<(String, bool), String> {
        let (new, mut warnings) = Config::load_with_warnings(&self.paths.config_file).map_err(|e| e.to_string())?;
        let old = std::mem::replace(&mut self.cfg, new);
        let theme_name = if self.cfg.ui.theme != old.ui.theme {
            self.theme_override = None;
            self.cfg.ui.theme.clone()
        } else {
            self.theme.name.clone()
        };
        let (theme, w) = Theme::from_config(&theme_name, &self.cfg.colors);
        self.theme = theme;
        warnings.extend(w);
        let (keymap, w) = Keymap::new(&self.cfg.keys);
        self.keymap = keymap;
        warnings.extend(w);
        warnings.dedup();
        let mut seen = std::collections::HashSet::new();
        warnings.retain(|w| seen.insert(w.clone()));
        self.icons = theme::icons(self.cfg.ui.icons);
        // runtime toggles follow the config only where the config itself changed
        let (ui, old_ui) = (&self.cfg.ui, &old.ui);
        if ui.compact != old_ui.compact {
            self.compact = ui.compact;
        }
        if ui.show_art != old_ui.show_art {
            self.show_art = ui.show_art;
        }
        if ui.show_lyrics != old_ui.show_lyrics {
            self.show_lyrics = ui.show_lyrics;
        }
        if ui.mini_visualizer != old_ui.mini_visualizer {
            self.mini_vis = ui.mini_visualizer;
        }
        if ui.time_display != old_ui.time_display {
            self.time_remaining = ui.time_display == TimeDisplay::Remaining;
        }
        if self.cfg.visualizer.mode != old.visualizer.mode {
            self.vis_mode = self.cfg.visualizer.mode;
        }
        if self.cfg.eq != old.eq {
            self.eq = config_eq(&self.cfg);
        }
        let (sort, desc, mode) = (self.cfg.ui.sort, self.cfg.ui.sort_desc, self.cfg.ui.browse_mode);
        if (sort, desc) != (old.ui.sort, old.ui.sort_desc) {
            self.set_sort(sort, desc);
        }
        if mode != old.ui.browse_mode {
            self.set_browse_mode(mode);
        }
        self.volume = self.volume.min(max_volume(&self.cfg));
        if let Some(track) = self.now_playing().cloned() {
            self.lyrics_offset_ms = self.effective_lyrics_offset(&track);
        }
        self.refresh_accent();
        self.apply_engine_settings();
        if self.cfg.ipc != old.ipc {
            // drop the old server first: that removes its socket so the new one can bind
            self.ipc = None;
            self.ipc = bind_ipc(&self.cfg, &self.paths).unwrap_or_else(|e| {
                warnings.push(e);
                None
            });
        }
        if self.cfg.library != old.library && self.scan.is_none() {
            self.scan = Some(library::spawn_scan(self.cfg.library.clone(), self.paths.library_cache.clone(), false));
            self.scan_progress = Some((0, 0));
        }
        let reply = match warnings.as_slice() {
            [] => "config reloaded".to_string(),
            [w] => format!("config reloaded, {w}"),
            [w, rest @ ..] => format!("config reloaded, {w} (+{} more)", rest.len()),
        };
        Ok((reply, !warnings.is_empty()))
    }

    // ---- playlists ----

    fn pl_name(&self, idx: usize) -> String {
        self.playlists.lists.get(idx).map_or_else(String::new, |p| p.name.clone())
    }

    /// The current index of the user playlist saved in `path`.
    fn playlist_at(&self, path: &std::path::Path) -> Result<usize, String> {
        self.playlists.lists.iter().position(|p| p.path == path).ok_or_else(|| "that playlist no longer exists".into())
    }

    fn find_playlist(&self, name: &str) -> Result<usize, String> {
        self.playlists.find(name).ok_or_else(|| format!("no playlist \"{name}\""))
    }

    fn selected_entry(&self) -> Option<PlaylistEntry> {
        self.playlist_view.list_state.selected().and_then(|i| self.playlist_entries().get(i).copied())
    }

    fn create_playlist(&mut self, name: &str, ids: &[TrackId]) -> Result<String, String> {
        let idx = self.playlists.create(name)?;
        let paths = self.paths_of(ids);
        let added = if paths.is_empty() { 0 } else { self.playlists.add(idx, &paths, &self.lib)? };
        let name = self.pl_name(idx);
        self.after_playlists_changed(Some(&name));
        Ok(match added {
            0 => format!("created \"{name}\""),
            1 => format!("created \"{name}\" with {}", self.describe(ids)),
            n => format!("created \"{name}\" with {n} tracks"),
        })
    }

    fn rename_playlist(&mut self, idx: usize, name: &str) -> Result<String, String> {
        let old = self.pl_name(idx);
        self.playlists.rename(idx, name)?;
        self.after_playlists_changed(Some(name));
        Ok(format!("renamed \"{old}\" to \"{name}\""))
    }

    fn delete_playlist(&mut self, idx: usize) -> Result<String, String> {
        let name = self.pl_name(idx);
        self.playlists.delete(idx)?;
        self.playlist_view.track_state = TableState::default();
        self.after_playlists_changed(None);
        Ok(format!("deleted \"{name}\""))
    }

    fn save_queue_as(&mut self, name: &str) -> Result<String, String> {
        if self.queue.is_empty() {
            return Err("the queue is empty".into());
        }
        let paths = self.paths_of(&self.queue.tracks);
        let verb = if self.playlists.find(name).is_some() { "replaced" } else { "saved" };
        let idx = self.playlists.save_as(name, &paths, &self.lib)?;
        let name = self.pl_name(idx);
        self.after_playlists_changed(Some(&name));
        Ok(format!("{verb} \"{name}\" ({})", library::plural(paths.len(), "track")))
    }

    /// Saving the queue over an existing playlist asks first. True when the question is open.
    fn confirm_replace(&mut self, name: &str) -> bool {
        let Some(idx) = self.playlists.find(name).filter(|_| !self.queue.is_empty()) else { return false };
        let message =
            format!("Replace playlist \"{}\" with the queue ({})?", self.pl_name(idx), library::plural(self.queue.len(), "track"));
        self.overlay = Some(Overlay::Confirm { message, action: ConfirmAction::SaveQueueAs(name.to_string()) });
        true
    }

    /// `load <name>`: a user playlist (or a smart playlist by label) replaces the queue and plays.
    fn load_playlist(&mut self, name: &str) -> Result<String, String> {
        let ids: Vec<TrackId> = match Smart::ALL.iter().find(|s| s.label().eq_ignore_ascii_case(name.trim())) {
            Some(kind) => self.smart_tracks(*kind),
            None => {
                let idx = self.find_playlist(name)?;
                let paths = self.playlists.lists[idx].tracks.clone();
                if self.add_outside(&paths) {
                    self.library_changed();
                }
                paths.iter().filter_map(|p| self.lib.find(p)).collect()
            }
        };
        if ids.is_empty() {
            return Err(format!("\"{name}\" has no playable tracks"));
        }
        let n = ids.len();
        self.replace_queue_and_play(ids);
        Ok(format!("loaded \"{name}\" ({})", library::plural(n, "track")))
    }

    /// Reload the playlists pane (selecting playlist `select` if given) and save.
    fn after_playlists_changed(&mut self, select: Option<&str>) {
        if let Some(i) = select.and_then(|name| self.playlists.find(name)) {
            let row = Smart::ALL.len() + i;
            if self.playlist_view.list_state.selected() != Some(row) {
                self.playlist_view.list_state.select(Some(row));
                self.playlist_view.track_state = TableState::default();
            }
        }
        let keep = self.selected_pl_track();
        self.load_playlist_view(keep);
        self.save_state();
    }

    fn selected_pl_track(&self) -> Option<TrackId> {
        let pv = &self.playlist_view;
        pv.track_state.selected().and_then(|i| pv.tracks.get(i)).copied()
    }

    /// Load the tracks of the selected playlist entry, keeping `keep` (else the row index) selected.
    fn load_playlist_view(&mut self, keep: Option<TrackId>) {
        let entries = self.playlist_entries();
        let sel = self.playlist_view.list_state.selected().unwrap_or(0).min(entries.len().saturating_sub(1));
        self.playlist_view.list_state.select((!entries.is_empty()).then_some(sel));
        let (mut tracks, mut rows, mut missing) = (Vec::new(), Vec::new(), 0);
        match entries.get(sel) {
            Some(PlaylistEntry::Smart(kind)) => {
                tracks = self.smart_tracks(*kind);
                rows = (0..tracks.len()).collect();
            }
            Some(PlaylistEntry::User(i)) => {
                let paths = self.playlists.lists.get(*i).map(|p| p.tracks.clone()).unwrap_or_default();
                if self.add_outside(&paths) {
                    self.library_changed();
                }
                for (row, path) in paths.iter().enumerate() {
                    match self.lib.find(path) {
                        Some(id) => {
                            tracks.push(id);
                            rows.push(row);
                        }
                        None => missing += 1,
                    }
                }
            }
            None => {}
        }
        let pv = &mut self.playlist_view;
        let i = keep.and_then(|id| tracks.iter().position(|t| *t == id)).or(pv.track_state.selected()).unwrap_or(0);
        pv.track_state.select((!tracks.is_empty()).then(|| i.min(tracks.len() - 1)));
        pv.tracks = tracks;
        pv.rows = rows;
        pv.missing = missing;
    }

    // ---- library view ----

    fn selected_lib_track(&self) -> Option<TrackId> {
        let v = &self.library_view;
        v.track_state.selected().and_then(|i| v.tracks.get(i)).copied()
    }

    fn set_browse_mode(&mut self, mode: BrowseMode) {
        let keep = self.selected_lib_track();
        self.library_view.mode = mode;
        self.rebuild_library_view(keep);
    }

    fn set_sort(&mut self, key: SortKey, desc: bool) {
        let keep = self.selected_lib_track();
        self.library_view.sort = key;
        self.library_view.sort_desc = desc;
        self.load_group_tracks(keep);
    }

    /// Tracks were added to the library: regroup, keeping the selection.
    fn library_changed(&mut self) {
        let keep = self.selected_lib_track();
        self.rebuild_library_view(keep);
    }

    /// Recompute the groups of the current mode and select the group holding `keep` (else the one
    /// with the previous group's name, else the first).
    fn rebuild_library_view(&mut self, keep: Option<TrackId>) {
        let v = &mut self.library_view;
        let prev = v.group_state.selected().and_then(|i| v.groups.get(i)).map(|g| g.name.clone());
        v.mode_state.select(BrowseMode::ALL.iter().position(|m| *m == v.mode));
        v.groups = self.lib.groups(v.mode);
        let g = keep
            .and_then(|id| v.groups.iter().position(|g| g.tracks.contains(&id)))
            .or_else(|| prev.and_then(|name| v.groups.iter().position(|g| g.name == name)))
            .unwrap_or(0);
        v.group_state = ListState::default().with_selected((!v.groups.is_empty()).then_some(g));
        self.load_group_tracks(keep);
    }

    /// Load and sort the selected group's tracks, selecting `keep` (else the first).
    fn load_group_tracks(&mut self, keep: Option<TrackId>) {
        let v = &self.library_view;
        let mut tracks = v.group_state.selected().and_then(|g| v.groups.get(g)).map(|g| g.tracks.clone()).unwrap_or_default();
        let (lib, state) = (&self.lib, &self.state);
        lib.sort(&mut tracks, v.sort, v.sort_desc, &|id| lib.get(id).map_or(0, |t| state.plays(&t.path)));
        let i = keep.and_then(|id| tracks.iter().position(|t| *t == id)).unwrap_or(0);
        let v = &mut self.library_view;
        v.track_state = TableState::default().with_selected((!tracks.is_empty()).then_some(i));
        v.tracks = tracks;
    }

    // ---- scanning and the session ----

    fn on_scan_done(&mut self, scanned: Library) {
        self.scan = None;
        self.scan_progress = None;
        let old = std::mem::replace(&mut self.lib, Library::empty());
        self.lib = self.adopt_library(scanned, &old);
        let lib = &self.lib;
        let map = |id: TrackId| old.get(id).and_then(|t| lib.find(&t.path));
        let keep_lib = self.selected_lib_track().and_then(map);
        let keep_pl = self.selected_pl_track().and_then(map);
        self.queue.remap(&map);
        self.setup_track = self.setup_track.and_then(map);
        if let Some(
            Overlay::PickPlaylist { tracks, .. } | Overlay::Prompt { purpose: PromptPurpose::NewPlaylist(tracks), .. },
        ) = &mut self.overlay
        {
            *tracks = tracks.iter().filter_map(|t| map(*t)).collect();
        }
        self.preload_failed = None;
        self.last_click = None;
        // don't hide a startup warning (config, keys, audio) behind the track count
        if !self.message.as_ref().is_some_and(|m| matches!(m.kind, MsgKind::Warn | MsgKind::Error)) {
            self.flash(library::plural(self.lib.len(), "track"), MsgKind::Ok);
        }
        self.fix_engine_ids(&old);
        if !self.scanned {
            self.scanned = true;
            if !self.cli_paths.is_empty() {
                self.play_cli_paths();
            } else if self.resume && self.queue.is_empty() && self.play_state() == PlayState::Stopped {
                self.restore_queue();
            }
            // forget play counts of files that are gone, so state.json doesn't grow forever
            // (prune keeps files that still exist outside the library, and whole missing folders)
            let lib = &self.lib;
            if self.state.prune(|p| lib.find(p).is_some() || p.exists()) > 0 {
                self.state_dirty = true;
            }
        }
        self.rebuild_library_view(keep_lib);
        self.load_playlist_view(keep_pl);
        if matches!(self.overlay, Some(Overlay::Search { .. })) {
            self.refresh_overlay();
        }
        self.start_analysis();
    }

    // ---- radio, downloads, media keys ----

    /// Fingerprint, in the background, the tracks the radio hasn't heard yet.
    fn start_analysis(&mut self) {
        let Some(f) = self.features.as_ref().filter(|_| self.analysis.is_none()) else { return };
        let todo: Vec<PathBuf> = self.lib.tracks.iter().map(|t| t.path.clone()).filter(|p| !f.has(p)).collect();
        if !todo.is_empty() {
            self.analysis_progress = Some((0, todo.len()));
            self.analysis = Some(radio::spawn_analyze(todo));
        }
    }

    fn drain_analysis(&mut self) {
        let (Some(rx), Some(features), Some((done, _))) = (&self.analysis, &mut self.features, &mut self.analysis_progress) else {
            return;
        };
        let before = *done;
        let finished = loop {
            match rx.try_recv() {
                Ok((path, v)) => {
                    features.insert(&path, v);
                    *done += 1;
                }
                Err(e) => break e == TryRecvError::Disconnected,
            }
        };
        if *done == before && !finished {
            return;
        }
        // checkpoints, so a long first analysis survives being killed
        if finished || before / 200 != *done / 200 {
            let _ = features.save(&self.paths.cache_dir.join(RADIO_FILE));
        }
        if finished {
            self.analysis = None;
            self.analysis_progress = None;
        }
        self.dirty = true;
    }

    /// Up to `k` tracks that sound most like `seeds`, nearest first.
    fn similar(&self, seeds: &[TrackId], k: usize) -> Vec<TrackId> {
        let Some(f) = &self.features else { return Vec::new() };
        let pool: Vec<(TrackId, &[f32])> = self.lib.tracks.iter().filter_map(|t| Some((t.id, f.get(&t.path)?))).collect();
        radio::recommend(&pool, seeds, k)
    }

    fn smart_tracks(&self, kind: Smart) -> Vec<TrackId> {
        match kind {
            // what sounds like the last few tracks played and the most played ones
            Smart::Radio => {
                let mut seeds = playlist::smart_tracks(Smart::RecentlyPlayed, &self.lib, &self.state, 4);
                seeds.extend(playlist::smart_tracks(Smart::MostPlayed, &self.lib, &self.state, 2));
                self.similar(&seeds, RADIO_LEN)
            }
            kind => playlist::smart_tracks(kind, &self.lib, &self.state, 0),
        }
    }

    /// Queue the first seed, then what sounds most like all of them, and play (a seed that is
    /// already playing carries on).
    fn start_radio(&mut self, seeds: Vec<TrackId>) -> Result<String, String> {
        self.need_engine()?;
        let Some(&first) = seeds.first() else { return Err("radio: select or play a track first".into()) };
        let similar = self.similar(&seeds, RADIO_LEN);
        if similar.is_empty() {
            return Err(match self.analysis_progress {
                Some((done, total)) => format!("radio: still listening to your library ({done}/{total})"),
                None => "radio: that audio couldn't be analysed".into(),
            });
        }
        let reply = format!("radio: {} like {}", library::plural(similar.len(), "track"), self.describe(&seeds));
        let ids = std::iter::once(first).chain(similar).collect();
        if self.is_current(first) && self.play_state() != PlayState::Stopped {
            self.queue.set(ids, Some(0));
        } else {
            self.replace_queue_and_play(ids);
        }
        Ok(reply)
    }

    fn start_download(&mut self, url: &str, folder: Option<String>) -> Result<String, String> {
        if self.download.is_some() {
            return Err("a download is already running".into());
        }
        if !download::available() {
            return Err("download needs yt-dlp on your PATH (https://github.com/yt-dlp/yt-dlp)".into());
        }
        let root = self.cfg.music_dirs().into_iter().next().ok_or("no music folder to download into, see [library] dirs")?;
        let folder = folder.unwrap_or_else(|| "Downloads".into());
        if folder.contains(['/', '\\']) || folder == ".." {
            return Err(format!("download: \"{folder}\" must be a folder name, not a path"));
        }
        let into = root.join(&folder).display().to_string();
        self.download = Some(download::spawn(root, folder, url.into()));
        self.download_progress = Some((0, 0));
        Ok(format!("downloading into {into}…"))
    }

    fn drain_download(&mut self) {
        let Some(rx) = &self.download else { return };
        let result = loop {
            match rx.try_recv() {
                Ok(DownloadMsg::Progress(done, total)) => self.download_progress = Some((done, total)),
                Ok(DownloadMsg::Done(r)) => break r,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => break Err("it stopped unexpectedly".into()),
            }
            self.dirty = true;
        };
        self.download = None;
        self.download_progress = None;
        match result {
            Ok(()) => {
                let msg = match self.exec(Command::Rescan) {
                    Ok(_) => "download finished, adding it to the library…",
                    Err(_) => "download finished; rescan once this scan is done",
                };
                self.flash(msg, MsgKind::Ok);
            }
            Err(e) => self.flash(format!("download failed: {e}"), MsgKind::Error),
        }
    }

    fn serve_media(&mut self) {
        let Some(commands) = self.media.as_mut().map(Media::poll) else { return };
        for cmd in commands {
            self.run_quiet(cmd);
        }
    }

    /// Keep the system Now Playing panel in step: on a new track or play state, and after a seek
    /// (the position straying from where the panel's own clock has it).
    fn sync_media(&mut self) {
        if self.media.is_none() {
            return;
        }
        let (id, state, pos) = (self.engine.as_ref().and_then(Engine::current), self.play_state(), self.position());
        let in_step = self.media_sent.is_some_and(|(sent_id, sent_state, sent_pos, at)| {
            let expected = if sent_state == PlayState::Playing { sent_pos + at.elapsed().mul_f32(self.speed) } else { sent_pos };
            (sent_id, sent_state) == (id, state) && pos.abs_diff(expected) < Duration::from_secs(2)
        });
        if in_step {
            return;
        }
        self.media_sent = Some((id, state, pos, Instant::now()));
        let track = self.now_playing().filter(|_| state != PlayState::Stopped).map(|t| (t.title.clone(), t.artist.clone(), t.album.clone()));
        let duration = self.duration();
        if let Some(media) = self.media.as_mut() {
            media.publish(track.as_ref().map(|(t, a, al)| (t.as_str(), a.as_str(), al.as_str())), state == PlayState::Paused, pos, duration);
        }
    }

    /// Build the new library so playback continues: tracks the queue or engine still use that the
    /// scan didn't find (added from outside the music folders) are kept, and the playing / preloaded
    /// tracks keep their ids (the engine refers to them by id).
    fn adopt_library(&self, scanned: Library, old: &Library) -> Library {
        let engine_ids: Vec<TrackId> =
            self.engine.as_ref().map(|e| [e.current(), e.preloaded()].into_iter().flatten().collect()).unwrap_or_default();
        let mut extra: Vec<Track> = Vec::new();
        for t in self.queue.tracks.iter().chain(&engine_ids).filter_map(|id| old.get(*id)) {
            if scanned.find(&t.path).is_none() && !extra.iter().any(|e| e.path == t.path) && t.path.exists() {
                extra.push(t.clone());
            }
        }
        let same_id = |id: &TrackId| old.get(*id).map(|t| &t.path) == scanned.get(*id).map(|t| &t.path);
        if extra.is_empty() && engine_ids.iter().all(same_id) {
            return scanned;
        }
        let roots = scanned.roots;
        let mut tracks = scanned.tracks;
        tracks.extend(extra);
        for id in engine_ids {
            let Some(path) = old.get(id).map(|t| &t.path) else { continue };
            if let Some(j) = tracks.iter().position(|t| &t.path == path).filter(|_| id < tracks.len()) {
                tracks.swap(j, id);
            }
        }
        Library::from_tracks(roots, tracks)
    }

    /// After a rescan: if the engine's ids no longer point at the same files, fix that.
    fn fix_engine_ids(&mut self, old: &Library) {
        let same = |id: TrackId, lib: &Library| old.get(id).map(|t| &t.path) == lib.get(id).map(|t| &t.path);
        let Some(engine) = self.engine.as_mut() else { return };
        if engine.preloaded().is_some_and(|p| !same(p, &self.lib)) {
            engine.cancel_preload();
        }
        let Some(cur) = engine.current().filter(|c| !same(*c, &self.lib)) else { return };
        let (pos, paused) = (engine.position(), engine.state() == PlayState::Paused);
        match old.get(cur).and_then(|t| self.lib.find(&t.path)).and_then(|id| self.lib.get(id)) {
            // ponytail: reloads (brief gap) in the rare case the id couldn't be kept; an
            // Engine::remap would avoid it.
            Some(track) => {
                self.load_start = pos;
                if let Err(e) = engine.load(track, pos, paused) {
                    self.flash(format!("can't continue {}: {e}", track.title), MsgKind::Error);
                }
            }
            None => {
                self.stop_playback();
                self.flash("the playing file is gone", MsgKind::Warn);
            }
        }
    }

    fn play_cli_paths(&mut self) {
        let paths: Vec<PathBuf> = std::mem::take(&mut self.cli_paths).into_iter().map(absolute).collect();
        let ids = self.lib.add_paths(&paths, &self.cfg.library);
        if ids.is_empty() {
            self.flash("nothing playable in the given paths", MsgKind::Warn);
            return;
        }
        self.replace_queue_and_play(ids);
    }

    /// Read files that exist but aren't in the library (played or added from outside the music
    /// folders) so a saved queue or playlist can use them. Only once the first scan is in: it
    /// finds everything else. True when tracks were added.
    /// ponytail: reads on the UI thread, fine for a few files; a playlist of thousands of tracks
    /// outside the music folders would stall its first view (read them in the scan thread then).
    fn add_outside(&mut self, paths: &[PathBuf]) -> bool {
        if !self.scanned {
            return false;
        }
        let outside: Vec<PathBuf> = paths.iter().filter(|p| self.lib.find(p).is_none() && p.is_file()).cloned().collect();
        !outside.is_empty() && !self.lib.add_paths(&outside, &self.cfg.library).is_empty()
    }

    /// Saved queue paths -> ids (missing files dropped); the current track resumes where it was.
    fn restore_queue(&mut self) {
        let saved = self.state.session.queue.clone();
        if self.add_outside(&saved) {
            self.library_changed();
        }
        let s = &self.state.session;
        let mut ids = Vec::with_capacity(s.queue.len());
        // saved queue index -> index in `ids` (None: the file is gone)
        let mut new_index = Vec::with_capacity(s.queue.len());
        let mut current = None;
        for (i, path) in s.queue.iter().enumerate() {
            if s.current == Some(i) {
                // the saved current track, or the next one that still exists
                current = Some(ids.len());
            }
            let found = self.lib.find(path);
            new_index.push(found.map(|_| ids.len()));
            ids.extend(found);
        }
        let order: Vec<usize> = s.order.iter().filter_map(|&i| new_index.get(i).copied().flatten()).collect();
        let exact = s.current.and_then(|i| s.queue.get(i)).is_some_and(|p| self.lib.find(p).is_some());
        let start = if exact && self.cfg.playback.resume_position {
            Duration::try_from_secs_f64(s.position_secs).unwrap_or_default()
        } else {
            Duration::ZERO
        };
        if ids.is_empty() {
            return;
        }
        let current = current.filter(|c| *c < ids.len());
        self.queue.set(ids, current);
        self.queue.restore_order(order);
        if self.queue.current_track().is_some() {
            self.play_queue_current(start, !self.cfg.playback.autoplay);
        }
    }

    /// Copy the live session into `state.session`.
    fn snapshot_session(&mut self) {
        let playing_current = self.queue.current_track().is_some_and(|id| self.is_current(id));
        let position = if playing_current { self.position().as_secs_f64() } else { 0.0 };
        let queue: Vec<PathBuf> = self.paths_of(&self.queue.tracks);
        let order = if self.queue.shuffle { self.queue.play_order() } else { Vec::new() };
        let s = &mut self.state.session;
        // before the first scan the queue isn't restored yet: keep the saved one
        if self.scanned {
            s.queue = queue;
            s.current = self.queue.current;
            s.position_secs = position;
            s.order = order;
        }
        s.shuffle = Some(self.queue.shuffle);
        s.repeat = Some(self.queue.repeat);
        s.volume = Some(self.volume);
        s.muted = self.muted;
        s.speed = self.speed;
        // the EQ and view settings only where they differ from the config, so config edits apply
        s.eq = (self.eq != self.cfg.eq).then(|| self.eq.clone());
        let ui = &self.cfg.ui;
        let differs = |now: bool, cfg: bool| (now != cfg).then_some(now);
        s.tab = (self.tab != ui.default_tab).then_some(self.tab);
        s.browse_mode = (self.library_view.mode != ui.browse_mode).then_some(self.library_view.mode);
        s.theme = self.theme_override.clone();
        s.theme_base = self.theme_override.as_ref().map(|_| ui.theme.clone());
        s.vis_mode = (self.vis_mode != self.cfg.visualizer.mode).then_some(self.vis_mode);
        s.command_history = self.command_history.clone();
        s.compact = differs(self.compact, ui.compact);
        s.show_art = differs(self.show_art, ui.show_art);
        s.show_lyrics = differs(self.show_lyrics, ui.show_lyrics);
        s.mini_visualizer = differs(self.mini_vis, ui.mini_visualizer);
        s.time_remaining = differs(self.time_remaining, ui.time_display == TimeDisplay::Remaining);
    }

    fn save_state(&mut self) {
        self.snapshot_session();
        if let Err(e) = self.state.save(&self.paths.state_file) {
            self.flash(format!("can't save state: {e}"), MsgKind::Warn);
        }
        self.state_dirty = false;
        self.last_save = Instant::now();
    }

    // ---- read-only helpers for the UI ----

    pub fn now_playing(&self) -> Option<&Track> {
        self.engine.as_ref().and_then(|e| e.current()).and_then(|id| self.lib.get(id))
    }

    pub fn play_state(&self) -> PlayState {
        self.engine.as_ref().map(|e| e.state()).unwrap_or_default()
    }

    pub fn position(&self) -> Duration {
        self.engine.as_ref().map(|e| e.position()).unwrap_or_default()
    }

    pub fn duration(&self) -> Option<Duration> {
        self.engine.as_ref().and_then(|e| e.duration()).or_else(|| self.now_playing().map(|t| t.duration))
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.lib.get(id)
    }

    pub fn is_current(&self, id: TrackId) -> bool {
        self.engine.as_ref().and_then(|e| e.current()) == Some(id)
    }

    pub fn plays(&self, id: TrackId) -> u32 {
        self.lib.get(id).map(|t| self.state.plays(&t.path)).unwrap_or(0)
    }

    pub fn is_favorite(&self, id: TrackId) -> bool {
        self.lib.get(id).is_some_and(|t| self.state.is_favorite(&t.path))
    }

    /// Smart playlists first, then user playlists.
    pub fn playlist_entries(&self) -> Vec<PlaylistEntry> {
        Smart::ALL.iter().map(|s| PlaylistEntry::Smart(*s)).chain((0..self.playlists.lists.len()).map(PlaylistEntry::User)).collect()
    }

    pub fn entry_label(&self, e: PlaylistEntry) -> String {
        match e {
            PlaylistEntry::Smart(s) => s.label().to_string(),
            PlaylistEntry::User(i) => self.playlists.lists.get(i).map(|p| p.name.clone()).unwrap_or_default(),
        }
    }

    /// The cover color for ui.dynamic_accent, darkened to stay readable on a light theme.
    fn refresh_accent(&mut self) {
        self.cover_accent = self.art.dominant_color().filter(|_| self.cfg.ui.dynamic_accent).map(|c| crate::art::readable_on(c, self.theme.bg));
    }

    /// The accent color to use (cover color when dynamic accent is on).
    pub fn accent(&self) -> Color {
        self.cover_accent.filter(|_| self.cfg.ui.dynamic_accent).unwrap_or(self.theme.accent)
    }

    /// Paths of `ids` (for playlists / persistence).
    pub fn paths_of(&self, ids: &[TrackId]) -> Vec<PathBuf> {
        ids.iter().filter_map(|i| self.lib.get(*i)).map(|t| t.path.clone()).collect()
    }

    /// True when the lyrics panel should follow playback (synced lyrics and no manual scroll in the
    /// last few seconds); otherwise it shows the lyrics scrolled to `lyrics_scroll`.
    pub fn lyrics_following(&self) -> bool {
        self.lyrics.as_ref().is_some_and(|l| l.synced) && self.lyrics_manual.is_none()
    }
}

/// The IPC server for the configured socket (None when disabled). Err(warning) if it can't bind.
fn bind_ipc(cfg: &Config, paths: &Paths) -> Result<Option<IpcServer>, String> {
    if !cfg.ipc.enabled {
        return Ok(None);
    }
    let socket = cfg.ipc.socket.as_deref().map(config::expand_tilde).unwrap_or_else(|| paths.socket.clone());
    IpcServer::bind(&socket).map(Some).map_err(|e| format!("remote control off ({}: {e})", socket.display()))
}

/// `[eq]` from the config with a named preset's bands filled in ("custom" keeps `bands`).
fn config_eq(cfg: &Config) -> EqSettings {
    let mut eq = cfg.eq.clone();
    if let Some(bands) = crate::dsp::eq_preset(&eq.preset) {
        eq.bands = bands;
    }
    eq
}

/// The engine takes 0..=150 %.
fn max_volume(cfg: &Config) -> u8 {
    cfg.playback.max_volume.clamp(1, 150)
}

fn absolute(p: PathBuf) -> PathBuf {
    std::path::absolute(&p).unwrap_or(p)
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// "31 Hz", "1 kHz" for EQ band 1..=10.
fn band_label(band: usize) -> String {
    let f = EQ_FREQS[band - 1];
    if f >= 1000.0 { format!("{} kHz", f / 1000.0) } else { format!("{f} Hz") }
}

/// Rows moved by a navigation action (isize::MIN / MAX jump to the ends).
fn nav_delta(a: Action, page: isize) -> isize {
    match a {
        Action::Up => -1,
        Action::Down => 1,
        Action::PageUp => -page,
        Action::PageDown => page,
        Action::Top => isize::MIN,
        Action::Bottom => isize::MAX,
        _ => 0,
    }
}

/// The selection after moving `delta` rows in a list of `len` (clamped); None for an empty list.
fn moved(sel: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    (len > 0).then(|| (sel.unwrap_or(0) as isize).saturating_add(delta).clamp(0, len as isize - 1) as usize)
}

/// Pane/band index after a step, clamped or wrapping.
fn step(i: usize, n: usize, delta: isize, wrap: bool) -> usize {
    let j = i as isize + delta;
    if wrap { j.rem_euclid(n as isize) as usize } else { j.clamp(0, n as isize - 1) as usize }
}

fn clamp_selection(sel: &mut Option<usize>, len: usize) {
    *sel = (len > 0).then(|| sel.unwrap_or(0).min(len - 1));
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::playlist::Playlist;
    use crate::queue::Repeat;

    const FIXTURES: usize = 12;
    const TRACK_SECS: u32 = 10;

    /// Silent 8 kHz mono WAVs shared by all tests, so a real engine can decode the fake tracks.
    fn fixture_dir() -> &'static Path {
        static DIR: OnceLock<PathBuf> = OnceLock::new();
        DIR.get_or_init(|| {
            let dir = std::env::temp_dir().join("orbit-app-test-fixtures");
            std::fs::create_dir_all(&dir).unwrap();
            let (rate, data) = (8000u32, 8000 * TRACK_SECS * 2);
            let mut wav = Vec::new();
            for part in [&b"RIFF"[..], &(36 + data).to_le_bytes(), b"WAVEfmt ", &16u32.to_le_bytes(), &1u16.to_le_bytes()] {
                wav.extend_from_slice(part);
            }
            for part in [&1u16.to_le_bytes()[..], &rate.to_le_bytes(), &(rate * 2).to_le_bytes(), &2u16.to_le_bytes()] {
                wav.extend_from_slice(part);
            }
            for part in [&16u16.to_le_bytes()[..], b"data", &data.to_le_bytes()] {
                wav.extend_from_slice(part);
            }
            wav.resize(44 + data as usize, 0);
            for i in 0..FIXTURES {
                let path = dir.join(format!("track{i:02}.wav"));
                if !path.exists() {
                    let tmp = dir.join(format!("track{i:02}.{}.tmp", std::process::id()));
                    std::fs::write(&tmp, &wav).unwrap();
                    std::fs::rename(&tmp, &path).unwrap();
                }
            }
            dir
        })
    }

    fn fake_tracks(n: usize) -> Vec<Track> {
        (0..n)
            .map(|i| {
                let mut t = Track::default();
                t.path = fixture_dir().join(format!("track{i:02}.wav"));
                t.title = format!("Song {i:02}");
                t.artist = ["Alpha", "Beta", "Gamma"][i % 3].to_string();
                t.album_artist = t.artist.clone();
                t.album = format!("Album {}", i / 4);
                t.folder = if i < n / 2 { "A" } else { "B" }.to_string();
                t.track_no = Some(i as u32 % 4 + 1);
                t.duration = Duration::from_secs(TRACK_SECS as u64);
                t.format = "WAV".into();
                t
            })
            .collect()
    }

    /// Every file location inside a fresh temp dir (never the user's real files).
    fn test_paths() -> Paths {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("orbit-app-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let mut p = Paths::new(Some(dir.join("config.toml")));
        p.config_dir = dir.clone();
        p.data_dir = dir.clone();
        p.cache_dir = dir.clone();
        p.state_file = dir.join("state.json");
        p.playlists_dir = dir.join("playlists");
        p.library_cache = dir.join("library.json");
        p.socket = dir.join("orbit.sock");
        p
    }

    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.library.dirs.clear();
        cfg.ipc.enabled = false;
        cfg.ui.terminal_title = false;
        cfg.notifications.enabled = false;
        cfg.playback.resume_session = false;
        cfg.playback.gapless = false;
        cfg.playback.volume = 0;
        cfg
    }

    /// An app over `n` fake tracks whose first scan has finished; `engine` = with the playback engine.
    fn app_with(n: usize, engine: bool) -> App {
        app_cfg(test_config(), n, engine)
    }

    fn app_cfg(cfg: Config, n: usize, engine: bool) -> App {
        let engine = if engine { Engine::new(&cfg.playback).ok() } else { None };
        let mut app = App::build(cfg, test_paths(), &crate::Cli::default(), engine);
        app.scan = None;
        app.on_scan_done(Library::from_tracks(vec![fixture_dir().to_path_buf()], fake_tracks(n)));
        app.message = None;
        app
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn typ(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_event(key(KeyCode::Char(c)));
        }
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE })
    }

    fn click(app: &mut App, x: u16, y: u16) {
        app.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        app.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    }

    fn msg(app: &App) -> String {
        app.message.as_ref().map(|m| m.text.clone()).unwrap_or_default()
    }

    fn playing(app: &App) -> Option<TrackId> {
        app.engine.as_ref().and_then(|e| e.current())
    }

    fn ok(app: &mut App, cmd: Command) -> String {
        app.exec(cmd).unwrap()
    }

    fn input_text(app: &App) -> String {
        match &app.overlay {
            Some(Overlay::Search { input, .. } | Overlay::Command { input, .. } | Overlay::Prompt { input, .. }) => input.text.clone(),
            _ => panic!("no text overlay open"),
        }
    }

    #[test]
    fn input_editing() {
        let press = |i: &mut Input, code, mods| i.handle_key(&KeyEvent::new(code, mods));
        let mut i = Input::with("héllo 世界");
        assert_eq!(i.cursor, 8);
        press(&mut i, KeyCode::Left, KeyModifiers::NONE);
        press(&mut i, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(i.text, "héllo 界");
        press(&mut i, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut i, KeyCode::Char('X'), KeyModifiers::SHIFT);
        assert_eq!((i.text.as_str(), i.cursor), ("Xhéllo 界", 1));
        press(&mut i, KeyCode::Char('e'), KeyModifiers::CONTROL);
        press(&mut i, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!((i.text.as_str(), i.cursor), ("Xhéllo ", 7));
        press(&mut i, KeyCode::Home, KeyModifiers::NONE);
        press(&mut i, KeyCode::Right, KeyModifiers::NONE);
        press(&mut i, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(i.text, "X");
        press(&mut i, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!((i.text.as_str(), i.cursor), ("", 0));
        assert!(!press(&mut i, KeyCode::Char('x'), KeyModifiers::ALT));
        i.insert_str("a\nb");
        assert_eq!((i.text.as_str(), i.cursor), ("a b", 3));
        press(&mut i, KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(i.text, "a ");
    }

    #[test]
    fn library_panes_and_navigation() {
        let mut app = app_with(8, false);
        let n = app.library_view.tracks.len();
        assert!(n > 0);
        assert_eq!(app.library_view.pane, LibPane::Tracks);
        app.handle_action(Action::Back);
        assert_eq!(app.library_view.pane, LibPane::Groups);
        app.handle_action(Action::Left);
        app.handle_action(Action::Left);
        assert_eq!(app.library_view.pane, LibPane::Modes);
        app.handle_action(Action::FocusPrev);
        assert_eq!(app.library_view.pane, LibPane::Tracks);
        app.handle_action(Action::Right);
        assert_eq!(app.library_view.pane, LibPane::Tracks);

        app.handle_action(Action::Down);
        assert_eq!(app.library_view.track_state.selected(), Some(1.min(n - 1)));
        app.handle_action(Action::Bottom);
        assert_eq!(app.library_view.track_state.selected(), Some(n - 1));
        app.handle_action(Action::Top);
        assert_eq!(app.library_view.track_state.selected(), Some(0));
        app.hit.lists = vec![ListHit { area: Rect::new(0, 0, 40, 3), target: ListTarget::LibTracks, offset: 0, len: n }];
        app.handle_action(Action::PageDown);
        assert_eq!(app.library_view.track_state.selected(), Some(3.min(n - 1)));

        // moving in the modes pane switches the browse mode, keeping the selected track in view
        let keep = app.selected_lib_track().unwrap();
        app.library_view.pane = LibPane::Modes;
        app.handle_action(Action::Down);
        assert_eq!(app.library_view.mode, BrowseMode::Artists);
        assert!(!app.library_view.groups.is_empty());
        assert_eq!(app.selected_lib_track(), Some(keep));
        app.handle_action(Action::CycleBrowseMode);
        assert_eq!(app.library_view.mode, BrowseMode::Albums);
        assert_eq!(app.library_view.mode_state.selected(), Some(2));

        // Select on a mode focuses the groups; moving there loads that group's tracks
        app.handle_action(Action::Select);
        assert_eq!(app.library_view.pane, LibPane::Groups);
        app.handle_action(Action::Bottom);
        let g = app.library_view.group_state.selected().unwrap();
        let mut want = app.library_view.groups[g].tracks.clone();
        let mut got = app.library_view.tracks.clone();
        want.sort();
        got.sort();
        assert_eq!(want, got);

        app.handle_action(Action::PrevTab);
        assert_eq!(app.tab, Tab::Equalizer);
        app.handle_action(Action::NextTab);
        assert_eq!(app.tab, Tab::Library);
    }

    #[test]
    fn sort_keeps_the_selected_track() {
        let mut app = app_with(6, false);
        app.library_view.track_state.select(Some(1));
        let keep = app.selected_lib_track();
        app.handle_action(Action::ReverseSort);
        assert!(app.library_view.sort_desc);
        assert_eq!(app.selected_lib_track(), keep);
        assert_eq!(msg(&app), "sort default (desc)");
        app.handle_action(Action::CycleSort);
        assert_eq!(app.library_view.sort, SortKey::Title);
        assert_eq!(app.selected_lib_track(), keep);
    }

    #[test]
    fn select_honors_on_select() {
        // list: the queue becomes the list, starting at the track
        let mut app = app_with(6, true);
        app.library_view.track_state.select(Some(2));
        app.handle_action(Action::Select);
        assert_eq!(app.queue.tracks, app.library_view.tracks);
        assert_eq!(app.queue.current, Some(2));
        if app.engine.is_some() {
            assert_eq!(playing(&app), Some(app.library_view.tracks[2]));
        }

        // single: play it now, keeping the queue
        let mut cfg = test_config();
        cfg.playback.on_select = PlayContext::Single;
        let mut app = app_cfg(cfg, 6, true);
        app.queue.set(vec![4, 5], Some(0));
        app.library_view.track_state.select(Some(1));
        let id = app.library_view.tracks[1];
        app.handle_action(Action::Select);
        assert_eq!(app.queue.tracks, vec![4, id, 5]);
        assert_eq!(app.queue.current, Some(1));

        // enqueue: appended, and started because nothing was playing
        let mut cfg = test_config();
        cfg.playback.on_select = PlayContext::Enqueue;
        let mut app = app_cfg(cfg, 6, true);
        app.queue.set(vec![4], None);
        app.library_view.track_state.select(Some(0));
        let id = app.library_view.tracks[0];
        app.handle_action(Action::Select);
        assert_eq!(app.queue.tracks, vec![4, id]);
        assert_eq!(app.queue.current, Some(1));

        // a group plays as a whole
        let mut app = app_with(6, false);
        app.library_view.pane = LibPane::Groups;
        app.handle_action(Action::Select);
        assert_eq!(app.queue.tracks, app.library_view.tracks);
        assert_eq!(app.queue.current, Some(0));
    }

    #[test]
    fn queue_actions_from_the_library() {
        let mut app = app_with(6, false);
        let v = app.library_view.tracks.clone();
        let last = v.len() - 1;
        app.library_view.track_state.select(Some(0));
        app.handle_action(Action::Enqueue);
        assert_eq!(app.queue.tracks, vec![v[0]]);
        app.queue.jump(0);
        app.library_view.track_state.select(Some(last));
        app.handle_action(Action::PlayNext);
        app.handle_action(Action::EnqueueAll);
        assert_eq!(app.queue.tracks.len(), 2 + v.len());
        assert_eq!(&app.queue.tracks[..2], &[v[0], v[last]]);
        assert_eq!(msg(&app), format!("queued {} tracks", v.len()));
        app.library_view.pane = LibPane::Modes;
        app.handle_action(Action::Enqueue);
        assert_eq!(msg(&app), "nothing selected");
    }

    #[test]
    fn queue_tab_editing() {
        let mut app = app_with(6, false);
        app.queue.set(vec![0, 1, 2, 3], Some(0));
        app.set_tab(Tab::Queue);
        app.queue_state.select(Some(1));
        app.handle_action(Action::MoveDown);
        assert_eq!(app.queue.tracks, vec![0, 2, 1, 3]);
        assert_eq!(app.queue_state.selected(), Some(2));
        app.handle_action(Action::MoveUp);
        assert_eq!(app.queue.tracks, vec![0, 1, 2, 3]);
        assert_eq!(app.queue_state.selected(), Some(1));
        app.queue_state.select(Some(3));
        app.handle_action(Action::MoveDown);
        assert_eq!(app.queue.tracks, vec![0, 1, 2, 3]);
        app.handle_action(Action::Remove);
        assert_eq!(app.queue.tracks, vec![0, 1, 2]);
        assert_eq!(app.queue_state.selected(), Some(2));
        // play-next on the queue moves the row right after the current track
        app.handle_action(Action::PlayNext);
        assert_eq!(app.queue.tracks, vec![0, 2, 1]);
        assert_eq!(app.queue_state.selected(), Some(1));
        app.handle_action(Action::JumpToCurrent);
        assert_eq!(app.queue_state.selected(), Some(0));
        // clearing asks first
        app.handle_action(Action::ClearQueue);
        assert!(matches!(app.overlay, Some(Overlay::Confirm { action: ConfirmAction::ClearQueue, .. })));
        app.handle_event(key(KeyCode::Char('x')));
        assert!(app.overlay.is_some());
        app.handle_event(key(KeyCode::Char('n')));
        assert!(app.overlay.is_none());
        assert_eq!(app.queue.len(), 3);
        app.handle_action(Action::ClearQueue);
        app.handle_event(key(KeyCode::Char('y')));
        assert!(app.queue.is_empty());
        assert_eq!(app.queue_state.selected(), None);
    }

    #[test]
    fn removing_the_playing_track_skips_or_stops() {
        let mut app = app_with(6, true);
        if app.engine.is_none() {
            return;
        }
        app.queue.set(vec![0, 1, 2], Some(1));
        app.play_queue_current(Duration::ZERO, false);
        app.set_tab(Tab::Queue);
        app.queue_state.select(Some(1));
        app.handle_action(Action::Remove);
        assert_eq!(app.queue.tracks, vec![0, 2]);
        assert_eq!(playing(&app), Some(2));
        app.queue_state.select(Some(1));
        app.handle_action(Action::Remove);
        assert_eq!(app.play_state(), PlayState::Stopped);
    }

    #[test]
    fn engine_events_drive_the_queue() {
        let mut app = app_with(6, true);
        if app.engine.is_none() {
            return;
        }
        app.queue.set(vec![0, 1, 2], Some(0));
        app.play_queue_current(Duration::ZERO, false);
        app.tick();
        assert_eq!(app.setup_track, Some(0));
        // gapless transition: the queue follows the engine
        app.on_engine_event(EngineEvent::Advanced { from: 0, to: 1 });
        assert_eq!((app.queue.current, app.setup_track), (Some(1), Some(1)));
        // a finished track advances and loads the next one
        app.on_engine_event(EngineEvent::Finished(1));
        assert_eq!(app.queue.current, Some(2));
        assert_eq!(playing(&app), Some(2));
        // stop after the current track
        app.queue.stop_after_current = true;
        app.engine.as_mut().unwrap().stop();
        app.on_engine_event(EngineEvent::Finished(2));
        assert!(!app.queue.stop_after_current);
        assert_eq!(app.play_state(), PlayState::Stopped);
        assert_eq!(msg(&app), "stopped after current track");
        // the sleep timer's end-of-track mode stops without advancing
        app.queue.set(vec![0, 1], Some(0));
        app.sleep = Some(Sleep::EndOfTrack);
        app.on_engine_event(EngineEvent::Finished(0));
        assert_eq!((app.queue.current, app.sleep), (Some(0), None));
        assert_eq!(msg(&app), "sleep timer: stopped");
    }

    #[test]
    fn failures_skip_ahead_then_give_up() {
        let mut app = app_with(8, true);
        if app.engine.is_none() {
            return;
        }
        app.queue.set((0..8).collect(), Some(0));
        app.play_queue_current(Duration::ZERO, false);
        for i in 0..MAX_FAILURES as usize {
            app.engine.as_mut().unwrap().stop();
            app.on_engine_event(EngineEvent::Error { track: Some(i), message: "bad file".into() });
        }
        assert_eq!(app.play_state(), PlayState::Stopped);
        assert!(msg(&app).contains("failed to play"));
        assert_eq!(app.queue.current, Some(MAX_FAILURES as usize - 1));
    }

    #[test]
    fn prev_restarts_or_goes_back() {
        let mut app = app_with(3, true);
        if app.engine.is_none() {
            return;
        }
        app.queue.set(vec![0, 1, 2], Some(1));
        app.play_queue_current(Duration::from_secs(5), true);
        app.handle_action(Action::Prev);
        assert_eq!(app.queue.current, Some(1));
        assert!(app.position() < Duration::from_secs(1));
        app.handle_action(Action::Prev);
        assert_eq!((app.queue.current, playing(&app)), (Some(0), Some(0)));
        app.handle_action(Action::Next);
        assert_eq!((app.queue.current, playing(&app)), (Some(1), Some(1)));
        // library: jump to the playing track
        app.set_tab(Tab::Library);
        app.library_view.track_state.select(Some(0));
        app.handle_action(Action::JumpToCurrent);
        assert_eq!(app.selected_lib_track(), Some(1));
    }

    #[test]
    fn toggle_pause_starts_the_queue() {
        let mut app = app_with(3, true);
        if app.engine.is_none() {
            return;
        }
        assert_eq!(app.exec(Command::Toggle), Err("the queue is empty".into()));
        app.queue.set(vec![2, 1], None);
        app.handle_action(Action::TogglePause);
        assert_eq!((app.play_state(), playing(&app)), (PlayState::Playing, Some(2)));
        assert_eq!(ok(&mut app, Command::Toggle), "paused");
        assert_eq!(ok(&mut app, Command::Play(None)), "playing Song 02 - Gamma");
        assert!(ok(&mut app, Command::Status(None)).starts_with("playing Gamma - Song 02 [0:0"));
        app.handle_action(Action::Stop);
        assert_eq!(app.play_state(), PlayState::Stopped);
    }

    #[test]
    fn exec_replies() {
        let mut app = app_with(4, false);
        assert_eq!(ok(&mut app, Command::Volume(Level::Set(55.0))), "volume 55%");
        assert_eq!(ok(&mut app, Command::Volume(Level::Up(80.0))), "volume 100%");
        assert_eq!(ok(&mut app, Command::Mute(None)), "muted");
        assert_eq!(ok(&mut app, Command::Mute(Some(false))), "unmuted, volume 100%");
        assert_eq!(ok(&mut app, Command::Speed(Level::Set(1.25))), "speed 1.25x");
        assert_eq!(ok(&mut app, Command::Speed(Level::Up(10.0))), "speed 4.00x");
        assert_eq!(ok(&mut app, Command::Speed(Level::Reset)), "speed 1.00x");
        assert_eq!(ok(&mut app, Command::Shuffle(None)), "shuffle on");
        assert_eq!(ok(&mut app, Command::Repeat(None)), "repeat all");
        assert_eq!(ok(&mut app, Command::Repeat(Some(Repeat::One))), "repeat one");
        assert_eq!(ok(&mut app, Command::StopAfter(None)), "stop after current on");
        assert_eq!(ok(&mut app, Command::Sleep(SleepArg::Minutes(30))), "sleep in 30 min");
        assert_eq!(ok(&mut app, Command::Sleep(SleepArg::EndOfTrack)), "sleep at end of track");
        assert_eq!(ok(&mut app, Command::Sleep(SleepArg::Off)), "sleep timer off");
        assert_eq!(ok(&mut app, Command::Eq(EqArg::Band(3, 4.0))), "eq 125 Hz +4.0 dB");
        assert_eq!((app.eq.preset.as_str(), app.eq.enabled), ("custom", true));
        assert_eq!(ok(&mut app, Command::Eq(EqArg::Preset("ROCK".into()))), "eq preset rock");
        assert_eq!(Some(app.eq.bands), crate::dsp::eq_preset("rock"));
        assert!(app.exec(Command::Eq(EqArg::Preset("nope".into()))).unwrap_err().contains("unknown preset"));
        assert!(app.exec(Command::Eq(EqArg::Band(11, 1.0))).is_err());
        assert_eq!(ok(&mut app, Command::Eq(EqArg::Preamp(-30.0))), "eq preamp -12.0 dB");
        assert_eq!(ok(&mut app, Command::Eq(EqArg::Reset)), "eq reset");
        assert_eq!(ok(&mut app, Command::Eq(EqArg::Toggle)), "eq off");
        assert_eq!(ok(&mut app, Command::Vis(Some(VisMode::Vu))), "visualizer vu");
        assert_eq!(ok(&mut app, Command::Sort(SortKey::Title, Some(true))), "sort title (desc)");
        assert_eq!(ok(&mut app, Command::View(BrowseMode::Albums)), "view albums");
        assert_eq!(app.library_view.mode, BrowseMode::Albums);
        assert_eq!(ok(&mut app, Command::Goto(Tab::Equalizer)), "");
        assert_eq!(app.tab, Tab::Equalizer);
        let status = Command::Status(Some("{state}|{volume}|{queue}|{shuffle}|{repeat}|{title}".into()));
        assert_eq!(ok(&mut app, status), "stopped|100|0/0|on|one|");
        assert_eq!(ok(&mut app, Command::Status(None)), "stopped, volume 100%");
        assert_eq!(app.exec(Command::Seek(SeekTarget::Forward(Duration::from_secs(5)))), Err("nothing playing".into()));
        assert!(app.exec(Command::Theme(Some("no-such-theme".into()))).is_err());
        assert_eq!(ok(&mut app, Command::Theme(Some("default".into()))), "theme default");
        assert_eq!(app.theme_override.as_deref(), Some("default"));
        assert!(app.exec(Command::Load("nothing".into())).is_err());
        assert!(app.exec(Command::Favorite(None)).is_err());
        assert!(app.exec(Command::LyricsOffset(Level::Up(250.0))).is_err());
        assert_eq!(app.exec(Command::Toggle), Err("no audio output".into()));
        assert!(app.exec(Command::Add(vec!["/definitely/not/here".into()])).unwrap_err().contains("no such file"));
        let file = fixture_dir().join("track01.wav").display().to_string();
        assert_eq!(ok(&mut app, Command::Add(vec![file])), "added Song 01 - Beta");
        assert_eq!(app.queue.tracks, vec![1]);
        assert_eq!(ok(&mut app, Command::Clear), "queue cleared");
        assert_eq!(ok(&mut app, Command::Help), "");
        assert!(matches!(app.overlay, Some(Overlay::Help { .. })));
        assert_eq!(ok(&mut app, Command::Rescan), "rescanning…");
        assert_eq!(app.exec(Command::Rescan), Err("already scanning".into()));
        assert_eq!(ok(&mut app, Command::Quit), "");
        assert!(app.should_quit);
    }

    #[test]
    fn session_round_trip_maps_tracks_by_path() {
        let mut a = app_with(6, false);
        a.queue.set(vec![5, 3, 1], Some(1));
        a.queue.repeat = Repeat::All;
        a.volume = 42;
        a.muted = true;
        a.eq.preamp_db = -3.0;
        a.tab = Tab::Queue;
        a.vis_mode = VisMode::Wave;
        a.library_view.mode = BrowseMode::Genres;
        a.command_history = vec!["vol 30".into()];
        a.snapshot_session();
        let saved = a.state.session.clone();
        assert_eq!(saved.queue, a.paths_of(&[5, 3, 1]));
        assert_eq!(saved.command_history, ["vol 30"]);

        // the next run finds the same files in another order: tracks map by path
        let restore_session = |tracks: Vec<Track>, session: &crate::state::Session| {
            let mut cfg = test_config();
            cfg.playback.resume_session = true;
            let mut b = App::build(cfg, test_paths(), &crate::Cli::default(), None);
            b.scan = None;
            b.state.session = session.clone();
            b.apply_session_settings(false);
            b.on_scan_done(Library::from_tracks(Vec::new(), tracks));
            b
        };
        let restore = |tracks: Vec<Track>| restore_session(tracks, &saved);
        let mut reversed = fake_tracks(6);
        reversed.reverse();
        let b = restore(reversed);
        assert_eq!(b.paths_of(&b.queue.tracks), a.paths_of(&[5, 3, 1]));
        assert_eq!(b.queue.current, Some(1));
        assert_eq!((b.queue.repeat, b.volume, b.muted, b.tab, b.vis_mode), (Repeat::All, 42, true, Tab::Queue, VisMode::Wave));
        assert_eq!((b.eq.preamp_db, b.library_view.mode), (-3.0, BrowseMode::Genres));

        // the saved current file is gone: resume at the next one that still exists
        let mut gone = saved.clone();
        gone.queue[1] = fixture_dir().join("gone.wav");
        let b = restore_session(fake_tracks(6), &gone);
        assert_eq!(b.paths_of(&b.queue.tracks), a.paths_of(&[5, 1]));

        // a file played from outside the music folders (not in the scan) is read again
        let without_3: Vec<Track> = fake_tracks(6).into_iter().filter(|t| !t.path.ends_with("track03.wav")).collect();
        let b = restore(without_3);
        assert_eq!(b.paths_of(&b.queue.tracks), a.paths_of(&[5, 3, 1]));
        assert_eq!(b.queue.current, Some(1));
        // a file the scan didn't find but that exists (played from outside the music folders) stays
        let without_3: Vec<Track> = fake_tracks(6).into_iter().filter(|t| !t.path.ends_with("track03.wav")).collect();
        let b = restore(without_3);
        assert_eq!(b.paths_of(&b.queue.tracks), saved.queue);

        // view settings are saved only where they differ from the config, so config edits apply
        let mut cfg = test_config();
        (cfg.playback.resume_session, cfg.ui.compact, cfg.visualizer.mode) = (true, true, VisMode::Mirror);
        let mut d = App::build(cfg, test_paths(), &crate::Cli::default(), None);
        assert!(d.queue.shuffle == test_config().playback.shuffle && d.compact);
        d.state.session = saved.clone();
        d.apply_session_settings(false);
        assert_eq!((d.compact, d.vis_mode), (true, VisMode::Wave), "compact follows the edited config, wave was chosen in orbit");
        // no saved session yet: playback.shuffle / repeat apply
        let mut cfg = test_config();
        (cfg.playback.resume_session, cfg.playback.shuffle, cfg.playback.repeat) = (true, true, Repeat::One);
        let d = App::build(cfg, test_paths(), &crate::Cli::default(), None);
        assert_eq!((d.queue.shuffle, d.queue.repeat), (true, Repeat::One));

        // quitting before the first scan keeps the saved queue
        let mut c = App::build(test_config(), test_paths(), &crate::Cli::default(), None);
        c.scan = None;
        c.state.session = saved.clone();
        c.snapshot_session();
        assert_eq!(c.state.session.queue, saved.queue);
    }

    #[test]
    fn rescan_keeps_playback_and_remaps_by_path() {
        let mut app = app_with(6, true);
        app.queue.set(vec![1, 3, 4], Some(1));
        if app.engine.is_some() {
            app.play_queue_current(Duration::ZERO, false);
            app.tick();
        }
        app.library_view.track_state.select(Some(2));
        let selected = app.selected_lib_track().map(|id| app.lib.tracks[id].path.clone());
        let queued = app.paths_of(&app.queue.tracks);
        let playing_path = app.now_playing().map(|t| t.path.clone());

        let mut tracks = fake_tracks(6);
        tracks.reverse();
        app.on_scan_done(Library::from_tracks(Vec::new(), tracks));
        assert_eq!(app.paths_of(&app.queue.tracks), queued);
        assert_eq!(app.queue.current, Some(1));
        assert_eq!(app.now_playing().map(|t| t.path.clone()), playing_path);
        assert_eq!(app.selected_lib_track().map(|id| app.lib.tracks[id].path.clone()), selected);
        if app.engine.is_some() {
            assert_eq!(app.play_state(), PlayState::Playing);
        }

        // files the scan no longer finds (e.g. added from elsewhere) stay while queued
        app.on_scan_done(Library::from_tracks(Vec::new(), fake_tracks(2)));
        assert_eq!(app.paths_of(&app.queue.tracks), queued);
        assert_eq!(app.now_playing().map(|t| t.path.clone()), playing_path);
    }

    #[test]
    fn mouse_tabs_lists_double_click_and_wheel() {
        let mut app = app_with(8, false);
        app.hit.tabs = vec![(Rect::new(0, 0, 10, 1), Tab::Library), (Rect::new(10, 0, 10, 1), Tab::Queue)];
        click(&mut app, 12, 0);
        assert_eq!(app.tab, Tab::Queue);
        click(&mut app, 3, 0);
        assert_eq!(app.tab, Tab::Library);
        app.handle_event(mouse(MouseEventKind::ScrollDown, 3, 0));
        assert_eq!(app.tab, Tab::Queue);
        app.handle_event(mouse(MouseEventKind::ScrollUp, 3, 0));
        assert_eq!(app.tab, Tab::Library);

        app.set_browse_mode(BrowseMode::Tracks);
        let n = app.library_view.tracks.len();
        assert_eq!(n, 8);
        app.library_view.pane = LibPane::Groups;
        app.hit.lists = vec![ListHit { area: Rect::new(40, 2, 60, 10), target: ListTarget::LibTracks, offset: 0, len: n }];
        click(&mut app, 45, 5);
        assert_eq!(app.library_view.pane, LibPane::Tracks);
        assert_eq!(app.library_view.track_state.selected(), Some(3));
        assert!(app.queue.is_empty());
        click(&mut app, 45, 5);
        assert_eq!(app.queue.current, Some(3));
        app.handle_event(mouse(MouseEventKind::ScrollDown, 45, 5));
        assert_eq!(app.library_view.track_state.selected(), Some(4));
        app.handle_event(mouse(MouseEventKind::ScrollUp, 45, 5));
        assert_eq!(app.library_view.track_state.selected(), Some(3));
        // below the last row: focus only
        app.library_view.pane = LibPane::Modes;
        click(&mut app, 45, 11);
        assert_eq!((app.library_view.pane, app.library_view.track_state.selected()), (LibPane::Tracks, Some(3)));
        // buttons run their action
        app.hit.buttons = vec![(Rect::new(0, 20, 3, 1), Action::ToggleShuffle)];
        click(&mut app, 1, 20);
        assert!(app.queue.shuffle);
        // mouse off in the config: events are ignored
        app.cfg.ui.mouse = false;
        click(&mut app, 12, 0);
        assert_eq!(app.tab, Tab::Library);
    }

    #[test]
    fn mouse_eq_volume_progress_and_overlays() {
        let mut app = app_with(4, true);
        app.tab = Tab::Equalizer;
        app.hit.eq_sliders = vec![(Rect::new(10, 5, 3, 13), 3)];
        click(&mut app, 11, 5);
        assert_eq!((app.eq.bands[2], app.eq_view.selected, app.eq.enabled), (12.0, 3, true));
        app.handle_event(mouse(MouseEventKind::Down(MouseButton::Left), 11, 11));
        assert_eq!(app.eq.bands[2], 0.0);
        app.handle_event(mouse(MouseEventKind::Drag(MouseButton::Left), 30, 17));
        assert_eq!(app.eq.bands[2], -12.0);
        app.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), 30, 17));
        app.handle_event(mouse(MouseEventKind::ScrollUp, 11, 8));
        assert_eq!(app.eq.bands[2], -11.0);

        app.hit.volume = Some(Rect::new(100, 30, 11, 1));
        click(&mut app, 105, 30);
        assert_eq!(app.volume, 50);
        app.handle_event(mouse(MouseEventKind::ScrollUp, 105, 30));
        assert_eq!(app.volume, 55);

        // clicks outside an overlay close it; wheel inside the help scrolls it
        app.handle_action(Action::Help);
        app.hit.overlay = Some(Rect::new(20, 5, 40, 10));
        app.handle_event(mouse(MouseEventKind::ScrollDown, 25, 7));
        assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 3 })));
        click(&mut app, 25, 7);
        app.handle_event(mouse(MouseEventKind::Down(MouseButton::Right), 25, 7));
        assert!(app.overlay.is_some());
        click(&mut app, 0, 0);
        assert!(app.overlay.is_none());

        if app.engine.is_none() {
            return;
        }
        app.queue.set(vec![0], Some(0));
        app.play_queue_current(Duration::ZERO, true);
        app.hit.progress = Some(Rect::new(0, 30, 100, 1));
        click(&mut app, 50, 30);
        assert!((app.position().as_secs_f32() - TRACK_SECS as f32 / 2.0).abs() < 0.3);
    }

    #[test]
    fn mouse_in_overlay_lists() {
        let mut app = app_with(6, false);
        app.exec(Command::Search("song".into())).unwrap();
        app.hit.overlay = Some(Rect::new(10, 2, 60, 20));
        app.hit.lists = vec![
            ListHit { area: Rect::new(0, 0, 100, 30), target: ListTarget::LibTracks, offset: 0, len: 6 },
            ListHit { area: Rect::new(12, 4, 50, 10), target: ListTarget::SearchResults, offset: 0, len: 6 },
        ];
        click(&mut app, 20, 6);
        let Some(Overlay::Search { state, results, .. }) = &app.overlay else { panic!("search closed") };
        assert_eq!(state.selected(), Some(2));
        let id = results[2].id;
        click(&mut app, 20, 6);
        assert!(app.overlay.is_none());
        assert_eq!(app.queue.current_track(), Some(id));
    }

    #[test]
    fn search_overlay_flow() {
        let mut app = app_with(6, true);
        app.handle_action(Action::Search);
        typ(&mut app, "song 04");
        let Some(Overlay::Search { results, .. }) = &app.overlay else { panic!("search closed") };
        assert_eq!(results.first().map(|h| h.id), Some(4));
        // tab enqueues and keeps searching
        app.handle_event(key(KeyCode::Tab));
        assert_eq!(app.queue.tracks, vec![4]);
        assert!(app.overlay.is_some());
        // enter plays it now, after the current track, and closes
        app.queue.set(vec![0, 1], Some(0));
        app.handle_event(key(KeyCode::Enter));
        assert!(app.overlay.is_none());
        assert_eq!((app.queue.tracks.clone(), app.queue.current), (vec![0, 4, 1], Some(1)));
        if app.engine.is_some() {
            assert_eq!(playing(&app), Some(4));
        }
        // opened prefilled by the command; ctrl+n / ctrl+p move
        app.exec(Command::Search("gamma".into())).unwrap();
        assert_eq!(input_text(&app), "gamma");
        app.handle_event(ctrl('n'));
        let Some(Overlay::Search { state, results, .. }) = &app.overlay else { panic!("search closed") };
        assert_eq!(state.selected(), Some(1.min(results.len() - 1)));
        let hits: Vec<TrackId> = results.iter().map(|h| h.id).collect();
        app.handle_event(ctrl('p'));
        // alt+enter: play next; alt+a: enqueue every result; esc closes
        let alt = |code| Event::Key(KeyEvent::new(code, KeyModifiers::ALT));
        app.handle_event(alt(KeyCode::Enter));
        assert_eq!(app.queue.tracks[2], hits[0]);
        let before = app.queue.len();
        app.handle_event(alt(KeyCode::Char('a')));
        assert_eq!(&app.queue.tracks[before..], &hits[..]);
        assert!(app.overlay.is_some());
        app.handle_event(key(KeyCode::Esc));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn command_palette_history_and_completion() {
        let mut app = app_with(3, false);
        app.handle_event(key(KeyCode::Char(':')));
        assert!(matches!(app.overlay, Some(Overlay::Command { .. })));
        typ(&mut app, "next");
        app.handle_event(key(KeyCode::Enter));
        assert!(app.overlay.is_none());
        for _ in 0..2 {
            app.handle_event(key(KeyCode::Char(':')));
            typ(&mut app, "vol 30");
            app.handle_event(key(KeyCode::Enter));
        }
        assert_eq!(app.command_history, ["next", "vol 30"]);

        app.handle_event(key(KeyCode::Char(':')));
        typ(&mut app, "dr");
        app.handle_event(key(KeyCode::Up));
        assert_eq!(input_text(&app), "vol 30");
        app.handle_event(key(KeyCode::Up));
        app.handle_event(key(KeyCode::Up));
        assert_eq!(input_text(&app), "next");
        app.handle_event(key(KeyCode::Down));
        assert_eq!(input_text(&app), "vol 30");
        app.handle_event(key(KeyCode::Down));
        assert_eq!(input_text(&app), "dr");

        app.handle_event(ctrl('u'));
        typ(&mut app, "re");
        let expected = app.completions_for("re");
        app.handle_event(key(KeyCode::Tab));
        if let Some(first) = expected.first() {
            assert_eq!(input_text(&app), *first);
        }
        app.handle_event(key(KeyCode::Esc));
        assert!(app.overlay.is_none());
        app.handle_event(key(KeyCode::Char(':')));
        app.handle_event(key(KeyCode::Backspace));
        assert!(app.overlay.is_none());

        // playlist names complete for commands that take one
        let mut p = Playlist::default();
        p.name = "Road Trip".into();
        app.playlists.lists.push(p);
        // names with spaces are quoted so the parser reads them as one argument
        assert!(app.completions_for("load ro").contains(&"load \"Road Trip\"".to_string()), "{:?}", app.completions_for("load ro"));
    }

    #[test]
    fn help_scrolls_and_closes_without_quitting() {
        let mut app = app_with(1, false);
        app.handle_event(key(KeyCode::Char('?')));
        app.handle_event(key(KeyCode::Char('j')));
        assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 1 })));
        app.handle_event(key(KeyCode::Char('G')));
        assert!(matches!(app.overlay, Some(Overlay::Help { scroll: u16::MAX })));
        app.handle_event(key(KeyCode::Char('g')));
        assert!(matches!(app.overlay, Some(Overlay::Help { scroll: 0 })));
        // playback keys still work while it's open
        app.handle_event(key(KeyCode::Char('+')));
        assert_eq!(app.volume, 5);
        app.handle_event(key(KeyCode::Char('q')));
        assert!(app.overlay.is_none());
        assert!(!app.should_quit);
    }

    #[test]
    fn playlist_picker_prompts_and_confirm() {
        let mut app = app_with(4, false);
        let mut p = Playlist::default();
        p.name = "Mix".into();
        app.playlists.lists.push(p);
        app.library_view.track_state.select(Some(1));
        let id = app.library_view.tracks[1];
        app.handle_action(Action::AddToPlaylist);
        let Some(Overlay::PickPlaylist { tracks, state }) = &app.overlay else { panic!("no picker") };
        assert_eq!((tracks.clone(), state.selected()), (vec![id], Some(1)));
        app.handle_event(key(KeyCode::Up));
        app.handle_event(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Some(Overlay::Prompt { purpose: PromptPurpose::NewPlaylist(t), .. }) if *t == vec![id]));
        app.handle_event(key(KeyCode::Esc));
        assert!(app.overlay.is_none());

        // rename is prefilled; an empty name is refused and the prompt stays open
        app.playlist_view.list_state.select(Some(Smart::ALL.len()));
        app.handle_action(Action::RenamePlaylist);
        assert!(matches!(&app.overlay, Some(Overlay::Prompt { input, purpose: PromptPurpose::RenamePlaylist(_), .. }) if input.text == "Mix"));
        app.handle_event(ctrl('u'));
        app.handle_event(key(KeyCode::Enter));
        assert!(app.overlay.is_some());
        app.handle_event(key(KeyCode::Esc));

        // deleting from the list pane asks first; smart playlists can't be deleted
        app.set_tab(Tab::Playlists);
        app.playlist_view.pane = PlPane::Lists;
        app.handle_action(Action::Remove);
        assert!(matches!(&app.overlay, Some(Overlay::Confirm { action: ConfirmAction::DeletePlaylist(_), .. })));
        app.handle_event(key(KeyCode::Esc));
        app.handle_action(Action::Top);
        app.handle_action(Action::Remove);
        assert!(app.overlay.is_none());
        assert_eq!(msg(&app), "smart playlists are read-only");

        app.handle_action(Action::SaveQueue);
        assert_eq!(msg(&app), "the queue is empty");
        app.queue.set(vec![0], Some(0));
        app.handle_action(Action::SaveQueue);
        assert!(matches!(&app.overlay, Some(Overlay::Prompt { purpose: PromptPurpose::SaveQueue, .. })));
        // saving over an existing playlist (any case) asks first
        typ(&mut app, "mix");
        app.handle_event(key(KeyCode::Enter));
        assert!(matches!(&app.overlay, Some(Overlay::Confirm { action: ConfirmAction::SaveQueueAs(n), .. }) if n == "mix"));
        app.handle_event(key(KeyCode::Char('n')));
        assert!(app.overlay.is_none() && app.playlists.lists[0].tracks.is_empty());
    }

    #[test]
    fn plays_count_at_the_end_and_once_across_a_resume() {
        // count_play_after = 1.0: no tick sees the very end, the track ending counts it; Most
        // Played follows while it's on screen
        let mut cfg = test_config();
        cfg.playback.count_play_after = 1.0;
        let mut app = app_cfg(cfg, 3, false);
        app.set_tab(Tab::Playlists);
        app.handle_action(Action::Down);
        assert_eq!(app.selected_entry(), Some(PlaylistEntry::Smart(Smart::MostPlayed)));
        app.queue.set(vec![0, 1, 2], Some(0));
        (app.setup_track, app.counted) = (Some(0), false);
        app.on_engine_event(EngineEvent::Advanced { from: 0, to: 1 });
        app.on_engine_event(EngineEvent::Finished(1));
        assert_eq!((app.plays(0), app.plays(1), app.plays(2)), (1, 1, 0));
        assert_eq!(app.playlist_view.tracks.len(), 2);

        // a session resumed past the count point was counted before the restart
        let mut app = app_with(3, true);
        if app.engine.is_none() {
            return;
        }
        app.queue.set(vec![0], Some(0));
        app.play_queue_current(Duration::from_secs(TRACK_SECS as u64 * 6 / 10), true);
        app.tick();
        assert!(app.counted);
        app.play_queue_current(Duration::ZERO, true);
        app.tick();
        assert!(!app.counted);
    }

    #[test]
    fn playlist_view_counts_missing_files() {
        let mut app = app_with(4, false);
        let paths = app.paths_of(&[2, 0]);
        let mut p = Playlist::default();
        p.name = "Road".into();
        p.tracks = vec![paths[0].clone(), PathBuf::from("/nope/gone.mp3"), paths[1].clone()];
        app.playlists.lists.push(p);
        app.set_tab(Tab::Playlists);
        app.handle_action(Action::Bottom);
        assert_eq!(app.selected_entry(), Some(PlaylistEntry::User(0)));
        assert_eq!((app.playlist_view.tracks.clone(), app.playlist_view.missing), (vec![2, 0], 1));
        assert_eq!(app.playlist_view.rows, vec![0, 2]);
        app.handle_action(Action::Select);
        assert_eq!(app.queue.tracks, vec![2, 0]);
        app.handle_action(Action::Right);
        assert_eq!(app.playlist_view.pane, PlPane::Tracks);
        assert_eq!(app.selection(), vec![2]);
    }

    #[test]
    fn equalizer_keys() {
        let mut app = app_with(1, false);
        app.set_tab(Tab::Equalizer);
        app.handle_action(Action::Right);
        assert_eq!(app.eq_view.selected, 1);
        app.handle_action(Action::Up);
        assert_eq!((app.eq.bands[0], app.eq.enabled, app.eq.preset.as_str()), (1.0, true, "custom"));
        app.handle_action(Action::Down);
        assert_eq!((app.eq.bands[0], app.eq.preset.as_str()), (0.0, "flat"));
        app.handle_action(Action::Bottom);
        assert_eq!(app.eq.bands[0], -EQ_MAX_DB);
        app.handle_action(Action::Select);
        assert_eq!(app.eq.bands[0], 0.0);
        app.handle_action(Action::Left);
        app.handle_action(Action::Left);
        assert_eq!(app.eq_view.selected, 0);
        app.handle_action(Action::PageUp);
        assert_eq!(app.eq.preamp_db, 3.0);
        app.handle_action(Action::EqNextPreset);
        assert_eq!(app.eq.preset, EQ_PRESETS[1].0);
        app.handle_action(Action::EqPrevPreset);
        assert_eq!(app.eq.preset, EQ_PRESETS[0].0);
        app.eq.preset = "custom".into();
        app.handle_action(Action::EqPrevPreset);
        assert_eq!(app.eq.preset, EQ_PRESETS[EQ_PRESETS.len() - 1].0);
        app.handle_action(Action::EqReset);
        assert_eq!((app.eq.preamp_db, app.eq.bands, app.eq.preset.as_str()), (0.0, [0.0; 10], "flat"));
        app.handle_action(Action::FocusPrev);
        assert_eq!(app.eq_view.selected, 10);
    }

    #[test]
    fn sleep_timer_cycles_and_expires() {
        let mut app = app_with(1, true);
        let seen: Vec<String> = (0..8)
            .map(|_| {
                app.handle_action(Action::SleepTimer);
                msg(&app)
            })
            .collect();
        let want = ["15", "30", "45", "60", "90"].map(|m| format!("sleep in {m} min"));
        assert_eq!(seen[..5], want);
        assert_eq!(seen[5..], ["sleep at end of track", "sleep timer off", "sleep in 15 min"]);
        app.queue.set(vec![0], Some(0));
        app.play_queue_current(Duration::ZERO, false);
        app.sleep = Some(Sleep::At(Instant::now()));
        app.tick();
        assert_eq!((app.sleep, msg(&app).as_str()), (None, "sleep timer: paused"));
        if app.engine.is_some() {
            assert_eq!(app.play_state(), PlayState::Paused);
        }
    }

    #[test]
    fn ab_loop_cycles() {
        let mut app = app_with(1, true);
        assert!(app.exec(Command::Loop(LoopArg::A)).is_err());
        if app.engine.is_none() {
            return;
        }
        let secs = |d: Option<Duration>| d.map(|d| d.as_secs_f32().round() as u64);
        app.queue.set(vec![0], Some(0));
        app.play_queue_current(Duration::from_secs(2), true);
        app.tick();
        app.handle_action(Action::AbLoop);
        assert_eq!((secs(app.ab.0), app.ab.1), (Some(2), None));
        app.exec(Command::Seek(SeekTarget::Absolute(Duration::from_secs(1)))).unwrap();
        // before A: `:loop b` explains, the key clears (rather than failing on every press)
        assert!(app.exec(Command::Loop(LoopArg::B)).unwrap_err().contains("B must be after A"));
        app.handle_action(Action::AbLoop);
        assert_eq!(app.ab, (None, None));
        app.exec(Command::Seek(SeekTarget::Absolute(Duration::from_secs(2)))).unwrap();
        app.handle_action(Action::AbLoop);
        app.exec(Command::Seek(SeekTarget::Absolute(Duration::from_secs(5)))).unwrap();
        app.handle_action(Action::AbLoop);
        assert_eq!((secs(app.ab.0), secs(app.ab.1)), (Some(2), Some(5)));
        app.handle_action(Action::AbLoop);
        assert_eq!(app.ab, (None, None));
    }

    #[test]
    fn gapless_preload_follows_the_queue() {
        let mut cfg = test_config();
        cfg.playback.gapless = true;
        let mut app = app_cfg(cfg, 4, true);
        if app.engine.is_none() {
            return;
        }
        let preloaded = |app: &App| app.engine.as_ref().unwrap().preloaded();
        app.queue.set(vec![0, 1, 2], Some(0));
        app.play_queue_current(Duration::ZERO, false);
        app.tick();
        assert_eq!(preloaded(&app), Some(1));
        app.queue.repeat = Repeat::One;
        app.tick();
        assert_eq!(preloaded(&app), Some(0));
        app.queue.repeat = Repeat::Off;
        app.queue.stop_after_current = true;
        app.tick();
        assert_eq!(preloaded(&app), None);
        app.queue.stop_after_current = false;
        app.sleep = Some(Sleep::EndOfTrack);
        app.tick();
        assert_eq!(preloaded(&app), None);
        app.sleep = None;
        app.tick();
        assert_eq!(preloaded(&app), Some(1));
    }

    #[test]
    fn lyrics_offset_is_per_track() {
        let mut app = app_with(2, true);
        if app.engine.is_none() {
            return;
        }
        app.cfg.lyrics.offset_ms = 100;
        app.queue.set(vec![0, 1], Some(0));
        app.play_queue_current(Duration::ZERO, false);
        app.tick();
        app.handle_action(Action::LyricsOffsetUp);
        app.handle_action(Action::LyricsOffsetUp);
        assert_eq!((msg(&app).as_str(), app.lyrics_offset_ms), ("lyrics offset +500 ms", 600));
        app.handle_action(Action::Next);
        app.tick();
        assert_eq!(app.lyrics_offset_ms, 100);
        app.handle_action(Action::LyricsOffsetDown);
        assert_eq!(msg(&app), "lyrics offset -250 ms");
        assert_eq!(app.state.session.lyrics_offsets.len(), 2);
        app.exec(Command::LyricsOffset(Level::Reset)).unwrap();
        assert_eq!(app.state.session.lyrics_offsets.len(), 1);
    }

    #[test]
    fn lyrics_scroll_and_follow() {
        let mut app = app_with(1, false);
        let mut lyrics = Lyrics::default();
        lyrics.synced = true;
        lyrics.lines = (0..20)
            .map(|i| {
                let mut line = crate::lyrics::LyricLine::default();
                line.time = Some(Duration::from_secs(i));
                line.text = format!("line {i}");
                line
            })
            .collect();
        app.lyrics = Some(lyrics);
        app.set_tab(Tab::NowPlaying);
        app.hit.lyrics = Some(Rect::new(0, 0, 40, 6));
        assert!(app.lyrics_following());
        app.handle_action(Action::Down);
        assert!(!app.lyrics_following());
        assert_eq!(app.lyrics_scroll, 1);
        app.handle_action(Action::PageDown);
        assert_eq!(app.lyrics_scroll, 7);
        app.handle_action(Action::Bottom);
        assert_eq!(app.lyrics_scroll, 19);
        // follows the song again a few seconds after the last scroll
        app.lyrics_manual = Some(Instant::now() - LYRICS_FOLLOW_AFTER);
        app.tick();
        assert!(app.lyrics_following());
        assert_eq!(app.lyrics_scroll, 0);
        app.handle_action(Action::Up);
        assert!(!app.lyrics_following());
        app.handle_action(Action::JumpToCurrent);
        assert!(app.lyrics_following());
        // the wheel scrolls three lines; a click on the panel follows again
        app.handle_event(mouse(MouseEventKind::ScrollDown, 5, 2));
        assert_eq!(app.lyrics_scroll, 3);
        click(&mut app, 5, 2);
        assert!(app.lyrics_following());
    }

    #[test]
    fn reload_config_applies_changes() {
        let mut app = app_with(1, false);
        app.volume = 90;
        std::fs::create_dir_all(&app.paths.config_dir).unwrap();
        let config = "[ui]\ncompact = true\n[playback]\nmax_volume = 50\n[keys]\n\"F2\" = \":vol 10\"\n\"w\" = \"fly\"\n";
        std::fs::write(&app.paths.config_file, config).unwrap();
        app.handle_action(Action::ReloadConfig);
        assert!(app.compact);
        assert_eq!(app.volume, 50);
        assert_eq!(app.message.as_ref().map(|m| m.kind), Some(MsgKind::Warn));
        assert!(msg(&app).starts_with("config reloaded"));
        let f2 = KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE);
        assert_eq!(app.keymap.get(&f2), Some(&Binding::Command("vol 10".into())));
        std::fs::write(&app.paths.config_file, "[ui\n").unwrap();
        assert!(app.exec(Command::ReloadConfig).is_err());
        assert!(app.compact);
        let _ = std::fs::remove_dir_all(&app.paths.config_dir);
    }

    #[test]
    fn favorites_and_redraw_policy() {
        let mut app = app_with(3, false);
        app.handle_action(Action::ToggleFavorite);
        let first = app.describe(&[app.library_view.tracks[0]]);
        assert_eq!(msg(&app), format!("{} {first}", app.icons.favorite));

        app.tick();
        app.dirty = false;
        app.message = None;
        assert!(!app.needs_redraw());
        assert_eq!(app.frame_timeout(), IDLE_TICK);
        app.flash("hi", MsgKind::Info);
        assert!(app.needs_redraw());
        assert!(app.frame_timeout() < IDLE_TICK);
        app.dirty = false;
        app.message.as_mut().unwrap().at = Instant::now() - Duration::from_secs(1);
        assert!(!app.needs_redraw());
        app.message.as_mut().unwrap().at = Instant::now() - Duration::from_secs(10);
        app.tick();
        assert!(app.message.is_none() && app.dirty);
        app.handle_action(Action::Redraw);
        assert!(app.force_clear);
    }

    // ---- REVIEW regression tests (each fails on the integrated code) ----

    #[test]
    fn review_delete_confirmation_deletes_the_named_playlist() {
        let mut app = app_with(2, false);
        app.exec(Command::PlaylistNew("Zed".into())).unwrap();
        app.set_tab(Tab::Playlists);
        app.playlist_view.pane = PlPane::Lists;
        app.playlist_view.list_state.select(Some(Smart::ALL.len() + app.playlists.find("Zed").unwrap()));
        app.handle_action(Action::Remove);
        assert!(matches!(&app.overlay, Some(Overlay::Confirm { message, .. }) if message.contains("Zed")));
        // meanwhile, `orbit ctl playlist new Alpha` sorts in before "Zed"
        app.exec(Command::PlaylistNew("Alpha".into())).unwrap();
        app.handle_event(key(KeyCode::Char('y')));
        let left: Vec<&str> = app.playlists.lists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(left, ["Alpha"], "the dialog asked about \"Zed\"");
    }

    #[test]
    fn review_f_on_the_favorites_list_keeps_favorites() {
        let mut app = app_with(3, false);
        app.library_view.track_state.select(Some(0));
        app.handle_action(Action::ToggleFavorite);
        app.library_view.track_state.select(Some(1));
        app.handle_action(Action::ToggleFavorite);
        let favs = |app: &App| app.lib.all_ids().into_iter().filter(|&id| app.is_favorite(id)).count();
        assert_eq!(favs(&app), 2);
        app.set_tab(Tab::Playlists); // default: lists pane, "Favorites" selected
        assert_eq!(app.selected_entry(), Some(PlaylistEntry::Smart(Smart::Favorites)));
        app.handle_action(Action::ToggleFavorite);
        assert_eq!(favs(&app), 2, "one keypress wiped every favorite: {}", msg(&app));
    }

    #[test]
    fn review_config_eq_preset_sets_the_bands() {
        let mut cfg = test_config();
        cfg.eq.enabled = true;
        cfg.eq.preset = "rock".into(); // bands left at their default (flat), as the docs allow
        let app = app_cfg(cfg, 1, false);
        assert_eq!(app.eq.preset, "rock");
        assert_eq!(Some(app.eq.bands), crate::dsp::eq_preset("rock"), "the EQ shows \"rock\" but is flat");
    }

    #[test]
    fn review_session_restore_keeps_queued_files_outside_the_library() {
        let outside = fixture_dir().join("track11.wav"); // exists on disk, not in the 4-track library
        let mut cfg = test_config();
        cfg.playback.resume_session = true;
        let mut b = App::build(cfg, test_paths(), &crate::Cli::default(), None);
        b.scan = None;
        b.state.session.queue = vec![outside.clone()];
        b.state.session.current = Some(0);
        b.on_scan_done(Library::from_tracks(vec![fixture_dir().to_path_buf()], fake_tracks(4)));
        assert_eq!(b.paths_of(&b.queue.tracks), vec![outside], "the saved queue was dropped on restore");
    }

    #[test]
    fn review_relative_seek_with_a_huge_step_does_not_panic() {
        let (engine, _out) = Engine::new_detached();
        let mut app = app_cfg(test_config(), 2, false);
        app.engine = Some(engine);
        app.queue.set(vec![0], Some(0));
        app.play_queue_current(Duration::from_secs(1), true);
        // `seek +18446744073709549568` parses to a Duration this large
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.exec(Command::Seek(SeekTarget::Forward(Duration::from_secs(u64::MAX))))));
        assert!(r.is_ok(), "seek panicked");
    }
}
