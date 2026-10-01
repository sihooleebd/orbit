//! Equalizer tab: preamp + 10 band sliders on a ±12 dB scale with a response curve through the
//! bands, the preset list with shape previews, and key hints. Everything dims while the EQ is off.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Widget;

use super::text::{fit, width};
use super::widgets::{self, Sel, put};
use crate::app::{App, Hit};
use crate::config::EqSettings;
use crate::dsp::{EQ_FREQS, EQ_MAX_DB, EQ_PRESETS, eq_response_db};
use crate::keymap::Action;

pub fn draw(buf: &mut Buffer, app: &App, area: Rect, hit: &mut Hit) {
    if area.width >= 96 {
        let gap = u16::from(widgets::borders(app).is_none());
        let [main, side] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(30)]).spacing(gap).areas(area);
        sliders(buf, app, main, hit);
        presets(buf, app, side);
    } else {
        sliders(buf, app, area, hit);
    }
}

/// "pre", "31", "62", … "1k", … "16k"
fn label(k: usize) -> String {
    match k {
        0 => "pre".into(),
        _ => {
            let f = EQ_FREQS[k - 1];
            if f >= 1000.0 { format!("{}k", (f / 1000.0) as u32) } else { format!("{}", f as u32) }
        }
    }
}

fn value(app: &App, k: usize) -> f32 {
    if k == 0 { app.eq.preamp_db } else { app.eq.bands[k - 1] }
}

fn fmt_db(v: f32, wide: bool) -> String {
    let v = if v.abs() < 0.05 { 0.0 } else { v };
    match (wide, v == 0.0) {
        (true, true) => "0.0".into(),
        (true, false) => format!("{v:+.1}"),
        (false, true) => "0".into(),
        (false, false) => format!("{v:+.0}"),
    }
}

