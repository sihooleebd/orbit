//! Terminal-cell text math: measuring, truncating with "…", padding, slicing and wrapping.
//! Widths are computed per grapheme exactly the way ratatui renders them, so CJK text (two cells
//! per character) and combining sequences (e.g. decomposed Hangul) line up in columns.

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use ratatui::text::Span;

const ELLIPSIS: &str = "…";

/// Calls `f(grapheme, cells)` for every grapheme of `s` (control characters are skipped, like
/// ratatui does when rendering) until `f` returns false.
fn graphemes(s: &str, mut f: impl FnMut(&str, usize) -> bool) {
    let span = Span::raw(s);
    for g in span.styled_graphemes(Style::default()) {
        if !f(g.symbol, g.symbol.cell_width() as usize) {
            break;
        }
    }
}

/// Display width of `s` in terminal cells.
pub fn width(s: &str) -> usize {
    if s.is_ascii() {
        return s.bytes().filter(|b| !b.is_ascii_control()).count();
    }
    let mut w = 0;
    graphemes(s, |_, gw| {
        w += gw;
        true
    });
    w
}

pub fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}

/// `s` cut to at most `max` cells; when anything is cut the last cell becomes "…".
/// A wide character that doesn't fit is dropped whole (the result may be one cell shorter).
pub fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if width(s) <= max {
        return Cow::Borrowed(s);
    }
    let mut out = String::new();
    if max > 0 {
        let mut w = 0;
        graphemes(s, |g, gw| {
            if w + gw >= max {
                return false;
            }
            out.push_str(g);
            w += gw;
            true
        });
        out.push_str(ELLIPSIS);
    }
    Cow::Owned(out)
}

/// Like `truncate` but keeps the end ("…/Jpop/song.mp3"), for paths.
/// `path` with the home folder shown as "~".
pub fn home_relative(path: &Path) -> String {
    let path = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path,
    }
}

pub fn truncate_start(s: &str, max: usize) -> Cow<'_, str> {
    if width(s) <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut gs: Vec<(String, usize)> = Vec::new();
    graphemes(s, |g, gw| {
        gs.push((g.to_string(), gw));
        true
    });
    let (mut start, mut w) = (gs.len(), 0);
    while start > 0 && w + gs[start - 1].1 < max {
        start -= 1;
        w += gs[start].1;
    }
    Cow::Owned(std::iter::once(ELLIPSIS).chain(gs[start..].iter().map(|(g, _)| g.as_str())).collect())
}

/// Exactly `w` cells: truncated with "…" or padded with spaces (on the left when `right`).
pub fn fit(s: &str, w: usize, right: bool) -> String {
    let t = truncate(s, w);
    let pad = " ".repeat(w.saturating_sub(width(&t)));
    if right { pad + &t } else { t.into_owned() + &pad }
}

