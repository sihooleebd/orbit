//! The command language shared by the `:` command palette, key bindings (":vol 30") and
//! `orbit ctl ...` over IPC. `parse` turns one line into a [`Command`]; `App::exec` runs it.

use std::path::PathBuf;
use std::time::Duration;

use crate::app::Tab;
use crate::config::{LibraryConfig, did_you_mean, expand_tilde};
use crate::dsp::{EQ_FREQS, EQ_MAX_DB, eq_preset_names};
use crate::library::{BrowseMode, SortKey, is_audio};
use crate::queue::Repeat;
use crate::theme;
use crate::visualizer::VisMode;

#[derive(Clone, Debug, PartialEq)]
pub enum SeekTarget {
    /// "seek 1:23", "seek 83"
    Absolute(Duration),
    /// "seek +10", "seek -5"
    Forward(Duration),
    Backward(Duration),
    /// "seek 50%": percent of the track, 0..=100
    Percent(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Level {
    Set(f32),
    Up(f32),
    Down(f32),
    Reset,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SleepArg {
    Minutes(u32),
    EndOfTrack,
    Off,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoopArg {
    A,
    B,
    Clear,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EqArg {
    On,
    Off,
    Toggle,
    Preset(String),
    /// band number 1..=10, gain in dB
    Band(usize, f32),
    Preamp(f32),
    Reset,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// "play" resumes; "play <file|folder>" plays it; "play <text>" plays the best search match.
    Play(Option<String>),
    Pause,
    Toggle,
    Stop,
    Next,
    Prev,
    Seek(SeekTarget),
    /// percent: "vol 50", "vol +5", "vol -5"
    Volume(Level),
    /// None = toggle
    Mute(Option<bool>),
    /// ratio: "speed 1.25", "speed +0.1", "speed reset"
    Speed(Level),
    Shuffle(Option<bool>),
    /// None = cycle
    Repeat(Option<Repeat>),
    StopAfter(Option<bool>),
    Sleep(SleepArg),
    Loop(LoopArg),
    Eq(EqArg),
    /// None = cycle
    Theme(Option<String>),
    /// None = cycle
    Vis(Option<VisMode>),
    Sort(SortKey, Option<bool>),
    View(BrowseMode),
    Goto(Tab),
    /// Enqueue files or folders.
    Add(Vec<String>),
    /// Clear the queue.
    Clear,
    /// Save the queue as a playlist.
    Save(String),
    /// Replace the queue with a playlist.
    Load(String),
    PlaylistNew(String),
    PlaylistDelete(String),
    PlaylistRename(String, String),
    Search(String),
    Rescan,
    /// Replace the queue with the playing track and what sounds most like it.
    Radio,
    /// Download a URL with yt-dlp: (url, folder in the first music folder).
    Download(String, Option<String>),
    /// milliseconds; Set / Up / Down / Reset
    LyricsOffset(Level),
    /// Favorite the playing track; None = toggle
    Favorite(Option<bool>),
    /// Reply with the player status, optionally formatted ("{artist} - {title}").
    Status(Option<String>),
    ReloadConfig,
    Help,
    Quit,
}

/// (usage, description) for every command: drives completion and the help screen.
pub const COMMANDS: &[(&str, &str)] = &[
    ("play [file|folder|search text]", "Resume, or play a file/folder/best match"),
    ("pause", "Pause"),
    ("toggle", "Play / pause"),
    ("stop", "Stop"),
    ("next", "Next track"),
    ("prev", "Previous track"),
    ("seek <+s|-s|m:ss|n%>", "Seek relative, absolute or to a percentage"),
    ("vol <n|+n|-n|reset>", "Set or change the volume (percent)"),
    ("mute [on|off]", "Mute / unmute (toggle without argument)"),
    ("speed <x|+x|-x|reset>", "Playback speed, e.g. 1.25"),
    ("shuffle [on|off]", "Shuffle (toggle without argument)"),
    ("repeat [off|all|one]", "Repeat mode (cycle without argument)"),
    ("stopafter [on|off]", "Stop after the current track"),
    ("sleep <minutes|1h30m|end|off>", "Sleep timer"),
    ("loop <a|b|clear>", "A-B loop points"),
    ("eq <on|off|toggle|reset|[preset] NAME|band N DB|preamp DB>", "Equalizer (N = 1-10 or a frequency like 1k)"),
    ("theme [name]", "Switch theme (cycle without argument)"),
    ("vis [bars|mirror|blocks|wave|vu|cassette]", "Visualizer style (cycle without argument)"),
    ("sort <default|title|artist|album|duration|year|added|plays|path|random> [asc|desc]", "Sort track lists"),
    ("view <folders|artists|albums|genres|years|tracks>", "Library browse mode"),
    ("goto <library|queue|playlists|now|eq|1-5>", "Switch tab"),
    ("add <path>...", "Add files or folders to the queue"),
    ("clear", "Clear the queue"),
    ("save <name>", "Save the queue as a playlist"),
    ("load <name>", "Replace the queue with a playlist"),
    ("playlist new <name>", "Create a playlist"),
    ("playlist delete <name>", "Delete a playlist"),
    ("playlist rename <old> <new>", "Rename a playlist"),
    ("search <text>", "Open search with text"),
    ("rescan", "Rescan the library"),
    ("radio", "Play the playing track, then what sounds most like it"),
    ("download <url> [folder]", "Download audio with yt-dlp into a music folder (default Downloads)"),
    ("lyrics offset <ms|+ms|-ms|reset>", "Shift lyric timing for this track"),
    ("fav [on|off]", "Favorite the playing track"),
    ("status [format]", "Print status, e.g. status {artist} - {title}"),
    ("reload", "Reload the config file"),
    ("help", "Show help"),
    ("quit", "Quit"),
];

/// Other spellings: (alias, expansion). The alias replaces the first word, so "unmute" means "mute off"
/// and "vol" can be typed as "volume" or "v". Shown in the help screen next to COMMANDS.
pub const ALIASES: &[(&str, &str)] = &[
    ("volume", "vol"),
    ("v", "vol"),
    ("unmute", "mute off"),
    ("favorite", "fav"),
    ("favourite", "fav"),
    ("unfav", "fav off"),
    ("rep", "repeat"),
    ("shuf", "shuffle"),
    ("stop-after", "stopafter"),
    ("pl", "playlist"),
    ("tab", "goto"),
    ("go", "goto"),
    ("q", "quit"),
    ("exit", "quit"),
    ("previous", "prev"),
    ("skip", "next"),
    ("resume", "play"),
    ("pp", "toggle"),
    ("play-pause", "toggle"),
    ("equalizer", "eq"),
    ("visualizer", "vis"),
    ("browse", "view"),
    ("find", "search"),
    ("scan", "rescan"),
    ("similar", "radio"),
    ("dl", "download"),
    ("reload-config", "reload"),
    ("ab", "loop"),
    ("timer", "sleep"),
    ("offset", "lyrics offset"),
    ("?", "help"),
];

/// `view` arguments, in `BrowseMode::ALL` order.
const BROWSE_NAMES: [&str; 6] = ["folders", "artists", "albums", "genres", "years", "tracks"];
/// `goto` arguments, in `Tab::ALL` order.
const TAB_NAMES: [&str; 5] = ["library", "queue", "playlists", "now-playing", "equalizer"];

/// Parse one command line (leading ':' allowed). Errors are short user-facing messages.
pub fn parse(input: &str) -> Result<Command, String> {
    let line = expand_alias(input.trim().trim_start_matches(':').trim());
    let (word, rest) = split_word(&line);
    let name = word.to_lowercase();
    // keyword arguments are case-insensitive; names, paths and formats keep their case (`rest`)
    let lower = rest.to_lowercase();
    let arg = lower.as_str();
    match name.as_str() {
        "" => Err("empty command".into()),
        "play" => Ok(Command::Play(text(rest).map(|t| expand_path(&t)))),
        "pause" => bare(&name, rest, Command::Pause),
        "toggle" => bare(&name, rest, Command::Toggle),
        "stop" => match split_word(arg) {
            ("after", on) => on_off("stopafter", on).map(Command::StopAfter),
            _ => bare(&name, rest, Command::Stop),
        },
        "next" => bare(&name, rest, Command::Next),
        "prev" => bare(&name, rest, Command::Prev),
        "seek" => seek(arg).map(Command::Seek),
        "vol" => volume(arg).map(Command::Volume),
        "mute" => on_off("mute", arg).map(Command::Mute),
        "speed" => speed(arg).map(Command::Speed),
        "shuffle" => on_off("shuffle", arg).map(Command::Shuffle),
        "repeat" => repeat(arg).map(Command::Repeat),
        "stopafter" => on_off("stopafter", arg).map(Command::StopAfter),
        "sleep" => sleep(arg).map(Command::Sleep),
        "loop" => ab_loop(arg).map(Command::Loop),
        "eq" => eq(arg).map(Command::Eq),
        "theme" => theme_name(rest).map(Command::Theme),
        "vis" => vis(arg).map(Command::Vis),
        "sort" => sort(arg),
        "view" => browse_mode(arg).map(Command::View).ok_or_else(|| format!("view: expected {}", BROWSE_NAMES.join(", "))),
        "goto" => tab(arg).map(Command::Goto).ok_or_else(|| "goto: expected library, queue, playlists, now or eq (or 1-5)".into()),
        "add" => {
            let paths: Vec<String> = tokenize(rest).iter().map(|p| expand_path(p)).collect();
            if paths.is_empty() { Err("add: expected one or more files or folders".into()) } else { Ok(Command::Add(paths)) }
        }
        "clear" => bare(&name, rest, Command::Clear),
        "save" => text(rest).map(Command::Save).ok_or_else(|| "save: expected a playlist name".into()),
        "load" => text(rest).map(Command::Load).ok_or_else(|| "load: expected a playlist name".into()),
        "playlist" => playlist(rest),
        "search" => Ok(Command::Search(text(rest).unwrap_or_default())),
        "rescan" => bare(&name, rest, Command::Rescan),
        "radio" => bare(&name, rest, Command::Radio),
        "download" => match split_word(rest) {
            ("", _) => Err("download: expected a URL".into()),
            (url, folder) => Ok(Command::Download(url.to_string(), text(folder))),
        },
        "lyrics" => match split_word(arg) {
            ("offset", ms) => lyrics_offset(ms).map(Command::LyricsOffset),
            _ => Err("lyrics: expected offset <ms|+ms|-ms|reset>".into()),
        },
        "fav" => on_off("fav", arg).map(Command::Favorite),
        "status" => Ok(Command::Status(text(rest))),
        "reload" => bare(&name, rest, Command::ReloadConfig),
        "help" => Ok(Command::Help),
        "quit" => bare(&name, rest, Command::Quit),
        _ => {
            let known = command_names().into_iter().chain(ALIASES.iter().map(|(a, _)| *a));
            let hint = did_you_mean(word, known).map(|n| format!(" (did you mean \"{n}\"?)")).unwrap_or_default();
            Err(format!("unknown command \"{word}\"{hint}"))
        }
    }
}

/// The `COMMANDS` entry a (partial) line is typing, aliases expanded: the one whose literal words
/// match most of the typed words ("pl delete Mix" -> "playlist delete <name>"), first on a tie.
pub fn usage_for(line: &str) -> Option<&'static (&'static str, &'static str)> {
    let line = expand_alias(line.trim().trim_start_matches(':').trim()).to_lowercase();
    let typed: Vec<&str> = line.split_whitespace().collect();
    let matched = |usage: &str| usage.split_whitespace().take_while(|w| !w.starts_with(['<', '['])).zip(&typed).take_while(|(u, t)| u == *t).count();
    COMMANDS.iter().filter(|(usage, _)| matched(usage) > 0).rev().max_by_key(|(usage, _)| matched(usage))
}

/// Canonical command names, in `COMMANDS` order.
fn command_names() -> Vec<&'static str> {
    let mut names: Vec<&str> = COMMANDS.iter().map(|(usage, _)| usage.split_whitespace().next().unwrap_or("")).collect();
    names.dedup();
    names
}

fn expand_alias(line: &str) -> String {
    let (word, rest) = split_word(line);
    match ALIASES.iter().find(|(alias, _)| alias.eq_ignore_ascii_case(word)) {
        Some((_, expansion)) if rest.is_empty() => expansion.to_string(),
        Some((_, expansion)) => format!("{expansion} {rest}"),
        None => line.to_string(),
    }
}

/// ("first", "the rest") of a trimmed line.
fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    s.split_once(char::is_whitespace).map_or((s, ""), |(w, rest)| (w, rest.trim()))
}

fn bare(name: &str, rest: &str, cmd: Command) -> Result<Command, String> {
    if rest.is_empty() { Ok(cmd) } else { Err(format!("{name}: takes no arguments")) }
}

fn expand_path(p: &str) -> String {
    expand_tilde(p).to_string_lossy().into_owned()
}

/// A free-text argument: the rest of the line, minus one pair of quotes around all of it. None if empty.
fn text(rest: &str) -> Option<String> {
    let rest = rest.trim();
    let t = match tokenize(rest).as_slice() {
        [only] if rest.starts_with(['"', '\'']) => only.clone(),
        _ => rest.to_string(),
    };
    (!t.is_empty()).then_some(t)
}

fn on_off(name: &str, arg: &str) -> Result<Option<bool>, String> {
    match arg {
        "" | "toggle" => Ok(None),
        "on" | "yes" | "true" | "1" | "enable" => Ok(Some(true)),
        "off" | "no" | "false" | "0" | "disable" => Ok(Some(false)),
        _ => Err(format!("{name}: expected on, off or toggle")),
    }
}

/// A plain non-negative number ("5", "0.25"); signs are handled by the callers.
fn number(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.starts_with(['+', '-']) {
        return None;
    }
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// "5" -> Set(5), "+5" -> Up(5), "-5" -> Down(5), with the magnitude read by `amount`.
fn level(arg: &str, amount: fn(&str) -> Option<f64>) -> Option<Level> {
    let (make, magnitude): (fn(f32) -> Level, &str) = match arg.as_bytes().first() {
        Some(b'+') => (Level::Up, &arg[1..]),
        Some(b'-') => (Level::Down, &arg[1..]),
        _ => (Level::Set, arg),
    };
    amount(magnitude).map(|v| make(v as f32))
}

fn volume(arg: &str) -> Result<Level, String> {
    match arg {
        "reset" | "default" => Ok(Level::Reset),
        _ => level(arg.trim_end_matches('%'), number).ok_or_else(|| "vol: expected a number like 50, +5 or -5".into()),
    }
}

fn speed(arg: &str) -> Result<Level, String> {
    const HELP: &str = "speed: expected a speed like 1.25, +0.1, -0.1 or reset";
    match arg {
        "reset" | "normal" | "default" | "1x" => Ok(Level::Reset),
        _ => match level(arg.trim_end_matches('x'), number).ok_or(HELP)? {
            Level::Set(v) if !(0.25..=4.0).contains(&v) => Err("speed: must be between 0.25 and 4".into()),
            l => Ok(l),
        },
    }
}

fn seek(arg: &str) -> Result<SeekTarget, String> {
    if let Some(p) = arg.strip_suffix('%') {
        return number(p)
            .filter(|v| *v <= 100.0)
            .map(|v| SeekTarget::Percent(v as f32))
            .ok_or_else(|| "seek: expected a percentage from 0% to 100%".into());
    }
    let target = match arg.as_bytes().first() {
        Some(b'+') => parse_time(&arg[1..]).map(SeekTarget::Forward),
        Some(b'-') => parse_time(&arg[1..]).map(SeekTarget::Backward),
        _ => parse_time(arg).map(SeekTarget::Absolute),
    };
    target.ok_or_else(|| "seek: expected a time like 1:23, 83, +10, -10, +1m or 50%".into())
}

/// "83", "83.5", "1:23", "1:02:03", "90s", "2m", "1m30s", "1h30" (= 1h30m), "2 min". None otherwise.
fn parse_time(s: &str) -> Option<Duration> {
    let mut s: String = s.to_lowercase().split_whitespace().collect();
    for (word, unit) in [
        ("hours", "h"),
        ("hour", "h"),
        ("hrs", "h"),
        ("hr", "h"),
        ("minutes", "m"),
        ("minute", "m"),
        ("mins", "m"),
        ("min", "m"),
        ("seconds", "s"),
        ("second", "s"),
        ("secs", "s"),
        ("sec", "s"),
    ] {
        s = s.replace(word, unit);
    }
    let secs = if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        let (last, lead) = parts.split_last()?;
        if lead.len() > 2 {
            return None;
        }
        let mut total = 0.0;
        for (i, p) in lead.iter().enumerate() {
            let v = f64::from(p.parse::<u32>().ok().filter(|_| !p.starts_with('+'))?);
            if i > 0 && v >= 60.0 {
                return None;
            }
            total = total * 60.0 + v;
        }
        let sec = number(last).filter(|v| *v < 60.0)?;
        total * 60.0 + sec
    } else if s.contains(['h', 'm', 's']) {
        let (mut total, mut num, mut last) = (0.0, String::new(), None::<u32>);
        for c in s.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
                continue;
            }
            let unit = match c {
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => return None,
            };
            // units go from big to small: "1h30m", not "30m1h"
            if last.is_some_and(|l| l <= unit) {
                return None;
            }
            total += number(&num)? * f64::from(unit);
            num.clear();
            last = Some(unit);
        }
        if !num.is_empty() {
            // a trailing bare number takes the next smaller unit: "1h30" = 1h30m, "1m30" = 1m30s
            let unit = match last {
                Some(3600) => 60,
                Some(60) => 1,
                _ => return None,
            };
            total += number(&num)? * f64::from(unit);
        }
        total
    } else {
        number(&s)?
    };
    Duration::try_from_secs_f64(secs).ok()
}

