//! All rendering for Orbit.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, BucketRow, Focus, InputKind, Mode};
use crate::audio::{BAND_LABELS, NUM_BANDS, PRESETS};
use crate::model::fmt_duration;
use crate::theme;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Paint the whole background.
    f.render_widget(Block::new().style(Style::new().bg(theme::bg())), area);

    if app.zen {
        draw_zen(f, area, app);
    } else {
        let chunks = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Min(3),    // panels
            Constraint::Length(5), // now playing
            Constraint::Length(1), // footer
        ])
        .split(area);

        draw_header(f, chunks[0], app);
        draw_panels(f, chunks[1], app);
        draw_now_playing(f, chunks[2], app);
        draw_footer(f, chunks[3], app);
    }

    match &app.mode {
        Mode::Help => draw_help(f, area),
        Mode::Eq => draw_eq(f, area, app),
        Mode::Input(_) => draw_input(f, area, app),
        Mode::PickBucket { .. } => draw_pick(f, area, app),
        Mode::FileBrowser => draw_browser(f, area, app),
        Mode::ManageFolders => draw_manage(f, area, app),
        Mode::BucketView(row) => draw_bucket_view(f, area, app, *row),
        Mode::Normal => {}
    }
}

// ---------------------------------------------------------------------------
// Header & footer
// ---------------------------------------------------------------------------

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let halves = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let brand = Line::from(vec![
        Span::styled(" ◈ ", Style::new().fg(theme::accent2())),
        Span::styled(
            "ORBIT",
            Style::new()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  local music", Style::new().fg(theme::dim())),
    ]);
    f.render_widget(
        Paragraph::new(brand).style(Style::new().bg(theme::bg())),
        halves[0],
    );

    // Right side: scan status / library size + EQ indicator.
    let mut right: Vec<Span> = Vec::new();
    if app.scanning {
        let frame = SPINNER[app.spinner_frame % SPINNER.len()];
        right.push(Span::styled(
            format!("{frame} scanning {} ", app.scan_count),
            Style::new().fg(theme::gold()),
        ));
    } else {
        right.push(Span::styled(
            format!("{} tracks  ", app.library.tracks.len()),
            Style::new().fg(theme::dim()),
        ));
    }
    let eq = app.eq();
    let eq_color = if eq.enabled() { theme::green() } else { theme::faint() };
    right.push(Span::styled(
        format!("EQ:{} ", if eq.enabled() { "on" } else { "off" }),
        Style::new().fg(eq_color),
    ));
    f.render_widget(
        Paragraph::new(Line::from(right))
            .alignment(Alignment::Right)
            .style(Style::new().bg(theme::bg())),
        halves[1],
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let hint = match app.mode {
        Mode::Eq => "←→ band · ↑↓ gain · x bypass · f flat · 1-5 presets · Esc close",
        Mode::Help => "Esc close",
        Mode::Input(_) => "type · Enter confirm · Esc cancel",
        Mode::PickBucket { .. } => "↑↓ pick · n new bucket · Enter add · Esc cancel",
        Mode::FileBrowser => "↑↓ move · Enter open · ⌫ up · a add folder · . hidden · Esc cancel",
        Mode::ManageFolders => "↑↓ move · a add · x remove · r rescan · Esc close",
        Mode::BucketView(_) => "↑↓ move · Enter play · x remove · K/J reorder · r rename · Esc back",
        Mode::Normal => {
            "Tab panes · Enter play · Space pause · n/p · ←→ seek · e EQ · z zen · b bucket · a add · ? help"
        }
    };

    let halves = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);

    let status_color = if app.status_is_error {
        theme::error()
    } else {
        theme::dim()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", app.status),
            Style::new().fg(status_color),
        )))
        .style(Style::new().bg(theme::bg())),
        halves[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{hint} "),
            Style::new().fg(theme::faint()),
        )))
        .alignment(Alignment::Right)
        .style(Style::new().bg(theme::bg())),
        halves[1],
    );
}

// ---------------------------------------------------------------------------
// Panels
// ---------------------------------------------------------------------------

fn panel_block(title_icon: &str, title: &str, focused: bool) -> Block<'static> {
    let border_color = if focused {
        theme::border_focus()
    } else {
        theme::border()
    };
    let title_color = if focused { theme::accent() } else { theme::dim() };
    let title_line = Line::from(vec![
        Span::styled(format!(" {title_icon} "), Style::new().fg(theme::accent2())),
        Span::styled(
            format!("{title} "),
            Style::new()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border_color))
        .title(title_line)
        .style(Style::new().bg(theme::panel_bg()))
}

fn draw_panels(f: &mut Frame, area: Rect, app: &mut App) {
    let cols = Layout::horizontal([
        Constraint::Percentage(40),
        Constraint::Percentage(28),
        Constraint::Percentage(32),
    ])
    .split(area);

    draw_library(f, cols[0], app);
    draw_buckets(f, cols[1], app);
    draw_queue(f, cols[2], app);
}

