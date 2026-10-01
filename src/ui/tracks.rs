//! The track table shared by the library, queue and playlists: the configured columns
//! (`ui.columns`) adapt to the width, a header shows the sort, the playing track and favorites are
//! marked, and only the visible rows are built (lists can have tens of thousands of tracks).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use super::text::{fit, fit_spans, width};
use super::widgets::{self, Sel};
use crate::app::App;
use crate::config::Column;
use crate::library::{SortKey, Track, TrackId, fmt_duration};

/// Cells left of the columns for the markers.
const GUTTER: u16 = 2;

/// What the gutter shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gutter {
    /// The playing marker on the playing track.
    Playing,
    /// Queue: the marker on the current entry, play-order numbers on the next nine (1 = next;
    /// the order that matters with shuffle); entries already played are dimmed.
    Queue,
}

pub struct Table<'a> {
    pub ids: &'a [TrackId],
    pub offset: usize,
    pub selected: Option<usize>,
    pub focused: bool,
    pub gutter: Gutter,
    /// The list's sort, shown as an arrow in the matching column header.
    pub sort: Option<(SortKey, bool)>,
}

/// Rows available for items in a table drawn in `inner` (one row is the header).
pub fn body_height(inner: Rect) -> usize {
    inner.height.saturating_sub(1) as usize
}

/// Draws the table (header, visible rows, scrollbar) in `inner` of the pane `outer`.
/// Returns the body rect (item rows only) for `hit.lists`.
pub fn draw(buf: &mut Buffer, app: &App, outer: Rect, inner: Rect, t: &Table) -> Rect {
    let overflow = t.ids.len() > body_height(inner);
    let (content, col) = widgets::scroll_split(app, outer, inner, overflow);
    if content.is_empty() {
        return content;
    }
    let long = t.ids.iter().any(|id| app.track(*id).is_some_and(|tr| tr.duration.as_secs() >= 3600));
    let fav_w = width(app.icons.favorite).max(1) as u16;
    let cols = layout(&app.cfg.ui.columns, content.width.saturating_sub(GUTTER), t.ids.len(), long, fav_w);

    let mut header = vec![Span::raw(" ".repeat(GUTTER as usize))];
    for (k, c) in cols.iter().enumerate() {
        if k > 0 {
            header.push(Span::raw(" "));
        }
        let sorted = t.sort.filter(|(key, _)| sort_column(*key) == Some(c.col));
        let name = c.spec.header;
        let label = match sorted {
            Some((_, desc)) if width(name) + 2 <= c.w as usize => format!("{name} {}", if desc { "▼" } else { "▲" }),
            _ => name.to_string(),
        };
        let style = if sorted.is_some() { widgets::accent(app) } else { widgets::dim(app) };
        header.push(Span::styled(fit(&label, c.w as usize, c.spec.right), style.add_modifier(Modifier::BOLD)));
    }
    widgets::line(buf, content.x, content.y, header, content.width);

    let body = Rect { y: content.y + 1, height: content.height - 1, ..content };
    let show_heart = !cols.iter().any(|c| c.col == Column::Favorite);
    for (i, y) in (t.offset..t.ids.len()).zip(body.top()..body.bottom()) {
        let Some(track) = app.track(t.ids[i]) else { continue };
        let sel = Sel::of(t.selected == Some(i), t.focused);
        let spans = row_spans(app, t, &cols, i, track, sel, show_heart);
        widgets::paint_row(buf, app, Rect::new(body.x, y, body.width, 1), spans, sel);
    }
    let col = col.map(|c| Rect { y: c.y + 1, height: c.height.saturating_sub(1), ..c });
    widgets::scrollbar(buf, app, col, t.ids.len(), t.offset, t.focused);
    body
}

