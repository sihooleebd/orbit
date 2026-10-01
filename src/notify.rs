//! Desktop notifications, the terminal window title, and `{placeholder}` formatting of track info.

use std::borrow::Cow;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError, TryLockError};

use ratatui::crossterm::Command as _;
use ratatui::crossterm::terminal::SetTitle;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::library::{Track, fmt_duration};

/// Fire-and-forget desktop notification (macOS: osascript; Linux: notify-send if present).
/// Never blocks the UI and never prints to the terminal. An empty title promotes the body to title.
pub fn notify(title: &str, body: &str) {
    let (title, body) = if title.trim().is_empty() { (body, "") } else { (title, body) };
    if title.trim().is_empty() {
        return;
    }
    let mut cmd = notifier(title, body);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // own process group: signals meant for orbit's terminal (hangup, ^C) don't cut it short
    cmd.process_group(0);
    // spawned and reaped off the caller's thread: no zombies, no stall on a slow exec
    let _ = std::thread::Builder::new().name("notify".into()).spawn(move || {
        if let Ok(mut child) = cmd.spawn() {
            let _ = child.wait();
        }
    });
}

#[cfg(target_os = "macos")]
fn notifier(title: &str, body: &str) -> Command {
    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(format!("display notification {} with title {}", applescript_string(body), applescript_string(title)));
    cmd
}

#[cfg(not(target_os = "macos"))]
fn notifier(title: &str, body: &str) -> Command {
    // If notify-send isn't installed the spawn fails, which is ignored. The stack tags make a new
    // track replace the previous notification instead of piling up (dunst, notify-osd, ...).
    let mut cmd = Command::new("notify-send");
    cmd.args([
        "--app-name=orbit",
        "--icon=audio-x-generic",
        "--urgency=low",
        "--hint=string:x-dunst-stack-tag:orbit",
        "--hint=string:x-canonical-private-synchronous:orbit",
        "--",
        title,
        body,
    ]);
    cmd
}

/// `s` as an AppleScript string literal: quotes and backslashes escaped, line breaks and tabs as
/// escapes, other control characters dropped.
#[cfg(any(target_os = "macos", test))]
fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// xterm title stack (XTWINOPS): save / restore the window and icon title. Terminals without it
/// ignore these.
const PUSH_TITLE: &str = "\x1b[22;0t";
const POP_TITLE: &str = "\x1b[23;0t";

/// What orbit has done to the terminal title, so repeats are skipped and exit restores it.
struct TitleState {
    /// orbit's current title ("" = none).
    wanted: String,
    /// `wanted` is on screen (false outside the TUI, e.g. while suspended).
    shown: bool,
    /// The user's own title is saved on the terminal's title stack.
    pushed: bool,
}

static TITLE: Mutex<TitleState> = Mutex::new(TitleState { wanted: String::new(), shown: false, pushed: false });

impl TitleState {
    /// Each method returns the escape sequences to write ("" = nothing to do).
    fn set(&mut self, title: &str) -> String {
        // a tag must not be able to smuggle escape sequences (or a BEL ending the OSC) in
        let clean: String = title.chars().filter(|c| !c.is_control()).collect();
        if self.shown && clean == self.wanted {
            return String::new();
        }
        self.wanted = clean;
        self.shown = true;
        osc_title(&self.wanted)
    }

    fn begin(&mut self) -> String {
        let mut out = String::new();
        if !self.pushed {
            out.push_str(PUSH_TITLE);
            self.pushed = true;
        }
        if !self.shown && !self.wanted.is_empty() {
            out += &osc_title(&self.wanted);
            self.shown = true;
        }
        out
    }

    fn end(&mut self) -> String {
        let mut out = String::new();
        if self.shown && !self.wanted.is_empty() {
            out += &osc_title("");
        }
        self.shown = false;
        if self.pushed {
            out.push_str(POP_TITLE);
            self.pushed = false;
        }
        out
    }
}

fn osc_title(title: &str) -> String {
    let mut out = String::new();
    let _ = SetTitle(title).write_ansi(&mut out);
    out
}

fn write_stdout(seq: &str) {
    if !seq.is_empty() {
        let mut out = std::io::stdout();
        let _ = out.write_all(seq.as_bytes()).and_then(|()| out.flush());
    }
}

