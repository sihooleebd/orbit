//! Library tab: [browse modes] [groups of the mode] [tracks of the group]. Narrow terminals show
//! fewer panes, always keeping the focused one and its nearest neighbour, so moving the focus
//! drills down through them.

use std::cmp::Reverse;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::text::{fit_spans, home_relative, truncate_start, width};
use super::tracks::{self, Gutter, Table};
use super::widgets::{self, Sel};
use crate::app::{App, Hit, LibPane, LibraryView, ListHit, ListTarget};
use crate::keymap::Action;
use crate::library::{BrowseMode, fmt_duration};

pub fn draw(f: &mut Frame, app: &mut App, area: Rect, hit: &mut Hit) {
    let panes = visible_panes(&app.library_view, area.width);
    let rects = pane_rects(app, area, &panes);
    for (pane, rect) in panes.into_iter().zip(rects.iter().copied()) {
        let focused = app.library_view.pane == pane;
        match pane {
            LibPane::Modes => modes(f.buffer_mut(), app, rect, focused, hit),
            LibPane::Groups => groups(f.buffer_mut(), app, rect, focused, hit),
            LibPane::Tracks => track_list(f.buffer_mut(), app, rect, focused, hit),
        }
    }
}

/// Panes that fit in `width`: 3 from ~90 columns, 2 from ~50, else just the focused one. The
/// groups pane is skipped in the flat "All Tracks" mode (it has a single group) unless focused.
fn visible_panes(v: &LibraryView, width: u16) -> Vec<LibPane> {
    let mut panes = vec![LibPane::Modes, LibPane::Groups, LibPane::Tracks];
    if v.mode == BrowseMode::Tracks && v.pane != LibPane::Groups {
        panes.retain(|p| *p != LibPane::Groups);
    }
    let cap = if width >= 90 {
        3
    } else if width >= 50 {
        2
    } else {
        1
    };
    while panes.len() > cap {
        let focus = panes.iter().position(|p| *p == v.pane).unwrap_or(panes.len() - 1);
        // drop the pane farthest from the focused one (the leftmost on ties)
        let far = (0..panes.len()).max_by_key(|&i| (i.abs_diff(focus), Reverse(i))).unwrap_or(0);
        panes.remove(far);
    }
    panes
}

/// Widths from `ui.library_split` (percent of the full width), with minimums; the last pane takes
/// what's left.
fn pane_rects(app: &App, area: Rect, panes: &[LibPane]) -> Vec<Rect> {
    let split = app.cfg.ui.library_split;
    let total: u32 = panes.iter().map(|p| split[pane_index(*p)].max(1) as u32).sum();
    let constraints: Vec<Constraint> = panes
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i + 1 == panes.len() {
                return Constraint::Fill(1);
            }
            let pct = area.width as u32 * split[pane_index(*p)].max(1) as u32 / total;
            let min = if *p == LibPane::Modes { 18 } else { 22 };
            Constraint::Length((pct as u16).max(min).min(area.width / 2))
        })
        .collect();
    let gap = u16::from(widgets::borders(app).is_none());
    Layout::horizontal(constraints).spacing(gap).split(area).to_vec()
}

fn pane_index(p: LibPane) -> usize {
    match p {
        LibPane::Modes => 0,
        LibPane::Groups => 1,
        LibPane::Tracks => 2,
    }
}

fn mode_icon(app: &App, m: BrowseMode) -> &'static str {
    let i = &app.icons;
    match m {
        BrowseMode::Folders => i.folder,
        BrowseMode::Artists => i.artist,
        BrowseMode::Albums => i.album,
        BrowseMode::Genres => i.genre,
        BrowseMode::Years => i.year,
        BrowseMode::Tracks => i.track,
    }
}

fn modes(buf: &mut Buffer, app: &mut App, rect: Rect, focused: bool, hit: &mut Hit) {
    let block = widgets::titles(widgets::panel(app, focused), vec![Span::raw("Browse")], Vec::new(), rect.width);
    let inner = block.inner(rect);
    let v = &mut app.library_view;
    let selected = v.mode_state.selected().or_else(|| BrowseMode::ALL.iter().position(|m| *m == v.mode));
    let len = BrowseMode::ALL.len();
    let offset = widgets::scroll_offset(v.mode_state.offset(), selected, len, inner.height as usize, 0);
    *v.mode_state.offset_mut() = offset;
    let app = &*app;
    block.render(rect, buf);
    let body = widgets::list(buf, app, rect, inner, len, offset, focused, |buf, i, row| {
        let m = BrowseMode::ALL[i];
        let sel = Sel::of(selected == Some(i), focused);
        let current = m == app.library_view.mode;
        let name_style = if current { widgets::accent(app) } else { Style::new().fg(app.theme.fg) };
        let spans = vec![
            Span::styled(mode_icon(app, m), Style::new().fg(app.theme.accent2)),
            Span::raw(" "),
            Span::styled(m.label(), sel.main(app, name_style)),
        ];
        widgets::draw_row(buf, app, row, None, spans, sel);
    });
    hit.lists.push(ListHit { area: body, target: ListTarget::LibModes, offset, len });
}