fn repeat(arg: &str) -> Result<Option<Repeat>, String> {
    match arg {
        "" | "cycle" | "next" | "toggle" => Ok(None),
        "off" | "none" | "no" => Ok(Some(Repeat::Off)),
        "all" | "on" | "queue" | "list" => Ok(Some(Repeat::All)),
        "one" | "single" | "track" | "song" | "1" => Ok(Some(Repeat::One)),
        _ => Err("repeat: expected off, all or one".into()),
    }
}

fn sleep(arg: &str) -> Result<SleepArg, String> {
    const HELP: &str = "sleep: expected minutes (30, 30m, 1h30m, 1:30), end or off";
    match arg {
        "end" | "eot" | "track" | "end-of-track" => return Ok(SleepArg::EndOfTrack),
        "off" | "cancel" | "none" | "0" => return Ok(SleepArg::Off),
        _ => {}
    }
    let secs = if let Some((h, m)) = arg.split_once(':') {
        // on a timer "1:30" means hours and minutes
        let h: u32 = h.trim().parse().map_err(|_| HELP)?;
        let m: u32 = m.trim().parse().ok().filter(|m| *m < 60).ok_or(HELP)?;
        (f64::from(h) * 60.0 + f64::from(m)) * 60.0
    } else if let Some(minutes) = number(arg) {
        minutes * 60.0
    } else {
        parse_time(arg).ok_or(HELP)?.as_secs_f64()
    };
    let minutes = (secs / 60.0).ceil();
    Ok(if minutes < 1.0 { SleepArg::Off } else { SleepArg::Minutes(minutes.min(f64::from(u32::MAX)) as u32) })
}

