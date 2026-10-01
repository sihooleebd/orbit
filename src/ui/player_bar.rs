//! The player bar: play state, the track (scrolling when it doesn't fit), a mini spectrum,
//! transport buttons, volume, and the progress bar (five styles) with A-B loop markers.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Widget;

use super::text::{fit_spans, marquee, spans_width, truncate_spans, width};
use super::widgets::{self, put};
use crate::app::{App, Hit, Sleep};
use crate::config::ProgressStyle;
use crate::keymap::Action;
use crate::library::fmt_duration;
use crate::player::PlayState;

/// Rows the bar takes: a bordered box with two lines when there's room, two bare lines on small
/// terminals, one line on tiny ones.
pub fn height(app: &App, area: Rect) -> u16 {
    if widgets::borders(app).is_some() && area.height >= 20 {
        4
    } else if area.height >= 10 {
        2
    } else {
        1
    }
}

/// Draws the bar; returns how many spectrum bands the mini visualizer shows (if it's visible).
pub fn draw(buf: &mut Buffer, app: &App, area: Rect, hit: &mut Hit) -> Option<usize> {
    if area.is_empty() {
        return None;
    }
    let inner = if area.height >= 4 && widgets::borders(app).is_some() {
        let block = widgets::titles_bottom(app, widgets::panel(app, false), Vec::new(), up_next(app), area.width);
        let inner = block.inner(area);
        block.render(area, buf);
        inner
    } else {
        let pad = u16::from(!app.compact);
        Rect { x: area.x + pad, width: area.width.saturating_sub(2 * pad), ..area }
    };
    if inner.is_empty() {
        return None;
    }
    if inner.height == 1 {
        one_line(buf, app, inner, hit);
        return None;
    }

    let (top, bottom) = (inner.y, inner.y + 1);
    let w = inner.width;
    let buttons = buttons(app);
    let buttons_w = buttons.iter().map(|(s, _)| width(s)).sum::<usize>() as u16 + 2 * (buttons.len() as u16 - 1);
    let (volume, slider) = volume(app, w >= 60);
    let volume_w = spans_width(&volume) as u16;
    let right_w = if w >= 40 { buttons_w.max(volume_w) } else { volume_w };
    let right_x = inner.right().saturating_sub(right_w);

    let mut left_end = right_x.saturating_sub(2);
    let mut bands = None;
    if app.mini_vis && w >= 70 {
        let vw = (w / 6).clamp(10, 28);
        let vis = Rect::new(right_x.saturating_sub(vw + 3), top, vw, 2);
        spectrum(buf, app, vis);
        bands = Some(vw as usize);
        left_end = vis.x.saturating_sub(3);
    }
    let left = Rect::new(inner.x, top, left_end.saturating_sub(inner.x), 1);
    // without the buttons (under 40 columns) the title has its row to itself
    let title_w = if w >= 40 { left.width } else { w };
    widgets::line(buf, left.x, top, title_spans(app, title_w as usize), title_w);
    progress_row(buf, app, Rect { y: bottom, ..left }, hit);

    if w >= 40 {
        let mut x = inner.right().saturating_sub(buttons_w);
        for (icon, action) in buttons {
            let bw = width(icon) as u16;
            let style = if action == Action::TogglePause {
                widgets::accent(app).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(app.theme.fg)
            };
            put(buf, x, top, icon, style);
            hit.buttons.push((Rect::new(x, top, bw, 1), action));
            x += bw + 2;
        }
    }
    let vx = inner.right().saturating_sub(volume_w);
    volume_hits(app, hit, Rect::new(vx, bottom, volume_w, 1), slider);
    widgets::line(buf, vx, bottom, volume, volume_w);
    bands
}

