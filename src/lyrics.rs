//! Lyrics: .lrc/.txt sidecar files, lyric folders, or tags embedded in the audio file.
//! Synced (LRC) lyrics follow playback; plain lyrics are shown as scrollable text.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::id3::v2::{Frame, Id3v2Tag, SyncTextContentType, SynchronizedTextFrame, TimestampFormat};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagType};

use crate::config::{LyricsConfig, expand_tilde};
use crate::library::Track;

/// Text of timed lines that have none (instrumental breaks), so the highlight has something to show.
pub const GAP: &str = "♪";

/// Lyric files are small; anything bigger is not a lyric file.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// LRC header tags; `[key:value]` with any other key is lyric text (e.g. "[Verse 1: Name]").
const META_KEYS: [&str; 13] = ["ar", "al", "ti", "au", "by", "re", "ve", "length", "offset", "id", "la", "lang", "tool"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lyrics {
    /// Sorted by time when synced.
    pub lines: Vec<LyricLine>,
    pub synced: bool,
    /// Where they came from, for display: "Song.lrc", "embedded", ...
    pub source: String,
    /// From an [offset:+/-ms] tag in the LRC (positive = lyrics later).
    pub offset_ms: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LyricLine {
    pub time: Option<Duration>,
    pub text: String,
}

/// Find lyrics for `track` according to `cfg` (sidecar -> lyric dirs -> embedded). None if none found
/// or lyrics are disabled.
///
/// - sidecar: "<file stem>.lrc", then ".txt", next to the audio file
/// - `cfg.dirs`: "<artist> - <title>", "<album artist> - <title>", "<file stem>", "<title>" (.lrc, then .txt)
/// - embedded: synced text in a lyrics tag, then ID3v2 SYLT frames, then unsynced text (all tags)
///
/// File names match ignoring case and the look-alikes tools substitute for characters that aren't
/// allowed in file names (see [`find_files`]). Empty or unreadable files are skipped.
pub fn load(track: &Track, cfg: &LyricsConfig) -> Option<Lyrics> {
    if !cfg.enabled {
        return None;
    }
    let stem = track.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let first_readable = |dir: &Path, names: &[&str]| find_files(dir, names, &["lrc", "txt"]).iter().find_map(|p| read_file(p));
    let sidecar = || first_readable(track.path.parent()?, &[&stem]);
    let from_dirs = || {
        let artist_title = format!("{} - {}", track.artist, track.title);
        let album_artist_title = format!("{} - {}", track.album_artist, track.title);
        let names = [artist_title.as_str(), album_artist_title.as_str(), stem.as_str(), track.title.as_str()];
        cfg.dirs.iter().find_map(|dir| first_readable(&expand_tilde(dir), &names))
    };
    cfg.sidecar
        .then(sidecar)
        .flatten()
        .or_else(from_dirs)
        .or_else(|| cfg.embedded.then(|| embedded(track)).flatten())
}

/// Files in `dir` named "<stem>.<ext>", best first: `stems` in order, each with `exts` in order.
/// Names match ignoring case and the characters tools substitute for ones file names can't hold, so
/// a title "W/X/Y" finds "w⧸x⧸y.LRC". When no listed name matches, an exact-name lookup lets the
/// file system match names that differ only in Unicode normalization (macOS).
pub fn find_files(dir: &Path, stems: &[&str], exts: &[&str]) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let files: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .map(|e| (fold(&e.file_name().to_string_lossy()), e.path()))
        .collect();
    let mut found = Vec::new();
    for stem in stems.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        for ext in exts {
            let name = format!("{stem}.{ext}");
            let key = fold(&name);
            let matches: Vec<PathBuf> = files.iter().filter(|(n, _)| *n == key).map(|(_, p)| p.clone()).collect();
            let exact = dir.join(&name);
            if !matches.is_empty() {
                found.extend(matches);
            } else if !name.contains(['/', '\\']) && exact.is_file() {
                found.push(exact);
            }
        }
    }
    found
}

/// File-name comparison key: lowercase, with characters that can't appear in file names and their
/// usual substitutes (fullwidth forms, "⧸" from yt-dlp, "_") folded together.
fn fold(name: &str) -> String {
    name.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '/' | '⧸' | '／' | '\\' | '＼' | ':' | '：' | '|' | '｜' | '?' | '？' | '*' | '＊' | '"' | '＂' | '<' | '＜'
            | '>' | '＞' => '_',
            c => c,
        })
        .collect()
}

