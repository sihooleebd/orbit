//! Playlists tab: smart and user playlists on the left, the selected one's tracks on the right.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::library::count;
use super::text::{fit_spans, width};
use super::tracks::{self, Gutter, Table};
use super::widgets::{self, Sel};
use crate::app::{App, Hit, ListHit, ListTarget, PlPane, PlaylistEntry};
use crate::keymap::Action;
use crate::library::fmt_duration;
use crate::playlist::Smart;

pub fn draw(f: &mut Frame, app: &mut App, area: Rect, hit: &mut Hit) {
    let pane = app.playlist_view.pane;
    let (lists, tracks) = if area.width < 60 {
        // narrow: only the focused pane
        match pane {
            PlPane::Lists => (Some(area), None),
            PlPane::Tracks => (None, Some(area)),
        }
    } else {
        let w = (u32::from(area.width) * 28 / 100).clamp(24, 40) as u16;
        let gap = u16::from(widgets::borders(app).is_none());
        let [l, t] = Layout::horizontal([Constraint::Length(w), Constraint::Fill(1)]).spacing(gap).areas(area);
        (Some(l), Some(t))
    };
    let entries = app.playlist_entries();
    if let Some(rect) = lists {
        entry_list(f, app, rect, &entries, pane == PlPane::Lists, hit);
    }
    if let Some(rect) = tracks {
        track_list(f, app, rect, &entries, pane == PlPane::Tracks, hit);
    }
}

fn entry_list(f: &mut Frame, app: &mut App, rect: Rect, entries: &[PlaylistEntry], focused: bool, hit: &mut Hit) {
    let user = app.playlists.lists.len();
    let right = vec![Span::styled(user.to_string(), widgets::dim(app))];
    let block =
        widgets::titles(widgets::panel(app, focused), vec![Span::raw(format!("{} Playlists", app.icons.playlist))], right, rect.width);
    let inner = block.inner(rect);
    let len = entries.len();
    let margin = app.cfg.ui.scroll_margin as usize;
    let st = &mut app.playlist_view.list_state;
    let selected = st.selected();
    let offset = widgets::scroll_offset(st.offset(), selected, len, inner.height as usize, margin);
    *st.offset_mut() = offset;
    let app = &*app;
    let buf = f.buffer_mut();
    block.render(rect, buf);
    let body = widgets::list(buf, app, rect, inner, len, offset, focused, |buf, i, row| {
        let e = entries[i];
        let sel = Sel::of(selected == Some(i), focused);
        let (icon, icon_color) = match e {
            PlaylistEntry::Smart(_) => (app.icons.smart, app.theme.accent2),
            PlaylistEntry::User(_) => (app.icons.playlist, app.accent()),
        };
        let detail = match e {
            PlaylistEntry::User(j) => app.playlists.lists.get(j).map(|p| {
                let n = p.tracks.len().to_string();
                if p.external { format!("ext · {n}") } else { n }
            }),
            // smart playlists are computed on selection; only the selected one has a count
            PlaylistEntry::Smart(_) if selected == Some(i) => Some(app.playlist_view.tracks.len().to_string()),
            PlaylistEntry::Smart(_) => None,
        }
        .unwrap_or_default();
        let content = row.width.saturating_sub(2) as usize;
        let dw = width(&detail);
        let name_w = content.saturating_sub(if dw > 0 { dw + 1 } else { 0 });
        let name = vec![
            Span::styled(icon, Style::new().fg(icon_color)),
            Span::raw(" "),
            Span::styled(app.entry_label(e), sel.main(app, Style::new().fg(app.theme.fg))),
        ];
        let mut spans = fit_spans(name, name_w, false);
        if dw > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(detail, widgets::dim(app)));
        }
        widgets::draw_row(buf, app, row, None, spans, sel);
    });
    hit.lists.push(ListHit { area: body, target: ListTarget::PlLists, offset, len });
}