/// The speaker icon mutes; the ramp (or the whole indicator without one) is the volume slider.
fn volume_hits(app: &App, hit: &mut Hit, rect: Rect, slider: Option<(u16, u16)>) {
    let icon = if app.muted { app.icons.muted } else { app.icons.volume };
    hit.buttons.push((Rect { width: (width(icon) as u16).min(rect.width), ..rect }, Action::Mute));
    hit.volume = Some(match slider {
        Some((off, w)) => Rect { x: rect.x + off, width: w.min(rect.width.saturating_sub(off)), ..rect },
        None => rect,
    });
}

/// Tiny terminals: title, progress and volume on a single line.
fn one_line(buf: &mut Buffer, app: &App, area: Rect, hit: &mut Hit) {
    let (volume, _) = volume(app, false);
    let vw = (spans_width(&volume) as u16).min(area.width);
    let rest = area.width.saturating_sub(vw + 1);
    let title_w = (u32::from(rest) * 2 / 5) as u16;
    widgets::line(buf, area.x, area.y, title_spans(app, title_w as usize), title_w);
    progress_row(buf, app, Rect::new(area.x + title_w + 1, area.y, rest.saturating_sub(title_w + 1), 1), hit);
    let vx = area.right() - vw;
    volume_hits(app, hit, Rect::new(vx, area.y, vw, 1), None);
    widgets::line(buf, vx, area.y, volume, vw);
}

fn buttons(app: &App) -> [(&'static str, Action); 3] {
    let i = &app.icons;
    let pp = if app.play_state() == PlayState::Playing { i.pause } else { i.play };
    [(i.prev, Action::Prev), (pp, Action::TogglePause), (i.next, Action::Next)]
}

/// State icon, then "Title — Artist · Album" scrolling as a marquee when it doesn't fit.
fn title_spans(app: &App, w: usize) -> Vec<Span<'static>> {
    let th = &app.theme;
    let (icon, icon_style) = match app.play_state() {
        PlayState::Playing => (app.icons.play, widgets::accent(app)),
        PlayState::Paused => (app.icons.pause, Style::new().fg(th.warn)),
        PlayState::Stopped => (app.icons.stop, widgets::dim(app)),
    };
    let head = vec![Span::styled(icon, icon_style.add_modifier(Modifier::BOLD)), Span::raw(" ")];
    let head_w = spans_width(&head);
    let body = match app.now_playing() {
        Some(t) => {
            let dim = widgets::dim(app);
            let mut spans = vec![Span::styled(t.title.clone(), Style::new().fg(th.fg).add_modifier(Modifier::BOLD))];
            if app.is_favorite(t.id) {
                spans.push(Span::styled(format!(" {}", app.icons.favorite), Style::new().fg(th.accent2)));
            }
            spans.extend([
                Span::styled(" — ", dim),
                Span::styled(t.artist.clone(), Style::new().fg(th.accent2)),
                Span::styled(" · ", dim),
                Span::styled(t.album.clone(), dim),
            ]);
            marquee(spans, w.saturating_sub(head_w), app.position())
        }
        None => {
            let mut v = vec![Span::styled("Nothing playing", widgets::dim(app))];
            if let Some(k) = app.keymap.keys_for(Action::Select).into_iter().next() {
                v.push(Span::styled(format!("  ·  {k} on a track to play"), widgets::dim(app)));
            }
            v
        }
    };
    let mut out = head;
    out.extend(body);
    truncate_spans(out, w)
}

/// "next: Title — Artist" for the bottom border (or why nothing follows).
fn up_next(app: &App) -> Vec<Span<'static>> {
    let dim = widgets::dim(app);
    if app.now_playing().is_none() {
        return Vec::new();
    }
    let stop = if app.sleep == Some(Sleep::EndOfTrack) {
        Some(app.icons.sleep)
    } else {
        app.queue.stop_after_current.then_some(app.icons.stop_after)
    };
    if let Some(icon) = stop {
        return vec![Span::styled(format!("{icon} stops after this track"), Style::new().fg(app.theme.warn))];
    }
    let next = app.queue.peek_advance().and_then(|id| app.track(id));
    match next {
        Some(t) => vec![
            Span::styled("next ", dim),
            Span::styled(t.title.clone(), Style::new().fg(app.theme.fg)),
            Span::styled(" — ", dim),
            Span::styled(t.artist.clone(), Style::new().fg(app.theme.accent2)),
        ],
        None => vec![Span::styled("end of queue", dim)],
    }
}

