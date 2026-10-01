//! Overlays drawn over everything: help, search, the command line, the playlist picker, prompts
//! and confirmations. Each records its area in `hit.overlay` (clicks outside close it).

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

use super::centered;
use super::library::count;
use super::text::{fit, fit_spans, highlight, home_relative, slice_spans, spans_width, truncate, truncate_spans, truncate_start, width, wrap};
use super::widgets::{self, Sel};
use crate::app::{App, Hit, Input, ListHit, ListTarget, Overlay};
use crate::command::{COMMANDS, usage_for};
use crate::keymap::Action;
use crate::library::fmt_duration;

pub fn draw(f: &mut Frame, app: &mut App, hit: &mut Hit) {
    let area = f.area();
    if area.width < 4 || area.height < 3 {
        return;
    }
    match app.overlay {
        None => {}
        Some(Overlay::Help { .. }) => help(f.buffer_mut(), app, area, hit),
        Some(Overlay::Search { .. }) => search(f, app, area, hit),
        Some(Overlay::Command { .. }) => command(f, app, area, hit),
        Some(Overlay::PickPlaylist { .. }) => pick_playlist(f.buffer_mut(), app, area, hit),
        Some(Overlay::Prompt { .. }) => prompt(f, app, area, hit),
        Some(Overlay::Confirm { .. }) => confirm(f.buffer_mut(), app, area, hit),
    }
}