fn ab_loop(arg: &str) -> Result<LoopArg, String> {
    match arg {
        "a" | "start" => Ok(LoopArg::A),
        "b" | "end" => Ok(LoopArg::B),
        "clear" | "off" | "reset" | "none" => Ok(LoopArg::Clear),
        _ => Err("loop: expected a, b or clear".into()),
    }
}

fn eq(arg: &str) -> Result<EqArg, String> {
    let words: Vec<&str> = arg.split_whitespace().collect();
    match words.as_slice() {
        [] => Err("eq: expected on, off, toggle, reset, a preset, band N DB or preamp DB".into()),
        ["on" | "enable"] => Ok(EqArg::On),
        ["off" | "disable"] => Ok(EqArg::Off),
        ["toggle"] => Ok(EqArg::Toggle),
        ["reset"] => Ok(EqArg::Reset),
        ["preset"] => Err(format!("eq preset: expected one of {}", eq_preset_names().join(", "))),
        ["preset", name @ ..] => eq_preset(&name.join("-")).map(EqArg::Preset),
        ["band", n, db] => Ok(EqArg::Band(eq_band(n)?, eq_gain(db)?)),
        ["band", ..] => Err("eq band: expected a band (1-10 or a frequency like 1k) and a gain, e.g. eq band 3 +2.5".into()),
        ["preamp", db] => Ok(EqArg::Preamp(eq_gain(db)?)),
        ["preamp", ..] => Err("eq preamp: expected a gain in dB, e.g. eq preamp -3".into()),
        name => eq_preset(&name.join("-")).map(EqArg::Preset),
    }
}

fn eq_preset(name: &str) -> Result<String, String> {
    let name = name.replace('_', "-");
    let presets = eq_preset_names();
    presets.iter().find(|p| p.eq_ignore_ascii_case(&name)).map(|p| p.to_string()).ok_or_else(|| {
        let hint = did_you_mean(&name, presets.iter().copied()).map(|p| format!(" (did you mean \"{p}\"?)")).unwrap_or_default();
        format!("eq: unknown preset \"{name}\"{hint}; presets: {}", presets.join(", "))
    })
}

/// Band number 1..=10, or a band's frequency ("125", "1k", "16khz").
fn eq_band(s: &str) -> Result<usize, String> {
    let bad = || format!("eq band: \"{s}\" is not a band (1-10) or one of 31, 62, 125, 250, 500, 1k, 2k, 4k, 8k, 16k");
    let t = s.trim_end_matches("hz");
    if let Some(n) = t.parse::<usize>().ok().filter(|n| (1..=10).contains(n)) {
        return Ok(n);
    }
    let hz = match t.strip_suffix('k') {
        Some(k) => number(k).map(|v| v * 1000.0),
        None => number(t),
    }
    .ok_or_else(bad)?;
    EQ_FREQS.iter().position(|&f| (hz - f64::from(f)).abs() <= f64::from(f) * 0.05).map(|i| i + 1).ok_or_else(bad)
}