/// Volume: the speaker icon, an 8-step level ramp over 0..=max_volume (when `ramp`) and the
/// percent, or "∅ muted" in the same width so nothing shifts; above 100 % in the warning color.
/// Also returns the ramp's cells (offset, width) within the spans: the part that works as a slider.
fn volume(app: &App, ramp: bool) -> (Vec<Span<'static>>, Option<(u16, u16)>) {
    const RAMP: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let th = &app.theme;
    let (v, max) = (app.volume as usize, (app.cfg.playback.max_volume as usize).max(1));
    let loud = if v > 100 { Style::new().fg(th.warn) } else { widgets::accent(app) };
    let mut s = vec![Span::styled(app.icons.volume, loud)];
    let mut slider = None;
    if ramp {
        // cell k lights up at k/7 of the range, matching where a click on it sets the volume
        let lit = if v == 0 { 0 } else { (v * 7 + max / 2) / max + 1 };
        slider = Some((width(app.icons.volume) as u16 + 1, RAMP.len() as u16));
        s.push(Span::raw(" "));
        s.extend(RAMP.iter().enumerate().map(|(i, r)| Span::styled(*r, if i < lit { loud } else { widgets::dim(app) })));
    }
    s.push(Span::styled(format!(" {v:>3}%"), Style::new().fg(if v > 100 { th.warn } else { th.fg })));
    if app.muted {
        let muted = vec![Span::styled(format!("{} muted", app.icons.muted), Style::new().fg(th.warn))];
        s = fit_spans(muted, spans_width(&s), false);
    }
    (s, slider)
}

/// "1:23 ━━━━━━●────── 3:45" (or "-2:22" with remaining time); records the bar as `hit.progress`.
fn progress_row(buf: &mut Buffer, app: &App, rect: Rect, hit: &mut Hit) {
    if rect.width < 3 {
        return;
    }
    let playing = app.now_playing().is_some();
    let (pos, dur) = (app.position(), app.duration().unwrap_or_default());
    let (elapsed, total) = if playing {
        let total = if app.time_remaining { format!("-{}", fmt_duration(dur.saturating_sub(pos))) } else { fmt_duration(dur) };
        (fmt_duration(pos), total)
    } else {
        ("--:--".to_string(), "--:--".to_string())
    };
    let (ew, tw) = (width(&elapsed) as u16, width(&total) as u16);
    let bar = if rect.width >= ew + tw + 8 {
        buf.set_stringn(rect.x, rect.y, &elapsed, ew as usize, Style::new().fg(app.theme.fg));
        buf.set_stringn(rect.right() - tw, rect.y, &total, tw as usize, widgets::dim(app));
        Rect::new(rect.x + ew + 1, rect.y, rect.width - ew - tw - 2, 1)
    } else {
        rect
    };
    let ratio = if dur.is_zero() { 0.0 } else { (pos.as_secs_f64() / dur.as_secs_f64()).clamp(0.0, 1.0) };
    let frac = |d: Option<Duration>| d.filter(|_| !dur.is_zero()).map(|d| (d.as_secs_f64() / dur.as_secs_f64()).clamp(0.0, 1.0));
    progress_bar(buf, app, bar, playing.then_some(ratio), (frac(app.ab.0), frac(app.ab.1)));
    hit.progress = Some(bar);
}