fn track_list(f: &mut Frame, app: &mut App, rect: Rect, entries: &[PlaylistEntry], focused: bool, hit: &mut Hit) {
    let pv = &app.playlist_view;
    let entry = pv.list_state.selected().and_then(|i| entries.get(i).copied());
    let len = pv.tracks.len();
    let name = entry.map(|e| app.entry_label(e)).unwrap_or_else(|| "Playlist".into());
    let icon = match entry {
        Some(PlaylistEntry::Smart(_)) => app.icons.smart,
        _ => app.icons.playlist,
    };
    let dim = widgets::dim(app).remove_modifier(Modifier::BOLD);
    let left = vec![
        Span::raw(format!("{icon} {name}")),
        Span::styled(format!(" · {} · {}", count(len, "track"), fmt_duration(app.lib.total_duration(&pv.tracks))), dim),
    ];
    let missing = if pv.missing > 0 {
        vec![Span::styled(format!("! {} missing", count(pv.missing, "file")), Style::new().fg(app.theme.warn))]
    } else {
        Vec::new()
    };
    let block = widgets::titles(widgets::panel(app, focused), left, missing, rect.width);
    let inner = block.inner(rect);
    let margin = app.cfg.ui.scroll_margin as usize;
    let st = &mut app.playlist_view.track_state;
    let offset = widgets::scroll_offset(st.offset(), st.selected(), len, tracks::body_height(inner), margin);
    *st.offset_mut() = offset;
    let app = &*app;
    let buf = f.buffer_mut();
    block.render(rect, buf);
    if len == 0 {
        widgets::empty_state(buf, inner, empty(app, entry));
        return;
    }
    let pv = &app.playlist_view;
    let table = Table { ids: &pv.tracks, offset, selected: pv.track_state.selected(), focused, gutter: Gutter::Playing, sort: None };
    let body = tracks::draw(buf, app, rect, inner, &table);
    hit.lists.push(ListHit { area: body, target: ListTarget::PlTracks, offset, len });
}

fn empty(app: &App, entry: Option<PlaylistEntry>) -> Vec<Line<'static>> {
    let dim = widgets::dim(app);
    let key = |a: Action| app.keymap.keys_for(a).into_iter().next();
    let hint = |a: Action, before: &str, after: &str| {
        key(a).map(|k| {
            Line::from(vec![
                Span::styled(before.to_string(), dim),
                Span::styled(k, widgets::accent(app)),
                Span::styled(after.to_string(), dim),
            ])
        })
    };
    let (title, hint) = match entry {
        None => ("Select a playlist", None),
        Some(PlaylistEntry::Smart(Smart::Favorites)) => {
            ("No favorites yet", hint(Action::ToggleFavorite, "press ", " on a track to favorite it"))
        }
        Some(PlaylistEntry::Smart(Smart::MostPlayed | Smart::RecentlyPlayed)) => ("Nothing played yet", None),
        Some(PlaylistEntry::Smart(Smart::NeverPlayed)) => ("You've played everything", None),
        Some(PlaylistEntry::Smart(Smart::RecentlyAdded)) => ("No tracks", None),
        Some(PlaylistEntry::Smart(Smart::Radio)) if app.analysis_progress.is_some() => ("Listening to your library…", None),
        Some(PlaylistEntry::Smart(Smart::Radio)) => ("Play a few tracks: the radio picks what sounds like them", None),
        Some(PlaylistEntry::User(_)) if app.playlist_view.missing > 0 => ("None of these files are in the library", None),
        Some(PlaylistEntry::User(_)) => {
            ("This playlist is empty", hint(Action::AddToPlaylist, "press ", " on tracks in the Library to add them"))
        }
    };
    let mut lines = vec![Line::styled(title, widgets::accent(app).add_modifier(Modifier::BOLD))];
    if let Some(h) = hint {
        lines.push(Line::from(""));
        lines.push(h);
    }
    lines
}
