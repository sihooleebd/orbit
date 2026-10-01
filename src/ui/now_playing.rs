//! Now Playing tab: album art, track details, lyrics (synced lines follow playback) and the big
//! visualizer. On small terminals the art goes first, then the lyrics.

use std::cmp::Reverse;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::text::{home_relative, marquee, truncate_start, width, wrap};
use super::widgets;
use crate::app::{App, Hit, Sleep};
use crate::config::Align;
use crate::keymap::Action;
use crate::library::{Track, fmt_duration};
use crate::lyrics::Lyrics;
use crate::visualizer::{VisMode, Visualizer, band_count};

struct Plan {
    info: Rect,
    /// Art rows (the image is twice as many columns wide); 0 = no art.
    art_rows: u16,
    lyrics: Option<Rect>,
    vis: Option<Rect>,
}

/// Returns the number of visualizer bands shown (None when the visualizer didn't fit).
pub fn draw(f: &mut Frame, app: &mut App, area: Rect, hit: &mut Hit) -> Option<usize> {
    let plan = plan(app, area);
    let block = info_block(app, plan.info.width);
    let inner = block.inner(plan.info);
    block.render(plan.info, f.buffer_mut());
    let mut text_area = inner;
    if plan.art_rows > 0 {
        let art_w = plan.art_rows * 2;
        app.art.render(Rect::new(inner.x, inner.y, art_w.min(inner.width), plan.art_rows.min(inner.height)), f.buffer_mut(), &app.theme);
        text_area = Rect { x: inner.x + art_w + 3, width: inner.width.saturating_sub(art_w + 3), ..inner };
    }
    let app = &*app;
    let buf = f.buffer_mut();
    details(buf, app, text_area);
    if let Some(rect) = plan.lyrics {
        lyrics_pane(buf, app, rect, hit);
    }
    plan.vis.map(|rect| visualizer(buf, app, rect))
}

/// Lyrics at the side from ~72 columns (stacked under the details on tall narrow terminals), art
/// when the details still get ~28 columns, the visualizer in what's left (if at least a few rows).
fn plan(app: &App, area: Rect) -> Plan {
    let bordered = widgets::borders(app).is_some();
    let chrome = if bordered { 2 } else { 1 };
    let side = app.show_lyrics && area.width >= 72;
    let (main, mut lyrics) = if side {
        let lw = (u32::from(area.width) * 2 / 5).clamp(30, 72) as u16;
        let gap = u16::from(!bordered);
        let [m, l] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(lw)]).spacing(gap).areas(area);
        (m, Some(l))
    } else {
        (area, None)
    };
    let mut art_rows = 0;
    if app.show_art && app.now_playing().is_some() {
        let max = (main.height.saturating_sub(chrome) * 3 / 5).min(20);
        art_rows = (6..=max).rev().find(|r| main.width >= r * 2 + 3 + 4 + 28 && main.height >= r + chrome + 4).unwrap_or(0);
    }
    let mut info_h = if art_rows > 0 { art_rows + chrome } else { (11 + chrome).min(main.height) };
    let mut rest = main.height - info_h;
    let mut lyrics_h = 0;
    if app.show_lyrics && !side && rest >= 8 {
        lyrics_h = if rest >= 16 { rest * 3 / 5 } else { rest };
        rest -= lyrics_h;
    }
    if rest < chrome + 2 {
        // too little for a visualizer: give the rows back
        if lyrics_h > 0 {
            lyrics_h += rest;
        } else {
            info_h += rest;
        }
        rest = 0;
    }
    let [info, lyr, vis] =
        Layout::vertical([Constraint::Length(info_h), Constraint::Length(lyrics_h), Constraint::Length(rest)]).areas(main);
    if lyrics_h > 0 {
        lyrics = Some(lyr);
    }
    Plan { info, art_rows, lyrics, vis: (rest > 0).then_some(vis) }
}

fn info_block(app: &App, w: u16) -> ratatui::widgets::Block<'static> {
    let q = &app.queue;
    let right = match q.current {
        Some(c) if app.now_playing().is_some() => vec![Span::styled(format!("{} / {}", c + 1, q.len()), widgets::dim(app))],
        _ => Vec::new(),
    };
    widgets::titles(widgets::panel(app, false), vec![Span::raw(format!("{} Now Playing", app.icons.track))], right, w)
}