fn groups(buf: &mut Buffer, app: &mut App, rect: Rect, focused: bool, hit: &mut Hit) {
    let v = &app.library_view;
    let left = vec![Span::raw(format!("{} {}", mode_icon(app, v.mode), v.mode.label()))];
    let right = vec![Span::styled(v.groups.len().to_string(), widgets::dim(app))];
    let block = widgets::titles(widgets::panel(app, focused), left, right, rect.width);
    let inner = block.inner(rect);
    let v = &mut app.library_view;
    let (len, selected) = (v.groups.len(), v.group_state.selected());
    let margin = app.cfg.ui.scroll_margin as usize;
    let offset = widgets::scroll_offset(v.group_state.offset(), selected, len, inner.height as usize, margin);
    *v.group_state.offset_mut() = offset;
    let app = &*app;
    block.render(rect, buf);
    if len == 0 {
        let msg = if app.scan_progress.is_some() { "Scanning…" } else { "Nothing here" };
        widgets::empty_state(buf, inner, vec![Line::styled(msg, widgets::dim(app))]);
        return;
    }
    let playing = app.now_playing().map(|t| t.id);
    let groups = &app.library_view.groups;
    let body = widgets::list(buf, app, rect, inner, len, offset, focused, |buf, i, row| {
        let g = &groups[i];
        let sel = Sel::of(selected == Some(i), focused);
        let content = row.width.saturating_sub(2) as usize;
        // the name comes first (up to 16 cells); the detail gets what's left
        let detail = fit_detail(&g.detail, content.saturating_sub(width(&g.name).min(16) + 1));
        let dw = width(&detail);
        let name_w = content.saturating_sub(if dw > 0 { dw + 1 } else { 0 });
        // a folder path is told apart by its end: "…/ALI/Unknown Album"
        let name = if app.library_view.mode == BrowseMode::Folders { truncate_start(&g.name, name_w) } else { g.name.as_str().into() };
        let mut spans = fit_spans(vec![Span::styled(name, sel.main(app, Style::new().fg(app.theme.fg)))], name_w, false);
        if dw > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(detail, widgets::dim(app)));
        }
        let marker =
            playing.filter(|id| g.tracks.contains(id)).map(|_| Span::styled(app.icons.playing_marker, Style::new().fg(app.theme.playing)));
        widgets::draw_row(buf, app, row, marker, spans, sel);
    });
    hit.lists.push(ListHit { area: body, target: ListTarget::LibGroups, offset, len });
}

/// The group's "·"-separated detail with trailing parts dropped until it fits in `max` cells
/// ("2019 · 12 tracks · 48:02" -> "2019 · 12 tracks" -> "2019").
fn fit_detail(detail: &str, max: usize) -> String {
    let mut parts: Vec<&str> = detail.split(" · ").filter(|p| !p.is_empty()).collect();
    while !parts.is_empty() && width(&parts.join(" · ")) > max {
        parts.pop();
    }
    parts.join(" · ")
}

fn track_list(buf: &mut Buffer, app: &mut App, rect: Rect, focused: bool, hit: &mut Hit) {
    let v = &app.library_view;
    let group = v.group_state.selected().and_then(|i| v.groups.get(i));
    let name = group.map(|g| g.name.clone()).unwrap_or_else(|| v.mode.label().to_string());
    let len = v.tracks.len();
    let info = format!(" · {} · {}", count(len, "track"), fmt_duration(app.lib.total_duration(&v.tracks)));
    let left = vec![Span::raw(name), Span::styled(info, widgets::dim(app).remove_modifier(Modifier::BOLD))];
    let arrow = if v.sort_desc { "↓" } else { "↑" };
    let right = vec![Span::styled(format!("{} {arrow}", v.sort.label()), widgets::dim(app))];
    let block = widgets::titles(widgets::panel(app, focused), left, right, rect.width);
    let inner = block.inner(rect);
    let margin = app.cfg.ui.scroll_margin as usize;
    let v = &mut app.library_view;
    let offset = widgets::scroll_offset(v.track_state.offset(), v.track_state.selected(), len, tracks::body_height(inner), margin);
    *v.track_state.offset_mut() = offset;
    let app = &*app;
    block.render(rect, buf);
    if len == 0 {
        widgets::empty_state(buf, inner, empty_library(app, inner.width as usize));
        return;
    }
    let v = &app.library_view;
    let table = Table {
        ids: &v.tracks,
        offset,
        selected: v.track_state.selected(),
        focused,
        gutter: Gutter::Playing,
        sort: Some((v.sort, v.sort_desc)),
    };
    let body = tracks::draw(buf, app, rect, inner, &table);
    hit.lists.push(ListHit { area: body, target: ListTarget::LibTracks, offset, len });
}