fn sliders(buf: &mut Buffer, app: &App, rect: Rect, hit: &mut Hit) {
    let eq = &app.eq;
    let on = eq.enabled;
    let th = &app.theme;
    let dim = widgets::dim(app);
    let state = if on { Span::styled("● on", Style::new().fg(th.ok)) } else { Span::styled("○ off", dim) };
    let preset_style = if on { widgets::accent(app) } else { dim };
    let right = vec![state, Span::styled(" · ", dim), Span::styled(eq.preset.clone(), preset_style)];
    let block = widgets::titles(widgets::panel(app, true), vec![Span::raw(format!("{} Equalizer", app.icons.eq))], right, rect.width);
    let inner = block.inner(rect);
    block.render(rect, buf);
    if inner.height < 5 || inner.width < 22 {
        // too small for sliders: the values on one line
        let text: Vec<String> = (0..11).map(|k| format!("{} {}", label(k), fmt_db(value(app, k), false))).collect();
        buf.set_stringn(inner.x, inner.y, text.join(" · "), inner.width as usize, if on { Style::new().fg(th.fg) } else { dim });
        return;
    }

    let hints_h = u16::from(inner.height >= 9);
    let pad_top = u16::from(inner.height >= 13);
    let mut track_h = inner.height - 2 - hints_h - pad_top;
    if track_h.is_multiple_of(2) {
        track_h -= 1; // odd, so 0 dB has a row of its own
    }
    let labels_y = inner.y + pad_top;
    let track_y = labels_y + 1;
    let values_y = track_y + track_h;
    let half = (track_h - 1) / 2;
    let center_y = track_y + half;

    let scale_w: u16 = if inner.width >= 11 * 4 + 6 { 5 } else { 0 };
    let avail = inner.width - scale_w - u16::from(scale_w > 0);
    let col_w = avail / 11;
    let x0 = inner.right() - avail + (avail - col_w * 11) / 2;
    let x_end = x0 + col_w * 11;
    let bw = col_w.saturating_sub(2).clamp(1, 5);
    let line_style = Style::new().fg(th.border);

    // scale and 0 dB line
    if scale_w > 0 {
        put(buf, inner.x + 1, labels_y, "dB", dim);
        for (text, db) in [("+12", 12.0), ("+6", 6.0), ("0", 0.0), ("-6", -6.0), ("-12", -12.0)] {
            // on the bars' scale, where ±EQ_MAX_DB is half a row past the outermost rows
            let off = (db / EQ_MAX_DB * (half as f32 + 0.5)).round().clamp(-(half as f32), half as f32);
            let y = (center_y as f32 - off) as u16;
            buf.set_stringn(inner.x, y, fit(text, 3, true), 3, dim);
            put(buf, inner.x + 4, y, if db == 0.0 { "┼" } else { "┤" }, line_style);
        }
    }
    for x in (inner.x + scale_w)..x_end {
        put(buf, x, center_y, "─", line_style);
    }

    let sel = app.eq_view.selected.min(10);
    let bar_x = |k: u16| x0 + k * col_w + (col_w - bw) / 2;
    // the rod each slider moves along
    for k in 0..11u16 {
        let x = bar_x(k) + bw / 2;
        let s = if k as usize == sel { widgets::accent(app) } else { line_style };
        for y in track_y..values_y {
            put(buf, x, y, if y == center_y { "┼" } else { "│" }, s);
        }
    }
    if col_w > 4 {
        // the preamp is not a band: set it apart
        for y in track_y..values_y {
            put(buf, x0 + col_w, y, "┊", line_style);
        }
    }

    // the filters' real response (what the audio thread applies), under the bars: sampled on a
    // log-frequency axis through each band's slider; the preamp has its own slider, so it's left out
    let units = 2 * half + 1;
    let y_px = |v: f32| (half as f32 + 0.5 - v / EQ_MAX_DB * (half as f32 + 0.5)) * 4.0;
    let curve_area = Rect::new(x0, track_y, col_w * 11, track_h);
    let shape = EqSettings { enabled: true, preamp_db: 0.0, ..app.eq.clone() };
    let xs: Vec<f32> = (1..11).map(|k| ((bar_x(k as u16) + bw / 2 - x0) * 2 + 1) as f32).collect();
    let mut curve = Vec::new();
    for (i, pair) in xs.windows(2).enumerate() {
        let (f0, f1) = (EQ_FREQS[i], EQ_FREQS[i + 1]);
        let steps = (pair[1] - pair[0]).max(1.0) as usize;
        for s in 0..steps + usize::from(i == xs.len() - 2) {
            let t = s as f32 / steps as f32;
            let db = eq_response_db(&shape, f0 * (f1 / f0).powf(t), 48_000).clamp(-EQ_MAX_DB, EQ_MAX_DB);
            curve.push((pair[0] + t * (pair[1] - pair[0]), y_px(db)));
        }
    }
    let curve_style = Style::new().fg(if on { th.accent2 } else { th.border });
    braille(buf, curve_area, &curve, curve_style);

    for k in 0..11usize {
        let v = value(app, k).clamp(-EQ_MAX_DB, EQ_MAX_DB);
        let n = (((v.abs() / EQ_MAX_DB) * units as f32).round() as u16).min(units);
        let x = bar_x(k as u16);
        let selected = k == sel;
        let color = |d: u16| {
            if !on {
                dim
            } else if selected {
                widgets::accent(app)
            } else {
                Style::new().fg(th.gradient_at(d as f32 / half.max(1) as f32))
            }
        };
        let fill = |buf: &mut Buffer, y: u16, sym: &str, s: Style| (x..x + bw).for_each(|xx| put(buf, xx, y, sym, s));
        if n == 0 {
            fill(buf, center_y, "━", if on { color(0).add_modifier(Modifier::BOLD) } else { dim });
        } else {
            let up = v > 0.0;
            fill(buf, center_y, if up { "▀" } else { "▄" }, color(0));
            for d in 1..=half {
                if n < 2 * d {
                    break;
                }
                let y = if up { center_y - d } else { center_y + d };
                let sym = if n > 2 * d {
                    "█"
                } else if up {
                    "▄"
                } else {
                    "▀"
                };
                fill(buf, y, sym, color(d));
            }
        }
        let col_x = x0 + k as u16 * col_w;
        let label_style = if selected {
            Style::new().fg(th.sel_fg).bg(th.sel_bg).add_modifier(Modifier::BOLD)
        } else if on {
            Style::new().fg(th.fg)
        } else {
            dim
        };
        // Labels and values need a cell of gap or they run together ("125250"): a narrower column
        // than 4 shows every other one (always the selected band's, never its neighbors'), and
        // "-12.0" needs 6.
        if col_w >= 4 || selected || (k % 2 == 0 && k.abs_diff(sel) != 1) {
            let text = label(k);
            let text = if selected && width(&text) + 2 <= col_w as usize { format!(" {text} ") } else { text };
            centered_text(buf, col_x, labels_y, col_w, &text, label_style);
            let value_style = if selected { widgets::accent(app).add_modifier(Modifier::BOLD) } else { dim };
            centered_text(buf, col_x, values_y, col_w, &fmt_db(v, col_w >= 6), value_style);
        }
        hit.eq_sliders.push((Rect::new(col_x, track_y, col_w, track_h), k));
    }

    if hints_h > 0 {
        let items: [(&[Action], &str); 5] = [
            (&[Action::Left, Action::Right], "band"),
            (&[Action::Up, Action::Down], "gain"),
            (&[Action::EqToggle], "on/off"),
            (&[Action::EqPrevPreset, Action::EqNextPreset], "preset"),
            (&[Action::EqReset], "flatten"),
        ];
        let spans = widgets::hints(app, &items, inner.width as usize);
        widgets::line(buf, inner.x, inner.bottom() - 1, spans, inner.width);
    }
}