fn row_spans<'a>(app: &'a App, t: &Table, cols: &[Col], i: usize, track: &'a Track, sel: Sel, show_heart: bool) -> Vec<Span<'a>> {
    let th = &app.theme;
    let playing_style = Style::new().fg(th.playing).add_modifier(Modifier::BOLD);
    let (marker, current, played) = match t.gutter {
        Gutter::Playing => {
            let cur = app.is_current(track.id);
            (cur.then(|| Span::styled(app.icons.playing_marker, playing_style)), cur, false)
        }
        Gutter::Queue => match app.queue.play_position(i) {
            Some(0) => (Some(Span::styled(app.icons.playing_marker, playing_style)), true, false),
            Some(k) if k < 10 => {
                let s = if k == 1 { widgets::accent(app) } else { widgets::dim(app) };
                (Some(Span::styled(k.to_string(), s)), false, false)
            }
            Some(_) => (None, false, false),
            None => (None, false, app.queue.current.is_some()),
        },
    };
    let marker = marker.or_else(|| (sel == Sel::Unfocused).then(|| Span::styled(app.icons.selected_marker, widgets::accent(app))));
    let mut spans = fit_spans(marker.into_iter().collect(), GUTTER as usize - 1, false);
    spans.push(Span::raw(" "));

    // row-wide color: the playing row, else dimmed history, else per-column colors
    let row_fg = if current && app.cfg.ui.highlight_playing {
        Some(th.playing)
    } else if played {
        Some(th.dim)
    } else {
        None
    };
    let style = |normal: Style| row_fg.map_or(normal, |c| normal.fg(c));
    let fg = Style::new().fg(th.fg);
    let dim = Style::new().fg(th.dim);
    let fav = || app.is_favorite(track.id);
    for (k, c) in cols.iter().enumerate() {
        if k > 0 {
            spans.push(Span::raw(" "));
        }
        let w = c.w as usize;
        let cell = |text: String, s: Style| Span::styled(fit(&text, w, c.spec.right), style(s));
        match c.col {
            Column::Title => {
                let mut title_style = sel.main(app, style(fg));
                if current {
                    title_style = title_style.add_modifier(Modifier::BOLD);
                }
                let mut parts = vec![Span::styled(track.title.as_str(), title_style)];
                if show_heart && fav() {
                    parts.push(Span::styled(format!(" {}", app.icons.favorite), style(Style::new().fg(th.accent2))));
                }
                spans.extend(fit_spans(parts, w, false));
            }
            Column::Index => spans.push(cell((i + 1).to_string(), dim)),
            Column::Track => spans.push(cell(track_no(track), dim)),
            Column::Artist => spans.push(cell(track.artist.clone(), Style::new().fg(th.accent2))),
            Column::Album => spans.push(cell(track.album.clone(), fg)),
            Column::AlbumArtist => spans.push(cell(track.album_artist.clone(), fg)),
            Column::Genre => spans.push(cell(track.genre.clone(), dim)),
            Column::Year => spans.push(cell(track.year.map(|y| y.to_string()).unwrap_or_default(), dim)),
            Column::Duration => spans.push(cell(fmt_duration(track.duration), dim)),
            Column::Plays => spans.push(cell(app.plays(track.id).to_string(), dim)),
            Column::Format => spans.push(cell(track.format.clone(), dim)),
            Column::Bitrate => spans.push(cell(track.bitrate.map(|b| b.to_string()).unwrap_or_default(), dim)),
            Column::Favorite => {
                let icon = if fav() { app.icons.favorite } else { "" };
                spans.push(cell(icon.to_string(), Style::new().fg(th.accent2)));
            }
        }
    }
    spans
}