fn draw_library(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Library;
    let inner_w = area.width.saturating_sub(4) as usize;

    let filter = app.library.filter();
    let title = if filter.is_empty() {
        "LIBRARY".to_string()
    } else {
        format!("LIBRARY /{filter}")
    };
    let block = panel_block("♪", &title, focused);

    let now_path = app.now_playing.as_ref().map(|t| t.path.clone());

    let items: Vec<ListItem> = app
        .library
        .view
        .iter()
        .filter_map(|&i| app.library.tracks.get(i))
        .map(|t| {
            let playing = now_path.as_ref() == Some(&t.path);
            let dur = fmt_duration(t.duration());
            let left = t.title_artist();
            let text = pad_between(&left, &dur, inner_w);
            let style = if playing {
                Style::new().fg(theme::gold())
            } else {
                Style::new().fg(theme::fg())
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    let empty_note = if app.library.tracks.is_empty() && !app.scanning {
        Some("No tracks. Press 'A' to add a music folder.")
    } else if app.library.view_len() == 0 {
        Some("No matches.")
    } else {
        None
    };

    render_list(f, area, block, items, focused, empty_note, &mut app.lib_state);
}

fn draw_buckets(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Buckets;

    // When focused, split the pane: bucket list on top, track preview below.
    let (list_area, preview_area) = if focused {
        let rows = Layout::vertical([Constraint::Min(4), Constraint::Percentage(45)]).split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };

    let inner_w = list_area.width.saturating_sub(4) as usize;
    let block = panel_block("◆", "BUCKETS", focused);

    let mut items: Vec<ListItem> = Vec::new();

    // Smart (auto) buckets first, in italic with their own icon + colour.
    for b in &app.smart {
        let meta = format!("{}", b.tracks.len());
        let left = format!("{} {}", b.icon, b.name);
        let text = pad_between(&left, &meta, inner_w);
        items.push(ListItem::new(Line::from(Span::styled(
            text,
            Style::new()
                .fg(theme::bucket_color(b.color))
                .add_modifier(Modifier::ITALIC),
        ))));
    }
    // Then user buckets, each in its accent colour.
    for b in &app.store.buckets {
        let meta = format!("{}", b.tracks.len());
        let left = format!("◆ {}", b.name);
        let text = pad_between(&left, &meta, inner_w);
        items.push(ListItem::new(Line::from(Span::styled(
            text,
            Style::new().fg(theme::bucket_color(b.color)),
        ))));
    }

    let empty = if items.is_empty() {
        Some("No buckets yet. Press 'b' to create one, or 'S' to save the queue.")
    } else {
        None
    };

    render_list(f, list_area, block, items, focused, empty, &mut app.bucket_state);

    if let Some(preview) = preview_area {
        draw_bucket_preview(f, preview, app);
    }
}

/// Bottom half of the focused Buckets pane: the tracks in the selected bucket.
fn draw_bucket_preview(f: &mut Frame, area: Rect, app: &App) {
    let sel = app.bucket_state.selected();
    // Resolve the selection to (icon, name, color, tracks).
    let resolved: Option<(&str, String, u8, &Vec<crate::model::Track>)> =
        sel.and_then(|i| {
            if i < app.smart.len() {
                let b = &app.smart[i];
                Some((b.icon, b.name.clone(), b.color, &b.tracks))
            } else {
                let j = i - app.smart.len();
                app.store
                    .buckets
                    .get(j)
                    .map(|b| ("◆", b.name.clone(), b.color, &b.tracks))
            }
        });

    let (icon, name, color, tracks) = match resolved {
        Some(v) => v,
        None => {
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(theme::border()))
                .style(Style::new().bg(theme::panel_bg()));
            f.render_widget(block, area);
            return;
        }
    };

    let title = Line::from(vec![
        Span::styled(
            format!(" {icon} {name} "),
            Style::new()
                .fg(theme::bucket_color(color))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· {} tracks ", tracks.len()),
            Style::new().fg(theme::dim()),
        ),
    ]);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::border()))
        .title(title)
        .style(Style::new().bg(theme::panel_bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if tracks.is_empty() {
        f.render_widget(
            Paragraph::new("(empty)").style(Style::new().fg(theme::dim()).bg(theme::panel_bg())),
            inner,
        );
        return;
    }

    let inner_w = inner.width as usize;
    let rows = inner.height as usize;
    let now_path = app.now_playing.as_ref().map(|t| t.path.clone());
    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for t in tracks.iter().take(rows) {
        let playing = now_path.as_ref() == Some(&t.path);
        let dur = fmt_duration(t.duration());
        let text = pad_between(&format!("  {}", t.title_artist()), &dur, inner_w);
        let style = if playing {
            Style::new().fg(theme::gold())
        } else {
            Style::new().fg(theme::fg())
        };
        lines.push(Line::from(Span::styled(text, style)));
    }
    if tracks.len() > rows && rows > 0 {
        // Replace the last visible line with a "+N more" note.
        lines.pop();
        lines.push(Line::from(Span::styled(
            format!("  +{} more…", tracks.len() - (rows - 1)),
            Style::new().fg(theme::dim()),
        )));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::new().bg(theme::panel_bg())),
        inner,
    );
}

fn draw_queue(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Queue;
    let inner_w = area.width.saturating_sub(4) as usize;
    let title = format!("QUEUE ({})", app.queue.len());
    let block = panel_block("≡", &title, focused);

    let current = app.queue.current_index();
    let items: Vec<ListItem> = app
        .queue
        .items
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let is_cur = Some(i) == current;
            let marker = if is_cur { "♪ " } else { "  " };
            let dur = fmt_duration(t.duration());
            let left = format!("{marker}{}", t.title_artist());
            let text = pad_between(&left, &dur, inner_w);
            let style = if is_cur {
                Style::new()
                    .fg(theme::gold())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme::fg())
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();

    let empty = if app.queue.is_empty() {
        Some("Queue empty. Enter on a track or dump a bucket.")
    } else {
        None
    };

    render_list(f, area, block, items, focused, empty, &mut app.queue_state);
}

fn render_list(
    f: &mut Frame,
    area: Rect,
    block: Block<'static>,
    items: Vec<ListItem<'static>>,
    focused: bool,
    empty_note: Option<&str>,
    state: &mut ListState,
) {
    if let Some(note) = empty_note {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let p = Paragraph::new(note)
            .style(Style::new().fg(theme::dim()).bg(theme::panel_bg()))
            .wrap(Wrap { trim: true });
        f.render_widget(p, inner);
        return;
    }

    let highlight = if focused {
        Style::new()
            .bg(theme::select_bg())
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(theme::select_bg())
    };

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight)
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, state);
}

