//! Rendering. `draw` lays out the tab bar, the active tab, the player bar and the status line,
//! then any overlay. It reads `App` and records clickable regions in `app.hit`.
//!
//! Everything is themed (`app.theme`, `app.accent()`), bordered per `ui.border` (none in compact
//! mode), CJK-aware (see `text`) and responsive: panes, columns, labels and decorations are dropped
//! in order of importance as the terminal shrinks.

mod eq;
mod library;
mod now_playing;
mod overlays;
mod player_bar;
mod playlists;
mod queue;
mod text;
mod tracks;
mod widgets;

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::app::{App, Hit, MsgKind, Sleep, Tab};
use crate::config::BarPosition;
use crate::keymap::Action;
use crate::library::fmt_duration;
use crate::queue::Repeat;
use text::{spans_width, truncate_spans, width};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let mut hit = Hit::default();
    f.buffer_mut().set_style(area, Style::new().fg(app.theme.fg).bg(app.theme.bg));
    if area.is_empty() {
        app.hit = hit;
        return;
    }
    let tabs_h = u16::from(app.cfg.ui.show_tab_bar && area.height >= 6);
    let status_h = u16::from(area.height >= 3);
    let bar_h = player_bar::height(app, area).min(area.height - tabs_h - status_h);
    let [tabs, middle, status] =
        Layout::vertical([Constraint::Length(tabs_h), Constraint::Fill(1), Constraint::Length(status_h)]).areas(area);
    let (bar, body) = match app.cfg.ui.player_bar {
        BarPosition::Top => {
            let [bar, body] = Layout::vertical([Constraint::Length(bar_h), Constraint::Fill(1)]).areas(middle);
            (bar, body)
        }
        BarPosition::Bottom => {
            let [body, bar] = Layout::vertical([Constraint::Fill(1), Constraint::Length(bar_h)]).areas(middle);
            (bar, body)
        }
    };

    let mut vis = None;
    if !body.is_empty() {
        match app.tab {
            Tab::Library => library::draw(f, app, body, &mut hit),
            Tab::Queue => queue::draw(f, app, body, &mut hit),
            Tab::Playlists => playlists::draw(f, app, body, &mut hit),
            Tab::NowPlaying => vis = now_playing::draw(f, app, body, &mut hit),
            Tab::Equalizer => eq::draw(f.buffer_mut(), app, body, &mut hit),
        }
    }
    if !tabs.is_empty() {
        tab_bar(f.buffer_mut(), app, tabs, &mut hit);
    }
    let mini = player_bar::draw(f.buffer_mut(), app, bar, &mut hit);
    if !status.is_empty() {
        status_line(f.buffer_mut(), app, status, &mut hit);
    }
    overlays::draw(f, app, &mut hit);
    if let Some(n) = vis.or(mini) {
        app.vis_bands = n;
    }
    app.hit = hit;
}

/// A w×h rect centered in `area` (clamped to it).
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn tab_icon(app: &App, t: Tab) -> &'static str {
    match t {
        Tab::Library => app.icons.album,
        Tab::Queue => app.icons.queue,
        Tab::Playlists => app.icons.playlist,
        Tab::NowPlaying => app.icons.track,
        Tab::Equalizer => app.icons.eq,
    }
}

/// "1", "1 Library", "1 ◉" or "1 ◉ Library" depending on `detail` (0..=3).
fn tab_label(app: &App, t: Tab, detail: u8) -> String {
    let n = t.index() + 1;
    match detail {
        3 => format!(" {n} {} {} ", tab_icon(app, t), t.label()),
        2 => format!(" {n} {} ", t.label()),
        1 => format!(" {n} {} ", tab_icon(app, t)),
        _ => format!(" {n} "),
    }
}

