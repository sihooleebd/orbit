//! Shared rendering primitives: themed panes, windowed list rows, scrollbars, key hints and
//! empty-state placeholders.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget,
};

use super::text::{fit_spans, spans_width, truncate_spans, width};
use crate::app::App;
use crate::config::BorderStyle;
use crate::keymap::Action;
use crate::theme;

/// Border type for panes; `None` (no borders) in compact mode or with `ui.border = "none"`.
pub fn borders(app: &App) -> Option<BorderType> {
    if app.compact { None } else { theme::border_type(app.cfg.ui.border) }
}

/// `themed`, or the album-art accent while `ui.dynamic_accent` has one, so accent-colored parts
/// (focused borders, the progress bar) follow the cover together with `app.accent()`.
pub fn tinted(app: &App, themed: Color) -> Color {
    if app.cfg.ui.dynamic_accent && app.cover_accent.is_some() { app.accent() } else { themed }
}

/// A themed pane. Focused panes get `border_focus` and an accent title. Without borders the title
/// takes the top row and horizontal padding keeps neighbours apart (none in compact mode).
pub fn panel(app: &App, focused: bool) -> Block<'static> {
    let t = &app.theme;
    let title = Style::new().fg(if focused { app.accent() } else { t.title }).add_modifier(Modifier::BOLD);
    let block = Block::new().title_style(title).padding(Padding::horizontal(u16::from(!app.compact)));
    match borders(app) {
        Some(bt) => block.borders(Borders::ALL).border_type(bt).border_style(Style::new().fg(if focused {
            tinted(app, t.border_focus)
        } else {
            t.border
        })),
        None => block,
    }
}

/// A popup's frame: a focused pane's, but never borderless (`ui.border = "none"`, compact mode),
/// so popups stand out from the panes behind them.
pub fn popup(app: &App) -> Block<'static> {
    let bt = borders(app).or(theme::border_type(app.cfg.ui.border)).unwrap_or(BorderType::Plain);
    panel(app, true).borders(Borders::ALL).border_type(bt).border_style(Style::new().fg(tinted(app, app.theme.border_focus)))
}