// ---------------------------------------------------------------------------
// Now playing
// ---------------------------------------------------------------------------

fn draw_now_playing(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::border()))
        .style(Style::new().bg(theme::panel_bg()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    match &app.now_playing {
        None => {
            let msg = Paragraph::new("Nothing playing — pick a track and press Enter.")
                .style(Style::new().fg(theme::dim()).bg(theme::panel_bg()))
                .alignment(Alignment::Center);
            f.render_widget(msg, rows[1]);
            return;
        }
        Some(track) => {
            let paused = app.engine.is_paused();
            let icon = if paused { "❚❚" } else { "▶" };
            let icon_color = if paused { theme::gold() } else { theme::green() };

            // Row 0: icon + artist — title ............ album (album omitted if unknown)
            let title_w = (rows[0].width as usize).saturating_sub(2); // leave room for the icon
            let left_full = track.artist_title();
            let bold = Style::new().fg(theme::fg()).add_modifier(Modifier::BOLD);
            let mut spans = vec![Span::styled(format!("{icon} "), Style::new().fg(icon_color))];
            match track.album_opt() {
                Some(album) if title_w > dw(album) + 2 => {
                    let left_max = title_w - dw(album) - 1;
                    let left_t = truncate(&left_full, left_max);
                    let gap = title_w.saturating_sub(dw(&left_t) + dw(album));
                    spans.push(Span::styled(left_t, bold));
                    spans.push(Span::raw(" ".repeat(gap)));
                    spans.push(Span::styled(album.to_string(), Style::new().fg(theme::dim())));
                }
                _ => spans.push(Span::styled(truncate(&left_full, title_w), bold)),
            }
            f.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

            // Row 1: time progress bar
            let pos = app.engine.position();
            let total = app.engine.total().unwrap_or(track.duration());
            let ratio = if total.as_secs_f64() > 0.0 {
                (pos.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let pos_str = fmt_duration(pos);
            let total_str = fmt_duration(total);
            let bar_w = rows[1]
                .width
                .saturating_sub((pos_str.len() + total_str.len() + 4) as u16)
                as usize;
            let mut spans = vec![Span::styled(
                format!(" {pos_str} "),
                Style::new().fg(theme::fg()),
            )];
            spans.extend(progress_spans(bar_w, ratio as f32));
            spans.push(Span::styled(
                format!(" {total_str}"),
                Style::new().fg(theme::dim()),
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), rows[1]);

            // Row 2: volume + modes
            f.render_widget(Paragraph::new(modes_line(app)), rows[2]);
        }
    }
}

fn modes_line(app: &App) -> Line<'static> {
    let vol = app.engine.volume();
    let vol_pct = (vol / 1.25 * 100.0).round() as u32;
    let mut spans = vec![Span::styled(" vol ", Style::new().fg(theme::dim()))];
    spans.extend(mini_bar(10, vol / 1.25, theme::accent()));
    spans.push(Span::styled(
        format!(" {vol_pct:>3}%   "),
        Style::new().fg(theme::fg()),
    ));

    let dot = Span::styled("·  ", Style::new().fg(theme::faint()));

    let sh_color = if app.queue.shuffle { theme::accent() } else { theme::dim() };
    spans.push(Span::styled(
        format!("shuffle:{}  ", if app.queue.shuffle { "on" } else { "off" }),
        Style::new().fg(sh_color),
    ));
    spans.push(dot.clone());
    spans.push(Span::styled(
        format!("repeat:{}  ", app.queue.repeat.label()),
        Style::new().fg(theme::dim()),
    ));
    spans.push(dot);
    let eq = app.eq();
    let eq_color = if eq.enabled() { theme::green() } else { theme::dim() };
    spans.push(Span::styled(
        format!("EQ:{}", if eq.enabled() { "on" } else { "off" }),
        Style::new().fg(eq_color),
    ));

    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Zen mode — full-screen player with a live spectrum analyzer
// ---------------------------------------------------------------------------

/// Visual gain applied to the analyzer envelopes before display.
const SPECTRUM_GAIN: f32 = 7.0;
/// Vertical eighth-blocks (bottom-anchored) for vertical bar tips.
const PARTIALS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Horizontal eighth-blocks (left-anchored) for horizontal bar tips.
const HBLOCKS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
/// Cap the vertical spectrum so it doesn't balloon on tall terminals.
const SPECTRUM_MAX_H: u16 = 16;
/// Cap the horizontal-bar length so it doesn't stretch edge-to-edge.
const SPECTRUM_MAX_BAR_W: usize = 48;

fn draw_zen(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([
        Constraint::Percentage(8), // top pad
        Constraint::Length(1),     // brand
        Constraint::Length(1),     // title
        Constraint::Length(1),     // album
        Constraint::Min(6),        // body (art + spectrum)
        Constraint::Length(1),     // progress
        Constraint::Length(1),     // modes
        Constraint::Length(3),     // lyrics
        Constraint::Length(1),     // hint
        Constraint::Percentage(4), // bottom pad
    ])
    .split(area);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "◈ ORBIT",
            Style::new().fg(theme::accent2()).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rows[1],
    );

    match &app.now_playing {
        Some(track) => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    track.artist_title(),
                    Style::new().fg(theme::fg()).add_modifier(Modifier::BOLD),
                )))
                .alignment(Alignment::Center),
                rows[2],
            );
            if let Some(album) = track.album_opt() {
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        album.to_string(),
                        Style::new().fg(theme::dim()),
                    )))
                    .alignment(Alignment::Center),
                    rows[3],
                );
            }

            draw_spectrum(f, rows[4], app);

            // Progress bar, centered with times on each side.
            let pos = app.engine.position();
            let total = app.engine.total().unwrap_or(track.duration());
            let ratio = if total.as_secs_f64() > 0.0 {
                (pos.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let pos_str = fmt_duration(pos);
            let total_str = fmt_duration(total);
            let bar_w = (area.width as usize)
                .saturating_sub(pos_str.len() + total_str.len() + 8)
                .min(72);
            let pad = (area.width as usize)
                .saturating_sub(bar_w + pos_str.len() + total_str.len() + 4)
                / 2;
            let mut spans = vec![
                Span::raw(" ".repeat(pad)),
                Span::styled(format!("{pos_str} "), Style::new().fg(theme::fg())),
            ];
            spans.extend(progress_spans(bar_w, ratio as f32));
            spans.push(Span::styled(format!(" {total_str}"), Style::new().fg(theme::dim())));
            f.render_widget(Paragraph::new(Line::from(spans)), rows[5]);

            f.render_widget(
                Paragraph::new(modes_line(app)).alignment(Alignment::Center),
                rows[6],
            );

            draw_lyrics(f, rows[7], app);
        }
        None => {
            f.render_widget(
                Paragraph::new("Nothing playing — exit zen (z) and pick a track.")
                    .style(Style::new().fg(theme::dim()))
                    .alignment(Alignment::Center),
                rows[4],
            );
        }
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "space pause · n/p track · ←→ seek · e EQ · z/Esc exit",
            Style::new().fg(theme::faint()),
        )))
        .alignment(Alignment::Center),
        rows[8],
    );
}