/// "◈ ORBIT", the tabs, and the playback state icons on the right. Labels lose their icons, then
/// their text, then the state icons and the name go as the terminal narrows.
fn tab_bar(buf: &mut Buffer, app: &App, area: Rect, hit: &mut Hit) {
    const BRAND: &str = " ◈ ORBIT ";
    let brand_w = width(BRAND);
    let items = state_items(app);
    let items_w = |n: usize| items[..n].iter().map(|(s, _)| spans_width(s) + 2).sum::<usize>();
    let tabs_w = |d: u8| Tab::ALL.iter().map(|t| width(&tab_label(app, *t, d))).sum::<usize>();
    let avail = area.width as usize;
    let (detail, brand, n_items) = [(3, true), (2, true), (2, false), (1, true), (1, false)]
        .into_iter()
        .map(|(d, b)| (d, b, items.len()))
        // then the state items go one at a time, last first
        .chain((0..items.len()).rev().map(|n| (1, false, n)))
        .find(|&(d, b, n)| usize::from(b) * brand_w + tabs_w(d) + items_w(n) <= avail)
        .unwrap_or((0, false, 0));

    let mut x = area.x;
    if brand {
        let spans = vec![
            Span::styled(" ◈ ", Style::new().fg(app.theme.accent2)),
            Span::styled("ORBIT ", widgets::accent(app).add_modifier(Modifier::BOLD)),
        ];
        widgets::line(buf, x, area.y, spans, brand_w as u16);
        x += brand_w as u16;
    }
    for t in Tab::ALL {
        let label = tab_label(app, t, detail);
        let w = (width(&label) as u16).min(area.right().saturating_sub(x));
        if w == 0 {
            break;
        }
        let spans = if t == app.tab {
            let active = Style::new().fg(app.theme.sel_fg).bg(app.accent()).add_modifier(Modifier::BOLD);
            vec![Span::styled(label, active)]
        } else {
            let n = format!(" {}", t.index() + 1);
            let rest = label[n.len()..].to_string();
            vec![Span::styled(n, widgets::dim(app)), Span::styled(rest, Style::new().fg(app.theme.fg))]
        };
        widgets::line(buf, x, area.y, spans, w);
        hit.tabs.push((Rect::new(x, area.y, w, 1), t));
        x += w;
    }
    let mut rx = area.right().saturating_sub(items_w(n_items) as u16) + 1;
    for (spans, action) in items.into_iter().take(n_items) {
        let w = spans_width(&spans) as u16;
        widgets::line(buf, rx, area.y, spans, w);
        if let Some(a) = action {
            hit.buttons.push((Rect::new(rx, area.y, w, 1), a));
        }
        rx += w + 2;
    }
}

/// Playback state indicators, each with the action a click triggers: scan progress, shuffle and
/// repeat (always shown, dim when off), stop-after, sleep countdown, A-B loop, EQ and speed.
fn state_items(app: &App) -> Vec<(Vec<Span<'static>>, Option<Action>)> {
    let i = &app.icons;
    let on = widgets::accent(app);
    let off = widgets::dim(app);
    let mut v: Vec<(Vec<Span<'static>>, Option<Action>)> = Vec::new();
    if let Some((done, total)) = app.scan_progress {
        let pct = (done * 100).checked_div(total).map(|p| format!(" {p}%")).unwrap_or_default();
        v.push((vec![Span::styled(format!("{}{pct}", spinner()), on)], None));
    }
    if let Some((done, total)) = app.download_progress {
        let of = if total > 0 { format!(" {done}/{total}") } else { String::new() };
        v.push((vec![Span::styled(format!("⇣{of}"), on)], None));
    }
    v.push((vec![Span::styled(i.shuffle, if app.queue.shuffle { on } else { off })], Some(Action::ToggleShuffle)));
    let (icon, style) = match app.queue.repeat {
        Repeat::Off => (i.repeat, off),
        Repeat::All => (i.repeat, on),
        Repeat::One => (i.repeat_one, on),
    };
    v.push((vec![Span::styled(icon, style)], Some(Action::CycleRepeat)));
    let warn = Style::new().fg(app.theme.warn);
    if app.queue.stop_after_current {
        v.push((vec![Span::styled(i.stop_after, warn)], Some(Action::ToggleStopAfter)));
    }
    if let Some(s) = app.sleep {
        let left = match s {
            Sleep::At(t) => fmt_duration(t.saturating_duration_since(Instant::now())),
            Sleep::EndOfTrack => "end".into(),
        };
        v.push((vec![Span::styled(format!("{} {left}", i.sleep), warn)], Some(Action::SleepTimer)));
    }
    let ab = match app.ab {
        (Some(_), Some(_)) => Some("A-B"),
        (Some(_), None) => Some("A-"),
        _ => None,
    };
    if let Some(ab) = ab {
        v.push((vec![Span::styled(format!("{} {ab}", i.ab_loop), Style::new().fg(app.theme.accent2))], Some(Action::AbLoop)));
    }
    if app.eq.enabled {
        v.push((vec![Span::styled(format!("{} EQ", i.eq), on)], Some(Action::EqToggle)));
    }
    if (app.speed - 1.0).abs() > 0.001 {
        v.push((vec![Span::styled(format!("{} {:.2}×", i.speed, app.speed), on)], Some(Action::SpeedReset)));
    }
    // last, so it's the first to go when the bar is narrow
    if let Some((done, total)) = app.analysis_progress {
        v.push((vec![Span::styled(format!("≈ {}%", done * 100 / total.max(1)), off)], None));
    }
    v
}

fn spinner() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    FRAMES[(ms / 80 % 10) as usize]
}