/// Title, artist, album, genre, technical details, stats, file and what's next — lower-priority
/// lines are dropped when the area is short; vertically centered when there's room to spare.
fn details(buf: &mut Buffer, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let Some(t) = app.now_playing() else {
        widgets::empty_state(buf, area, nothing_playing(app));
        return;
    };
    let w = area.width as usize;
    let th = &app.theme;
    let dim = widgets::dim(app);
    let fg = Style::new().fg(th.fg);
    let icon = |s: &'static str| Span::styled(format!("{s} "), Style::new().fg(th.accent2));
    let mut items: Vec<(u8, Line<'static>)> = Vec::new();
    let title = Span::styled(t.title.clone(), widgets::accent(app).add_modifier(Modifier::BOLD));
    items.push((10, Line::from(marquee(vec![title], w, app.position()))));
    items.push((9, Line::from(vec![icon(app.icons.artist), Span::styled(t.artist.clone(), Style::new().fg(th.accent2))])));
    let mut album = vec![icon(app.icons.album), Span::styled(t.album.clone(), fg)];
    if let Some(y) = t.year {
        album.push(Span::styled(format!(" · {y}"), dim));
    }
    items.push((8, Line::from(album)));
    if !t.genre.is_empty() {
        items.push((5, Line::from(vec![icon(app.icons.genre), Span::styled(t.genre.clone(), dim)])));
    }
    if t.album_artist != t.artist && !t.album_artist.is_empty() {
        items.push((3, Line::from(vec![Span::styled("album by ", dim), Span::styled(t.album_artist.clone(), fg)])));
    }
    items.push((2, Line::from("")));
    items.push((6, Line::styled(tech(t), dim)));
    items.push((7, Line::from(stats(app, t))));
    items.push((1, Line::from("")));
    items.push((4, Line::styled(truncate_start(&home_relative(&t.path), w).into_owned(), dim)));
    if let Some(next) = app.queue.peek_advance().and_then(|id| app.track(id)).filter(|_| !app.queue.stop_after_current && app.sleep != Some(Sleep::EndOfTrack)) {
        items.push((1, Line::from("")));
        items.push((
            3,
            Line::from(vec![
                Span::styled("next  ", dim),
                Span::styled(next.title.clone(), fg),
                Span::styled(" — ", dim),
                Span::styled(next.artist.clone(), Style::new().fg(th.accent2)),
            ]),
        ));
    }
    // keep the most important lines that fit, in their original order
    let mut keep: Vec<usize> = (0..items.len()).collect();
    keep.sort_by_key(|&i| Reverse(items[i].0));
    keep.truncate(area.height as usize);
    keep.sort_unstable();
    let mut lines: Vec<Line> = keep.into_iter().map(|i| items[i].1.clone()).collect();
    while lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
    let top = area.y + (area.height - lines.len() as u16) / 2;
    for (k, l) in lines.into_iter().enumerate() {
        widgets::line(buf, area.x, top + k as u16, super::text::truncate_spans(l.spans, w), area.width);
    }
}

fn nothing_playing(app: &App) -> Vec<Line<'static>> {
    let dim = widgets::dim(app);
    let key = |a: Action| app.keymap.keys_for(a).into_iter().next();
    let mut lines = vec![
        Line::styled(format!("{} Nothing playing", app.icons.track), widgets::accent(app).add_modifier(Modifier::BOLD)),
        Line::from(""),
    ];
    if let Some(k) = key(Action::Select) {
        lines.push(Line::from(vec![Span::styled("Pick a track in the Library and press ", dim), Span::styled(k, widgets::accent(app))]));
    }
    if let Some(k) = key(Action::Search) {
        lines.push(Line::from(vec![Span::styled("or search the whole library with ", dim), Span::styled(k, widgets::accent(app))]));
    }
    lines
}

/// "MP3 · 320 kbps · 44.1 kHz · stereo · 7.2 MB"
fn tech(t: &Track) -> String {
    let mut parts = Vec::new();
    if !t.format.is_empty() {
        parts.push(t.format.clone());
    }
    if let Some(b) = t.bitrate {
        parts.push(format!("{b} kbps"));
    }
    if let Some(sr) = t.sample_rate {
        parts.push(if sr % 1000 == 0 { format!("{} kHz", sr / 1000) } else { format!("{:.1} kHz", sr as f32 / 1000.0) });
    }
    match t.channels {
        Some(1) => parts.push("mono".into()),
        Some(2) => parts.push("stereo".into()),
        Some(n) => parts.push(format!("{n} ch")),
        None => {}
    }
    if t.size > 0 {
        parts.push(if t.size >= 1 << 20 { format!("{:.1} MB", t.size as f64 / (1 << 20) as f64) } else { format!("{} KB", t.size / 1024) });
    }
    parts.push(fmt_duration(t.duration));
    parts.join(" · ")
}