fn eq_gain(s: &str) -> Result<f32, String> {
    let t = s.trim_end_matches("db");
    let db = match t.as_bytes().first() {
        Some(b'-') => number(&t[1..]).map(|v| -v),
        Some(b'+') => number(&t[1..]),
        _ => number(t),
    }
    .ok_or_else(|| format!("eq: \"{s}\" is not a gain in dB (like +3 or -2.5)"))?;
    if db.abs() > f64::from(EQ_MAX_DB) {
        return Err(format!("eq: gains go from -{EQ_MAX_DB} to +{EQ_MAX_DB} dB"));
    }
    Ok(db as f32)
}

fn theme_name(rest: &str) -> Result<Option<String>, String> {
    match rest.to_lowercase().as_str() {
        "" | "next" | "cycle" => Ok(None),
        _ => theme::canonical_name(rest).map(|n| Some(n.to_string())).ok_or_else(|| {
            let hint = did_you_mean(rest, theme::names()).map(|n| format!(" (did you mean \"{n}\"?)")).unwrap_or_default();
            format!("theme: unknown theme \"{rest}\"{hint}; orbit --list-themes shows them all")
        }),
    }
}

fn vis(arg: &str) -> Result<Option<VisMode>, String> {
    let mode = match arg {
        "" | "next" | "cycle" => return Ok(None),
        "spectrum" => VisMode::Bars,
        "mirrored" => VisMode::Mirror,
        "led" | "leds" => VisMode::Blocks,
        "scope" | "oscilloscope" | "waveform" => VisMode::Wave,
        "meter" | "meters" | "vu-meter" | "levels" => VisMode::Vu,
        "tape" | "reels" => VisMode::Cassette,
        _ => VisMode::ALL.into_iter().find(|m| m.label() == arg).ok_or("vis: expected bars, mirror, blocks, wave, vu or cassette")?,
    };
    Ok(Some(mode))
}

fn sort(arg: &str) -> Result<Command, String> {
    let help = || format!("sort: expected a key ({}) and optionally asc or desc", SortKey::ALL.map(|k| k.label()).join(", "));
    let (key, dir) = split_word(arg);
    let key = sort_key(key).ok_or_else(help)?;
    let desc = match dir {
        "" => None,
        "asc" | "ascending" | "up" | "a-z" => Some(false),
        "desc" | "descending" | "down" | "reverse" | "rev" | "z-a" => Some(true),
        _ => return Err(help()),
    };
    Ok(Command::Sort(key, desc))
}

fn sort_key(s: &str) -> Option<SortKey> {
    Some(match s {
        "name" | "song" => SortKey::Title,
        "time" | "length" | "len" => SortKey::Duration,
        "date" | "new" | "newest" | "recent" | "date-added" | "mtime" => SortKey::Added,
        "count" | "playcount" | "play-count" | "popular" => SortKey::Plays,
        "file" | "filename" | "folder" => SortKey::Path,
        "shuffle" | "shuffled" => SortKey::Random,
        "natural" | "none" => SortKey::Default,
        _ => return SortKey::ALL.into_iter().find(|k| k.label() == s),
    })
}

fn browse_mode(s: &str) -> Option<BrowseMode> {
    Some(match s {
        "folders" | "folder" | "dirs" | "dir" | "directories" => BrowseMode::Folders,
        "artists" | "artist" => BrowseMode::Artists,
        "albums" | "album" => BrowseMode::Albums,
        "genres" | "genre" => BrowseMode::Genres,
        "years" | "year" => BrowseMode::Years,
        "tracks" | "track" | "songs" | "all" | "all-tracks" => BrowseMode::Tracks,
        _ => return None,
    })
}

fn tab(s: &str) -> Option<Tab> {
    Some(match s {
        "library" | "lib" | "1" => Tab::Library,
        "queue" | "2" => Tab::Queue,
        "playlists" | "playlist" | "pl" | "3" => Tab::Playlists,
        "now" | "now-playing" | "nowplaying" | "np" | "playing" | "4" => Tab::NowPlaying,
        "eq" | "equalizer" | "equaliser" | "5" => Tab::Equalizer,
        _ => return None,
    })
}

fn playlist(rest: &str) -> Result<Command, String> {
    let (sub, arg) = split_word(rest);
    let name = |what: &str| text(arg).ok_or_else(|| format!("playlist {what}: expected a playlist name"));
    match sub.to_lowercase().as_str() {
        "new" | "create" => name("new").map(Command::PlaylistNew),
        "delete" | "del" | "rm" | "remove" => name("delete").map(Command::PlaylistDelete),
        "rename" | "mv" => match tokenize(arg).as_slice() {
            [old, new] if !old.trim().is_empty() && !new.trim().is_empty() => Ok(Command::PlaylistRename(old.clone(), new.clone())),
            _ => Err("playlist rename: expected the old and the new name; quote names with spaces: \
                      playlist rename \"Old name\" \"New name\""
                .into()),
        },
        "load" | "open" | "play" => name("load").map(Command::Load),
        "save" => name("save").map(Command::Save),
        "" => Err("playlist: expected new, delete or rename".into()),
        other => Err(format!("playlist: unknown subcommand \"{other}\" (expected new, delete or rename)")),
    }
}

fn lyrics_offset(arg: &str) -> Result<Level, String> {
    fn millis(s: &str) -> Option<f64> {
        match (s.strip_suffix("ms"), s.strip_suffix('s')) {
            (Some(ms), _) => number(ms),
            (None, Some(secs)) => number(secs).map(|v| v * 1000.0),
            (None, None) => number(s),
        }
    }
    match arg {
        "reset" | "default" => Ok(Level::Reset),
        _ => level(arg, millis).ok_or_else(|| "lyrics offset: expected milliseconds like 250, +250 or -250 (or +0.5s), or reset".into()),
    }
}

/// One word of a command line. `start` is the byte offset of its first character (the opening quote
/// for quoted words).
struct Token {
    text: String,
    start: usize,
    quoted: bool,
    closed: bool,
}

/// Split a line into words. "double" or 'single' quotes group words; a quote only opens at the start of
/// a word, so "Don't" stays one word. Backslash escapes the next character (not inside single quotes).
/// An unterminated quote runs to the end of the line.
fn tokens(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut it = s.char_indices().peekable();
    while let Some(&(start, first)) = it.peek() {
        if first.is_whitespace() {
            it.next();
            continue;
        }
        let quote = matches!(first, '"' | '\'').then_some(first);
        if quote.is_some() {
            it.next();
        }
        let (mut text, mut closed) = (String::new(), false);
        while let Some((_, c)) = it.next_if(|&(_, c)| quote.is_some() || !c.is_whitespace()) {
            if Some(c) == quote {
                closed = true;
                break;
            }
            if c == '\\' && quote != Some('\'') {
                text.push(it.next().map_or('\\', |(_, escaped)| escaped));
            } else {
                text.push(c);
            }
        }
        out.push(Token { text, start, quoted: quote.is_some(), closed });
    }
    out
}

fn tokenize(s: &str) -> Vec<String> {
    tokens(s).into_iter().map(|t| t.text).collect()
}

/// Quote a word for the command line when it needs it (spaces, quotes, backslashes).
fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\')) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Completions for a partially typed line: full candidate lines (e.g. "repeat all").
#[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
pub fn complete(input: &str) -> Vec<String> {
    complete_with(input, &[])
}