/// Key hints for the status line: the global entry points, then what the current tab offers.
fn hint_items(tab: Tab) -> Vec<(&'static [Action], &'static str)> {
    let global: [(&[Action], &str); 3] = [(&[Action::Help], "help"), (&[Action::Search], "search"), (&[Action::CommandPalette], "command")];
    let tab: &[(&[Action], &str)] = match tab {
        Tab::Library => &[
            (&[Action::Select], "play"),
            (&[Action::Enqueue], "enqueue"),
            (&[Action::PlayNext], "play next"),
            (&[Action::AddToPlaylist], "add to playlist"),
            (&[Action::ToggleFavorite], "favorite"),
            (&[Action::CycleBrowseMode], "browse"),
            (&[Action::CycleSort], "sort"),
        ],
        Tab::Queue => &[
            (&[Action::Select], "play"),
            (&[Action::Remove], "remove"),
            (&[Action::MoveUp, Action::MoveDown], "move"),
            (&[Action::ShuffleQueue], "shuffle"),
            (&[Action::SaveQueue], "save"),
            (&[Action::ClearQueue], "clear"),
        ],
        Tab::Playlists => &[
            (&[Action::Select], "play"),
            (&[Action::NewPlaylist], "new"),
            (&[Action::RenamePlaylist], "rename"),
            (&[Action::Remove], "remove"),
            (&[Action::MoveUp, Action::MoveDown], "move"),
        ],
        Tab::NowPlaying => &[
            (&[Action::CycleVisualizer], "visualizer"),
            (&[Action::ToggleLyrics], "lyrics"),
            (&[Action::ToggleArt], "art"),
            (&[Action::LyricsOffsetDown, Action::LyricsOffsetUp], "lyrics offset"),
            (&[Action::ToggleFavorite], "favorite"),
        ],
        Tab::Equalizer => &[
            (&[Action::Left, Action::Right], "band"),
            (&[Action::Up, Action::Down], "gain"),
            (&[Action::EqToggle], "on/off"),
            (&[Action::EqPrevPreset, Action::EqNextPreset], "preset"),
            (&[Action::EqReset], "flatten"),
        ],
    };
    global.into_iter().chain(tab.iter().copied()).collect()
}