/// A lyrics file, or None if unreadable, too big or empty. `source` is its file name.
fn read_file(path: &Path) -> Option<Lyrics> {
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let lyrics = parse_lrc(&decode(&std::fs::read(path).ok()?));
    let source = path.file_name()?.to_string_lossy().into_owned();
    (!lyrics.is_empty()).then_some(Lyrics { source, ..lyrics })
}

/// Text of a lyrics file: UTF-8/UTF-16 by BOM, else UTF-8, else the legacy CJK encoding the bytes
/// decode cleanly in and look like (Shift_JIS or EUC-JP: kana among the non-ASCII characters;
/// EUC-KR: mostly hangul; GBK: GB2312 byte pairs), else Windows-1252.
fn decode(bytes: &[u8]) -> String {
    use encoding_rs::{EUC_JP, EUC_KR, Encoding, GBK, SHIFT_JIS, WINDOWS_1252};
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        return encoding.decode_without_bom_handling(&bytes[bom..]).0.into_owned();
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    let kana = |c: char| matches!(c, '\u{3041}'..='\u{30ff}');
    let hangul = |c: char| matches!(c, '\u{ac00}'..='\u{d7a3}');
    // Decodes without errors, and at least `percent` of the non-ASCII characters are in `script`.
    let attempt = |encoding: &'static Encoding, script: &dyn Fn(char) -> bool, percent: usize| {
        let text = encoding.decode_without_bom_handling_and_without_replacement(bytes)?;
        let (hits, total) = text.chars().filter(|c| !c.is_ascii()).fold((0, 0), |(h, t), c| (h + usize::from(script(c)), t + 1));
        (hits > 0 && hits * 100 >= total * percent).then(|| text.into_owned())
    };
    attempt(SHIFT_JIS, &kana, 15)
        .or_else(|| attempt(EUC_JP, &kana, 15))
        .or_else(|| attempt(EUC_KR, &hangul, 70))
        .or_else(|| gb2312_like(bytes).then(|| attempt(GBK, &|_| true, 0)).flatten())
        .unwrap_or_else(|| WINDOWS_1252.decode_without_bom_handling(bytes).0.into_owned())
}

/// Every non-ASCII byte is part of a pair in 0xA1..=0xFE (EUC-CN). Latin-1 text fails this because
/// accented letters sit next to ASCII.
fn gb2312_like(bytes: &[u8]) -> bool {
    let high = |b: u8| (0xa1..=0xfe).contains(&b);
    let mut it = bytes.iter().copied();
    while let Some(b) = it.next() {
        if b >= 0x80 && !(high(b) && it.next().is_some_and(high)) {
            return false;
        }
    }
    true
}

/// Lyrics in the file's tags: a lyrics item with LRC timestamps, then an ID3v2 SYLT frame, then an
/// unsynced lyrics item. Every tag of the file is checked.
fn embedded(track: &Track) -> Option<Lyrics> {
    let options = ParseOptions::new().read_properties(false).read_cover_art(false);
    let file = Probe::open(&track.path).ok()?.options(options).guess_file_type().ok()?.read().ok()?;
    let tags = file.tags();
    let texts = || {
        tags.iter()
            .flat_map(|t| t.get_strings(ItemKey::Lyrics).chain(t.get_strings(ItemKey::UnsyncLyrics)))
            .map(parse_lrc)
            .filter(|l| !l.is_empty())
    };
    let lyrics = texts()
        .find(|l| l.synced)
        .or_else(|| tags.iter().filter(|t| t.tag_type() == TagType::Id3v2).find_map(|t| sylt(t, track.sample_rate)))
        .or_else(|| texts().next())?;
    Some(Lyrics { source: "embedded".into(), ..lyrics })
}

