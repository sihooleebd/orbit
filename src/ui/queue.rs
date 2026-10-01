//! Queue tab: the play queue with the current entry marked, play-order numbers on what comes next
//! (meaningful with shuffle), played entries dimmed, and totals / modes in the borders.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::library::count;
use super::tracks::{self, Gutter, Table};
use super::widgets;
use crate::app::{App, Hit, ListHit, ListTarget};
use crate::keymap::Action;
use crate::library::fmt_duration;
use crate::queue::Repeat;

pub fn draw(f: &mut Frame, app: &mut App, area: Rect, hit: &mut Hit) {
    let q = &app.queue;
    let len = q.len();
    let dim = widgets::dim(app).remove_modifier(Modifier::BOLD);
    let left = vec![
        Span::raw(format!("{} Queue", app.icons.queue)),
        Span::styled(format!(" · {} · {}", count(len, "track"), fmt_duration(app.lib.total_duration(&q.tracks))), dim),
    ];
    let (on, off) = (widgets::accent(app), widgets::dim(app));
    let repeat = match q.repeat {
        Repeat::Off => format!("{} off", app.icons.repeat),
        Repeat::All => format!("{} all", app.icons.repeat),
        Repeat::One => format!("{} one", app.icons.repeat_one),
    };
    let right = vec![
        Span::styled(format!("{} shuffle", app.icons.shuffle), if q.shuffle { on } else { off }),
        Span::styled("  ", off),
        Span::styled(repeat, if q.repeat == Repeat::Off { off } else { on }),
    ];
    let block = widgets::titles(widgets::panel(app, true), left, right, area.width);
    let block = widgets::titles_bottom(app, block, Vec::new(), remaining(app), area.width);
    let inner = block.inner(area);
    let margin = app.cfg.ui.scroll_margin as usize;
    let st = &mut app.queue_state;
    let offset = widgets::scroll_offset(st.offset(), st.selected(), len, tracks::body_height(inner), margin);
    *st.offset_mut() = offset;
    let app = &*app;
    let buf = f.buffer_mut();
    block.render(area, buf);
    if len == 0 {
        widgets::empty_state(buf, inner, empty(app));
        return;
    }
    let table =
        Table { ids: &app.queue.tracks, offset, selected: app.queue_state.selected(), focused: true, gutter: Gutter::Queue, sort: None };
    let body = tracks::draw(buf, app, area, inner, &table);
    hit.lists.push(ListHit { area: body, target: ListTarget::Queue, offset, len });
}

/// "▶ 3/42 · 1:02:13 left": position of the current entry and the time until the queue runs out.
fn remaining(app: &App) -> Vec<Span<'static>> {
    let q = &app.queue;
    let Some(cur) = q.current else { return Vec::new() };
    let upcoming: Duration =
        q.up_next(q.len()).iter().filter_map(|&i| q.tracks.get(i)).filter_map(|&id| app.track(id)).map(|t| t.duration).sum();
    let left_in_current = app.duration().unwrap_or_default().saturating_sub(app.position());
    vec![
        Span::styled(format!("{} {}/{}", app.icons.playing_marker, cur + 1, q.len()), Style::new().fg(app.theme.playing)),
        Span::styled(format!(" · {} left", fmt_duration(upcoming + left_in_current)), widgets::dim(app)),
    ]
}

fn empty(app: &App) -> Vec<Line<'static>> {
    let dim = widgets::dim(app);
    let mut lines = vec![
        Line::styled(format!("{} The queue is empty", app.icons.queue), widgets::accent(app).add_modifier(Modifier::BOLD)),
        Line::from(""),
    ];
    let key = |a: Action| app.keymap.keys_for(a).into_iter().next();
    for (action, what) in
        [(Action::Select, "to play a list"), (Action::Enqueue, "to add a track"), (Action::EnqueueAll, "to add a whole list")]
    {
        if let Some(k) = key(action) {
            lines.push(Line::from(vec![Span::styled(k, widgets::accent(app)), Span::styled(format!(" in the Library {what}"), dim)]));
        }
    }
    lines
}