/// `spans` cut to at most `max` cells; the last visible cell becomes "…" in the cut span's style.
pub fn truncate_spans(spans: Vec<Span<'_>>, max: usize) -> Vec<Span<'_>> {
    if spans_width(&spans) <= max {
        return spans;
    }
    let mut out = Vec::new();
    if max == 0 {
        return out;
    }
    let budget = max - 1;
    let mut used = 0;
    for span in spans {
        let w = width(&span.content);
        if used + w <= budget {
            used += w;
            out.push(span);
            continue;
        }
        let mut cut = String::new();
        graphemes(&span.content, |g, gw| {
            if used + gw > budget {
                return false;
            }
            cut.push_str(g);
            used += gw;
            true
        });
        cut.push_str(ELLIPSIS);
        out.push(Span::styled(cut, span.style));
        break;
    }
    out
}

/// Exactly `w` cells of styled text (padding is unstyled, on the left when `right`).
pub fn fit_spans(spans: Vec<Span<'_>>, w: usize, right: bool) -> Vec<Span<'_>> {
    let mut out = truncate_spans(spans, w);
    let pad = w.saturating_sub(spans_width(&out));
    if pad > 0 {
        let p = Span::raw(" ".repeat(pad));
        if right {
            out.insert(0, p);
        } else {
            out.push(p);
        }
    }
    out
}

/// The cells `start..start + len` of `spans`. Wide characters cut by the window become spaces,
/// so the result is exactly as wide as the part of the window that has text.
pub fn slice_spans(spans: &[Span<'_>], start: usize, len: usize) -> Vec<Span<'static>> {
    let end = start + len;
    let mut pos = 0;
    let mut out = Vec::new();
    for span in spans {
        if pos >= end {
            break;
        }
        let mut s = String::new();
        graphemes(&span.content, |g, gw| {
            let (a, b) = (pos, pos + gw);
            pos = b;
            if b <= start {
                return true;
            }
            if a >= end {
                return false;
            }
            if a < start || b > end {
                s.push_str(&" ".repeat(b.min(end) - a.max(start)));
            } else {
                s.push_str(g);
            }
            true
        });
        if !s.is_empty() {
            out.push(Span::styled(s, span.style));
        }
    }
    out
}

/// Text that doesn't fit in `width` scrolls like a marquee: it holds still for 2 s, then moves one
/// cell every 250 ms and wraps around with a gap. `t` drives the animation (e.g. the track position,
/// so it restarts with each track and freezes while paused).
pub fn marquee<'a>(spans: Vec<Span<'a>>, width: usize, t: Duration) -> Vec<Span<'a>> {
    const GAP: usize = 6;
    const HOLD: usize = 8;
    let total = spans_width(&spans);
    if total <= width {
        return spans;
    }
    let step = (t.as_millis() / 250) as usize % (total + GAP + HOLD);
    let offset = step.saturating_sub(HOLD);
    let mut ring = spans.clone();
    ring.push(Span::raw(" ".repeat(GAP)));
    ring.extend(spans);
    slice_spans(&ring, offset, width)
}

/// `s` as spans where every grapheme containing a matched char is styled `hl`. `is_hl` receives
/// char indices counted from `first_char` (the position of `s` inside a larger search haystack);
/// whole graphemes are highlighted so decomposed Hangul / combining marks never split.
pub fn highlight(s: &str, first_char: usize, is_hl: impl Fn(usize) -> bool, normal: Style, hl: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let (mut idx, mut cur, mut cur_hl) = (first_char, String::new(), false);
    graphemes(s, |g, _| {
        let n = g.chars().count();
        let h = (idx..idx + n).any(&is_hl);
        idx += n;
        if h != cur_hl && !cur.is_empty() {
            out.push(Span::styled(std::mem::take(&mut cur), if cur_hl { hl } else { normal }));
        }
        cur_hl = h;
        cur.push_str(g);
        true
    });
    if !cur.is_empty() {
        out.push(Span::styled(cur, if cur_hl { hl } else { normal }));
    }
    out
}

/// Greedy word wrap to `width` cells. Lines break at spaces and between wide (CJK) characters,
/// which are written without spaces; a word longer than a line is split. '\n' starts a new line.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    s.split('\n').flat_map(|line| wrap_line(line, width.max(1))).collect()
}

fn wrap_line(s: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    // byte index in `cur` where the line may be broken
    let mut brk: Option<usize> = None;
    graphemes(s, |g, gw| {
        let space = g == " ";
        if space && cur_w + gw > width {
            lines.push(std::mem::take(&mut cur).trim_end().to_string());
            (cur_w, brk) = (0, None);
            return true;
        }
        while cur_w > 0 && cur_w + gw > width {
            match brk.filter(|&b| b > 0) {
                Some(b) => {
                    let rest = cur.split_off(b);
                    lines.push(cur.trim_end().to_string());
                    cur = rest.trim_start().to_string();
                    cur_w = self::width(&cur);
                }
                None => {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
            }
            brk = None;
        }
        if space && cur_w == 0 {
            return true;
        }
        if gw > 1 && cur_w > 0 {
            brk = Some(cur.len());
        }
        cur.push_str(g);
        cur_w += gw;
        if space || gw > 1 {
            brk = Some(cur.len());
        }
        true
    });
    if !cur.trim().is_empty() || lines.is_empty() {
        lines.push(cur.trim_end().to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn widths() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("夜に駆ける"), 10);
        assert_eq!(width("아이유 - 좋은 날"), 16);
        // decomposed Hangul (as macOS file names often are): 3 jamo, one 2-cell syllable
        assert_eq!(width("\u{1112}\u{1161}\u{11ab}"), 2);
        assert_eq!(width("a\tb"), 2);
    }

    #[test]
    fn truncation_is_cjk_safe() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("夜に駆ける", 5), "夜に…");
        assert_eq!(truncate("夜に駆ける", 4), "夜…");
        assert_eq!(truncate("夜に駆ける", 0), "");
        assert_eq!(truncate_start("/music/jpop/song.mp3", 10), "…/song.mp3");
        for w in 0..12 {
            assert!(width(&truncate("夜に駆けるabc", w)) <= w);
            assert_eq!(width(&fit("夜に駆けるabc", w, false)), w);
            assert_eq!(width(&fit("夜に駆けるabc", w, true)), w);
        }
        assert_eq!(fit("7", 3, true), "  7");
    }

    #[test]
    fn span_truncation_and_padding() {
        let spans = vec![Span::raw("夜に駆ける"), Span::raw(" — "), Span::raw("YOASOBI")];
        assert_eq!(spans_width(&spans), 20);
        assert_eq!(text(&truncate_spans(spans.clone(), 20)), "夜に駆ける — YOASOBI");
        assert_eq!(text(&truncate_spans(spans.clone(), 15)), "夜に駆ける — Y…");
        assert_eq!(text(&truncate_spans(spans.clone(), 14)), "夜に駆ける — …");
        assert_eq!(text(&truncate_spans(spans.clone(), 12)), "夜に駆ける …");
        assert_eq!(text(&truncate_spans(spans.clone(), 4)), "夜…");
        for w in 0..24 {
            assert_eq!(spans_width(&fit_spans(spans.clone(), w, false)), w, "{w}");
        }
    }

    #[test]
    fn slicing_and_marquee() {
        let spans = vec![Span::raw("ab夜に")];
        assert_eq!(text(&slice_spans(&spans, 0, 3)), "ab ");
        assert_eq!(text(&slice_spans(&spans, 1, 4)), "b夜 ");
        assert_eq!(text(&slice_spans(&spans, 3, 3)), " に");
        let long = vec![Span::raw("a long title that scrolls")];
        assert_eq!(text(&marquee(long.clone(), 30, Duration::ZERO)), "a long title that scrolls");
        // holds for 2 s, then moves one cell per 250 ms
        assert_eq!(text(&marquee(long.clone(), 10, Duration::from_millis(1900))), "a long tit");
        assert_eq!(text(&marquee(long.clone(), 10, Duration::from_millis(2250))), " long titl");
        for ms in (0..20_000).step_by(250) {
            assert_eq!(spans_width(&marquee(vec![Span::raw("夜に駆ける — YOASOBI")], 9, Duration::from_millis(ms))), 9);
        }
    }

    #[test]
    fn highlighting_whole_graphemes() {
        let (n, h) = (Style::new(), Style::new().bold());
        let spans = highlight("abcd", 10, |i| i == 11 || i == 12, n, h);
        assert_eq!(text(&spans), "abcd");
        assert_eq!(
            spans.iter().map(|s| (s.content.as_ref(), s.style == h)).collect::<Vec<_>>(),
            [("a", false), ("bc", true), ("d", false)]
        );
        // one matched jamo highlights the whole decomposed syllable
        let spans = highlight("\u{1112}\u{1161}\u{11ab}x", 0, |i| i == 1, n, h);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.chars().count(), 3);
        assert_eq!(spans[0].style, h);
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("hello world foo", 11), ["hello world", "foo"]);
        assert_eq!(wrap("夜に駆ける夜に駆ける", 6), ["夜に駆", "ける夜", "に駆け", "る"]);
        assert_eq!(wrap("abc 夜に", 5), ["abc", "夜に"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("", 5), [""]);
        assert_eq!(wrap("one\ntwo", 10), ["one", "two"]);
        for line in wrap("사랑을 했다 우리가 만나 지우지 못할 추억이 됐다", 7) {
            assert!(width(&line) <= 7, "{line}");
        }
    }
}