/// Synchronized lyrics from an ID3v2 SYLT frame. Entries are whole lines, unless some entry starts
/// with a line break: then entries are syllables and the breaks start new lines (ID3v2 karaoke style).
fn sylt(tag: &Tag, sample_rate: Option<u32>) -> Option<Lyrics> {
    let frame = Id3v2Tag::from(tag.clone()).into_iter().find_map(|f| match f {
        Frame::Binary(b) if b.id().as_str() == "SYLT" => SynchronizedTextFrame::parse(&b.data, b.flags())
            .ok()
            .filter(|s| matches!(s.content_type, SyncTextContentType::Lyrics | SyncTextContentType::TextTranscription | SyncTextContentType::Other)),
        _ => None,
    })?;
    let millis = |t: u32| match frame.timestamp_format {
        TimestampFormat::MS => u64::from(t),
        // ponytail: assumes MPEG-1 Layer III frames (1152 samples); MPEG-2 files would need 576.
        TimestampFormat::MPEG => u64::from(t) * 1152 * 1000 / u64::from(sample_rate.unwrap_or(44_100).max(1)),
    };
    let syllables = frame.content.iter().any(|(_, s)| s.starts_with(['\n', '\r']));
    let mut lines: Vec<LyricLine> = Vec::new();
    for (time, text) in &frame.content {
        match lines.last_mut() {
            Some(line) if syllables && !text.starts_with(['\n', '\r']) => line.text.push_str(text),
            _ => lines.push(LyricLine { time: Some(Duration::from_millis(millis(*time))), text: text.clone() }),
        }
    }
    for line in &mut lines {
        line.text = match line.text.trim() {
            "" => GAP.to_string(),
            t => t.to_string(),
        };
    }
    lines.sort_by_key(|l| l.time);
    let lyrics = Lyrics { lines, synced: true, source: String::new(), offset_ms: 0 };
    (!lyrics.is_empty()).then_some(lyrics)
}

/// Parse LRC text. Handles several timestamps per line ([00:12.00][01:15.30]text), [mm:ss],
/// [mm:ss.xx], [mm:ss.xxx], [offset:], metadata tags ([ar:], [ti:] ... are dropped), and strips
/// enhanced word timings (<00:12.34>). Text without timestamps becomes unsynced lines.
///
/// Also: [mm:ss:xx], [h:mm:ss.xx], "," as decimal separator, minutes above 59, a BOM, CRLF or CR line
/// ends, inline [mm:ss.xx] word timings, [#comments]. Timed lines without text become [`GAP`] lines
/// (instrumental breaks). Lines are sorted by time (stable; duplicate timestamps, e.g. a line and its
/// translation, are kept in file order). If any line is timed, untimed lines are dropped.
///
/// `offset_ms` is the negated [offset:] value: in LRC a positive offset makes lyrics appear sooner.
pub fn parse_lrc(text: &str) -> Lyrics {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).replace("\r\n", "\n").replace('\r', "\n");
    let mut timed = Vec::new();
    let mut plain = Vec::new();
    let mut offset_ms = 0;
    for line in text.split('\n').map(str::trim) {
        let mut rest = line;
        let mut times = Vec::new();
        let mut header = false;
        while let Some((tag, after)) = rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
            if let Some(t) = parse_time(tag) {
                times.push(t);
            } else if tag.starts_with('#') {
                header = true;
            } else if let Some((key, value)) = tag.split_once(':').filter(|(k, _)| META_KEYS.contains(&k.trim().to_ascii_lowercase().as_str())) {
                if key.trim().eq_ignore_ascii_case("offset") {
                    offset_ms = value.trim().parse::<i32>().map_or(offset_ms, |v| v.saturating_neg());
                }
                header = true;
            } else {
                break;
            }
            rest = after.trim_start();
        }
        if !times.is_empty() {
            let text = match strip_word_times(rest) {
                t if t.is_empty() => GAP.to_string(),
                t => t,
            };
            timed.extend(times.into_iter().map(|t| LyricLine { time: Some(t), text: text.clone() }));
        } else if !header {
            plain.push(LyricLine { time: None, text: strip_word_times(line) });
        }
    }
    if !timed.is_empty() {
        timed.sort_by_key(|l| l.time);
        return Lyrics { lines: timed, synced: true, source: String::new(), offset_ms };
    }
    let start = plain.iter().position(|l| !l.text.is_empty()).unwrap_or(plain.len());
    let end = plain.iter().rposition(|l| !l.text.is_empty()).map_or(start, |i| i + 1);
    plain.truncate(end);
    plain.drain(..start);
    Lyrics { lines: plain, synced: false, source: String::new(), offset_ms }
}