/// The popup frame (titles, borders per config); `inner` of a popup at `rect`.
fn frame(app: &App, rect: Rect, left: Vec<Span<'static>>, right: Vec<Span<'static>>, bottom: Vec<Span<'static>>) -> Block<'static> {
    let block = widgets::titles(widgets::popup(app), left, right, rect.width).style(Style::new().fg(app.theme.fg).bg(app.theme.bg));
    widgets::titles_at(block, Vec::new(), bottom, rect.width, true)
}

/// Inner area of a popup frame at `rect`.
fn frame_inner(app: &App, rect: Rect) -> Rect {
    widgets::popup(app).inner(rect)
}

fn show(buf: &mut Buffer, block: Block<'static>, rect: Rect) {
    Clear.render(rect, buf);
    block.render(rect, buf);
}

/// Draws `input` on one row, scrolled so the cursor stays visible; returns the cursor cell.
fn input_line(buf: &mut Buffer, rect: Rect, input: &Input, style: Style) -> Position {
    let avail = rect.width as usize;
    if avail == 0 {
        return rect.as_position();
    }
    let before: String = input.text.chars().take(input.cursor).collect();
    let cursor = width(&before);
    let start = (cursor + 1).saturating_sub(avail);
    let spans = slice_spans(&[Span::styled(input.text.as_str(), style)], start, avail);
    widgets::line(buf, rect.x, rect.y, spans, rect.width);
    Position::new(rect.x + (cursor - start).min(avail.saturating_sub(1)) as u16, rect.y)
}

fn first_key(app: &App, a: Action) -> Option<String> {
    app.keymap.keys_for(a).into_iter().next()
}

fn help(buf: &mut Buffer, app: &mut App, area: Rect, hit: &mut Hit) {
    let rect = centered(area, area.width.saturating_sub(4).min(108), area.height.saturating_sub(2));
    let inner = frame_inner(app, rect);
    // without borders the scrollbar takes the last column
    let text_w = inner.width.saturating_sub(u16::from(widgets::borders(app).is_none()));
    let lines = help_lines(app, text_w as usize);
    let max = lines.len().saturating_sub(inner.height as usize);
    let scroll = match &mut app.overlay {
        Some(Overlay::Help { scroll }) => {
            *scroll = (*scroll).min(max as u16);
            *scroll as usize
        }
        _ => 0,
    };
    let app = &*app;
    let shown = scroll + (inner.height as usize).min(lines.len());
    let right = vec![Span::styled(format!("{}–{} of {}", (scroll + 1).min(shown), shown, lines.len()), widgets::dim(app))];
    let mut bottom = Vec::new();
    if let (Some(up), Some(down)) = (first_key(app, Action::Up), first_key(app, Action::Down)) {
        bottom.push(Span::styled(format!("{up}/{down} scroll · "), widgets::dim(app)));
    }
    let close = first_key(app, Action::Back).unwrap_or_else(|| "esc".into());
    bottom.push(Span::styled(format!("{close} close"), widgets::dim(app)));
    let title = match first_key(app, Action::Help) {
        Some(k) => format!("{k} Help — keys & commands"),
        None => "Help — keys & commands".into(),
    };
    show(buf, frame(app, rect, vec![Span::raw(title)], right, bottom), rect);
    for (k, line) in lines.into_iter().skip(scroll).take(inner.height as usize).enumerate() {
        // set_line, not widgets::line: headings and the logo are styled as whole lines
        buf.set_line(inner.x, inner.y + k as u16, &line, text_w);
    }
    let (_, col) = widgets::scroll_split(app, rect, inner, max > 0);
    widgets::scrollbar(buf, app, col, max + inner.height as usize, scroll, true);
    hit.overlay = Some(rect);
    hit.help_max = Some(max);
}

/// Keys by category (with each action's config name, for `[keys]`), custom `:command` bindings,
/// the command reference and mouse support.
fn help_lines(app: &App, w: usize) -> Vec<Line<'static>> {
    let th = &app.theme;
    let dim = widgets::dim(app);
    let fg = Style::new().fg(th.fg);
    let key_style = Style::new().fg(th.accent2).add_modifier(Modifier::BOLD);
    let head = |s: &str| Line::styled(s.to_string(), widgets::accent(app).add_modifier(Modifier::BOLD));
    let key_w = 18.min(w / 3);
    let name_w = Action::ALL.iter().map(|a| a.name().len()).max().unwrap_or(0);
    let names = w >= key_w + name_w + 40;
    let desc_w = w.saturating_sub(4 + key_w + if names { name_w + 2 } else { 0 });

    const LOGO: [&str; 5] = [
        r"   ____  ____  ____  __________",
        r"  / __ \/ __ \/ __ )/  _/_  __/",
        r" / / / / /_/ / __  |/ /  / /   ",
        r"/ /_/ / _, _/ /_/ // /  / /    ",
        r"\____/_/ |_/_____/___/ /_/     ",
    ];
    let mut lines = Vec::new();
    if w >= LOGO[0].len() + 2 {
        for (i, row) in LOGO.iter().enumerate() {
            lines.push(Line::styled(format!("  {row}"), Style::new().fg(th.gradient_at(i as f32 / (LOGO.len() - 1) as f32))));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled(format!("  orbit {}", env!("CARGO_PKG_VERSION")), widgets::accent(app).add_modifier(Modifier::BOLD)),
        Span::styled(" · a terminal music player · by ", dim),
        Span::styled("Benjamin Lee", Style::new().fg(th.accent2)),
    ]));
    lines.push(Line::from(""));
    lines.extend([
        Line::from(vec![
            Span::styled("config  ", dim),
            Span::styled(truncate_start(&home_relative(&app.paths.config_file), w.saturating_sub(8)).into_owned(), fg),
        ]),
        Line::styled("Rebind in [keys]:  \"ctrl+n\" = \"next\"   \"F1\" = \":vol 30\"   \"x\" = \"none\"", dim),
    ]);
    let mut cats: Vec<&str> = Vec::new();
    for a in Action::ALL {
        if !cats.contains(&a.category()) {
            cats.push(a.category());
        }
    }
    for cat in cats {
        lines.push(Line::from(""));
        lines.push(head(cat));
        for a in Action::ALL.iter().filter(|a| a.category() == cat) {
            let keys = app.keymap.keys_for(*a);
            let (k, ks) = if keys.is_empty() { ("—".to_string(), dim) } else { (keys.join(", "), key_style) };
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(fit(&k, key_w, false), ks),
                Span::raw("  "),
                Span::styled(fit(a.description(), desc_w, false), fg),
            ];
            if names {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(fit(a.name(), name_w, true), dim));
            }
            lines.push(Line::from(spans));
        }
    }
    let custom = app.keymap.command_bindings();
    if !custom.is_empty() {
        lines.push(Line::from(""));
        lines.push(head("Custom bindings"));
        for (k, c) in custom {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(fit(&k, key_w, false), key_style),
                Span::raw("  "),
                Span::styled(format!(":{c}"), fg),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(head("Commands"));
    let cmd_w = COMMANDS.iter().map(|(u, _)| width(u) + 1).max().unwrap_or(0).min(w * 2 / 5);
    for (usage, desc) in COMMANDS {
        let usage = format!(":{usage}");
        let desc = truncate(desc, w.saturating_sub(cmd_w + 4)).into_owned();
        if width(&usage) <= cmd_w {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(fit(&usage, cmd_w, false), key_style),
                Span::raw("  "),
                Span::styled(desc, fg),
            ]));
        } else {
            lines.push(Line::from(vec![Span::raw("  "), Span::styled(truncate(&usage, w.saturating_sub(2)).into_owned(), key_style)]));
            lines.push(Line::from(vec![Span::raw(" ".repeat(cmd_w + 4)), Span::styled(desc, fg)]));
        }
    }
    lines.push(Line::from(""));
    lines.push(head("Mouse"));
    for (what, how) in [
        ("tabs, buttons", "click (the state icons in the tab bar too)"),
        ("progress bar", "click to seek"),
        ("volume", "click or scroll"),
        ("lists", "click to select, scroll to move"),
        ("EQ sliders", "click to set a band"),
    ] {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(fit(what, key_w, false), key_style),
            Span::raw("  "),
            Span::styled(truncate(how, w.saturating_sub(key_w + 4)).into_owned(), fg),
        ]));
    }
    lines
}