fn centered_text(buf: &mut Buffer, x: u16, y: u16, w: u16, text: &str, style: Style) {
    let tw = (width(text) as u16).min(w);
    buf.set_stringn(x + (w - tw) / 2, y, text, tw as usize, style);
}

/// Plots points (x, y in braille dots: 2 per cell across, 4 down) as a connected braille line.
/// Only cells the line passes through are touched.
fn braille(buf: &mut Buffer, area: Rect, points: &[(f32, f32)], style: Style) {
    const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    let (w, h) = (area.width as i32, area.height as i32);
    let mut grid = vec![0u8; (w * h).max(0) as usize];
    let mut set = |px: i32, py: i32| {
        if (0..w * 2).contains(&px) && (0..h * 4).contains(&py) {
            grid[((py / 4) * w + px / 2) as usize] |= BITS[(px % 2) as usize][(py % 4) as usize];
        }
    };
    let mut prev: Option<i32> = None;
    for &(x, y) in points {
        let (px, py) = (x.round() as i32, y.round() as i32);
        let from = prev.unwrap_or(py);
        for yy in from.min(py)..=from.max(py) {
            set(px, yy);
        }
        prev = Some(py);
    }
    for (i, bits) in grid.into_iter().enumerate().filter(|(_, b)| *b != 0) {
        let (cx, cy) = (i as i32 % w, i as i32 / w);
        if let Some(ch) = char::from_u32(0x2800 + bits as u32) {
            put(buf, area.x + cx as u16, area.y + cy as u16, ch.encode_utf8(&mut [0; 4]), style);
        }
    }
}

/// The built-in presets with a 10-cell preview of each curve; the active one is marked.
fn presets(buf: &mut Buffer, app: &App, rect: Rect) {
    const LEVELS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let eq = &app.eq;
    let current = EQ_PRESETS.iter().position(|(n, _)| n.eq_ignore_ascii_case(&eq.preset));
    let len = EQ_PRESETS.len() + usize::from(current.is_none());
    let active = current.unwrap_or(EQ_PRESETS.len());
    let block = widgets::titles(widgets::panel(app, false), vec![Span::raw("Presets")], Vec::new(), rect.width);
    let inner = block.inner(rect);
    block.render(rect, buf);
    let offset = widgets::scroll_offset(0, Some(active), len, inner.height as usize, 2);
    widgets::list(buf, app, rect, inner, len, offset, false, |buf, i, row| {
        let (name, bands) = EQ_PRESETS.get(i).map_or((eq.preset.as_str(), eq.bands), |(n, b)| (*n, *b));
        let is_active = i == active;
        let preview: String =
            bands.iter().map(|v| LEVELS[(((v + EQ_MAX_DB) / (2.0 * EQ_MAX_DB)) * 7.0).round().clamp(0.0, 7.0) as usize]).collect();
        let (pv, nm) = if is_active {
            (Style::new().fg(app.theme.accent2), widgets::accent(app).add_modifier(Modifier::BOLD))
        } else {
            (widgets::dim(app), Style::new().fg(app.theme.fg))
        };
        let marker = is_active.then(|| Span::styled(app.icons.selected_marker, widgets::accent(app)));
        widgets::draw_row(buf, app, row, marker, vec![Span::styled(preview, pv), Span::raw("  "), Span::styled(name, nm)], Sel::No);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_values() {
        let labels: Vec<String> = (0..11).map(label).collect();
        assert_eq!(labels, ["pre", "31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"]);
        assert_eq!(fmt_db(4.0, true), "+4.0");
        assert_eq!(fmt_db(-1.5, true), "-1.5");
        assert_eq!(fmt_db(0.01, true), "0.0");
        assert_eq!(fmt_db(-0.0, false), "0");
        assert_eq!(fmt_db(3.6, false), "+4");
    }

}