/// `complete`, also offering `playlist_names` for "load ", "save ", "playlist delete " and
/// "playlist rename ". Completes command names (aliases too), keyword arguments, theme / EQ preset
/// names, and files and folders for "add " and "play ~/...". Matching is by prefix, then by substring,
/// ignoring case; each candidate is the whole new line (a leading ':' is kept).
pub fn complete_with(input: &str, playlist_names: &[String]) -> Vec<String> {
    let line = input.trim_start().trim_start_matches(':').trim_start();
    let prefix = &input[..input.len() - line.len()];
    let toks = tokens(line);
    let new_word = line.is_empty() || (line.ends_with(char::is_whitespace) && toks.last().is_none_or(|t| !t.quoted || t.closed));
    let (done, current) = if new_word { (&toks[..], None) } else { (&toks[..toks.len() - 1], toks.last()) };
    let at = current.map_or(line.len(), |t| t.start);
    let typed = current.map_or("", |t| t.text.as_str());
    let head = |at: usize| format!("{prefix}{}", &line[..at]);
    let words = |values: Vec<String>| filter(values, typed).iter().map(|v| head(at) + &quote(v)).collect::<Vec<_>>();

    let Some(first) = done.first() else {
        let names = filter(command_names().into_iter().map(String::from).collect(), typed);
        let names = if names.is_empty() && !typed.is_empty() {
            filter(ALIASES.iter().map(|(a, _)| a.to_string()).collect(), typed)
        } else {
            names
        };
        return names.into_iter().map(|n| head(at) + &n).collect();
    };
    let word = first.text.to_lowercase();
    let cmd = match ALIASES.iter().find(|(alias, _)| *alias == word) {
        Some((_, expansion)) if !expansion.contains(' ') => *expansion,
        Some(_) => return Vec::new(),
        None => word.as_str(),
    };
    let args: Vec<String> = done[1..].iter().map(|t| t.text.to_lowercase()).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // where free text starts after the command and `n` argument words
    let rest_at = |n: usize| toks.get(1 + n).map_or(line.len(), |t| t.start);
    let names_from = |at: usize| {
        let typed = line[at..].trim_start_matches(['"', '\'']);
        filter(playlist_names.to_vec(), typed).iter().map(|n| head(at) + &quote(n)).collect()
    };
    match (cmd, args.as_slice()) {
        ("load" | "save", _) => names_from(rest_at(0)),
        ("playlist", [sub, ..]) if matches!(*sub, "delete" | "del" | "rm" | "remove" | "load" | "open" | "play" | "save") => {
            names_from(rest_at(1))
        }
        ("playlist", ["rename" | "mv"]) => words(playlist_names.to_vec()),
        ("add", _) => complete_path(typed)
            .iter()
            .map(|p| {
                // a folder's quote stays open so completion can go on inside it (tokens() lets an
                // open quote run to the end of the line)
                let q = quote(p);
                head(at) + if p.ends_with('/') { q.strip_suffix('"').unwrap_or(&q) } else { &q }
            })
            .collect(),
        ("play", _) => {
            let at = rest_at(0);
            let typed = &line[at..];
            if typed.starts_with(['/', '~', '.']) { complete_path(typed).into_iter().map(|p| head(at) + &p).collect() } else { Vec::new() }
        }
        _ => words(arg_values(cmd, &args)),
    }
}

/// Keyword values for argument number `args.len()` of `cmd`.
fn arg_values(cmd: &str, args: &[&str]) -> Vec<String> {
    let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    match (cmd, args) {
        ("mute" | "shuffle" | "stopafter" | "fav", []) => list(&["on", "off"]),
        ("repeat", []) => list(&["off", "all", "one"]),
        ("theme", []) => list(&theme::names()),
        ("vis", []) => VisMode::ALL.iter().map(|m| m.label().to_string()).collect(),
        ("sort", []) => SortKey::ALL.iter().map(|k| k.label().to_string()).collect(),
        ("sort", [_]) => list(&["asc", "desc"]),
        ("view", []) => list(&BROWSE_NAMES),
        ("goto", []) => list(&TAB_NAMES),
        ("sleep", []) => list(&["15", "30", "45", "60", "90", "end", "off"]),
        ("loop", []) => list(&["a", "b", "clear"]),
        ("speed", []) => list(&["reset", "0.5", "0.75", "1.25", "1.5", "2"]),
        ("eq", []) => [list(&["on", "off", "toggle", "reset"]), list(&eq_preset_names()), list(&["preset", "band", "preamp"])].concat(),
        ("eq", ["preset"]) => list(&eq_preset_names()),
        ("eq", ["band"]) => (1..=10).map(|n| n.to_string()).collect(),
        ("lyrics", []) => list(&["offset"]),
        ("lyrics", ["offset"]) => list(&["reset", "+250", "-250"]),
        ("playlist", []) => list(&["new", "delete", "rename"]),
        _ => Vec::new(),
    }
}

/// Values starting with `typed`, or else containing it (ignoring case), in their original order.
fn filter(values: Vec<String>, typed: &str) -> Vec<String> {
    let t = typed.to_lowercase();
    if values.iter().any(|v| v.to_lowercase().starts_with(&t)) {
        values.into_iter().filter(|v| v.to_lowercase().starts_with(&t)).collect()
    } else {
        values.into_iter().filter(|v| v.to_lowercase().contains(&t)).collect()
    }
}