fn search(f: &mut Frame, app: &mut App, area: Rect, hit: &mut Hit) {
    let w = area.width.saturating_sub(4).clamp(area.width.min(20), 110);
    let h = area.height.saturating_sub(2).min(26);
    let rect = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 4, w, h);
    let inner = frame_inner(app, rect);
    let sep = u16::from(inner.height >= 4);
    let list = Rect { y: inner.y + 1 + sep, height: inner.height.saturating_sub(1 + sep), ..inner };
    let Some(Overlay::Search { results, state, .. }) = &mut app.overlay else { return };
    let len = results.len();
    let offset = widgets::scroll_offset(state.offset(), state.selected(), len, list.height as usize, 2);
    *state.offset_mut() = offset;
    let app = &*app;
    let Some(Overlay::Search { input, results, state }) = &app.overlay else { return };
    let dim = widgets::dim(app);
    let right = match (input.text.trim().is_empty(), len) {
        (true, _) => Vec::new(),
        (false, 0) => vec![Span::styled("no matches", Style::new().fg(app.theme.warn))],
        (false, n) => vec![Span::styled(count(n, "result"), dim)],
    };
    let bottom = vec![Span::styled("↑↓ select · enter play · esc close", dim)];
    let buf = f.buffer_mut();
    show(buf, frame(app, rect, vec![Span::raw(format!("{} Search", app.icons.search))], right, bottom), rect);
    if inner.is_empty() {
        return;
    }

    let prompt = format!("{} ", app.icons.search);
    let pw = width(&prompt) as u16;
    buf.set_stringn(inner.x, inner.y, &prompt, inner.width as usize, widgets::accent(app).add_modifier(Modifier::BOLD));
    let field = Rect { x: inner.x + pw, width: inner.width.saturating_sub(pw), height: 1, ..inner };
    if input.text.is_empty() {
        buf.set_stringn(field.x, field.y, "title, artist or album — all words must match", field.width as usize, dim);
    }
    let cursor = input_line(buf, field, input, Style::new().fg(app.theme.fg).add_modifier(Modifier::BOLD));
    f.set_cursor_position(cursor);
    let buf = f.buffer_mut();
    if sep > 0 {
        buf.set_stringn(inner.x, inner.y + 1, "─".repeat(inner.width as usize), inner.width as usize, Style::new().fg(app.theme.border));
    }
    if list.is_empty() {
        hit.overlay = Some(rect);
        return;
    }
    let selected = state.selected();
    let hl = widgets::accent(app).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let body = widgets::list(buf, app, rect, list, len, offset, true, |buf, i, row| {
        let r = &results[i];
        let Some(t) = app.track(r.id) else { return };
        let is_hl = |c: usize| r.matched.contains(&(c as u32));
        let content = row.width.saturating_sub(2) as usize;
        let dur_w = 6;
        let tw = content.saturating_sub(dur_w) * 9 / 20;
        let aw = content.saturating_sub(dur_w) * 3 / 10;
        let alw = content.saturating_sub(dur_w + tw + aw + 2);
        let (tc, ac) = (t.title.chars().count(), t.artist.chars().count());
        let mut spans = fit_spans(highlight(&t.title, 0, is_hl, Style::new().fg(app.theme.fg), hl), tw, false);
        spans.push(Span::raw(" "));
        spans.extend(fit_spans(highlight(&t.artist, tc + 1, is_hl, Style::new().fg(app.theme.accent2), hl), aw, false));
        spans.push(Span::raw(" "));
        spans.extend(fit_spans(highlight(&t.album, tc + ac + 2, is_hl, dim, hl), alw, false));
        spans.push(Span::styled(fit(&fmt_duration(t.duration), dur_w, true), dim));
        let marker = app.is_current(t.id).then(|| Span::styled(app.icons.playing_marker, Style::new().fg(app.theme.playing)));
        widgets::draw_row(buf, app, row, marker, spans, Sel::of(selected == Some(i), true));
    });
    hit.lists.push(ListHit { area: body, target: ListTarget::SearchResults, offset, len });
    if len == 0 && !input.text.trim().is_empty() {
        widgets::empty_state(buf, list, vec![Line::styled("No matches", dim)]);
    }
    hit.overlay = Some(rect);
}