/// "3", or "2-03" for a track on a later disc.
fn track_no(t: &Track) -> String {
    match (t.disc_no, t.track_no) {
        (Some(d), Some(n)) if d > 1 => format!("{d}-{n:02}"),
        (_, Some(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn sort_column(k: SortKey) -> Option<Column> {
    match k {
        SortKey::Title => Some(Column::Title),
        SortKey::Artist => Some(Column::Artist),
        SortKey::Album => Some(Column::Album),
        SortKey::Duration => Some(Column::Duration),
        SortKey::Year => Some(Column::Year),
        SortKey::Plays => Some(Column::Plays),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Spec {
    /// Width a column needs; fixed columns never grow.
    min: u16,
    /// Share of the leftover space (0 = fixed width).
    weight: u16,
    max: u16,
    right: bool,
    header: &'static str,
    /// Columns with the lowest priority are dropped first when the table is too narrow.
    priority: u8,
}

fn spec(c: Column, len: usize, long: bool, fav_w: u16) -> Spec {
    let fixed = |w: u16, right, header, priority| Spec { min: w, weight: 0, max: w, right, header, priority };
    let flex = |min, weight, max, header, priority| Spec { min, weight, max, right: false, header, priority };
    match c {
        Column::Index => fixed(len.max(1).ilog10() as u16 + 1, true, "#", 60),
        Column::Track => fixed(4, true, "No.", 50),
        Column::Title => flex(12, 6, u16::MAX, "Title", 100),
        Column::Artist => flex(8, 3, 48, "Artist", 90),
        Column::Album => flex(8, 3, 56, "Album", 70),
        Column::AlbumArtist => flex(8, 2, 40, "Album Artist", 30),
        Column::Genre => flex(6, 1, 20, "Genre", 25),
        Column::Year => fixed(4, true, "Year", 40),
        Column::Duration => fixed(if long { 7 } else { 5 }, true, "Time", 80),
        Column::Plays => fixed(5, true, "Plays", 35),
        Column::Format => fixed(6, false, "Format", 20),
        Column::Bitrate => fixed(4, true, "kbps", 15),
        Column::Favorite => fixed(fav_w, false, "", 55),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Col {
    col: Column,
    w: u16,
    spec: Spec,
}

/// Column widths for `avail` cells (one-cell gaps between columns): the lowest-priority columns are
/// dropped until the rest fit, then flexible columns share the leftover space by weight.
fn layout(configured: &[Column], avail: u16, len: usize, long: bool, fav_w: u16) -> Vec<Col> {
    let mut cols: Vec<Column> = if configured.is_empty() { vec![Column::Title] } else { configured.to_vec() };
    let specs = |c: Column| spec(c, len, long, fav_w);
    let need = |cols: &[Column]| cols.iter().map(|c| specs(*c).min as usize).sum::<usize>() + cols.len().saturating_sub(1);
    while cols.len() > 1 && need(&cols) > avail as usize {
        let drop = (0..cols.len()).min_by_key(|&i| specs(cols[i]).priority).unwrap_or(0);
        cols.remove(drop);
    }
    let mut out: Vec<Col> = cols.iter().map(|&c| Col { col: c, w: specs(c).min, spec: specs(c) }).collect();
    if out.len() == 1 {
        out[0].w = out[0].w.min(avail);
    }
    let mut extra = (avail as usize).saturating_sub(need(&cols));
    while extra > 0 {
        let open: Vec<usize> = (0..out.len()).filter(|&i| out[i].spec.weight > 0 && out[i].w < out[i].spec.max).collect();
        let total: usize = open.iter().map(|&i| out[i].spec.weight as usize).sum();
        if total == 0 {
            break;
        }
        let mut given = 0;
        for &i in &open {
            let c = &mut out[i];
            let share = (extra * c.spec.weight as usize / total).max(1);
            let add = share.min((c.spec.max - c.w) as usize).min(extra - given);
            c.w += add as u16;
            given += add;
            if given == extra {
                break;
            }
        }
        extra -= given;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(cols: &[Col]) -> Vec<(Column, u16)> {
        cols.iter().map(|c| (c.col, c.w)).collect()
    }

    #[test]
    fn layout_fills_width_and_respects_fixed_columns() {
        let cols = [Column::Index, Column::Title, Column::Artist, Column::Album, Column::Duration];
        let l = layout(&cols, 100, 376, false, 1);
        let total: u16 = l.iter().map(|c| c.w).sum::<u16>() + l.len() as u16 - 1;
        assert_eq!(total, 100);
        assert_eq!(l[0].w, 3); // "376" digits
        assert_eq!(l[4].w, 5);
        // title gets the biggest share
        assert!(l[1].w > l[2].w && l[2].w.abs_diff(l[3].w) <= 1, "{:?}", widths(&l));
    }

    #[test]
    fn narrow_tables_drop_low_priority_columns() {
        let cols = [Column::Index, Column::Title, Column::Artist, Column::Album, Column::Genre, Column::Format, Column::Duration];
        let l = layout(&cols, 30, 50, false, 1);
        let kept: Vec<Column> = l.iter().map(|c| c.col).collect();
        assert_eq!(kept, [Column::Title, Column::Artist, Column::Duration]);
        let l = layout(&cols, 5, 50, false, 1);
        assert_eq!(widths(&l), [(Column::Title, 5)]);
        // never panics on zero width, and an empty config still shows titles
        assert_eq!(widths(&layout(&cols, 0, 0, false, 1)), [(Column::Title, 0)]);
        assert_eq!(widths(&layout(&[], 20, 0, false, 1)), [(Column::Title, 20)]);
    }

    #[test]
    fn capped_columns_leave_room_for_title() {
        let l = layout(&[Column::Title, Column::Genre, Column::Year], 200, 10, true, 1);
        assert_eq!(l[1].w, 20);
        assert_eq!(l[0].w, 200 - 20 - 4 - 2);
    }
}