/// Synced lyrics: previous / current / next line, centered.
fn draw_lyrics(f: &mut Frame, area: Rect, app: &App) {
    let Some(lyrics) = &app.lyrics else {
        return;
    };
    if lyrics.len() == 0 {
        return;
    }
    let pos_ms = app.engine.position().as_millis() as u64;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let dim = Style::new().fg(theme::dim());
    let cur_style = Style::new().fg(theme::accent()).add_modifier(Modifier::BOLD);

    let line = |s: &str, style: Style| {
        Paragraph::new(Line::from(Span::styled(s.to_string(), style))).alignment(Alignment::Center)
    };

    match lyrics.current_index(pos_ms) {
        Some(c) => {
            if c > 0 {
                f.render_widget(line(lyrics.line(c - 1), dim), rows[0]);
            }
            f.render_widget(line(lyrics.line(c), cur_style), rows[1]);
            if c + 1 < lyrics.len() {
                f.render_widget(line(lyrics.line(c + 1), dim), rows[2]);
            }
        }
        None => {
            // Before the first timestamp: preview the opening line.
            f.render_widget(line(lyrics.line(0), dim), rows[2]);
        }
    }
}

/// Responsive spectrum: tall terminals get vertical bars (capped + centered),
/// shorter ones fall back to a horizontal bar chart that uses width instead.
fn draw_spectrum(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Enough rows for one-band-per-row, but not enough for tall vertical bars.
    if area.height >= NUM_BANDS as u16 && area.height < 14 {
        draw_spectrum_horizontal(f, area, app);
        return;
    }

    // Vertical bars, height-capped and vertically centered.
    let eff = area.height.min(SPECTRUM_MAX_H);
    let y = area.y + (area.height - eff) / 2;
    let sub = Rect {
        x: area.x,
        y,
        width: area.width,
        height: eff,
    };
    draw_spectrum_vertical(f, sub, app);
}