/// A block title: the parts with a space on each side.
pub fn title(mut parts: Vec<Span<'static>>) -> Line<'static> {
    parts.insert(0, Span::raw(" "));
    parts.push(Span::raw(" "));
    Line::from(parts)
}

/// Adds a left and a right title to `block` for a pane `width` cells wide, shortening the left one
/// (and dropping the right one when there's no room) so they never overlap.
pub fn titles(block: Block<'static>, left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> Block<'static> {
    titles_at(block, left, right, width, false)
}

/// Same as `titles` on the bottom border (nothing without borders: it would cost a row).
pub fn titles_bottom(app: &App, block: Block<'static>, left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> Block<'static> {
    if borders(app).is_none() {
        return block;
    }
    titles_at(block, left, right, width, true)
}

pub fn titles_at(mut block: Block<'static>, left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16, bottom: bool) -> Block<'static> {
    // corners + one cell of air on each side
    let avail = (width as usize).saturating_sub(4);
    let right_w = if right.is_empty() { 0 } else { spans_width(&right) + 2 };
    let (right, right_w) = if right_w + 10 > avail { (Vec::new(), 0) } else { (right, right_w) };
    let mut lines = Vec::new();
    // kept even when cut to nothing: without borders a title decides whether the top row is taken
    if !left.is_empty() {
        lines.push(title(truncate_spans(left, avail.saturating_sub(right_w + 3))));
    }
    if !right.is_empty() {
        lines.push(title(right).right_aligned());
    }
    for l in lines {
        block = if bottom { block.title_bottom(l) } else { block.title(l) };
    }
    block
}

pub fn dim(app: &App) -> Style {
    Style::new().fg(app.theme.dim)
}

pub fn accent(app: &App) -> Style {
    Style::new().fg(app.accent())
}

/// Writes `symbol` at (x, y), one cell per grapheme ("dB" takes two cells), clipped to the buffer.
pub fn put(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if buf.area.contains(Position::new(x, y)) {
        buf.set_stringn(x, y, symbol, usize::MAX, style);
    }
}

/// Renders `spans` on row `y` from `x`, at most `max` cells.
pub fn line(buf: &mut Buffer, x: u16, y: u16, spans: Vec<Span<'_>>, max: u16) {
    buf.set_line(x, y, &Line::from(spans), max);
}

/// First visible row of a list so that the selection keeps `margin` rows from the edges.
pub fn scroll_offset(offset: usize, selected: Option<usize>, len: usize, height: usize, margin: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    let max = len - height;
    let mut off = offset.min(max);
    if let Some(sel) = selected {
        let sel = sel.min(len - 1);
        let m = margin.min((height - 1) / 2);
        if sel < off + m {
            off = sel.saturating_sub(m);
        } else if sel + m >= off + height {
            off = sel + m + 1 - height;
        }
    }
    off.min(max)
}

/// Where a list's scrollbar goes: on the pane's right border, or (borderless) in the last column of
/// the body, which then loses that column. Returns (body, scrollbar column).
pub fn scroll_split(app: &App, outer: Rect, body: Rect, overflow: bool) -> (Rect, Option<Rect>) {
    if !overflow || body.width < 2 || body.height == 0 {
        return (body, None);
    }
    if borders(app).is_some() {
        (body, Some(Rect::new(outer.right() - 1, body.y, 1, body.height)))
    } else {
        (Rect { width: body.width - 1, ..body }, Some(Rect::new(body.right() - 1, body.y, 1, body.height)))
    }
}

pub fn scrollbar(buf: &mut Buffer, app: &App, col: Option<Rect>, len: usize, offset: usize, focused: bool) {
    let Some(col) = col else { return };
    let h = col.height as usize;
    if h == 0 || len <= h {
        return;
    }
    let bordered = borders(app).is_some();
    let thumb = if bordered && app.cfg.ui.border == BorderStyle::Thick { "█" } else { "┃" };
    let mut state = ScrollbarState::new(len - h + 1).position(offset).viewport_content_length(h);
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol((!bordered).then_some("│"))
        .track_style(Style::new().fg(app.theme.border))
        .thumb_symbol(thumb)
        .thumb_style(Style::new().fg(if focused { app.theme.fg } else { app.theme.dim }))
        .render(col, buf, &mut state);
}

/// How a list row is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sel {
    No,
    /// Selected in the focused pane: selection colors across the row.
    Focused,
    /// Selected in another pane: accent text and the selection marker.
    Unfocused,
}

impl Sel {
    pub fn of(selected: bool, focused: bool) -> Sel {
        match (selected, focused) {
            (false, _) => Sel::No,
            (true, true) => Sel::Focused,
            (true, false) => Sel::Unfocused,
        }
    }

    /// Style for the row's main text (name / title) given its normal style.
    pub fn main(self, app: &App, normal: Style) -> Style {
        match self {
            Sel::Unfocused => normal.fg(app.accent()).add_modifier(Modifier::BOLD),
            Sel::Focused => normal.add_modifier(Modifier::BOLD),
            Sel::No => normal,
        }
    }
}

/// One list row: a 2-cell gutter (`marker`, else the selection marker for an unfocused selection)
/// followed by `spans` fitted to the rest of the row.
pub fn draw_row(buf: &mut Buffer, app: &App, rect: Rect, marker: Option<Span<'_>>, spans: Vec<Span<'_>>, sel: Sel) {
    if rect.width == 0 {
        return;
    }
    let marker = marker.or_else(|| (sel == Sel::Unfocused).then(|| Span::styled(app.icons.selected_marker, accent(app))));
    let gutter = 2.min(rect.width as usize);
    let mut row = fit_spans(marker.into_iter().collect(), gutter, false);
    row.extend(fit_spans(spans, rect.width as usize - gutter, false));
    paint_row(buf, app, rect, row, sel);
}

/// Renders a row's spans; a focused selection paints the whole row in the selection colors.
pub fn paint_row(buf: &mut Buffer, app: &App, rect: Rect, mut spans: Vec<Span<'_>>, sel: Sel) {
    if sel == Sel::Focused {
        let s = Style::new().fg(app.theme.sel_fg).bg(app.theme.sel_bg);
        buf.set_style(rect, s);
        for span in &mut spans {
            span.style = span.style.patch(s);
        }
    }
    line(buf, rect.x, rect.y, spans, rect.width);
}

/// Draws the visible rows of a one-row-per-item list (from `offset`) in `inner` of pane `outer`,
/// with a scrollbar when it overflows. Returns the list body (for `hit.lists`).
#[allow(clippy::too_many_arguments)]
pub fn list(
    buf: &mut Buffer,
    app: &App,
    outer: Rect,
    inner: Rect,
    len: usize,
    offset: usize,
    focused: bool,
    mut row: impl FnMut(&mut Buffer, usize, Rect),
) -> Rect {
    let (body, col) = scroll_split(app, outer, inner, len > inner.height as usize);
    for (i, y) in (offset..len).zip(body.top()..body.bottom()) {
        row(buf, i, Rect::new(body.x, y, body.width, 1));
    }
    scrollbar(buf, app, col, len, offset, focused);
    body
}

/// "key label · key label …" for the actions that have a key bound, up to `max` cells.
/// Several actions in one item show their first keys joined with "/" ("h/l band").
pub fn hints(app: &App, items: &[(&[Action], &str)], max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for (actions, label) in items {
        let keys: Vec<String> = actions.iter().filter_map(|a| app.keymap.keys_for(*a).into_iter().next()).collect();
        if keys.is_empty() {
            continue;
        }
        let key = keys.join("/");
        let sep = if out.is_empty() { 0 } else { 3 };
        let w = sep + width(&key) + 1 + width(label);
        if used + w > max {
            break;
        }
        if sep > 0 {
            out.push(Span::styled(" · ", dim(app)));
        }
        out.push(Span::styled(key, accent(app).add_modifier(Modifier::BOLD)));
        out.push(Span::styled(format!(" {label}"), dim(app)));
        used += w;
    }
    out
}

/// Lines centered in `area` (vertically and horizontally): placeholders for empty lists.
pub fn empty_state(buf: &mut Buffer, area: Rect, lines: Vec<Line<'_>>) {
    if area.is_empty() {
        return;
    }
    let lines: Vec<Line> = lines
        .into_iter()
        .take(area.height as usize)
        .map(|l| Line::from(truncate_spans(l.spans, area.width as usize)).style(l.style))
        .collect();
    let h = lines.len() as u16;
    let rect = Rect::new(area.x, area.y + (area.height - h) / 2, area.width, h);
    Paragraph::new(lines).alignment(Alignment::Center).render(rect, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_window_follows_selection_with_margin() {
        // fits: never scrolls
        assert_eq!(scroll_offset(5, Some(3), 8, 10, 3), 0);
        // selection near the bottom edge pulls the window down, keeping 3 rows below it
        assert_eq!(scroll_offset(0, Some(12), 100, 10, 3), 6);
        // selection near the top edge pulls it up
        assert_eq!(scroll_offset(50, Some(51), 100, 10, 3), 48);
        // inside the comfortable zone the offset is kept
        assert_eq!(scroll_offset(20, Some(25), 100, 10, 3), 20);
        // clamped at the end, and a stale offset past the end is fixed
        assert_eq!(scroll_offset(0, Some(99), 100, 10, 3), 90);
        assert_eq!(scroll_offset(500, None, 100, 10, 3), 90);
        // huge margins are capped to half the height
        assert_eq!(scroll_offset(0, Some(5), 100, 4, 50), 3);
        assert_eq!(scroll_offset(0, Some(5), 100, 0, 3), 0);
    }
}