/// The transient message (colored by kind), else scan progress, else key hints; on the right the
/// library size (or the state icons when the tab bar is hidden).
fn status_line(buf: &mut Buffer, app: &App, area: Rect, hit: &mut Hit) {
    let th = &app.theme;
    let right: Vec<(Vec<Span<'static>>, Option<Action>)> = if app.cfg.ui.show_tab_bar {
        let n = app.lib.len();
        let s = if n == 1 { "1 track".to_string() } else { format!("{n} tracks") };
        if app.lib.is_empty() { Vec::new() } else { vec![(vec![Span::styled(s, widgets::dim(app))], None)] }
    } else {
        state_items(app)
    };
    let right_w: usize = right.iter().map(|(s, _)| spans_width(s) + 2).sum();
    let left_w = (area.width as usize).saturating_sub(right_w + 1);

    let left: Vec<Span> = if let Some(m) = &app.message {
        let (icon, color) = match m.kind {
            MsgKind::Info => ("•", app.accent()),
            MsgKind::Ok => ("✓", th.ok),
            MsgKind::Warn => ("!", th.warn),
            MsgKind::Error => ("✗", th.error),
        };
        let text = if m.kind == MsgKind::Info { th.fg } else { color };
        vec![
            Span::styled(format!(" {icon} "), Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(m.text.clone(), Style::new().fg(text)),
        ]
    } else if let Some((done, total)) = app.scan_progress {
        let mut v = vec![Span::styled(format!(" {} Scanning", spinner()), widgets::accent(app))];
        const W: usize = 12;
        if let Some(filled) = (done.min(total) * W).checked_div(total) {
            v.push(Span::styled(format!(" {done}/{total} "), Style::new().fg(th.fg)));
            v.push(Span::styled("▰".repeat(filled), widgets::accent(app)));
            v.push(Span::styled("▱".repeat(W - filled), widgets::dim(app)));
            v.push(Span::styled(format!(" {}%", done.min(total) * 100 / total), widgets::dim(app)));
        } else {
            v.push(Span::styled(" your music…", widgets::dim(app)));
        }
        v
    } else {
        let mut v = vec![Span::raw(" ")];
        v.extend(widgets::hints(app, &hint_items(app.tab), left_w.saturating_sub(1)));
        v
    };
    widgets::line(buf, area.x, area.y, truncate_spans(left, left_w), left_w as u16);

    let mut x = area.right().saturating_sub(right_w as u16) + 1;
    for (spans, action) in right {
        let w = spans_width(&spans) as u16;
        widgets::line(buf, x, area.y, spans, w);
        if let Some(a) = action {
            hit.buttons.push((Rect::new(x, area.y, w, 1), a));
        }
        x += w + 2;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::widgets::ListState;

    use super::*;
    use crate::app::{ConfirmAction, Input, ListTarget, Overlay, PromptPurpose};
    use crate::config::{BorderStyle, Column, Config, Paths, ProgressStyle};
    use crate::library::{Library, Track};
    use crate::lyrics::{LyricLine, Lyrics};
    use crate::playlist::Playlist;

    const SIZES: [(u16, u16); 7] = [(80, 24), (120, 40), (200, 60), (40, 12), (20, 6), (8, 3), (1, 1)];

    fn track(i: usize, title: &str, artist: &str, album: &str, secs: u64, folder: &str) -> Track {
        Track {
            path: PathBuf::from(format!("/nonexistent/orbit-ui-test/{folder}/{i:03} {title}.mp3")),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            album_artist: artist.into(),
            genre: "Pop".into(),
            year: Some(2020),
            track_no: Some(i as u32 + 1),
            duration: Duration::from_secs(secs),
            bitrate: Some(320),
            sample_rate: Some(44100),
            channels: Some(2),
            format: "MP3".into(),
            size: 7 << 20,
            folder: folder.into(),
            ..Track::default()
        }
    }

    fn tracks(extra: usize) -> Vec<Track> {
        let base = [
            ("夜に駆ける", "YOASOBI", "THE BOOK", 261, "Jpop"),
            ("Lemon", "米津玄師", "STRAY SHEEP", 255, "Jpop"),
            ("좋은 날", "아이유", "Real", 233, "Kpop"),
            ("Dynamite", "BTS", "BE", 199, "Kpop"),
            ("白日", "King Gnu", "CEREMONY", 275, "Jpop"),
            ("Pretender", "Official髭男dism", "Traveler", 327, "Jpop"),
            ("사건의 지평선", "윤하", "END THEORY", 301, "Kpop"),
            ("A Very Long Title That Will Certainly Not Fit In Any Column At All", "Some Artist", "Some Album", 3725, "tmp"),
            // decomposed Hangul (NFD), as macOS file names often are
            ("\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}", "NFD 아티스트", "Album", 180, "Kpop"),
        ];
        let mut v: Vec<Track> = base.iter().enumerate().map(|(i, (t, a, al, s, f))| track(i, t, a, al, *s, f)).collect();
        for i in 0..extra {
            v.push(track(v.len(), &format!("Track {i} 曲"), "Filler", "Filler Album", 200 + i as u64 % 120, "tmp"));
        }
        v
    }

    /// A hermetic App: temp paths, no library folders (no scan of real files), no IPC, volume 0,
    /// no session restore. The library is then replaced with `tracks(extra)`.
    fn app(extra: usize) -> App {
        // one fixed scratch dir (reused by every run) in case the app creates its folders
        let base = std::env::temp_dir().join("orbit-ui-tests");
        let paths = Paths {
            config_file: base.join("config.toml"),
            config_dir: base.clone(),
            data_dir: base.join("data"),
            cache_dir: base.join("cache"),
            state_file: base.join("data/state.json"),
            playlists_dir: base.join("data/playlists"),
            library_cache: base.join("cache/library.json"),
            socket: base.join("orbit.sock"),
        };
        let mut cfg = Config::default();
        cfg.library.dirs.clear();
        cfg.library.use_cache = false;
        cfg.ipc.enabled = false;
        let cli = crate::Cli { volume: Some(0), no_resume: true, ..crate::Cli::default() };
        let mut app = App::new(cfg, paths, &cli).expect("app");
        app.scan = None;
        app.scan_progress = None;
        app.message = None;
        app.lib = Library::from_tracks(Vec::new(), tracks(extra));
        let v = &mut app.library_view;
        v.groups = app.lib.groups(v.mode);
        v.group_state.select(Some(0));
        // every track, whatever grouping the library implements
        v.tracks = app.lib.all_ids();
        v.track_state.select(Some(1));
        app.queue.set(app.lib.all_ids(), Some(2));
        app.queue_state.select(Some(3));
        app.playlists.lists.push(Playlist {
            name: "드라이브 Drive".into(),
            path: PathBuf::from("/nonexistent/p.m3u8"),
            tracks: Vec::new(),
            external: false,
        });
        app.playlists.lists.push(Playlist {
            name: "Imported".into(),
            path: PathBuf::from("/nonexistent/i.m3u"),
            tracks: Vec::new(),
            external: true,
        });
        app.playlist_view.list_state.select(Some(5));
        app.playlist_view.tracks = vec![0, 2, 4];
        app.playlist_view.missing = 2;
        app
    }

    /// Plays track `id` when the engine allows it (the silent test engine does; a real one can't
    /// open the fake path, so assertions about the playing track must check `now_playing()`).
    fn play(app: &mut App, id: usize) {
        let t = app.lib.get(id).cloned().expect("track");
        if let Some(e) = app.engine.as_mut() {
            let _ = e.load(&t, Duration::from_secs(40), true);
        }
    }

    fn render(app: &mut App, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        term.draw(|f| draw(f, app)).expect("draw");
        term
    }

    /// Row `y` as text, skipping the cells hidden behind wide characters; with the x of each char.
    fn row(term: &Terminal<TestBackend>, y: u16) -> (String, Vec<u16>) {
        let buf = term.backend().buffer();
        let (mut s, mut xs, mut x) = (String::new(), Vec::new(), 0);
        while x < buf.area.width {
            let sym = buf[(x, y)].symbol();
            for c in sym.chars() {
                s.push(c);
                xs.push(x);
            }
            x += (width(sym) as u16).max(1);
        }
        (s, xs)
    }

    fn screen(term: &Terminal<TestBackend>) -> String {
        (0..term.backend().buffer().area.height).map(|y| row(term, y).0 + "\n").collect()
    }

    /// x of the first occurrence of `needle` on row `y`.
    fn find_x(term: &Terminal<TestBackend>, y: u16, needle: &str) -> Option<u16> {
        let (s, xs) = row(term, y);
        s.find(needle).map(|b| xs[s[..b].chars().count()])
    }

    fn overlays() -> Vec<Overlay> {
        vec![
            Overlay::Help { scroll: 5 },
            Overlay::Help { scroll: 10_000 },
            Overlay::Search {
                input: Input::with("夜 yoasobi"),
                results: vec![
                    crate::search::Hit { id: 0, score: 9, matched: vec![0, 6, 7, 8] },
                    crate::search::Hit { id: 8, score: 5, matched: vec![1] },
                ],
                state: ListState::default().with_selected(Some(0)),
            },
            Overlay::Search { input: Input::default(), results: Vec::new(), state: ListState::default() },
            Overlay::Command {
                input: Input::with("vo"),
                completions: vec!["vol".into(), "view folders".into(), "view artists".into()],
                selected: Some(1),
                history_pos: None,
            },
            Overlay::Command { input: Input::with("seek 1:23"), completions: Vec::new(), selected: None, history_pos: Some(0) },
            Overlay::PickPlaylist { tracks: vec![0, 1], state: ListState::default().with_selected(Some(1)) },
            Overlay::Prompt {
                title: "Save queue as".into(), input: Input::with("밤 산책 night walk"), purpose: PromptPurpose::SaveQueue
            },
            Overlay::Confirm { message: "Clear the queue (9 tracks)? This can't be undone.".into(), action: ConfirmAction::ClearQueue },
        ]
    }

    #[test]
    fn every_tab_and_overlay_at_every_size() {
        let mut app = app(40);
        play(&mut app, 0);
        app.lyrics = Some(Lyrics {
            lines: ["first line", "", "夜に駆ける 二番目の行はとても長いので折り返されるはずです", "last"]
                .iter()
                .enumerate()
                .map(|(i, t)| LyricLine { time: Some(Duration::from_secs(10 * i as u64)), text: t.to_string() })
                .collect(),
            synced: true,
            source: "song.lrc".into(),
            offset_ms: 0,
        });
        for tab in Tab::ALL {
            app.tab = tab;
            for (w, h) in SIZES {
                app.overlay = None;
                render(&mut app, w, h);
                for o in overlays() {
                    app.overlay = Some(o);
                    render(&mut app, w, h);
                }
            }
        }
    }

    #[test]
    fn config_variants_render() {
        let variants: [fn(&mut App); 7] = [
            |a| a.compact = true,
            |a| a.cfg.ui.border = BorderStyle::None,
            |a| a.cfg.ui.border = BorderStyle::Thick,
            |a| {
                a.cfg.ui.player_bar = BarPosition::Top;
                a.cfg.ui.show_tab_bar = false;
            },
            |a| {
                a.cfg.ui.columns = vec![
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
                ]
            },
            |a| {
                a.cfg.ui.columns.clear();
                a.show_art = false;
                a.show_lyrics = false;
                a.mini_vis = false;
            },
            |a| {
                a.eq.enabled = true;
                a.eq.bands = [12.0, 9.0, 4.5, 0.0, -3.0, -12.0, 1.0, 6.5, -7.0, 11.0];
                a.eq.preamp_db = -4.0;
                a.eq.preset = "custom".into();
                a.muted = true;
                a.speed = 1.25;
                a.ab = (Some(Duration::from_secs(20)), Some(Duration::from_secs(60)));
                a.queue.stop_after_current = true;
                a.sleep = Some(Sleep::At(Instant::now() + Duration::from_secs(900)));
                a.scan_progress = Some((120, 376));
            },
        ];
        for (i, variant) in variants.iter().enumerate() {
            let mut app = app(30);
            play(&mut app, 2);
            variant(&mut app);
            for style in [ProgressStyle::Line, ProgressStyle::Block, ProgressStyle::Segments, ProgressStyle::Dots, ProgressStyle::Gradient]
            {
                app.cfg.ui.progress = style;
                for tab in Tab::ALL {
                    app.tab = tab;
                    for (w, h) in SIZES {
                        let term = render(&mut app, w, h);
                        if i == 1 && w >= 80 {
                            assert!(!screen(&term).contains('╭'), "borderless mode drew a border");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cjk_rows_line_up() {
        let mut app = app(0);
        app.cfg.ui.columns = vec![Column::Index, Column::Title, Column::Artist, Column::Album, Column::Duration];
        let term = render(&mut app, 120, 40);
        let body = app.hit.lists.iter().find(|l| l.target == ListTarget::LibTracks).expect("tracks list").area;
        let mut artist_x = Vec::new();
        let mut time_end = Vec::new();
        for (i, y) in (body.top()..body.bottom()).enumerate().take(app.library_view.tracks.len()) {
            let t = app.lib.get(app.library_view.tracks[i]).unwrap();
            let artist: String = t.artist.chars().take(6).collect();
            artist_x.push(find_x(&term, y, &artist).unwrap_or_else(|| panic!("artist of row {i} missing:\n{}", screen(&term))));
            let dur = fmt_duration(t.duration);
            time_end.push(find_x(&term, y, &dur).expect("duration") + dur.len() as u16);
        }
        assert!(artist_x.len() >= 9);
        assert!(artist_x.windows(2).all(|w| w[0] == w[1]), "artist column misaligned: {artist_x:?}\n{}", screen(&term));
        assert!(time_end.windows(2).all(|w| w[0] == w[1]), "durations not right-aligned: {time_end:?}");
        // the long title is cut with an ellipsis, not wrapped or overflowing into the artist column
        assert!(screen(&term).contains('…'));
    }

    #[test]
    fn key_elements_and_hit_regions() {
        let mut app = app(100);
        play(&mut app, 0);
        let term = render(&mut app, 120, 40);
        let s = screen(&term);
        for needle in
            ["◈ ORBIT", "Library", "Queue", "Playlists", "Now Playing", "Equalizer", "Browse", "Title", "Artist", "夜に駆ける", "help"]
        {
            assert!(s.contains(needle), "missing {needle:?}:\n{s}");
        }
        let hit = &app.hit;
        assert_eq!(hit.tabs.len(), 5);
        assert!(hit.tabs.windows(2).all(|w| w[0].0.right() <= w[1].0.x));
        let progress = hit.progress.expect("progress");
        assert!(progress.width > 20 && progress.height == 1);
        // the volume slider is the 8-cell level ramp, right of the speaker icon (which mutes)
        let volume = hit.volume.expect("volume");
        assert_eq!(volume.width, 8);
        let mute = hit.buttons.iter().find(|(_, a)| *a == Action::Mute).expect("mute button").0;
        assert!(mute.right() < volume.x && mute.y == volume.y);
        for a in [Action::Prev, Action::TogglePause, Action::Next, Action::ToggleShuffle, Action::CycleRepeat] {
            assert!(hit.buttons.iter().any(|(_, b)| *b == a), "no button for {a:?}");
        }
        let targets: Vec<ListTarget> = hit.lists.iter().map(|l| l.target).collect();
        assert_eq!(targets, [ListTarget::LibModes, ListTarget::LibGroups, ListTarget::LibTracks]);
        let tracks = hit.lists[2];
        assert_eq!(tracks.len, app.library_view.tracks.len());
        assert_eq!(tracks.offset, app.library_view.track_state.offset());
        // the body excludes the header: its first row shows the first visible track
        let first = app.lib.get(app.library_view.tracks[tracks.offset]).unwrap();
        assert!(row(&term, tracks.area.y).0.contains(&first.title));
        if app.now_playing().is_some() {
            assert!(s.contains(app.icons.playing_marker));
        }

        app.tab = Tab::Equalizer;
        render(&mut app, 120, 40);
        let sliders = &app.hit.eq_sliders;
        assert_eq!(sliders.iter().map(|(_, i)| *i).collect::<Vec<_>>(), (0..11).collect::<Vec<_>>());
        assert!(sliders.windows(2).all(|w| w[0].0.right() <= w[1].0.x && w[0].0.height == w[1].0.height));

        app.tab = Tab::NowPlaying;
        render(&mut app, 120, 40);
        assert!(app.hit.lyrics.is_some());
        assert!(app.vis_bands > 10);

        app.overlay = Some(Overlay::PickPlaylist { tracks: vec![1], state: ListState::default().with_selected(Some(0)) });
        let term = render(&mut app, 120, 40);
        assert!(app.hit.overlay.is_some());
        let pick = app.hit.lists.iter().find(|l| l.target == ListTarget::PickPlaylist).expect("picker");
        assert_eq!(pick.len, 3);
        assert!(row(&term, pick.area.y).0.contains("New playlist"));
    }

    #[test]
    fn inputs_place_the_cursor() {
        let mut app = app(0);
        app.overlay = Some(Overlay::Command { input: Input::with("vol 5"), completions: Vec::new(), selected: None, history_pos: None });
        let mut term = render(&mut app, 80, 24);
        assert_eq!(term.get_cursor_position().unwrap(), (6, 23).into());
        assert!(row(&term, 23).0.starts_with(":vol 5"));
        // wide characters count two cells
        app.overlay = Some(Overlay::Search { input: Input::with("夜に"), results: Vec::new(), state: ListState::default() });
        let mut term = render(&mut app, 80, 24);
        let pos = term.get_cursor_position().unwrap();
        let query_x = find_x(&term, pos.y, "夜に").expect("query shown");
        assert_eq!(pos.x, query_x + 4);
    }

    #[test]
    fn theme_background_fills_everything() {
        let mut app = app(0);
        app.theme.bg = Color::Rgb(1, 2, 3);
        for tab in Tab::ALL {
            app.tab = tab;
            for o in [None, Some(Overlay::Help { scroll: 0 })] {
                app.overlay = o;
                let term = render(&mut app, 100, 30);
                let buf = term.backend().buffer();
                // cells hidden behind wide characters are never drawn; skip them
                let mut reset = 0;
                for y in 0..buf.area.height {
                    let mut x = 0;
                    while x < buf.area.width {
                        let c = &buf[(x, y)];
                        reset += usize::from(c.bg == Color::Reset);
                        x += (width(c.symbol()) as u16).max(1);
                    }
                }
                assert_eq!(reset, 0, "{tab:?}: {reset} cells lost the theme background");
            }
        }
    }

    #[test]
    fn large_library_draws_only_visible_rows_fast() {
        let mut app = app(10_000);
        app.library_view.track_state.select(Some(5_000));
        render(&mut app, 200, 60);
        let t0 = Instant::now();
        let frames = 20;
        for _ in 0..frames {
            render(&mut app, 200, 60);
        }
        let per_frame = t0.elapsed() / frames;
        let list = app.hit.lists.iter().find(|l| l.target == ListTarget::LibTracks).unwrap();
        assert!(list.offset <= 5_000 && 5_000 < list.offset + list.area.height as usize);
        assert_eq!(list.len, 10_009);
        println!("200x60 library frame with 10k tracks: {per_frame:?} (debug build, incl. TestBackend)");
        assert!(per_frame < Duration::from_millis(60), "{per_frame:?}");
    }

    #[test]
    fn empty_states() {
        let mut app = app(0);
        app.lib = Library::empty();
        app.library_view.groups.clear();
        app.library_view.tracks.clear();
        app.queue.clear();
        app.playlist_view.tracks.clear();
        app.playlist_view.missing = 0;
        let s = screen(&render(&mut app, 120, 40));
        assert!(s.contains("No music found") && s.contains("config.toml"), "{s}");
        app.scan_progress = Some((120, 376));
        let s = screen(&render(&mut app, 120, 40));
        assert!(s.contains("Scanning") && s.contains("120"), "{s}");
        app.tab = Tab::Queue;
        assert!(screen(&render(&mut app, 120, 40)).contains("queue is empty"));
        app.tab = Tab::NowPlaying;
        assert!(screen(&render(&mut app, 120, 40)).contains("Nothing playing"));
    }
}