fn draw_spectrum_vertical(f: &mut Frame, area: Rect, app: &App) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let eq = app.eq();
    let levels: Vec<f32> = (0..NUM_BANDS)
        .map(|i| (eq.level(i) * SPECTRUM_GAIN).clamp(0.0, 1.0))
        .collect();

    // Sizing: bars sit centered in the available width.
    let bar_w: usize = 4;
    let gap: usize = 2;
    let group = NUM_BANDS * bar_w + (NUM_BANDS - 1) * gap;
    let left_pad = (area.width as usize).saturating_sub(group) / 2;

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for r in 0..rows {
        let from_bottom = (rows - 1 - r) as f32; // 0 at the bottom row
        let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(left_pad))];
        for (i, &lv) in levels.iter().enumerate() {
            let h = lv * rows as f32; // bar height in rows
            let full = h.floor();
            let frac = h - full;
            let color = theme::gradient(i as f32 / (NUM_BANDS - 1) as f32);

            let (glyph, style) = if from_bottom < full {
                ("█".repeat(bar_w), Style::new().fg(color))
            } else if (from_bottom - full).abs() < 0.5 && frac > 0.06 {
                let idx = ((frac * 8.0) as usize).clamp(1, 8) - 1;
                (PARTIALS[idx].repeat(bar_w), Style::new().fg(color))
            } else {
                (" ".repeat(bar_w), Style::new())
            };
            spans.push(Span::styled(glyph, style));
            if i + 1 < NUM_BANDS {
                spans.push(Span::raw(" ".repeat(gap)));
            }
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Horizontal bar chart: one band per row, growing rightward (for short areas).
fn draw_spectrum_horizontal(f: &mut Frame, area: Rect, app: &App) {
    let eq = app.eq();
    let label_w = 4usize;
    let avail = (area.width as usize).saturating_sub(label_w + 2);
    if avail == 0 {
        return;
    }
    let bar_w = avail.min(SPECTRUM_MAX_BAR_W);
    let left_pad = (area.width as usize).saturating_sub(label_w + 1 + bar_w) / 2;

    // Vertically centre the NUM_BANDS rows.
    let top_pad = (area.height as usize).saturating_sub(NUM_BANDS) / 2;
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
    for _ in 0..top_pad {
        lines.push(Line::from(""));
    }

    for i in 0..NUM_BANDS {
        let lv = (eq.level(i) * SPECTRUM_GAIN).clamp(0.0, 1.0);
        let filled = lv * bar_w as f32;
        let full = (filled.floor() as usize).min(bar_w);
        let frac = filled - full as f32;
        let color = theme::gradient(i as f32 / (NUM_BANDS - 1) as f32);

        let mut bar = "█".repeat(full);
        let mut used = full;
        if used < bar_w && frac > 0.1 {
            let idx = ((frac * 8.0) as usize).clamp(1, 8) - 1;
            bar.push_str(HBLOCKS[idx]);
            used += 1;
        }

        let mut spans = vec![
            Span::raw(" ".repeat(left_pad)),
            Span::styled(
                format!("{:>w$} ", BAND_LABELS[i], w = label_w),
                Style::new().fg(theme::dim()),
            ),
            Span::styled(bar, Style::new().fg(color)),
        ];
        if used < bar_w {
            spans.push(Span::styled(
                "·".repeat(bar_w - used),
                Style::new().fg(theme::faint()),
            ));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

// ---------------------------------------------------------------------------
// Bars
// ---------------------------------------------------------------------------

fn progress_spans(width: usize, ratio: f32) -> Vec<Span<'static>> {
    if width == 0 {
        return vec![];
    }
    let pos = ((ratio * width as f32).round() as usize).min(width);
    let knob = pos.saturating_sub(1);
    let mut spans = Vec::with_capacity(width);
    for i in 0..width {
        if i < knob {
            let c = theme::gradient(i as f32 / width as f32);
            spans.push(Span::styled("━", Style::new().fg(c)));
        } else if i == knob && pos > 0 {
            spans.push(Span::styled(
                "●",
                Style::new()
                    .fg(theme::gradient(ratio))
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled("─", Style::new().fg(theme::faint())));
        }
    }
    spans
}

fn mini_bar(width: usize, ratio: f32, color: ratatui::style::Color) -> Vec<Span<'static>> {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f32).round() as usize).min(width);
    let mut spans = Vec::with_capacity(width);
    for i in 0..width {
        if i < filled {
            spans.push(Span::styled("▰", Style::new().fg(color)));
        } else {
            spans.push(Span::styled("▱", Style::new().fg(theme::faint())));
        }
    }
    spans
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

fn overlay_block(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::border_focus()))
        .title(Line::from(vec![Span::styled(
            format!(" {title} "),
            Style::new()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )]))
        .style(Style::new().bg(theme::panel_bg()))
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rect = centered_rect(66, 37, area);
    f.render_widget(Clear, rect);
    let block = overlay_block("ORBIT · KEYS");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let key = |k: &str| Span::styled(format!("  {k:<10}"), Style::new().fg(theme::accent()));
    let desc = |d: &str| Span::styled(d.to_string(), Style::new().fg(theme::fg()));
    let head = |h: &str| {
        Line::from(Span::styled(
            format!(" {h}"),
            Style::new().fg(theme::accent2()).add_modifier(Modifier::BOLD),
        ))
    };

    let lines = vec![
        head("NAVIGATE"),
        Line::from(vec![key("Tab/⇧Tab"), desc("cycle panes")]),
        Line::from(vec![key("↑↓ j k"), desc("move selection")]),
        Line::from(vec![key("g / G"), desc("top / bottom")]),
        Line::from(vec![key("/"), desc("search library")]),
        Line::from(""),
        head("PLAYBACK"),
        Line::from(vec![key("Enter"), desc("play track / dump bucket / play queue item")]),
        Line::from(vec![key("Space"), desc("play / pause")]),
        Line::from(vec![key("n / p"), desc("next / previous")]),
        Line::from(vec![key("← → h l"), desc("seek ∓5s")]),
        Line::from(vec![key("+ / -"), desc("volume")]),
        Line::from(vec![key("s / r"), desc("shuffle / repeat")]),
        Line::from(""),
        head("BUCKETS & QUEUE"),
        Line::from(vec![key("b"), desc("new bucket")]),
        Line::from(vec![key("S"), desc("save current queue as a bucket")]),
        Line::from(vec![key("a"), desc("add track to a bucket")]),
        Line::from(vec![key("o"), desc("open bucket (remove/reorder/rename tracks)")]),
        Line::from(vec![key("d / Enter"), desc("dump bucket → queue")]),
        Line::from(vec![key("x"), desc("delete bucket / remove queue item")]),
        Line::from(vec![key("c"), desc("clear queue")]),
        Line::from(vec![key(""), desc("smart buckets (↻ ★ ◷) fill themselves")]),
        Line::from(""),
        head("LIBRARY & EQ"),
        Line::from(vec![key("A / R"), desc("manage library folders / rescan")]),
        Line::from(vec![key("e"), desc("open equalizer (x enables it inside)")]),
        Line::from(vec![key("E"), desc("toggle EQ on/off")]),
        Line::from(vec![key("z"), desc("zen mode (full-screen player + spectrum)")]),
        Line::from(vec![key("t"), desc("cycle colour theme")]),
        Line::from(vec![key("q"), desc("quit")]),
    ];

    f.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::new().bg(theme::panel_bg())),
        inner,
    );
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Input(input) = &app.mode else {
        return;
    };
    let title = match input.kind {
        InputKind::Search => "SEARCH",
        InputKind::NewBucket | InputKind::NewBucketForTrack(_) => "NEW BUCKET",
        InputKind::SaveQueueAsBucket => "SAVE QUEUE AS BUCKET",
        InputKind::RenameBucket(_) => "RENAME BUCKET",
    };
    let rect = centered_rect(54, 3, area);
    f.render_widget(Clear, rect);
    let block = overlay_block(title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let line = Line::from(vec![
        Span::styled(" › ", Style::new().fg(theme::accent2())),
        Span::styled(input.buffer.clone(), Style::new().fg(theme::fg())),
        Span::styled("▏", Style::new().fg(theme::accent())),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::new().bg(theme::panel_bg())),
        inner,
    );
}

fn draw_pick(f: &mut Frame, area: Rect, app: &mut App) {
    let track_label = match &app.mode {
        Mode::PickBucket { track } => track.artist_title(),
        _ => return,
    };
    let h = (app.store.len() as u16 + 4).min(18).max(6);
    let rect = centered_rect(52, h, area);
    f.render_widget(Clear, rect);
    let block = overlay_block("ADD TO BUCKET");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {} ", truncate(&track_label, inner.width.saturating_sub(2) as usize)),
            Style::new().fg(theme::dim()),
        )))
        .style(Style::new().bg(theme::panel_bg())),
        rows[0],
    );

    let items: Vec<ListItem> = app
        .store
        .buckets
        .iter()
        .map(|b| {
            ListItem::new(Line::from(Span::styled(
                format!("◆ {}  ({})", b.name, b.tracks.len()),
                Style::new().fg(theme::fg()),
            )))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::new()
                .bg(theme::select_bg())
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌ ")
        .style(Style::new().bg(theme::panel_bg()));
    f.render_stateful_widget(list, rows[1], &mut app.pick_state);
}

fn draw_bucket_view(f: &mut Frame, area: Rect, app: &mut App, row: BucketRow) {
    // Resolve the bucket's icon, name, color, tracks, and editability.
    let (icon, name, color, editable, tracks): (&str, String, u8, bool, Vec<crate::model::Track>) =
        match row {
            BucketRow::Smart(i) => match app.smart.get(i) {
                Some(b) => (b.icon, b.name.clone(), b.color, false, b.tracks.clone()),
                None => return,
            },
            BucketRow::User(i) => match app.store.buckets.get(i) {
                Some(b) => ("◆", b.name.clone(), b.color, true, b.tracks.clone()),
                None => return,
            },
        };

    let rect = centered_rect(76, (tracks.len() as u16 + 6).clamp(8, 28), area);
    f.render_widget(Clear, rect);

    let title = Line::from(vec![
        Span::styled(
            format!(" {icon} {name} "),
            Style::new()
                .fg(theme::bucket_color(color))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· {} tracks{} ", tracks.len(), if editable { "" } else { " · auto" }),
            Style::new().fg(theme::dim()),
        ),
    ]);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::border_focus()))
        .title(title)
        .style(Style::new().bg(theme::panel_bg()));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let inner_w = rows[0].width as usize;
    let now_path = app.now_playing.as_ref().map(|t| t.path.clone());

    if tracks.is_empty() {
        f.render_widget(
            Paragraph::new("(empty)").style(Style::new().fg(theme::dim()).bg(theme::panel_bg())),
            rows[0],
        );
    } else {
        let items: Vec<ListItem> = tracks
            .iter()
            .map(|t| {
                let playing = now_path.as_ref() == Some(&t.path);
                let dur = fmt_duration(t.duration());
                let text = pad_between(&t.title_artist(), &dur, inner_w.saturating_sub(2));
                let style = if playing {
                    Style::new().fg(theme::gold())
                } else {
                    Style::new().fg(theme::fg())
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::new()
                    .bg(theme::select_bg())
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌ ")
            .style(Style::new().bg(theme::panel_bg()));
        f.render_stateful_widget(list, rows[0], &mut app.bucket_view_state);
    }

    let hint = if editable {
        "Enter play · x remove · K/J reorder · r rename · Esc back"
    } else {
        "Enter play · Esc back  (auto bucket — not editable)"
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {hint}"),
            Style::new().fg(theme::faint()),
        )))
        .style(Style::new().bg(theme::panel_bg())),
        rows[1],
    );
}