/// Placeholder for an empty track list: scan progress, "no music found" with where to configure
/// music folders, or an empty group.
fn empty_library(app: &App, w: usize) -> Vec<Line<'static>> {
    let dim = widgets::dim(app);
    let accent = widgets::accent(app).add_modifier(Modifier::BOLD);
    if let Some((done, total)) = app.scan_progress {
        let mut lines = vec![Line::styled(format!("{} Scanning your music…", app.icons.track), accent)];
        const W: usize = 24;
        if let Some(filled) = (done.min(total) * W).checked_div(total) {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("━".repeat(filled), widgets::accent(app)),
                Span::styled("─".repeat(W - filled), Style::new().fg(app.theme.progress_bg)),
            ]));
            lines.push(Line::styled(format!("{done} of {total} files"), dim));
        }
        return lines;
    }
    if !app.lib.is_empty() {
        return vec![Line::styled("Nothing in this group", dim)];
    }
    let fg = Style::new().fg(app.theme.fg);
    let mut lines = vec![Line::styled(format!("{} No music found", app.icons.track), accent), Line::from("")];
    let dirs = &app.cfg.library.dirs;
    if dirs.is_empty() {
        lines.push(Line::styled("No music folders are configured.", dim));
    } else {
        lines.push(Line::styled("orbit looked for music in", dim));
        lines.extend(dirs.iter().map(|d| Line::styled(d.clone(), fg)));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("Add folders to  dirs = [...]  under [library] in", dim));
    lines.push(Line::styled(truncate_start(&home_relative(&app.paths.config_file), w).into_owned(), fg));
    if let Some(k) = app.keymap.keys_for(Action::Rescan).into_iter().next() {
        lines.push(Line::from(vec![
            Span::styled("then press ", dim),
            Span::styled(k, widgets::accent(app)),
            Span::styled(" to rescan", dim),
        ]));
    }
    lines
}

pub fn count(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mode: BrowseMode, pane: LibPane) -> LibraryView {
        LibraryView { mode, pane, ..LibraryView::default() }
    }

    #[test]
    fn panes_collapse_around_focus() {
        use LibPane::*;
        assert_eq!(visible_panes(&view(BrowseMode::Folders, Tracks), 120), [Modes, Groups, Tracks]);
        assert_eq!(visible_panes(&view(BrowseMode::Folders, Tracks), 80), [Groups, Tracks]);
        assert_eq!(visible_panes(&view(BrowseMode::Folders, Groups), 80), [Groups, Tracks]);
        assert_eq!(visible_panes(&view(BrowseMode::Folders, Modes), 80), [Modes, Groups]);
        assert_eq!(visible_panes(&view(BrowseMode::Folders, Modes), 40), [Modes]);
        assert_eq!(visible_panes(&view(BrowseMode::Folders, Groups), 40), [Groups]);
        // the single "All Tracks" group isn't worth a pane
        assert_eq!(visible_panes(&view(BrowseMode::Tracks, Tracks), 120), [Modes, Tracks]);
        assert_eq!(visible_panes(&view(BrowseMode::Tracks, Groups), 120), [Modes, Groups, Tracks]);
    }

    #[test]
    fn group_detail_drops_trailing_parts() {
        assert_eq!(fit_detail("2019 · 12 tracks · 48:02", 30), "2019 · 12 tracks · 48:02");
        assert_eq!(fit_detail("2019 · 12 tracks · 48:02", 17), "2019 · 12 tracks");
        assert_eq!(fit_detail("2019 · 12 tracks · 48:02", 6), "2019");
        assert_eq!(fit_detail("2019 · 12 tracks · 48:02", 3), "");
        assert_eq!(fit_detail("", 10), "");
    }
}