/// Set the terminal window/tab title (crossterm `SetTitle`: OSC 0, window and icon title). Empty
/// string restores a neutral title.
/// Control characters are stripped, and repeating the current title writes nothing, so this is
/// cheap enough to call every frame. Doesn't touch the screen contents or the cursor.
pub fn set_terminal_title(title: &str) {
    let mut state = TITLE.lock().unwrap_or_else(PoisonError::into_inner);
    write_stdout(&state.set(title));
}

/// Call when orbit takes over the terminal (startup, resume after suspend): saves the user's own
/// title on the terminal's title stack and shows orbit's title again if it had one.
pub fn terminal_title_begin() {
    let mut state = TITLE.lock().unwrap_or_else(PoisonError::into_inner);
    write_stdout(&state.begin());
}

/// Call when orbit gives the terminal back (exit, suspend, crash): clears orbit's title and restores
/// the user's. Idempotent, and never blocks (safe from a panic hook).
pub fn terminal_title_end() {
    let mut state = match TITLE.try_lock() {
        Ok(state) => state,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    write_stdout(&state.end());
}

/// Replace `{placeholders}` with track info and caller-supplied values.
///
/// From `track`: {title} {artist} {album} {album_artist} {year} {genre} {track} {disc}
/// {duration} {file} (file name) {path} {folder} {format} {bitrate} (kbps) {sample_rate} (Hz)
/// {channels}; plus any caller-supplied `extra` pairs such as ("position", "1:23"),
/// ("remaining", "-2:01"), ("state", "playing"), ("icon", "▶"), ("volume", "70"),
/// ("speed", "1.00"). Extras win over track fields of the same name.
///
/// - A known placeholder without a value (no track, no year tag, ...) becomes "".
/// - Unknown placeholders are left as-is ("{foo}" stays "{foo}"), and a '{' that doesn't start a
///   placeholder is plain text, so JSON-like templates work: `{"title": "{title}"}`.
/// - "{{" / "}}" escape braces.
/// - `{name|fallback}` prints `fallback` when the value is empty: "{year|n/a}".
/// - `{name:spec}` pads / truncates in terminal columns (CJK counts double), Python-style
///   `[[fill]align][width][.max]`: "{title:.30}" cuts to 30 columns ending in "…", "{volume:>3}"
///   right-aligns, "{state:-^12}" centers padded with dashes. Combined: "{year:>4|----}".
pub fn format(fmt: &str, track: Option<&Track>, extra: &[(&str, String)]) -> String {
    let none = Track::default();
    let track = track.unwrap_or(&none);
    let mut out = String::with_capacity(fmt.len() + 32);
    let mut rest = fmt;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with("{{") || rest.starts_with("}}") {
            out.push_str(&rest[..1]);
            rest = &rest[2..];
            continue;
        }
        if let Some(p) = rest.strip_prefix('{').and_then(Placeholder::parse)
            && let Some(value) = lookup(p.name, track, extra)
        {
            out += &p.spec.apply(if value.is_empty() { p.fallback } else { &value });
            rest = &rest[p.len..];
            continue;
        }
        // not a placeholder we know: the brace is plain text
        out.push_str(&rest[..1]);
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

/// Value of a placeholder: Some("") when known but missing, None when unknown.
fn lookup(name: &str, t: &Track, extra: &[(&str, String)]) -> Option<String> {
    if let Some((_, v)) = extra.iter().find(|(k, _)| *k == name) {
        return Some(v.clone());
    }
    let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
    Some(match name {
        "title" => t.title.clone(),
        "artist" => t.artist.clone(),
        "album" => t.album.clone(),
        "album_artist" => t.album_artist.clone(),
        "genre" => t.genre.clone(),
        "year" => num(t.year),
        "track" => num(t.track_no),
        "disc" => num(t.disc_no),
        "duration" if t.duration.is_zero() => String::new(),
        "duration" => fmt_duration(t.duration),
        "file" => t.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
        "path" => t.path.to_string_lossy().into_owned(),
        "folder" => t.folder.clone(),
        "format" => t.format.clone(),
        "bitrate" => num(t.bitrate),
        "sample_rate" => num(t.sample_rate),
        "channels" => num(t.channels.map(u32::from)),
        _ => return None,
    })
}

/// A parsed `{name[:spec][|fallback]}`.
struct Placeholder<'a> {
    name: &'a str,
    spec: Spec,
    fallback: &'a str,
    /// Bytes from the opening to the closing brace, inclusive.
    len: usize,
}

impl<'a> Placeholder<'a> {
    /// `s` starts right after the '{'.
    fn parse(s: &'a str) -> Option<Placeholder<'a>> {
        let close = s.find('}')?;
        let inner = &s[..close];
        let (head, fallback) = inner.split_once('|').unwrap_or((inner, ""));
        let (name, spec) = head.split_once(':').unwrap_or((head, ""));
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return None;
        }
        Some(Placeholder { name, spec: Spec::parse(spec)?, fallback, len: close + 2 })
    }
}

/// `[[fill]align][width][.max]`, widths in terminal columns.
struct Spec {
    fill: char,
    align: char,
    width: usize,
    max: Option<usize>,
}

impl Spec {
    /// Widths beyond this are rejected (the placeholder is left as-is) rather than allocated.
    const LIMIT: usize = 1000;

    fn parse(s: &str) -> Option<Spec> {
        let mut spec = Spec { fill: ' ', align: '<', width: 0, max: None };
        let mut chars = s.chars();
        let rest = match (chars.next(), chars.next()) {
            (Some(fill), Some(align)) if "<>^".contains(align) => {
                (spec.fill, spec.align) = (fill, align);
                &s[fill.len_utf8() + 1..]
            }
            (Some(align), _) if "<>^".contains(align) => {
                spec.align = align;
                &s[1..]
            }
            _ => s,
        };
        let (width, max) = rest.split_once('.').map_or((rest, None), |(w, m)| (w, Some(m)));
        if !width.is_empty() {
            spec.width = width.parse().ok().filter(|w| *w <= Self::LIMIT)?;
        }
        if let Some(max) = max {
            spec.max = Some(max.parse().ok().filter(|m| *m <= Self::LIMIT)?);
        }
        Some(spec)
    }

    fn apply(&self, value: &str) -> String {
        let text = match self.max {
            Some(max) => truncate(value, max),
            None => Cow::Borrowed(value),
        };
        let pad = self.width.saturating_sub(text.width());
        let (left, right) = match self.align {
            '>' => (pad, 0),
            '^' => (pad / 2, pad - pad / 2),
            _ => (0, pad),
        };
        let fill = |n| std::iter::repeat_n(self.fill, n);
        fill(left).chain(text.chars()).chain(fill(right)).collect()
    }
}

/// `s` cut to at most `max` columns, ending in "…" when something was cut.
fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    if max == 0 {
        return Cow::Borrowed("");
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w >= max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn track() -> Track {
        Track {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            album_artist: "Various".into(),
            year: Some(2019),
            track_no: Some(3),
            disc_no: Some(2),
            duration: Duration::from_secs(187),
            path: PathBuf::from("/music/Jpop/03 Song.mp3"),
            folder: "Jpop".into(),
            format: "MP3".into(),
            bitrate: Some(320),
            sample_rate: Some(44100),
            channels: Some(2),
            ..Track::default()
        }
    }

    fn fmt(f: &str) -> String {
        format(f, Some(&track()), &[])
    }

    #[test]
    fn track_placeholders() {
        assert_eq!(fmt("{artist} - {title} [{album} / {album_artist}]"), "Artist - Song [Album / Various]");
        assert_eq!(fmt("{year} {disc}-{track} {duration} {file}"), "2019 2-3 3:07 03 Song.mp3");
        assert_eq!(
            fmt("{path}|{folder}|{format} {bitrate}kbps {sample_rate}Hz {channels}ch"),
            "/music/Jpop/03 Song.mp3|Jpop|MP3 320kbps 44100Hz 2ch"
        );
    }

    #[test]
    fn missing_values_are_empty_and_unknown_ones_stay() {
        assert_eq!(fmt("<{genre}>"), "<>");
        assert_eq!(format("{title}|{artist}|{year}|{duration}|{file}", None, &[]), "||||");
        assert_eq!(fmt("{foo} {Title} {} {title:bogus} {ti tle} { title} {"), "{foo} {Title} {} {title:bogus} {ti tle} { title} {");
        assert_eq!(fmt("unclosed {title"), "unclosed {title");
    }

    #[test]
    fn escapes_and_literal_braces() {
        assert_eq!(fmt("{{title}} }}{{ {{{title}}}"), "{title} }{ {Song}");
        assert_eq!(fmt("lone } brace"), "lone } brace");
        assert_eq!(fmt(r#"{"title": "{title}", "year": {year}}"#), r#"{"title": "Song", "year": 2019}"#);
    }

    #[test]
    fn extras_fill_in_and_override() {
        let extra = [("icon", "▶".to_string()), ("position", "1:23".to_string()), ("title", "Live".to_string())];
        assert_eq!(format("{icon} {title} {position}/{duration}", Some(&track()), &extra), "▶ Live 1:23/3:07");
        assert_eq!(format("{state}", Some(&track()), &[]), "{state}", "an extra nobody passed is unknown");
        assert_eq!(format("{volume}%", None, &[("volume", "70".into())]), "70%");
        assert_eq!(format("{sleep-left}", None, &[("sleep-left", "12:00".into())]), "12:00");
    }

    #[test]
    fn fallbacks() {
        assert_eq!(fmt("{genre|no genre} {title|x} {year|?}"), "no genre Song 2019");
        assert_eq!(format("{title|nothing playing}", None, &[]), "nothing playing");
        assert_eq!(fmt("{genre|a:b}"), "a:b");
    }

    #[test]
    fn alignment_padding_and_truncation() {
        assert_eq!(fmt("[{title:6}]"), "[Song  ]");
        assert_eq!(fmt("[{title:>6}]"), "[  Song]");
        assert_eq!(fmt("[{title:^7}]"), "[ Song  ]");
        assert_eq!(fmt("[{title:-^8}]"), "[--Song--]");
        assert_eq!(fmt("[{title:2}]"), "[Song]", "width never cuts");
        assert_eq!(fmt("[{title:.3}]"), "[So…]");
        assert_eq!(fmt("[{title:.4}]"), "[Song]");
        assert_eq!(fmt("[{title:.1}] [{title:.0}]"), "[…] []");
        assert_eq!(fmt("[{title:>6.3}]"), "[   So…]");
        assert_eq!(fmt("[{genre:>4|----}]"), "[----]");
        assert_eq!(fmt("{title:99999}"), "{title:99999}", "absurd widths are refused, not allocated");
    }

    #[test]
    fn widths_count_terminal_columns() {
        let t = Track { title: "東京タワー".into(), ..Track::default() };
        assert_eq!(format("[{title:.5}]", Some(&t), &[]), "[東京…]");
        assert_eq!(format("[{title:.6}]", Some(&t), &[]), "[東京…]", "a wide char never overflows the limit");
        assert_eq!(format("[{title:12}]", Some(&t), &[]), "[東京タワー  ]");
        assert_eq!(format("[{title:>11}]", Some(&t), &[]), "[ 東京タワー]");
    }

    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(applescript_string(r#"say "hi" \o/"#), r#""say \"hi\" \\o/""#);
        assert_eq!(applescript_string("a\nb\r\tc\u{7}\u{1b}d 東京"), r#""a\nb\r\tcd 東京""#);
    }

    /// The escaped literal must read back as the original text in the real AppleScript compiler
    /// (`return` instead of `display notification`, so nothing pops up).
    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_round_trip() {
        let s = "He said \"hi\" \\ back\\slash 東京 – 사랑\nline two\ttab";
        let out = Command::new("osascript").arg("-e").arg(format!("return {}", applescript_string(s))).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).strip_suffix('\n'), Some(s));
    }

    #[test]
    fn title_state_machine() {
        let mut st = TitleState { wanted: String::new(), shown: false, pushed: false };
        assert_eq!(st.begin(), PUSH_TITLE);
        assert_eq!(st.set("▶ Song\x07\x1b]0;evil"), "\x1b]0;▶ Song]0;evil\x07", "control chars are stripped");
        assert_eq!(st.set("▶ Song\x07\x1b]0;evil"), "", "an unchanged title writes nothing");
        assert_eq!(st.set("⏸ Song"), "\x1b]0;⏸ Song\x07");
        // suspend: neutral title + restore the user's; resume: save theirs again + show ours
        assert_eq!(st.end(), format!("\x1b]0;\x07{POP_TITLE}"));
        assert_eq!(st.begin(), format!("{PUSH_TITLE}\x1b]0;⏸ Song\x07"));
        assert_eq!(st.begin(), "", "begin is idempotent");
        assert_eq!(st.end(), format!("\x1b]0;\x07{POP_TITLE}"));
        assert_eq!(st.end(), "", "end is idempotent");
    }

    #[test]
    fn title_without_begin_or_after_clearing() {
        let mut st = TitleState { wanted: String::new(), shown: false, pushed: false };
        assert_eq!(st.end(), "", "nothing to restore if orbit never touched the title");
        st.set("x");
        assert_eq!(st.set(""), "\x1b]0;\x07");
        assert_eq!(st.end(), "", "already neutral, nothing pushed");
    }
}