/// The ":" line replaces the status line; completions pop up above it.
fn command(f: &mut Frame, app: &App, area: Rect, hit: &mut Hit) {
    let Some(Overlay::Command { input, completions, selected, history_pos }) = &app.overlay else { return };
    let th = &app.theme;
    let dim = widgets::dim(app);
    let y = area.bottom() - 1;
    let line_rect = Rect::new(area.x, y, area.width, 1);
    let buf = f.buffer_mut();
    Clear.render(line_rect, buf);
    buf.set_style(line_rect, Style::new().fg(th.fg).bg(th.bg));
    buf.set_stringn(area.x, y, ":", 1, widgets::accent(app).add_modifier(Modifier::BOLD));

    // hint on the right: history position, or the usage of the command being typed
    let mut hint: Vec<String> = history_pos.map(|i| format!("history {}/{}", i + 1, app.command_history.len())).into_iter().collect();
    hint.extend(usage_for(&input.text).map(|(u, d)| format!("{u}  —  {d}")));
    let hint = hint.join("  ·  ");
    let text_w = width(&input.text) as u16 + 2;
    let hint_w = (width(&hint) as u16).min(area.width.saturating_sub(text_w + 4));
    let field_w = area.width.saturating_sub(1 + if hint_w >= 8 { hint_w + 2 } else { 0 });
    if hint_w >= 8 {
        let hint = truncate(&hint, hint_w as usize);
        buf.set_stringn(area.right() - hint_w, y, hint, hint_w as usize, dim);
    }
    let cursor = input_line(buf, Rect::new(area.x + 1, y, field_w, 1), input, Style::new().fg(th.fg));
    f.set_cursor_position(cursor);

    let mut overlay = line_rect;
    let n = completions.len();
    let chrome = 2;
    let room = y.saturating_sub(area.y);
    if n > 0 && room > chrome {
        let rows = (n as u16).min(10).min(room - chrome);
        let desc = |c: &str| usage_for(c).map(|(_, d)| *d).unwrap_or("");
        let cw = completions.iter().map(|c| width(c)).max().unwrap_or(0);
        let dw = completions.iter().map(|c| width(desc(c))).max().unwrap_or(0);
        let w = ((cw + dw + 4 + 4) as u16).clamp(24.min(area.width), area.width.min(96));
        let rect = Rect::new(area.x, y - rows - chrome, w, rows + chrome);
        let block = widgets::popup(app).style(Style::new().fg(th.fg).bg(th.bg));
        let inner = block.inner(rect);
        show(f.buffer_mut(), block, rect);
        let offset = widgets::scroll_offset(0, *selected, n, inner.height as usize, 0);
        let body = widgets::list(f.buffer_mut(), app, rect, inner, n, offset, true, |buf, i, row| {
            let c = &completions[i];
            let content = row.width.saturating_sub(2) as usize;
            let cw = cw.min(content);
            let mut spans = fit_spans(vec![Span::styled(c.as_str(), Style::new().fg(th.accent2))], cw, false);
            spans.push(Span::raw("  "));
            spans.push(Span::styled(desc(c), dim));
            widgets::draw_row(buf, app, row, None, spans, Sel::of(*selected == Some(i), true));
        });
        hit.lists.push(ListHit { area: body, target: ListTarget::Completions, offset, len: n });
        overlay = overlay.union(rect);
    }
    hit.overlay = Some(overlay);
}