/// "♥ favorite · 12 plays · track 3 · disc 2"
fn stats(app: &App, t: &Track) -> Vec<Span<'static>> {
    let dim = widgets::dim(app);
    let mut v = Vec::new();
    if app.is_favorite(t.id) {
        v.push(Span::styled(format!("{} favorite", app.icons.favorite), Style::new().fg(app.theme.accent2)));
        v.push(Span::styled(" · ", dim));
    }
    let plays = app.plays(t.id);
    v.push(Span::styled(
        match plays {
            0 => "never played".to_string(),
            1 => "1 play".to_string(),
            n => format!("{n} plays"),
        },
        Style::new().fg(app.theme.fg),
    ));
    if let Some(n) = t.track_no {
        v.push(Span::styled(format!(" · track {n}"), dim));
    }
    if let Some(d) = t.disc_no.filter(|d| *d > 1) {
        v.push(Span::styled(format!(" · disc {d}"), dim));
    }
    v
}

fn lyrics_pane(buf: &mut Buffer, app: &App, rect: Rect, hit: &mut Hit) {
    let dim = widgets::dim(app);
    let mut right = Vec::new();
    if let Some(ly) = &app.lyrics {
        let mut parts = vec![if ly.synced { "synced" } else { "plain" }.to_string()];
        if !ly.source.is_empty() {
            parts.push(ly.source.clone());
        }
        if app.lyrics_offset_ms != 0 {
            parts.push(format!("{:+} ms", app.lyrics_offset_ms));
        }
        right.push(Span::styled(parts.join(" · "), dim));
    }
    let block = widgets::titles(widgets::panel(app, false), vec![Span::raw(format!("{} Lyrics", app.icons.lyrics))], right, rect.width);
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.is_empty() {
        return;
    }
    hit.lyrics = Some(inner);
    match &app.lyrics {
        None => {
            let mut lines = vec![Line::styled("No lyrics", dim)];
            if app.now_playing().is_some() && app.cfg.lyrics.enabled {
                lines.push(Line::styled("(no .lrc file or embedded lyrics found)", dim));
            }
            widgets::empty_state(buf, inner, lines);
        }
        Some(ly) if ly.lines.is_empty() => widgets::empty_state(buf, inner, vec![Line::styled("No lyrics", dim)]),
        Some(ly) if ly.synced && app.lyrics_following() => follow(buf, app, inner, ly),
        Some(ly) => scrolled(buf, app, rect, inner, ly),
    }
}

/// Synced lyrics following playback: the current line centered, bold, in `lyric_active`; the
/// others in `lyric`, fading with distance.
fn follow(buf: &mut Buffer, app: &App, area: Rect, ly: &Lyrics) {
    let w = area.width as usize;
    let h = area.height as i32;
    // every line of the current timestamp lights up (an original line and its translation)
    let range = ly.current_range(app.position(), app.lyrics_offset_ms);
    let active = |i: usize| range.as_ref().is_some_and(|r| r.contains(&i));
    let anchor = range.as_ref().map_or(0, |r| r.start).min(ly.lines.len() - 1);
    let text = |i: usize| {
        let t = ly.lines[i].text.trim();
        if t.is_empty() && active(i) { "♪" } else { t }
    };
    let wrapped: Vec<Vec<String>> = (0..ly.lines.len()).map(|i| wrap(text(i), w)).collect();
    let rows = |i: usize| wrapped[i].len() as i32;
    let mut draw = |i: usize, y: i32| {
        let style = lyric_style(app, if active(i) { 0 } else { i.abs_diff(anchor) }, active(i));
        for (r, s) in wrapped[i].iter().enumerate() {
            let yy = y + r as i32;
            if (0..h).contains(&yy) {
                aligned(buf, app, area, area.y + yy as u16, s, style);
            }
        }
    };
    let anchor_y = h / 2 - rows(anchor) / 2;
    draw(anchor, anchor_y);
    let mut y = anchor_y;
    for i in (0..anchor).rev() {
        y -= rows(i);
        if y + rows(i) <= 0 {
            break;
        }
        draw(i, y);
    }
    let mut y = anchor_y + rows(anchor);
    for i in anchor + 1..ly.lines.len() {
        if y >= h {
            break;
        }
        draw(i, y);
        y += rows(i);
    }
}

