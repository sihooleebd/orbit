//! Synced .lrc lyrics for the zen view.

use std::fs;
use std::path::Path;

/// Parsed, time-stamped lyrics, sorted by time.
pub struct Lyrics {
    /// (milliseconds, text) pairs.
    lines: Vec<(u64, String)>,
}

impl Lyrics {
    /// Load a `.lrc` sidecar sitting next to the track.
    pub fn load(track_path: &Path) -> Option<Self> {
        let lrc = track_path.with_extension("lrc");
        let text = fs::read_to_string(lrc).ok()?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines: Vec<(u64, String)> = Vec::new();
        for raw in text.lines() {
            // A line may carry several timestamps, e.g. "[00:12.34][00:56.78]words".
            let mut rest = raw;
            let mut stamps: Vec<u64> = Vec::new();
            while rest.starts_with('[') {
                let Some(end) = rest.find(']') else { break };
                let tag = &rest[1..end];
                if let Some(ms) = parse_timestamp(tag) {
                    stamps.push(ms);
                }
                rest = &rest[end + 1..];
            }
            let words = rest.trim().to_string();
            for ms in stamps {
                lines.push((ms, words.clone()));
            }
        }
        if lines.is_empty() {
            return None;
        }
        lines.sort_by_key(|(ms, _)| *ms);
        Some(Self { lines })
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, i: usize) -> &str {
        self.lines.get(i).map(|(_, s)| s.as_str()).unwrap_or("")
    }

    /// Index of the line that should be active at `pos_ms`.
    pub fn current_index(&self, pos_ms: u64) -> Option<usize> {
        let mut idx = None;
        for (i, (ms, _)) in self.lines.iter().enumerate() {
            if *ms <= pos_ms {
                idx = Some(i);
            } else {
                break;
            }
        }
        idx
    }
}

/// Parse an LRC timestamp like `mm:ss.xx` or `mm:ss` into milliseconds.
fn parse_timestamp(tag: &str) -> Option<u64> {
    let (mm, rest) = tag.split_once(':')?;
    let minutes: u64 = mm.trim().parse().ok()?;
    let (ss, frac) = match rest.split_once('.') {
        Some((s, f)) => (s, f),
        None => (rest, "0"),
    };
    let seconds: u64 = ss.trim().parse().ok()?;
    // Fractions may be hundredths or thousandths.
    let frac_ms: u64 = match frac.len() {
        0 => 0,
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        _ => frac[..3].parse::<u64>().ok()?,
    };
    Some(minutes * 60_000 + seconds * 1000 + frac_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps() {
        assert_eq!(parse_timestamp("01:02.50"), Some(62_500));
        assert_eq!(parse_timestamp("00:00"), Some(0));
        assert_eq!(parse_timestamp("02:30.123"), Some(150_123));
        assert_eq!(parse_timestamp("bogus"), None);
    }

    #[test]
    fn parses_lrc_and_tracks_position() {
        let text = "[00:01.00]first\n[00:03.00]second\n[00:05.00]third";
        let lrc = Lyrics::parse(text).expect("should parse");
        assert_eq!(lrc.len(), 3);
        assert_eq!(lrc.current_index(0), None);
        assert_eq!(lrc.current_index(1_500), Some(0));
        assert_eq!(lrc.current_index(4_000), Some(1));
        assert_eq!(lrc.current_index(9_000), Some(2));
        assert_eq!(lrc.line(1), "second");
    }

    #[test]
    fn multi_timestamp_lines_expand() {
        let lrc = Lyrics::parse("[00:01.00][00:10.00]chorus").expect("parse");
        assert_eq!(lrc.len(), 2);
    }
}