fn draw_manage(f: &mut Frame, area: Rect, app: &mut App) {
    let rect = centered_rect(
        72,
        (app.config.roots.len() as u16 + 6).clamp(8, 22),
        area,
    );
    f.render_widget(Clear, rect);
    let block = overlay_block("MANAGE LIBRARY FOLDERS");
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let inner_w = inner.width.saturating_sub(2) as usize;

    if app.config.roots.is_empty() {
        f.render_widget(
            Paragraph::new("No folders yet — press 'a' to add one.")
                .style(Style::new().fg(theme::dim()).bg(theme::panel_bg())),
            rows[0],
        );
    } else {
        let items: Vec<ListItem> = app
            .config
            .roots
            .iter()
            .map(|p| {
                ListItem::new(Line::from(vec![
                    Span::styled("▸ ", Style::new().fg(theme::accent2())),
                    Span::styled(
                        truncate(&p.display().to_string(), inner_w.saturating_sub(2)),
                        Style::new().fg(theme::fg()),
                    ),
                ]))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::new()
                    .bg(theme::select_bg())
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌ ")
            .style(Style::new().bg(theme::panel_bg()));
        f.render_stateful_widget(list, rows[0], &mut app.folders_state);
    }

    let hint = Line::from(vec![
        Span::styled(" a ", Style::new().fg(theme::accent())),
        Span::styled("add   ", Style::new().fg(theme::dim())),
        Span::styled("x ", Style::new().fg(theme::accent())),
        Span::styled("remove   ", Style::new().fg(theme::dim())),
        Span::styled("r ", Style::new().fg(theme::accent())),
        Span::styled("rescan   ", Style::new().fg(theme::dim())),
        Span::styled("Esc ", Style::new().fg(theme::accent())),
        Span::styled("close", Style::new().fg(theme::dim())),
    ]);
    f.render_widget(
        Paragraph::new(hint).style(Style::new().bg(theme::panel_bg())),
        rows[1],
    );
}

fn draw_browser(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(b) = &app.browser else {
        return;
    };
    let rect = centered_rect(74, 24, area);
    f.render_widget(Clear, rect);

    let path_str = b.dir.display().to_string();
    let title_w = rect.width.saturating_sub(14) as usize;
    let block = overlay_block(&format!("ADD FOLDER · {}", truncate(&path_str, title_w)));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let inner_w = inner.width.saturating_sub(2) as usize;

    // Build the directory list (".." first, then sub-folders).
    let mut items: Vec<ListItem> = Vec::new();
    if b.has_parent {
        items.push(ListItem::new(Line::from(Span::styled(
            "⮤  ..",
            Style::new().fg(theme::dim()),
        ))));
    }
    for entry in &b.entries {
        let name = entry
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        items.push(ListItem::new(Line::from(vec![
            Span::styled("▸ ", Style::new().fg(theme::accent2())),
            Span::styled(
                truncate(&format!("{name}/"), inner_w.saturating_sub(2)),
                Style::new().fg(theme::fg()),
            ),
        ])));
    }

    if items.is_empty() {
        f.render_widget(
            Paragraph::new("(no sub-folders — press 'a' to add this folder)")
                .style(Style::new().fg(theme::dim()).bg(theme::panel_bg())),
            rows[0],
        );
    } else {
        let list = List::new(items)
            .highlight_style(
                Style::new()
                    .bg(theme::select_bg())
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌ ")
            .style(Style::new().bg(theme::panel_bg()));
        f.render_stateful_widget(list, rows[0], &mut app.fs_state);
    }

    // Bottom line: what 'a' will add.
    let idx = app.fs_state.selected().unwrap_or(0);
    let target = if b.is_parent_row(idx) {
        b.dir.clone()
    } else {
        b.path_at(idx).unwrap_or_else(|| b.dir.clone())
    };
    let hint = Line::from(vec![
        Span::styled(" a adds ", Style::new().fg(theme::dim())),
        Span::styled("▸ ", Style::new().fg(theme::gold())),
        Span::styled(
            truncate(&target.display().to_string(), inner_w.saturating_sub(10)),
            Style::new().fg(theme::gold()),
        ),
    ]);
    f.render_widget(
        Paragraph::new(hint).style(Style::new().bg(theme::panel_bg())),
        rows[1],
    );
}

fn draw_eq(f: &mut Frame, area: Rect, app: &App) {
    let rect = centered_rect(62, 26, area);
    f.render_widget(Clear, rect);
    let eq = app.eq();
    let title = format!(
        "EQUALIZER · {}",
        if eq.enabled() { "ON" } else { "BYPASSED" }
    );
    let block = overlay_block(&title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // Layout: legend, the merged graph (gain bars over live spectrum), labels,
    // selected value, then presets on their own line.
    let parts = Layout::vertical([
        Constraint::Length(1), // legend
        Constraint::Min(8),    // merged graph
        Constraint::Length(1), // band labels
        Constraint::Length(1), // selected value
        Constraint::Length(1), // presets
    ])
    .split(inner);

    // Legend.
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("███", Style::new().fg(theme::accent())),
            Span::styled(" gain    ", Style::new().fg(theme::dim())),
            Span::styled("▒▒▒", Style::new().fg(theme::toward_bg(theme::violet(), 0.4))),
            Span::styled(" live spectrum", Style::new().fg(theme::dim())),
        ]))
        .style(Style::new().bg(theme::panel_bg())),
        parts[0],
    );

    draw_eq_graph(f, parts[1], app);

    // Labels row (band frequencies + preamp).
    let max_db = app.eq_max_db();
    let mut label_spans: Vec<Span> = vec![Span::raw("  ")];
    for (i, lbl) in BAND_LABELS.iter().enumerate() {
        let selected = app.eq_sel == i;
        let style = if selected {
            Style::new().fg(theme::accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme::dim())
        };
        label_spans.push(Span::styled(format!("{:^4}", lbl), style));
    }
    label_spans.push(Span::styled("  ", Style::new().fg(theme::faint())));
    let pre_sel = app.eq_sel >= NUM_BANDS;
    label_spans.push(Span::styled(
        format!("{:^5}", "pre"),
        if pre_sel {
            Style::new().fg(theme::accent()).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme::dim())
        },
    ));
    f.render_widget(
        Paragraph::new(Line::from(label_spans)).style(Style::new().bg(theme::panel_bg())),
        parts[2],
    );

    // Footer: selected value + presets.
    let sel_db = if pre_sel {
        eq.preamp_db()
    } else {
        eq.gain_db(app.eq_sel)
    };
    let sel_name = if pre_sel {
        "preamp".to_string()
    } else {
        format!("{} Hz", crate::audio::BAND_FREQS[app.eq_sel] as u32)
    };
    let value_line = Line::from(vec![
        Span::styled(format!(" {sel_name}: "), Style::new().fg(theme::dim())),
        Span::styled(
            format!("{:+.0} dB  ", sel_db),
            Style::new().fg(theme::gold()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("(±{:.0})", max_db), Style::new().fg(theme::faint())),
    ]);
    f.render_widget(
        Paragraph::new(value_line).style(Style::new().bg(theme::panel_bg())),
        parts[3],
    );

    // Presets on their own line.
    let mut preset_spans: Vec<Span> = vec![Span::styled(" presets ", Style::new().fg(theme::dim()))];
    for (i, p) in PRESETS.iter().enumerate() {
        preset_spans.push(Span::styled(
            format!("{}", i + 1),
            Style::new().fg(theme::accent()).add_modifier(Modifier::BOLD),
        ));
        preset_spans.push(Span::styled(
            format!(":{}  ", p.name),
            Style::new().fg(theme::violet()),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(preset_spans)).style(Style::new().bg(theme::panel_bg())),
        parts[4],
    );
}

/// The merged EQ visualizer: gain bars (anchored to the 0 dB centerline) drawn
/// on top of the live spectrum (anchored to the bottom, shaded behind).
fn draw_eq_graph(f: &mut Frame, area: Rect, app: &App) {
    let eq = app.eq();
    let max_db = app.eq_max_db();
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let mut gains: Vec<f32> = eq.all_gains_db().to_vec();
    gains.push(eq.preamp_db()); // preamp is the last column (no spectrum)
    let levels: Vec<f32> = (0..NUM_BANDS)
        .map(|i| (eq.level(i) * SPECTRUM_GAIN).clamp(0.0, 1.0))
        .collect();

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    for r in 0..rows {
        // dB value at this row: +max at the top, -max at the bottom.
        let db = max_db - (r as f32 / (rows - 1).max(1) as f32) * (2.0 * max_db);
        let is_zero_row = db.abs() <= (max_db / rows as f32);
        let from_bottom = (rows - 1 - r) as f32;

        let mut spans: Vec<Span> = vec![Span::raw("  ")];
        for (i, &g) in gains.iter().enumerate() {
            let is_preamp = i == NUM_BANDS;
            if is_preamp {
                spans.push(Span::raw("  ")); // gap matching the labels row
            }
            let selected = app.eq_sel == i;

            // Foreground: gain bar, filled between the centerline and the gain.
            let gain_fill =
                (g >= 0.0 && db <= g && db >= 0.0) || (g < 0.0 && db >= g && db <= 0.0);
            // Background: live spectrum rising from the bottom.
            let spec_fill = !is_preamp && from_bottom < levels[i] * rows as f32;

            let (glyph, color) = if gain_fill {
                let c = if selected {
                    theme::gold()
                } else {
                    theme::gradient(i as f32 / NUM_BANDS as f32)
                };
                ("███", c)
            } else if spec_fill {
                let base = theme::gradient(i as f32 / (NUM_BANDS - 1) as f32);
                let c = theme::toward_bg(base, if selected { 0.25 } else { 0.5 });
                ("▒▒▒", c)
            } else if is_zero_row {
                ("───", theme::faint())
            } else if selected {
                (" ╎ ", theme::faint())
            } else {
                ("   ", theme::panel_bg())
            };

            let width = if is_preamp { 5 } else { 4 };
            spans.push(Span::styled(
                format!("{:^width$}", glyph, width = width),
                Style::new().fg(color),
            ));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::new().bg(theme::panel_bg())),
        area,
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Display width in terminal columns (CJK/wide chars count as 2).
fn dw(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max` display columns, appending `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    if dw(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let budget = max - 1; // leave a column for the ellipsis
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Left text + right text padded to `width` display columns, truncating the
/// left if needed. Uses display width so CJK/wide titles stay aligned.
fn pad_between(left: &str, right: &str, width: usize) -> String {
    let rw = dw(right);
    if width <= rw + 1 {
        return truncate(left, width);
    }
    let left_max = width - rw - 1;
    let left_t = truncate(left, left_max);
    let gap = width.saturating_sub(dw(&left_t) + rw);
    format!("{left_t}{}{right}", " ".repeat(gap))
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