/// The bar itself in `ui.progress` style. `ratio` is None when nothing plays. `ab` are the loop
/// points as fractions: the loop region is tinted and bracketed.
fn progress_bar(buf: &mut Buffer, app: &App, area: Rect, ratio: Option<f64>, ab: (Option<f64>, Option<f64>)) {
    let th = &app.theme;
    let w = area.width as usize;
    if w == 0 {
        return;
    }
    let r = ratio.unwrap_or(0.0);
    let filled = r * w as f64;
    let (fill, empty) = (Style::new().fg(widgets::tinted(app, th.progress)), Style::new().fg(th.progress_bg));
    let grad = |x: usize| Style::new().fg(th.gradient_at(x as f32 / (w - 1).max(1) as f32));
    let knob = ratio.map(|r| ((r * (w - 1) as f64).round() as usize).min(w - 1));
    let style = app.cfg.ui.progress;
    for x in 0..w {
        let (sym, s) = match style {
            ProgressStyle::Line | ProgressStyle::Gradient => match knob {
                Some(k) if x < k => ("━", if style == ProgressStyle::Gradient { grad(x) } else { fill }),
                _ => ("─", empty),
            },
            ProgressStyle::Block => {
                const EIGHTHS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
                let part = ((filled - x as f64) * 8.0) as usize;
                if (x as f64) + 1.0 <= filled {
                    ("█", fill)
                } else if (x as f64) < filled && part > 0 {
                    (EIGHTHS[part.min(7)], fill)
                } else {
                    ("░", empty)
                }
            }
            ProgressStyle::Segments => {
                if (x as f64) + 0.5 < filled {
                    ("▰", fill)
                } else {
                    ("▱", empty)
                }
            }
            ProgressStyle::Dots => {
                if (x as f64) + 1.0 <= filled {
                    ("⣿", fill)
                } else if (x as f64) + 0.5 <= filled {
                    ("⣇", fill)
                } else {
                    ("⣀", empty)
                }
            }
        };
        put(buf, area.x + x as u16, area.y, sym, s);
    }
    let at = |f: f64| area.x + ((f * (w - 1) as f64).round() as u16).min(area.width - 1);
    let loop_style = Style::new().fg(th.accent2);
    if let Some(a) = ab.0 {
        let (xa, xb) = (at(a), ab.1.map(at));
        if let Some(xb) = xb.filter(|xb| *xb > xa) {
            buf.set_style(Rect::new(xa, area.y, xb - xa + 1, 1), loop_style);
            put(buf, xb, area.y, "]", loop_style.add_modifier(Modifier::BOLD));
        }
        put(buf, xa, area.y, "[", loop_style.add_modifier(Modifier::BOLD));
    }
    if let (Some(k), ProgressStyle::Line | ProgressStyle::Gradient) = (knob, style) {
        let s = if style == ProgressStyle::Gradient { grad(k) } else { widgets::accent(app) };
        put(buf, area.x + k as u16, area.y, "●", s.add_modifier(Modifier::BOLD));
    }
}

/// Mini spectrum: one column per band (resampled from the analyzer), eighth-block precision,
/// colored along the theme gradient by height; a dim baseline where it's silent.
fn spectrum(buf: &mut Buffer, app: &App, area: Rect) {
    const LEVELS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let bands = app.analyzer.bands();
    let (w, h, n) = (area.width as usize, area.height as usize, bands.len());
    for x in 0..w {
        let v = if n == 0 {
            0.0
        } else {
            let a = x * n / w;
            let b = ((x + 1) * n / w).clamp(a + 1, n);
            bands[a..b].iter().copied().fold(0.0f32, f32::max)
        };
        let eighths = (v.clamp(0.0, 1.0) * (h * 8) as f32).round() as usize;
        let cx = area.x + x as u16;
        if eighths == 0 {
            put(buf, cx, area.bottom() - 1, "▁", widgets::dim(app));
            continue;
        }
        for row in 0..h {
            let fill = eighths.saturating_sub(row * 8).min(8);
            if fill == 0 {
                break;
            }
            let top = (row * 8 + fill) as f32 / (h * 8) as f32;
            put(buf, cx, area.bottom() - 1 - row as u16, LEVELS[fill], Style::new().fg(app.theme.gradient_at(top)));
        }
    }
}