fn pick_playlist(buf: &mut Buffer, app: &mut App, area: Rect, hit: &mut Hit) {
    let len = app.playlists.lists.len() + 1;
    let chrome = 2;
    let rect = centered(area, 56, len as u16 + chrome);
    let inner = frame_inner(app, rect);
    let Some(Overlay::PickPlaylist { state, .. }) = &mut app.overlay else { return };
    let offset = widgets::scroll_offset(state.offset(), state.selected(), len, inner.height as usize, 1);
    *state.offset_mut() = offset;
    let app = &*app;
    let Some(Overlay::PickPlaylist { tracks, state }) = &app.overlay else { return };
    let left = vec![Span::raw(format!("Add {} to…", count(tracks.len(), "track")))];
    let bottom = vec![Span::styled("enter add · esc cancel", widgets::dim(app))];
    show(buf, frame(app, rect, left, Vec::new(), bottom), rect);
    let selected = state.selected();
    let body = widgets::list(buf, app, rect, inner, len, offset, true, |buf, i, row| {
        let sel = Sel::of(selected == Some(i), true);
        let spans = if i == 0 {
            vec![Span::styled("+ New playlist…", sel.main(app, widgets::accent(app)))]
        } else {
            let p = &app.playlists.lists[i - 1];
            let detail = if p.external { format!("read-only · {}", p.tracks.len()) } else { p.tracks.len().to_string() };
            let content = row.width.saturating_sub(2) as usize;
            let name_w = content.saturating_sub(width(&detail) + 1);
            let name_style = if p.external { widgets::dim(app) } else { Style::new().fg(app.theme.fg) };
            let mut s = fit_spans(
                vec![
                    Span::styled(app.icons.playlist, widgets::accent(app)),
                    Span::raw(" "),
                    Span::styled(p.name.as_str(), sel.main(app, name_style)),
                ],
                name_w,
                false,
            );
            s.push(Span::raw(" "));
            s.push(Span::styled(detail, widgets::dim(app)));
            s
        };
        widgets::draw_row(buf, app, row, None, spans, sel);
    });
    hit.lists.push(ListHit { area: body, target: ListTarget::PickPlaylist, offset, len });
    hit.overlay = Some(rect);
}

fn prompt(f: &mut Frame, app: &App, area: Rect, hit: &mut Hit) {
    let Some(Overlay::Prompt { title, input, .. }) = &app.overlay else { return };
    let chrome = 2;
    let rect = centered(area, area.width.saturating_sub(4).clamp(area.width.min(24), 64), 3 + chrome);
    let bottom = vec![Span::styled("enter confirm · esc cancel", widgets::dim(app))];
    let block = frame(app, rect, vec![Span::raw(title.clone())], Vec::new(), bottom);
    let inner = block.inner(rect);
    show(f.buffer_mut(), block, rect);
    if inner.is_empty() {
        return;
    }
    let buf = f.buffer_mut();
    buf.set_stringn(inner.x, inner.y + 1, "›", 1, widgets::accent(app).add_modifier(Modifier::BOLD));
    let field = Rect::new(inner.x + 2, inner.y + 1, inner.width.saturating_sub(2), 1);
    let cursor = input_line(buf, field, input, Style::new().fg(app.theme.fg).add_modifier(Modifier::BOLD));
    buf.set_style(Rect { y: field.y, ..field }, Style::new().add_modifier(Modifier::UNDERLINED));
    f.set_cursor_position(cursor);
    hit.overlay = Some(rect);
}

fn confirm(buf: &mut Buffer, app: &App, area: Rect, hit: &mut Hit) {
    let Some(Overlay::Confirm { message, .. }) = &app.overlay else { return };
    let chrome = 2;
    let w = area.width.saturating_sub(4).clamp(area.width.min(24), 60);
    let text_w = w.saturating_sub(4) as usize;
    let lines = wrap(message, text_w);
    let rect = centered(area, w, lines.len() as u16 + 2 + chrome);
    let title = vec![Span::styled("Confirm", Style::new().fg(app.theme.warn).add_modifier(Modifier::BOLD))];
    let block = frame(app, rect, title, Vec::new(), Vec::new());
    let inner = block.inner(rect);
    show(buf, block, rect);
    let fg = Style::new().fg(app.theme.fg);
    for (k, l) in lines.iter().take(inner.height as usize).enumerate() {
        let lw = width(l) as u16;
        buf.set_stringn(inner.x + inner.width.saturating_sub(lw) / 2, inner.y + k as u16, l, inner.width as usize, fg);
    }
    let key = Style::new().fg(app.theme.sel_fg).bg(app.accent()).add_modifier(Modifier::BOLD);
    let buttons = vec![
        Span::styled(" y ", key),
        Span::styled(" yes", fg),
        Span::raw("      "),
        Span::styled(" n ", Style::new().fg(app.theme.sel_fg).bg(app.theme.dim).add_modifier(Modifier::BOLD)),
        Span::styled(" no", fg),
    ];
    let bw = spans_width(&buttons) as u16;
    if inner.height > 0 {
        let y = inner.bottom() - 1;
        widgets::line(buf, inner.x + inner.width.saturating_sub(bw) / 2, y, truncate_spans(buttons, inner.width as usize), inner.width);
    }
    hit.overlay = Some(rect);
}