/// "mm:ss", "mm:ss.x", "mm:ss.xx", "mm:ss.xxx", "mm:ss:xx", "h:mm:ss.xx" ("," works as "."); minutes
/// may exceed 59.
fn parse_time(tag: &str) -> Option<Duration> {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let num = |s: &str| digits(s).then(|| s.parse::<u64>().ok()).flatten();
    let tag = tag.trim().replace(',', ".");
    let parts: Vec<&str> = tag.split(':').collect();
    let (hours, minutes, seconds) = match parts[..] {
        [m, s] => ("0", m, s.to_string()),
        [m, s, frac] if !frac.contains('.') => ("0", m, format!("{s}.{frac}")),
        [h, m, s] => (h, m, s.to_string()),
        _ => return None,
    };
    let (secs, frac) = seconds.split_once('.').unwrap_or((&seconds, ""));
    if secs.len() > 2 || !(frac.is_empty() || digits(frac)) {
        return None;
    }
    let millis = frac.bytes().chain(std::iter::repeat(b'0')).take(3).fold(0, |acc, b| acc * 10 + u64::from(b - b'0'));
    let total = (num(hours)? * 60 + num(minutes)?) * 60_000 + num(secs)? * 1000 + millis;
    Some(Duration::from_millis(total))
}

/// Remove enhanced-LRC word timings ("<00:12.34>" and inline "[00:12.34]") and trim. Brackets that
/// aren't timestamps ("<3", "[Chorus]") stay.
fn strip_word_times(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut stripped = false;
    while let Some(i) = rest.find(['<', '[']) {
        let close = if rest.as_bytes()[i] == b'<' { '>' } else { ']' };
        match rest[i + 1..].find(close) {
            Some(j) if parse_time(&rest[i + 1..i + 1 + j]).is_some() => {
                out.push_str(&rest[..i]);
                rest = &rest[i + j + 2..];
                stripped = true;
            }
            _ => {
                out.push_str(&rest[..=i]);
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    if stripped {
        // "word <t> word" leaves double spaces behind
        out = out.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    }
    out.trim().to_string()
}

impl Lyrics {
    /// Index of the line being sung at `pos` (after applying `offset_ms` plus `extra_offset_ms`).
    /// None before the first timestamp or for unsynced lyrics.
    ///
    /// When several lines share the timestamp (a line and its translation), this is the first of
    /// them; [`Lyrics::current_range`] covers all of them.
    pub fn current(&self, pos: Duration, extra_offset_ms: i32) -> Option<usize> {
        if !self.synced {
            return None;
        }
        let at = pos.as_millis() as i64 - i64::from(self.offset_ms) - i64::from(extra_offset_ms);
        let millis = |l: &LyricLine| l.time.map_or(0, |t| t.as_millis() as i64);
        let last = self.lines.partition_point(|l| millis(l) <= at).checked_sub(1)?;
        let time = millis(&self.lines[last]);
        Some(self.lines[..last].partition_point(|l| millis(l) < time))
    }

    /// All lines sharing the current line's timestamp (e.g. an original line and its translation).
    pub fn current_range(&self, pos: Duration, extra_offset_ms: i32) -> Option<Range<usize>> {
        let first = self.current(pos, extra_offset_ms)?;
        let time = self.lines[first].time;
        Some(first..first + self.lines[first..].iter().take_while(|l| l.time == time).count())
    }

    /// True when there is nothing to show: no lines, or only blank / [`GAP`] lines.
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.text.is_empty() || l.text == GAP)
    }

    /// The lyrics as plain text, one line per line ([`GAP`] lines become blank lines), for copying
    /// or a plain view.
    #[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
    pub fn plain_text(&self) -> String {
        self.lines.iter().map(|l| if l.text == GAP { "" } else { l.text.as_str() }).collect::<Vec<_>>().join("\n")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lofty::TextEncoding;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::{BinaryFrame, FrameId, UnsynchronizedTextFrame};
    use lofty::tag::TagExt;

    /// A fresh directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!("orbit-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        /// Write `contents` to `rel` (creating folders) and return its path.
        pub(crate) fn file(&self, rel: &str, contents: impl AsRef<[u8]>) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A tiny valid WAV file (silence) that tags can be written into.
    pub(crate) fn wav() -> Vec<u8> {
        let data = [0u8; 400];
        let mut v = b"RIFF".to_vec();
        v.extend((36 + data.len() as u32).to_le_bytes());
        v.extend(b"WAVEfmt ");
        v.extend(16u32.to_le_bytes());
        v.extend([1u16, 1].iter().flat_map(|n| n.to_le_bytes())); // PCM, mono
        v.extend([8000u32, 16000].iter().flat_map(|n| n.to_le_bytes())); // rate, bytes/s
        v.extend([2u16, 16].iter().flat_map(|n| n.to_le_bytes())); // block align, bits
        v.extend(b"data");
        v.extend((data.len() as u32).to_le_bytes());
        v.extend(data);
        v
    }

    pub(crate) fn track(path: &Path) -> Track {
        Track {
            path: path.to_path_buf(),
            title: "Title".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            album_artist: "Artist".into(),
            ..Track::default()
        }
    }

    fn times(l: &Lyrics) -> Vec<u64> {
        l.lines.iter().map(|l| l.time.unwrap().as_millis() as u64).collect()
    }

    fn texts(l: &Lyrics) -> Vec<&str> {
        l.lines.iter().map(|l| l.text.as_str()).collect()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn timestamp_formats() {
        let l = parse_lrc(
            "[00:01]a\n[00:02.5]b\n[00:03.25]c\n[00:04.125]d\n[00:05:50]e\n[00:06,75]f\n[75:00.00]g\n[1:02:03.45]h\n[ 00:07.1239 ]i",
        );
        assert!(l.synced);
        assert_eq!(times(&l), [1000, 2500, 3250, 4125, 5500, 6750, 7123, 3_723_450, 4_500_000]);
        assert_eq!(texts(&l), ["a", "b", "c", "d", "e", "f", "i", "h", "g"]);
        for bad in ["", "ab:cd", "00:123", "00:12.x", "-1:00", "1:2:3:4"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn repeated_timestamps_sort_stably_and_keep_duplicates() {
        let l = parse_lrc("[00:10.00][00:30.00]chorus\n[00:20.00]verse\n[00:10.00]翻訳\n[00:05.00] [00:06.00]spaced");
        assert_eq!(times(&l), [5000, 6000, 10_000, 10_000, 20_000, 30_000]);
        assert_eq!(texts(&l), ["spaced", "spaced", "chorus", "翻訳", "verse", "chorus"]);
    }

    #[test]
    fn metadata_offset_bom_and_line_endings() {
        let l = parse_lrc(
            "\u{feff}[ar:Artist]\r\n[ti:Title]\r\n[al:Album]\r\n[by:me]\r\n[length: 03:45]\r\n[re:tool]\r\n[ve:1.0]\r\n[#a comment]\r\n[offset:+500]\r\n[00:01.00]Hello\r\n",
        );
        assert_eq!(texts(&l), ["Hello"]);
        assert!(l.synced);
        assert_eq!(l.offset_ms, -500, "LRC: positive offset = sooner");
        assert_eq!(parse_lrc("[offset: -250]\n[00:01.00]x").offset_ms, 250);
        assert_eq!(texts(&parse_lrc("[00:01.00]a\r[00:02.00]b")), ["a", "b"]);
    }

    #[test]
    fn word_timings_are_stripped() {
        let l = parse_lrc(
            "[00:01.00]<00:01.00>Hel<00:01.40>lo <00:02.00>world <00:02.50>\n[00:03.00]inline [00:04.00]timed\n[00:05.00]<3 [Chorus] a<b\n[00:06.00]<00:06.00>　君と　僕",
        );
        assert_eq!(texts(&l), ["Hello world", "inline timed", "<3 [Chorus] a<b", "君と　僕"]);
    }

    #[test]
    fn blank_timed_lines_are_gaps() {
        let l = parse_lrc("[00:01.00]a\n[00:05.00]\n[00:09.00]   \n[00:12.00]<00:12.00>");
        assert_eq!(texts(&l), ["a", GAP, GAP, GAP]);
        assert_eq!(l.plain_text(), "a\n\n\n");
    }

    #[test]
    fn unsynced_text() {
        let l = parse_lrc("\n\n[ar:Someone]\nFirst line  \n\n[Chorus]\n[Verse 1: Someone]\n　Last\n\n");
        assert!(!l.synced);
        assert!(l.lines.iter().all(|l| l.time.is_none()));
        assert_eq!(texts(&l), ["First line", "", "[Chorus]", "[Verse 1: Someone]", "Last"]);
        assert_eq!(l.current(ms(1000), 0), None);
    }

    #[test]
    fn mixed_files_keep_only_timed_lines() {
        let l = parse_lrc("intro text\n[00:01.00]a\nuntimed\n[00:02.00]b");
        assert!(l.synced);
        assert_eq!(texts(&l), ["a", "b"]);
    }

    #[test]
    fn current_line() {
        let l = parse_lrc("[00:05.00]a\n[00:10.00]b\n[00:10.00]b2\n[00:20.00]c");
        assert_eq!(l.current(ms(0), 0), None);
        assert_eq!(l.current(ms(4999), 0), None);
        assert_eq!(l.current(ms(5000), 0), Some(0));
        assert_eq!(l.current(ms(9999), 0), Some(0));
        assert_eq!(l.current(ms(10_000), 0), Some(1), "first of lines sharing a timestamp");
        assert_eq!(l.current_range(ms(15_000), 0), Some(1..3));
        assert_eq!(l.current_range(ms(25_000), 0), Some(3..4));
        assert_eq!(l.current_range(ms(1000), 0), None);
        assert_eq!(l.current(ms(3_600_000), 0), Some(3));
        // extra offset: positive = lyrics later, negative = sooner
        assert_eq!(l.current(ms(5499), 500), None);
        assert_eq!(l.current(ms(5500), 500), Some(0));
        assert_eq!(l.current(ms(4500), -500), Some(0));
        assert_eq!(l.current(ms(4499), -500), None);
        // [offset:+1000] shows lyrics a second sooner; combined with extra +1500 it's net 500 ms later
        let o = parse_lrc("[offset:+1000]\n[00:05.00]a");
        assert_eq!(o.current(ms(3999), 0), None);
        assert_eq!(o.current(ms(4000), 0), Some(0));
        assert_eq!(o.current(ms(5499), 1500), None);
        assert_eq!(o.current(ms(5500), 1500), Some(0));
        assert_eq!(parse_lrc("").current(ms(1000), 0), None);
    }

    #[test]
    fn emptiness_and_plain_text() {
        assert!(parse_lrc("").is_empty());
        assert!(parse_lrc("[ar:x]\n[ti:y]").is_empty());
        assert!(parse_lrc("[00:01.00]\n[00:02.00]").is_empty());
        assert!(!parse_lrc("x").is_empty());
        assert_eq!(parse_lrc("[00:01.00]a\n[00:02.00]\n[00:03.00]b").plain_text(), "a\n\nb");
        assert_eq!(parse_lrc("one\n\ntwo").plain_text(), "one\n\ntwo");
    }

    #[test]
    fn decodes_bom_utf16_and_legacy_cjk() {
        assert_eq!(decode(b"\xef\xbb\xbfhi"), "hi");
        let utf16 = |bom: [u8; 2], f: fn(u16) -> [u8; 2]| [bom.to_vec(), "歌詞 ok".encode_utf16().flat_map(f).collect()].concat();
        assert_eq!(decode(&utf16([0xff, 0xfe], u16::to_le_bytes)), "歌詞 ok");
        assert_eq!(decode(&utf16([0xfe, 0xff], u16::to_be_bytes)), "歌詞 ok");
        for (encoding, text) in [
            (encoding_rs::SHIFT_JIS, "[00:01.00]こんにちは、世界"),
            (encoding_rs::EUC_JP, "[00:01.00]かな漢字"),
            (encoding_rs::EUC_KR, "[00:01.00]안녕하세요 세상"),
            (encoding_rs::GBK, "[00:01.00]你好世界，我们的歌"),
        ] {
            let (bytes, _, lossy) = encoding.encode(text);
            assert!(!lossy);
            assert_eq!(decode(&bytes), text, "{}", encoding.name());
        }
        assert_eq!(decode(b"caf\xe9 cr\xe8me"), "café crème");
    }

    #[test]
    fn finds_files_ignoring_case_and_substitute_characters() {
        let dir = TempDir::new("find-file");
        let lrc = dir.file("W⧸X⧸Y： Remix.LRC", "");
        let txt = dir.file("w_x_y_ remix.txt", "");
        dir.file("sub.lrc/keep", "");
        assert_eq!(find_files(&dir.0, &["missing", "w/x/y: remix"], &["lrc", "txt"]), [lrc, txt]);
        assert_eq!(find_files(&dir.0, &["sub"], &["lrc"]), Vec::<PathBuf>::new(), "folders are not files");
        assert!(find_files(&dir.0.join("nope"), &["x"], &["lrc"]).is_empty());
    }

    #[test]
    fn load_prefers_sidecar_then_dirs_then_embedded() {
        let dir = TempDir::new("load-order");
        let audio = dir.file("music/Song.mp3", "not really audio");
        let mut cfg = LyricsConfig { dirs: vec![dir.0.join("lyrics").to_string_lossy().into_owned()], ..LyricsConfig::default() };
        let t = Track { title: "W/X/Y".into(), ..track(&audio) };
        assert_eq!(load(&t, &cfg), None);

        dir.file("lyrics/W⧸X⧸Y.lrc", "[00:01.00]by title");
        assert_eq!(load(&t, &cfg).unwrap().source, "W⧸X⧸Y.lrc");
        dir.file("lyrics/artist - w_x_y.LRC", "[00:01.00]by artist and title");
        assert_eq!(texts(&load(&t, &cfg).unwrap()), ["by artist and title"]);

        dir.file("music/SONG.txt", "plain words");
        let l = load(&t, &cfg).unwrap();
        assert_eq!((l.source.as_str(), l.synced), ("SONG.txt", false));
        dir.file("music/song.LRC", "[ti:Song]\n[00:01.00]sidecar");
        let l = load(&t, &cfg).unwrap();
        assert_eq!((l.source.as_str(), l.synced, texts(&l)), ("song.LRC", true, vec!["sidecar"]));

        dir.file("music/song.LRC", "[ti:only metadata]");
        assert_eq!(load(&t, &cfg).unwrap().source, "SONG.txt", "empty files are skipped");

        cfg.sidecar = false;
        assert_eq!(load(&t, &cfg).unwrap().source, "artist - w_x_y.LRC");
        cfg.enabled = false;
        assert_eq!(load(&t, &cfg), None);
    }

    fn tagged_wav(dir: &TempDir, frames: Vec<Frame<'static>>) -> PathBuf {
        let path = dir.file("tagged.wav", wav());
        let mut tag = Id3v2Tag::new();
        for f in frames {
            tag.insert(f);
        }
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        path
    }

    fn uslt(text: &str) -> Frame<'static> {
        Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(TextEncoding::UTF8, *b"eng", String::new(), text.to_string()))
    }

    fn sylt_frame(content: &[(u32, &str)]) -> Frame<'static> {
        let content = content.iter().map(|(t, s)| (*t, s.to_string())).collect();
        let frame = SynchronizedTextFrame::new(TextEncoding::UTF8, *b"eng", TimestampFormat::MS, SyncTextContentType::Lyrics, None, content);
        Frame::Binary(BinaryFrame::new(FrameId::new("SYLT").unwrap(), frame.as_bytes(WriteOptions::default()).unwrap()))
    }

    #[test]
    fn embedded_lrc_text_plain_text_and_sylt() {
        let dir = TempDir::new("embedded");
        let cfg = LyricsConfig::default();

        let l = load(&track(&tagged_wav(&dir, vec![uslt("[00:01.50]synced in USLT")])), &cfg).unwrap();
        assert_eq!((l.source.as_str(), l.synced, times(&l)), ("embedded", true, vec![1500]));

        let plain = uslt("just words\nmore words");
        let l = load(&track(&tagged_wav(&dir, vec![plain.clone()])), &cfg).unwrap();
        assert_eq!((l.synced, texts(&l)), (false, vec!["just words", "more words"]));

        let lines = sylt_frame(&[(2000, "second"), (1000, "first"), (3000, "")]);
        let l = load(&track(&tagged_wav(&dir, vec![plain.clone(), lines])), &cfg).unwrap();
        assert_eq!((l.synced, times(&l), texts(&l)), (true, vec![1000, 2000, 3000], vec!["first", "second", GAP]), "SYLT beats unsynced text");

        let syllables = sylt_frame(&[(1000, "Hel"), (1200, "lo "), (1500, "world"), (4000, "\nNext"), (4300, " line")]);
        let l = load(&track(&tagged_wav(&dir, vec![syllables])), &cfg).unwrap();
        assert_eq!((times(&l), texts(&l)), (vec![1000, 4000], vec!["Hello world", "Next line"]));

        let off = LyricsConfig { embedded: false, ..LyricsConfig::default() };
        assert_eq!(load(&track(&tagged_wav(&dir, vec![plain])), &off), None);
    }
}