/// Folders (ending in '/') and audio files matching a partly typed path: "~/Mu" -> "~/Music/".
fn complete_path(typed: &str) -> Vec<String> {
    if typed == "~" {
        return vec!["~/".into()];
    }
    let (dir, stem) = typed.rfind('/').map_or(("", typed), |i| typed.split_at(i + 1));
    let base = if dir.is_empty() { PathBuf::from(".") } else { expand_tilde(dir) };
    let Ok(entries) = std::fs::read_dir(base) else { return Vec::new() };
    let extensions = LibraryConfig::default().extensions;
    let stem = stem.to_lowercase();
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if (name.starts_with('.') && !stem.starts_with('.')) || !name.to_lowercase().starts_with(&stem) {
                return None;
            }
            let path = e.path();
            if path.is_dir() {
                Some(format!("{dir}{name}/"))
            } else {
                is_audio(&path, &extensions).then(|| format!("{dir}{name}"))
            }
        })
        .collect();
    out.sort_by_key(|p| p.to_lowercase());
    // ponytail: a fixed cap instead of paging; nobody scrolls through thousands of candidates
    out.truncate(200);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> Command {
        parse(line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
    }

    fn err(line: &str) -> String {
        parse(line).expect_err(line)
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn every_command_parses() {
        use Command as C;
        let home = expand_tilde("~").to_string_lossy().into_owned();
        let cases: Vec<(&str, Command)> = vec![
            ("play", C::Play(None)),
            ("resume", C::Play(None)),
            ("play some song", C::Play(Some("some song".into()))),
            ("play \"a b\"", C::Play(Some("a b".into()))),
            ("play ~/Music/x.mp3", C::Play(Some(format!("{home}/Music/x.mp3")))),
            ("pause", C::Pause),
            ("toggle", C::Toggle),
            ("pp", C::Toggle),
            ("stop", C::Stop),
            ("next", C::Next),
            ("skip", C::Next),
            ("prev", C::Prev),
            ("previous", C::Prev),
            ("seek 83", C::Seek(SeekTarget::Absolute(secs(83)))),
            ("vol 50", C::Volume(Level::Set(50.0))),
            ("volume +5", C::Volume(Level::Up(5.0))),
            ("v -5", C::Volume(Level::Down(5.0))),
            ("vol 30%", C::Volume(Level::Set(30.0))),
            ("vol reset", C::Volume(Level::Reset)),
            ("mute", C::Mute(None)),
            ("mute on", C::Mute(Some(true))),
            ("mute off", C::Mute(Some(false))),
            ("unmute", C::Mute(Some(false))),
            ("speed 1.25", C::Speed(Level::Set(1.25))),
            ("shuffle", C::Shuffle(None)),
            ("shuf on", C::Shuffle(Some(true))),
            ("shuffle off", C::Shuffle(Some(false))),
            ("repeat", C::Repeat(None)),
            ("rep all", C::Repeat(Some(Repeat::All))),
            ("repeat one", C::Repeat(Some(Repeat::One))),
            ("repeat off", C::Repeat(Some(Repeat::Off))),
            ("stopafter", C::StopAfter(None)),
            ("stop-after on", C::StopAfter(Some(true))),
            ("stop after off", C::StopAfter(Some(false))),
            ("sleep 30", C::Sleep(SleepArg::Minutes(30))),
            ("loop a", C::Loop(LoopArg::A)),
            ("loop b", C::Loop(LoopArg::B)),
            ("ab clear", C::Loop(LoopArg::Clear)),
            ("eq on", C::Eq(EqArg::On)),
            ("theme", C::Theme(None)),
            ("theme dracula", C::Theme(Some("dracula".into()))),
            ("theme Tokyo Night", C::Theme(Some("tokyo-night".into()))),
            ("vis", C::Vis(None)),
            ("vis wave", C::Vis(Some(VisMode::Wave))),
            ("visualizer scope", C::Vis(Some(VisMode::Wave))),
            ("sort title", C::Sort(SortKey::Title, None)),
            ("view artists", C::View(BrowseMode::Artists)),
            ("browse album", C::View(BrowseMode::Albums)),
            ("goto queue", C::Goto(Tab::Queue)),
            ("tab 5", C::Goto(Tab::Equalizer)),
            ("add a.mp3 \"b c.flac\"", C::Add(vec!["a.mp3".into(), "b c.flac".into()])),
            ("add ~/x", C::Add(vec![format!("{home}/x")])),
            ("clear", C::Clear),
            ("save My Mix", C::Save("My Mix".into())),
            ("save \"My Mix\"", C::Save("My Mix".into())),
            ("load Chill", C::Load("Chill".into())),
            ("playlist new Road Trip", C::PlaylistNew("Road Trip".into())),
            ("pl create x", C::PlaylistNew("x".into())),
            ("playlist delete Old", C::PlaylistDelete("Old".into())),
            ("pl rm \"Old one\"", C::PlaylistDelete("Old one".into())),
            ("playlist rename \"Old name\" \"New name\"", C::PlaylistRename("Old name".into(), "New name".into())),
            ("playlist rename old new", C::PlaylistRename("old".into(), "new".into())),
            ("pl mv \"Don't\" x", C::PlaylistRename("Don't".into(), "x".into())),
            ("pl mv Don't 'x y'", C::PlaylistRename("Don't".into(), "x y".into())),
            ("playlist load Chill", C::Load("Chill".into())),
            ("search hello world", C::Search("hello world".into())),
            ("search", C::Search(String::new())),
            ("find \"exact phrase\"", C::Search("exact phrase".into())),
            ("rescan", C::Rescan),
            ("scan", C::Rescan),
            ("radio", C::Radio),
            ("similar", C::Radio),
            ("download https://x.test/a", C::Download("https://x.test/a".into(), None)),
            ("dl https://x.test/A My Mix", C::Download("https://x.test/A".into(), Some("My Mix".into()))),
            ("vis tape", C::Vis(Some(VisMode::Cassette))),
            ("lyrics offset 250", C::LyricsOffset(Level::Set(250.0))),
            ("fav", C::Favorite(None)),
            ("favorite on", C::Favorite(Some(true))),
            ("unfav", C::Favorite(Some(false))),
            ("status", C::Status(None)),
            ("status {artist} - {title}", C::Status(Some("{artist} - {title}".into()))),
            ("status \"{title}\"", C::Status(Some("{title}".into()))),
            ("reload", C::ReloadConfig),
            ("reload-config", C::ReloadConfig),
            ("help", C::Help),
            ("?", C::Help),
            ("quit", C::Quit),
            ("q", C::Quit),
            ("exit", C::Quit),
            (":vol 10", C::Volume(Level::Set(10.0))),
            ("  VOL   20  ", C::Volume(Level::Set(20.0))),
            ("Repeat ONE", C::Repeat(Some(Repeat::One))),
        ];
        for (line, want) in cases {
            assert_eq!(ok(line), want, "{line:?}");
        }
        // every canonical name is accepted by the parser (errors are about arguments, not the name)
        for name in command_names() {
            if let Err(e) = parse(name) {
                assert!(e.starts_with(name) || e.starts_with(&format!("{name} ")), "{name}: {e}");
            }
        }
    }

    #[test]
    fn times() {
        let t = |s: &str| parse_time(s);
        assert_eq!(t("83"), Some(secs(83)));
        assert_eq!(t("83.5"), Some(Duration::from_millis(83_500)));
        assert_eq!(t("1:23"), Some(secs(83)));
        assert_eq!(t("01:02:03"), Some(secs(3723)));
        assert_eq!(t("90s"), Some(secs(90)));
        assert_eq!(t("2m"), Some(secs(120)));
        assert_eq!(t("1m30s"), Some(secs(90)));
        assert_eq!(t("1m30"), Some(secs(90)));
        assert_eq!(t("1h30"), Some(secs(5400)));
        assert_eq!(t("1h 2m 3s"), Some(secs(3723)));
        assert_eq!(t("2 min"), Some(secs(120)));
        assert_eq!(t("1.5 minutes"), Some(secs(90)));
        for bad in ["", "abc", "1:75", "1:2:3:4", "30m1h", "5x", "1e400", "-5", "+5", "m", "1:+5", ":30"] {
            assert_eq!(t(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn seek_targets() {
        use SeekTarget as S;
        let s = |line: &str| match ok(line) {
            Command::Seek(t) => t,
            other => panic!("{other:?}"),
        };
        assert_eq!(s("seek 83"), S::Absolute(secs(83)));
        assert_eq!(s("seek 1:23"), S::Absolute(secs(83)));
        assert_eq!(s("seek 1:02:03"), S::Absolute(secs(3723)));
        assert_eq!(s("seek +10"), S::Forward(secs(10)));
        assert_eq!(s("seek -10"), S::Backward(secs(10)));
        assert_eq!(s("seek +1:00"), S::Forward(secs(60)));
        assert_eq!(s("seek -1m"), S::Backward(secs(60)));
        assert_eq!(s("seek 50%"), S::Percent(50.0));
        assert_eq!(s("seek 0%"), S::Percent(0.0));
        assert!(err("seek").contains("expected a time"));
        assert!(err("seek soon").contains("expected a time"));
        assert!(err("seek 150%").contains("percentage"));
        assert!(err("seek +50%").contains("percentage"));
    }

    #[test]
    fn levels() {
        let speed = |line: &str| match ok(line) {
            Command::Speed(l) => l,
            other => panic!("{other:?}"),
        };
        assert_eq!(speed("speed 1.25"), Level::Set(1.25));
        assert_eq!(speed("speed 1.5x"), Level::Set(1.5));
        assert_eq!(speed("speed +0.1"), Level::Up(0.1));
        assert_eq!(speed("speed -0.1"), Level::Down(0.1));
        for reset in ["speed reset", "speed 1x", "speed normal", "speed default"] {
            assert_eq!(speed(reset), Level::Reset, "{reset}");
        }
        assert!(err("speed 9").contains("between 0.25 and 4"));
        assert!(err("speed fast").contains("expected a speed"));
        assert!(err("speed").contains("expected a speed"));
        assert_eq!(err("vol loud"), "vol: expected a number like 50, +5 or -5");
        assert_eq!(err("vol"), "vol: expected a number like 50, +5 or -5");
        assert_eq!(err("vol +-5"), "vol: expected a number like 50, +5 or -5");
        assert_eq!(err("vol nan"), "vol: expected a number like 50, +5 or -5");

        let offset = |line: &str| match ok(line) {
            Command::LyricsOffset(l) => l,
            other => panic!("{other:?}"),
        };
        assert_eq!(offset("lyrics offset 250"), Level::Set(250.0));
        assert_eq!(offset("lyrics offset +250"), Level::Up(250.0));
        assert_eq!(offset("lyrics offset -250ms"), Level::Down(250.0));
        assert_eq!(offset("lyrics offset +0.5s"), Level::Up(500.0));
        assert_eq!(offset("lyrics offset reset"), Level::Reset);
        assert_eq!(offset("offset -100"), Level::Down(100.0));
        assert!(err("lyrics offset soon").contains("milliseconds"));
        assert!(err("lyrics").contains("expected offset"));
    }

    #[test]
    fn sleep_timer() {
        let s = |line: &str| match ok(line) {
            Command::Sleep(a) => a,
            other => panic!("{other:?}"),
        };
        assert_eq!(s("sleep 30"), SleepArg::Minutes(30));
        assert_eq!(s("sleep 30m"), SleepArg::Minutes(30));
        assert_eq!(s("sleep 30 min"), SleepArg::Minutes(30));
        assert_eq!(s("sleep 1h"), SleepArg::Minutes(60));
        assert_eq!(s("sleep 1h30m"), SleepArg::Minutes(90));
        assert_eq!(s("sleep 1:30"), SleepArg::Minutes(90));
        assert_eq!(s("sleep 90s"), SleepArg::Minutes(2));
        assert_eq!(s("timer 45"), SleepArg::Minutes(45));
        assert_eq!(s("sleep end"), SleepArg::EndOfTrack);
        assert_eq!(s("sleep off"), SleepArg::Off);
        assert_eq!(s("sleep 0"), SleepArg::Off);
        assert!(err("sleep").contains("expected minutes"));
        assert!(err("sleep later").contains("expected minutes"));
        assert!(err("sleep 1:75").contains("expected minutes"));
    }

    #[test]
    fn equalizer() {
        let e = |line: &str| match ok(line) {
            Command::Eq(a) => a,
            other => panic!("{other:?}"),
        };
        assert_eq!(e("eq on"), EqArg::On);
        assert_eq!(e("eq off"), EqArg::Off);
        assert_eq!(e("eq toggle"), EqArg::Toggle);
        assert_eq!(e("eq reset"), EqArg::Reset);
        assert_eq!(e("eq preset rock"), EqArg::Preset("rock".into()));
        assert_eq!(e("eq Rock"), EqArg::Preset("rock".into()));
        assert_eq!(e("eq bass boost"), EqArg::Preset("bass-boost".into()));
        assert_eq!(e("equalizer preset hip_hop"), EqArg::Preset("hip-hop".into()));
        assert_eq!(e("eq band 3 +2.5"), EqArg::Band(3, 2.5));
        assert_eq!(e("eq band 10 -12"), EqArg::Band(10, -12.0));
        assert_eq!(e("eq band 1k 3db"), EqArg::Band(6, 3.0));
        assert_eq!(e("eq band 16kHz -1"), EqArg::Band(10, -1.0));
        assert_eq!(e("eq band 125 0"), EqArg::Band(3, 0.0));
        assert_eq!(e("eq preamp -3"), EqArg::Preamp(-3.0));
        assert!(err("eq").contains("expected on, off"));
        assert!(err("eq rok").contains("(did you mean \"rock\"?)"));
        assert!(err("eq preset").contains("flat"));
        assert!(err("eq band 11 3").contains("not a band"));
        assert!(err("eq band 3 20").contains("-12 to +12"));
        assert!(err("eq band 3").contains("e.g. eq band 3 +2.5"));
        assert!(err("eq band 3 loud").contains("not a gain"));
        assert!(err("eq preamp").contains("gain in dB"));
    }

    #[test]
    fn keyword_arguments() {
        let sort = |line: &str| match ok(line) {
            Command::Sort(k, d) => (k, d),
            other => panic!("{other:?}"),
        };
        assert_eq!(sort("sort title"), (SortKey::Title, None));
        assert_eq!(sort("sort plays desc"), (SortKey::Plays, Some(true)));
        assert_eq!(sort("sort artist asc"), (SortKey::Artist, Some(false)));
        assert_eq!(sort("sort date reverse"), (SortKey::Added, Some(true)));
        for (i, k) in SortKey::ALL.iter().enumerate() {
            assert_eq!(sort(&format!("sort {}", k.label())).0, SortKey::ALL[i]);
        }
        assert!(err("sort").contains("expected a key"));
        assert!(err("sort colour").contains("expected a key"));
        assert!(err("sort title sideways").contains("asc or desc"));

        for (name, mode) in BROWSE_NAMES.iter().zip(BrowseMode::ALL) {
            assert_eq!(ok(&format!("view {name}")), Command::View(mode));
        }
        assert!(err("view planets").contains("expected folders"));

        for (i, (name, t)) in TAB_NAMES.iter().zip(Tab::ALL).enumerate() {
            assert_eq!(ok(&format!("goto {name}")), Command::Goto(t));
            assert_eq!(ok(&format!("goto {}", i + 1)), Command::Goto(t));
        }
        assert_eq!(ok("goto now"), Command::Goto(Tab::NowPlaying));
        assert_eq!(ok("goto eq"), Command::Goto(Tab::Equalizer));
        assert!(err("goto mars").contains("expected library"));

        for m in VisMode::ALL {
            assert_eq!(ok(&format!("vis {}", m.label())), Command::Vis(Some(m)));
        }
        assert!(err("vis lasers").contains("expected bars"));
        assert!(err("repeat twice").contains("expected off, all or one"));
        assert!(err("mute maybe").contains("mute: expected on, off or toggle"));
        assert!(err("loop c").contains("expected a, b or clear"));
    }

    #[test]
    fn friendly_errors() {
        assert_eq!(err(""), "empty command");
        assert_eq!(err("volme 5"), "unknown command \"volme\" (did you mean \"volume\"?)");
        assert_eq!(err("shufle"), "unknown command \"shufle\" (did you mean \"shuffle\"?)");
        assert_eq!(err("xyzzy"), "unknown command \"xyzzy\"");
        assert_eq!(err("next 5"), "next: takes no arguments");
        assert!(err("theme nordd").contains("(did you mean \"nord\"?)"));
        assert!(err("playlist rename a b c").contains("quote names with spaces"));
        assert!(err("playlist rename \"\" b").contains("quote names"));
        assert!(err("playlist").contains("expected new, delete or rename"));
        assert!(err("playlist fly x").contains("unknown subcommand"));
        assert_eq!(err("save"), "save: expected a playlist name");
        assert_eq!(err("playlist new"), "playlist new: expected a playlist name");
        assert_eq!(err("add"), "add: expected one or more files or folders");
    }

    #[test]
    fn usage_follows_subcommands_and_aliases() {
        let usage = |line: &str| usage_for(line).map(|(u, _)| *u);
        assert_eq!(usage("playlist rename Chill"), Some("playlist rename <old> <new>"));
        assert_eq!(usage(":pl delete "), Some("playlist delete <name>"));
        assert_eq!(usage("playlist"), Some("playlist new <name>"));
        assert_eq!(usage("unmute"), Some("mute [on|off]"));
        assert_eq!(usage("vol 30"), Some("vol <n|+n|-n|reset>"));
        assert_eq!(usage("bogus"), None);
    }

    #[test]
    fn quoting() {
        assert_eq!(tokenize("a \"b c\" 'd e' f\\ g"), ["a", "b c", "d e", "f g"]);
        assert_eq!(tokenize("Don't stop"), ["Don't", "stop"]);
        assert_eq!(tokenize("\"say \\\"hi\\\"\""), ["say \"hi\""]);
        assert_eq!(tokenize("\"open ended"), ["open ended"]);
        assert_eq!(tokenize("  "), Vec::<String>::new());
        for s in ["plain", "two words", "it's", "a\"b", "back\\slash", ""] {
            assert_eq!(tokenize(&quote(s)), [s], "{s:?}");
        }
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn completes_commands_and_arguments() {
        assert_eq!(complete(""), command_names());
        assert_eq!(complete("rep"), ["repeat"]);
        assert_eq!(complete(":rep"), [":repeat"]);
        assert_eq!(complete("p"), ["play", "pause", "prev", "playlist"]);
        assert_eq!(complete("unm"), ["unmute"]);
        assert_eq!(complete("repeat "), ["repeat off", "repeat all", "repeat one"]);
        assert_eq!(complete("repeat o"), ["repeat off", "repeat one"]);
        assert_eq!(complete("REPEAT A"), ["REPEAT all"]);
        assert_eq!(complete("rep "), ["rep off", "rep all", "rep one"]);
        assert_eq!(complete("mute "), ["mute on", "mute off"]);
        assert_eq!(complete("shuffle "), ["shuffle on", "shuffle off"]);
        assert_eq!(complete("stopafter o"), ["stopafter on", "stopafter off"]);
        assert_eq!(complete("theme ").len(), theme::names().len());
        assert_eq!(complete("theme dr"), ["theme dracula"]);
        assert_eq!(complete("theme mocha"), ["theme catppuccin-mocha"]);
        let eq = complete("eq ");
        assert_eq!(&eq[..4], strs(&["eq on", "eq off", "eq toggle", "eq reset"]));
        assert!(eq.contains(&"eq rock".to_string()) && eq.contains(&"eq band".to_string()));
        assert_eq!(complete("eq p"), ["eq pop", "eq preset", "eq preamp"]);
        assert_eq!(complete("eq preset r"), ["eq preset rock"]);
        assert_eq!(complete("eq band ").len(), 10);
        assert_eq!(complete("vis "), ["vis bars", "vis mirror", "vis blocks", "vis wave", "vis vu", "vis cassette"]);
        assert_eq!(complete("sort ").len(), SortKey::ALL.len());
        assert_eq!(complete("sort pl"), ["sort plays"]);
        assert_eq!(complete("sort title "), ["sort title asc", "sort title desc"]);
        assert_eq!(complete("view a"), ["view artists", "view albums"]);
        assert_eq!(complete("goto "), ["goto library", "goto queue", "goto playlists", "goto now-playing", "goto equalizer"]);
        assert_eq!(complete("tab n"), ["tab now-playing"]);
        assert_eq!(complete("sleep e"), ["sleep end"]);
        assert_eq!(complete("loop "), ["loop a", "loop b", "loop clear"]);
        assert_eq!(complete("speed r"), ["speed reset"]);
        assert_eq!(complete("lyrics "), ["lyrics offset"]);
        assert_eq!(complete("lyrics offset r"), ["lyrics offset reset"]);
        assert_eq!(complete("playlist "), ["playlist new", "playlist delete", "playlist rename"]);
        assert!(complete("vol ").is_empty());
        assert!(complete("unmute ").is_empty());
        assert!(complete("nonsense ").is_empty());
        // every completion parses (or only lacks a further argument)
        for line in complete("eq ").into_iter().chain(complete("theme ")).chain(complete("goto ")).chain(complete("sort ")) {
            if !["eq preset", "eq band", "eq preamp"].contains(&line.as_str()) {
                ok(&line);
            }
        }
    }

    #[test]
    fn completes_playlist_names() {
        let names = strs(&["Chill", "My Mix", "Morning"]);
        let c = |line: &str| complete_with(line, &names);
        assert_eq!(c("load "), ["load Chill", "load \"My Mix\"", "load Morning"]);
        assert_eq!(c("load m"), ["load \"My Mix\"", "load Morning"]);
        assert_eq!(c("load My M"), ["load \"My Mix\""]);
        assert_eq!(c("load \"my"), ["load \"My Mix\""]);
        assert_eq!(c("load mix"), ["load \"My Mix\""]);
        assert_eq!(c("save C"), ["save Chill"]);
        assert_eq!(c("playlist delete "), ["playlist delete Chill", "playlist delete \"My Mix\"", "playlist delete Morning"]);
        assert_eq!(c("pl rm ch"), ["pl rm Chill"]);
        assert_eq!(c("playlist rename "), ["playlist rename Chill", "playlist rename \"My Mix\"", "playlist rename Morning"]);
        assert_eq!(c("playlist rename \"My"), ["playlist rename \"My Mix\""]);
        assert!(c("playlist rename Chill ").is_empty());
        assert!(complete("load ").is_empty());
        // the completed lines parse back to the name
        assert_eq!(ok(&c("load My M")[0]), Command::Load("My Mix".into()));
        assert_eq!(ok(&format!("{} New", c("playlist rename \"My")[0])), Command::PlaylistRename("My Mix".into(), "New".into()));
    }

    #[test]
    fn completes_paths() {
        let dir = std::env::temp_dir().join(format!("orbit-complete-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub dir")).unwrap();
        for f in ["a song.mp3", "b.FLAC", "cover.jpg", ".hidden.mp3"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        std::fs::write(dir.join("sub dir/c.mp3"), b"").unwrap();
        let d = format!("{}/", dir.display());
        // a folder's quote stays open, so completion goes on inside it
        let sub = format!("add \"{d}sub dir/");
        assert_eq!(complete(&format!("add {d}")), [format!("add {}", quote(&format!("{d}a song.mp3"))), format!("add {d}b.FLAC"), sub.clone()]);
        assert_eq!(complete(&format!("{sub}c")), [format!("add {}", quote(&format!("{d}sub dir/c.mp3")))]);
        assert_eq!(tokenize(&sub), ["add", &format!("{d}sub dir/")]);
        assert_eq!(complete(&format!("add x.mp3 {d}s")), [format!("add x.mp3 \"{d}sub dir/")]);
        assert_eq!(complete(&format!("add {d}.h")), [format!("add {d}.hidden.mp3")]);
        assert_eq!(complete(&format!("play {d}su")), [format!("play {d}sub dir/")]);
        assert!(complete("play some song").is_empty());
        assert_eq!(complete("add ~"), ["add ~/"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