/// Plain lyrics (or synced ones the user scrolled): from lyric line `app.lyrics_scroll` on (never
/// scrolled past the last page), with a scrollbar.
fn scrolled(buf: &mut Buffer, app: &App, outer: Rect, inner: Rect, ly: &Lyrics) {
    let cur = if ly.synced { ly.current(app.position(), app.lyrics_offset_ms) } else { None };
    let wrap_w = |w: u16| {
        ly.lines.iter().enumerate().flat_map(|(i, l)| wrap(l.text.trim(), w as usize).into_iter().map(move |s| (i, s))).collect::<Vec<_>>()
    };
    let mut rows = wrap_w(inner.width);
    let (area, col) = widgets::scroll_split(app, outer, inner, rows.len() > inner.height as usize);
    if area.width != inner.width {
        rows = wrap_w(area.width);
    }
    let first = rows.iter().position(|(i, _)| *i >= app.lyrics_scroll as usize).unwrap_or(rows.len());
    let offset = first.min(rows.len().saturating_sub(area.height as usize));
    let plain = Style::new().fg(if ly.synced { app.theme.lyric } else { app.theme.fg });
    for (k, (i, s)) in rows.iter().skip(offset).take(area.height as usize).enumerate() {
        let style = if cur == Some(*i) { lyric_style(app, 0, true) } else { plain };
        aligned(buf, app, area, area.y + k as u16, s, style);
    }
    widgets::scrollbar(buf, app, col, rows.len(), offset, false);
}

fn aligned(buf: &mut Buffer, app: &App, area: Rect, y: u16, s: &str, style: Style) {
    let x = match app.cfg.ui.lyrics_align {
        Align::Center => area.x + (area.width.saturating_sub(width(s) as u16)) / 2,
        Align::Left => area.x,
    };
    buf.set_stringn(x, y, s, area.right().saturating_sub(x) as usize, style);
}

/// `lyric_active` + bold for the current line; `lyric` for the others, blended toward the
/// background with distance (dimmed instead when the colors aren't RGB).
fn lyric_style(app: &App, distance: usize, active: bool) -> Style {
    let th = &app.theme;
    if active {
        return Style::new().fg(th.lyric_active).add_modifier(Modifier::BOLD);
    }
    let t = distance.saturating_sub(1).min(4) as f32 / 6.0;
    match blend(th.lyric, th.bg, t) {
        Some(c) => Style::new().fg(c),
        None if distance >= 3 => Style::new().fg(th.lyric).add_modifier(Modifier::DIM),
        None => Style::new().fg(th.lyric),
    }
}

/// `a` moved toward `b` by `t` (0..=1), for RGB colors.
fn blend(a: Color, b: Color, t: f32) -> Option<Color> {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Some(Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2)))
        }
        _ => None,
    }
}

/// The big visualizer, centered; returns its band count (what `app.vis_bands` should be).
fn visualizer(buf: &mut Buffer, app: &App, rect: Rect) -> usize {
    let right = vec![Span::styled(app.vis_mode.label(), widgets::accent(app))];
    let block = widgets::titles(widgets::panel(app, false), vec![Span::raw("Visualizer")], right, rect.width);
    let inner = block.inner(rect);
    block.render(rect, buf);
    let cfg = &app.cfg.visualizer;
    let n = band_count(inner.width, cfg);
    if inner.is_empty() {
        return n;
    }
    let per = (cfg.bar_width + cfg.bar_gap).max(1);
    let used = ((n as u16).saturating_mul(per)).saturating_sub(cfg.bar_gap).clamp(1, inner.width);
    let area = if cfg.bars == 0 { Rect { x: inner.x + (inner.width - used) / 2, width: used, ..inner } } else { inner };
    if !app.analyzer.active() && app.vis_mode != VisMode::Cassette {
        let base = "▁".repeat(inner.width as usize);
        buf.set_stringn(inner.x, inner.bottom() - 1, base, inner.width as usize, Style::new().fg(app.theme.border));
    }
    Visualizer { analyzer: &app.analyzer, cfg, theme: &app.theme, mode: app.vis_mode }.render(area, buf);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending() {
        assert_eq!(blend(Color::Rgb(200, 100, 0), Color::Rgb(0, 0, 0), 0.5), Some(Color::Rgb(100, 50, 0)));
        assert_eq!(blend(Color::Rgb(200, 100, 0), Color::Rgb(0, 0, 0), 0.0), Some(Color::Rgb(200, 100, 0)));
        assert_eq!(blend(Color::DarkGray, Color::Reset, 0.5), None);
    }

    #[test]
    fn tech_line() {
        let t = Track {
            format: "MP3".into(),
            bitrate: Some(320),
            sample_rate: Some(44100),
            channels: Some(2),
            size: 7 << 20,
            duration: std::time::Duration::from_secs(185),
            ..Track::default()
        };
        assert_eq!(tech(&t), "MP3 · 320 kbps · 44.1 kHz · stereo · 7.0 MB · 3:05");
    }
}
