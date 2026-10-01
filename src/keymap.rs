//! Actions, key parsing, and the remappable keymap.
//!
//! A binding maps a key to either an [`Action`] (context-sensitive UI verb) or a `:command` string
//! (anything the command palette accepts, see `command.rs`). Users override bindings in `[keys]`:
//! `"ctrl+n" = "next"`, `"F1" = ":vol 30"`, `"x" = "none"` (unbind).

use std::collections::{BTreeMap, HashMap};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

macro_rules! actions {
    ($($variant:ident => $name:literal, $cat:literal, $desc:literal;)*) => {
        /// Every bindable action. Names are kebab-case and used in the config file.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Action { $($variant),* }

        impl Action {
            pub const ALL: &'static [Action] = &[$(Action::$variant),*];
            pub fn name(self) -> &'static str { match self { $(Action::$variant => $name),* } }
            pub fn category(self) -> &'static str { match self { $(Action::$variant => $cat),* } }
            pub fn description(self) -> &'static str { match self { $(Action::$variant => $desc),* } }
            pub fn from_name(s: &str) -> Option<Action> {
                let s = s.trim().to_ascii_lowercase().replace('_', "-");
                Self::ALL.iter().copied().find(|a| a.name() == s)
            }
        }
    };
}

actions! {
    Quit => "quit", "General", "Quit orbit";
    Help => "help", "General", "Show keybindings and commands";
    CommandPalette => "command-palette", "General", "Open the command line (:)";
    Search => "search", "General", "Fuzzy-search the whole library";
    ReloadConfig => "reload-config", "General", "Reload config.toml (theme, keys, options)";
    Redraw => "redraw", "General", "Force a full redraw";

    Up => "up", "Navigation", "Move up / EQ: raise band";
    Down => "down", "Navigation", "Move down / EQ: lower band";
    Left => "left", "Navigation", "Previous pane / EQ: previous band";
    Right => "right", "Navigation", "Next pane / EQ: next band";
    PageUp => "page-up", "Navigation", "Page up";
    PageDown => "page-down", "Navigation", "Page down";
    Top => "top", "Navigation", "Jump to top";
    Bottom => "bottom", "Navigation", "Jump to bottom";
    Select => "select", "Navigation", "Play / open the selected item";
    Back => "back", "Navigation", "Close overlay / go back a pane";
    FocusNext => "focus-next", "Navigation", "Focus next pane";
    FocusPrev => "focus-prev", "Navigation", "Focus previous pane";
    NextTab => "next-tab", "Navigation", "Next tab";
    PrevTab => "prev-tab", "Navigation", "Previous tab";
    TabLibrary => "tab-library", "Navigation", "Go to Library";
    TabQueue => "tab-queue", "Navigation", "Go to Queue";
    TabPlaylists => "tab-playlists", "Navigation", "Go to Playlists";
    TabNowPlaying => "tab-now-playing", "Navigation", "Go to Now Playing";
    TabEqualizer => "tab-equalizer", "Navigation", "Go to Equalizer";
    JumpToCurrent => "jump-to-current", "Navigation", "Select the playing track";

    TogglePause => "toggle-pause", "Playback", "Play / pause";
    Stop => "stop", "Playback", "Stop";
    Next => "next", "Playback", "Next track";
    Prev => "prev", "Playback", "Previous track (restarts if past a few seconds)";
    SeekForward => "seek-forward", "Playback", "Seek forward (small step)";
    SeekBackward => "seek-backward", "Playback", "Seek backward (small step)";
    SeekForwardLarge => "seek-forward-large", "Playback", "Seek forward (large step)";
    SeekBackwardLarge => "seek-backward-large", "Playback", "Seek backward (large step)";
    VolumeUp => "volume-up", "Playback", "Volume up";
    VolumeDown => "volume-down", "Playback", "Volume down";
    Mute => "mute", "Playback", "Mute / unmute";
    SpeedUp => "speed-up", "Playback", "Faster playback";
    SpeedDown => "speed-down", "Playback", "Slower playback";
    SpeedReset => "speed-reset", "Playback", "Normal speed";
    ToggleShuffle => "toggle-shuffle", "Playback", "Shuffle on / off";
    CycleRepeat => "cycle-repeat", "Playback", "Repeat: off / all / one";
    ToggleStopAfter => "toggle-stop-after", "Playback", "Stop after the current track";
    AbLoop => "ab-loop", "Playback", "A-B loop: set A, set B, clear";
    SleepTimer => "sleep-timer", "Playback", "Sleep timer: 15/30/45/60/90 min, end of track, off";

    Enqueue => "enqueue", "Queue", "Add selection to the end of the queue";
    PlayNext => "play-next", "Queue", "Play selection right after the current track";
    EnqueueAll => "enqueue-all", "Queue", "Add the whole current list to the queue";
    Remove => "remove", "Queue", "Remove selection (queue / playlist) or delete playlist";
    MoveUp => "move-up", "Queue", "Move selection up (queue / playlist)";
    MoveDown => "move-down", "Queue", "Move selection down (queue / playlist)";
    ClearQueue => "clear-queue", "Queue", "Clear the queue";
    ShuffleQueue => "shuffle-queue", "Queue", "Shuffle the queue order in place";
    Radio => "radio", "Queue", "Play the selection, then what sounds most like it";

    ToggleFavorite => "toggle-favorite", "Library", "Favorite / unfavorite the selection";
    AddToPlaylist => "add-to-playlist", "Library", "Add selection to a playlist";
    NewPlaylist => "new-playlist", "Library", "Create a playlist";
    RenamePlaylist => "rename-playlist", "Library", "Rename the selected playlist";
    SaveQueue => "save-queue", "Library", "Save the queue as a playlist";
    CycleSort => "cycle-sort", "Library", "Change sort order";
    ReverseSort => "reverse-sort", "Library", "Reverse sort order";
    CycleBrowseMode => "cycle-browse-mode", "Library", "Browse by folders / artists / albums / genres / years / tracks";
    Rescan => "rescan", "Library", "Rescan the music folders";
    Download => "download", "Library", "Download audio from a URL (needs yt-dlp)";

    CycleVisualizer => "cycle-visualizer", "View", "Next visualizer style";
    ToggleLyrics => "toggle-lyrics", "View", "Show / hide lyrics";
    ToggleArt => "toggle-art", "View", "Show / hide album art";
    LyricsOffsetUp => "lyrics-offset-up", "View", "Lyrics later (+250 ms)";
    LyricsOffsetDown => "lyrics-offset-down", "View", "Lyrics earlier (-250 ms)";
    CycleTheme => "cycle-theme", "View", "Next color theme";
    ToggleTimeDisplay => "toggle-time-display", "View", "Elapsed / remaining time";
    ToggleCompact => "toggle-compact", "View", "Compact layout";
    ToggleMiniVisualizer => "toggle-mini-visualizer", "View", "Mini visualizer in the player bar";

    EqToggle => "eq-toggle", "Equalizer", "Equalizer on / off";
    EqNextPreset => "eq-next-preset", "Equalizer", "Next EQ preset";
    EqPrevPreset => "eq-prev-preset", "Equalizer", "Previous EQ preset";
    EqReset => "eq-reset", "Equalizer", "Flatten the EQ";
}

/// Default bindings: (key, action name). Several keys may map to one action.
pub const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("ctrl+c", "quit"),
    ("?", "help"),
    (":", "command-palette"),
    ("/", "search"),
    ("ctrl+r", "reload-config"),
    ("ctrl+l", "redraw"),
    ("k", "up"),
    ("up", "up"),
    ("j", "down"),
    ("down", "down"),
    ("h", "left"),
    ("left", "left"),
    ("l", "right"),
    ("right", "right"),
    ("ctrl+u", "page-up"),
    ("pageup", "page-up"),
    ("ctrl+d", "page-down"),
    ("pagedown", "page-down"),
    ("g", "top"),
    ("home", "top"),
    ("G", "bottom"),
    ("end", "bottom"),
    ("enter", "select"),
    ("esc", "back"),
    ("backspace", "back"),
    ("tab", "focus-next"),
    ("backtab", "focus-prev"),
    ("]", "next-tab"),
    ("[", "prev-tab"),
    ("1", "tab-library"),
    ("2", "tab-queue"),
    ("3", "tab-playlists"),
    ("4", "tab-now-playing"),
    ("5", "tab-equalizer"),
    ("o", "jump-to-current"),
    ("space", "toggle-pause"),
    ("S", "stop"),
    ("n", "next"),
    ("p", "prev"),
    (".", "seek-forward"),
    ("shift+right", "seek-forward"),
    (",", "seek-backward"),
    ("shift+left", "seek-backward"),
    (">", "seek-forward-large"),
    ("<", "seek-backward-large"),
    ("+", "volume-up"),
    ("=", "volume-up"),
    ("-", "volume-down"),
    ("m", "mute"),
    ("}", "speed-up"),
    ("{", "speed-down"),
    ("\\", "speed-reset"),
    ("s", "toggle-shuffle"),
    ("r", "cycle-repeat"),
    ("x", "toggle-stop-after"),
    ("B", "ab-loop"),
    ("Z", "sleep-timer"),
    ("a", "enqueue"),
    ("i", "play-next"),
    ("A", "enqueue-all"),
    ("d", "remove"),
    ("delete", "remove"),
    ("K", "move-up"),
    ("shift+up", "move-up"),
    ("J", "move-down"),
    ("shift+down", "move-down"),
    ("D", "clear-queue"),
    ("X", "shuffle-queue"),
    ("M", "radio"),
    ("f", "toggle-favorite"),
    ("P", "add-to-playlist"),
    ("N", "new-playlist"),
    ("R", "rename-playlist"),
    ("W", "save-queue"),
    ("t", "cycle-sort"),
    ("T", "reverse-sort"),
    ("b", "cycle-browse-mode"),
    ("U", "rescan"),
    ("w", "download"),
    ("v", "cycle-visualizer"),
    ("y", "toggle-lyrics"),
    ("c", "toggle-art"),
    (")", "lyrics-offset-up"),
    ("(", "lyrics-offset-down"),
    ("ctrl+t", "cycle-theme"),
    ("ctrl+e", "toggle-time-display"),
    ("ctrl+b", "toggle-compact"),
    ("ctrl+v", "toggle-mini-visualizer"),
    ("E", "eq-toggle"),
    ("ctrl+n", "eq-next-preset"),
    ("ctrl+p", "eq-prev-preset"),
];

/// What a key does.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    Action(Action),
    /// A command-palette line without the leading ':' (e.g. "vol 30").
    Command(String),
}

/// A normalized key: for printable characters the SHIFT modifier is folded into the character
/// ("G", not "shift+g"); for everything else SHIFT is kept ("shift+left"). shift+tab is BackTab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Key {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Key {
        let mut mods = mods & (KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT);
        let code = match code {
            KeyCode::Char(c) => {
                mods.remove(KeyModifiers::SHIFT);
                // ctrl+letter arrives lowercase from most terminals; normalize so "ctrl+D" == "ctrl+d"
                if mods.contains(KeyModifiers::CONTROL) { KeyCode::Char(c.to_ascii_lowercase()) } else { KeyCode::Char(c) }
            }
            // terminals report shift+tab as BackTab (usually with SHIFT set); "shift+tab" in the config
            // must mean the same key
            KeyCode::BackTab => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            KeyCode::Tab if mods.contains(KeyModifiers::SHIFT) => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            other => other,
        };
        Key { code, mods }
    }

    pub fn from_event(e: &KeyEvent) -> Key {
        Key::new(e.code, e.modifiers)
    }

    /// Parse "ctrl+shift+left", "space", "G", "F5", "alt+enter", "+" ...
    pub fn parse(s: &str) -> Option<Key> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // a lone "+" or a trailing "++" means the plus key itself
        let (mods_part, key_part) = if s == "+" {
            ("", "+")
        } else if let Some(stripped) = s.strip_suffix("++") {
            (stripped, "+")
        } else {
            match s.rfind('+') {
                Some(i) => (&s[..i], &s[i + 1..]),
                None => ("", s),
            }
        };
        let mut mods = KeyModifiers::NONE;
        for m in mods_part.split('+').filter(|m| !m.is_empty()) {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "c" => mods |= KeyModifiers::CONTROL,
                "shift" | "s" => mods |= KeyModifiers::SHIFT,
                "alt" | "meta" | "option" | "opt" | "m" | "a" => mods |= KeyModifiers::ALT,
                _ => return None,
            }
        }
        let lower = key_part.to_ascii_lowercase();
        let code = match lower.as_str() {
            "space" | "spc" => KeyCode::Char(' '),
            "enter" | "return" | "ret" | "cr" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" | "bs" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
            // the arrows also parse from the glyphs `display` prints
            "up" | "↑" => KeyCode::Up,
            "down" | "↓" => KeyCode::Down,
            "left" | "←" => KeyCode::Left,
            "right" | "→" => KeyCode::Right,
            "plus" => KeyCode::Char('+'),
            "minus" => KeyCode::Char('-'),
            "comma" => KeyCode::Char(','),
            "period" | "dot" => KeyCode::Char('.'),
            "slash" => KeyCode::Char('/'),
            "backslash" => KeyCode::Char('\\'),
            "colon" => KeyCode::Char(':'),
            "semicolon" => KeyCode::Char(';'),
            f if f.len() >= 2 && f.starts_with('f') && f[1..].chars().all(|c| c.is_ascii_digit()) => {
                KeyCode::F(f[1..].parse().ok().filter(|n: &u8| (1..=24).contains(n))?)
            }
            _ => {
                let mut chars = key_part.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                // "shift+g" means "G"
                let c = if mods.contains(KeyModifiers::SHIFT) { c.to_ascii_uppercase() } else { c };
                KeyCode::Char(c)
            }
        };
        Some(Key::new(code, mods))
    }

    /// Human-readable form used in the help screen, e.g. "ctrl+d", "G", "space", "shift+left".
    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            out.push_str("ctrl+");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            out.push_str("alt+");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            out.push_str("shift+");
        }
        match self.code {
            KeyCode::Char(' ') => out.push_str("space"),
            KeyCode::Char(c) => out.push(c),
            KeyCode::Enter => out.push_str("enter"),
            KeyCode::Esc => out.push_str("esc"),
            KeyCode::Tab => out.push_str("tab"),
            KeyCode::BackTab => out.push_str("shift+tab"),
            KeyCode::Backspace => out.push_str("backspace"),
            KeyCode::Delete => out.push_str("delete"),
            KeyCode::Insert => out.push_str("insert"),
            KeyCode::Home => out.push_str("home"),
            KeyCode::End => out.push_str("end"),
            KeyCode::PageUp => out.push_str("pageup"),
            KeyCode::PageDown => out.push_str("pagedown"),
            KeyCode::Up => out.push('↑'),
            KeyCode::Down => out.push('↓'),
            KeyCode::Left => out.push('←'),
            KeyCode::Right => out.push('→'),
            KeyCode::F(n) => out.push_str(&format!("F{n}")),
            other => out.push_str(&format!("{other:?}").to_lowercase()),
        }
        out
    }
}

/// The active keymap: defaults with the user's `[keys]` overrides applied.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    map: HashMap<Key, Binding>,
}

impl Keymap {
    /// Build defaults + overrides. Returns warnings for unparsable keys, unknown actions and
    /// `:commands` that don't parse (those entries are skipped; the rest still apply). A value that is
    /// not an action name but a valid command ("vol 30") is bound as that command.
    pub fn new(overrides: &BTreeMap<String, String>) -> (Keymap, Vec<String>) {
        let mut map = HashMap::new();
        let mut warnings = Vec::new();
        for (key, action) in DEFAULT_BINDINGS {
            let k = Key::parse(key).unwrap_or_else(|| panic!("bad default key {key}"));
            let a = Action::from_name(action).unwrap_or_else(|| panic!("bad default action {action}"));
            map.insert(k, Binding::Action(a));
        }
        for (key, value) in overrides {
            let Some(k) = Key::parse(key) else {
                warnings.push(format!("[keys] can't parse key \"{key}\""));
                continue;
            };
            let v = value.trim();
            // the main loop quits on ctrl+c before looking at the keymap
            if k == Key::new(KeyCode::Char('c'), KeyModifiers::CONTROL) && !v.eq_ignore_ascii_case("quit") {
                warnings.push(format!("[keys] \"{key}\": ctrl+c always quits"));
                continue;
            }
            if v.eq_ignore_ascii_case("none") || v.is_empty() {
                map.remove(&k);
            } else if let Some(cmd) = v.strip_prefix(':') {
                match crate::command::parse(cmd) {
                    Ok(_) => {
                        map.insert(k, Binding::Command(cmd.trim().to_string()));
                    }
                    Err(e) => warnings.push(format!("[keys] \"{key}\": {e}")),
                }
            } else if let Some(a) = Action::from_name(v) {
                map.insert(k, Binding::Action(a));
            } else if crate::command::parse(v).is_ok() {
                map.insert(k, Binding::Command(v.to_string()));
            } else {
                let normalized = v.to_ascii_lowercase().replace('_', "-");
                let hint = crate::config::did_you_mean(&normalized, Action::ALL.iter().map(|a| a.name()))
                    .map_or_else(|| "use an action name or \":command\"".to_string(), |a| format!("did you mean \"{a}\"?"));
                warnings.push(format!("[keys] \"{key}\": unknown action \"{v}\" ({hint})"));
            }
        }
        (Keymap { map }, warnings)
    }

    /// Binding for a key press (ignores key-release events).
    pub fn get(&self, e: &KeyEvent) -> Option<&Binding> {
        if e.kind == KeyEventKind::Release {
            return None;
        }
        self.map.get(&Key::from_event(e))
    }

    /// Display strings of every key bound to `a`, shortest first.
    pub fn keys_for(&self, a: Action) -> Vec<String> {
        let mut keys: Vec<String> =
            self.map.iter().filter(|(_, b)| **b == Binding::Action(a)).map(|(k, _)| k.display()).collect();
        keys.sort_by_key(|k| (k.chars().count(), k.clone()));
        keys
    }

    /// Keys bound to `:commands`, for the help screen.
    pub fn command_bindings(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .map
            .iter()
            .filter_map(|(k, b)| match b {
                Binding::Command(c) => Some((k.display(), c.clone())),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_resolve() {
        let (km, warnings) = Keymap::new(&BTreeMap::new());
        assert!(warnings.is_empty());
        let press = |code, mods| KeyEvent::new(code, mods);
        assert_eq!(km.get(&press(KeyCode::Char('q'), KeyModifiers::NONE)), Some(&Binding::Action(Action::Quit)));
        // terminals send shift+G as 'G' with SHIFT set
        assert_eq!(km.get(&press(KeyCode::Char('G'), KeyModifiers::SHIFT)), Some(&Binding::Action(Action::Bottom)));
        assert_eq!(km.get(&press(KeyCode::Char('d'), KeyModifiers::CONTROL)), Some(&Binding::Action(Action::PageDown)));
        assert_eq!(km.get(&press(KeyCode::Right, KeyModifiers::SHIFT)), Some(&Binding::Action(Action::SeekForward)));
        assert_eq!(km.get(&press(KeyCode::BackTab, KeyModifiers::SHIFT)), Some(&Binding::Action(Action::FocusPrev)));
    }

    #[test]
    fn overrides_commands_and_unbind() {
        let mut o = BTreeMap::new();
        o.insert("F1".to_string(), ":vol 30".to_string());
        o.insert("q".to_string(), "none".to_string());
        o.insert("ctrl+q".to_string(), "quit".to_string());
        o.insert("hyper+x".to_string(), "quit".to_string());
        o.insert("w".to_string(), "fly".to_string());
        let (km, warnings) = Keymap::new(&o);
        assert_eq!(warnings.len(), 2);
        assert_eq!(km.get(&KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)), Some(&Binding::Command("vol 30".into())));
        assert_eq!(km.get(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)), None);
        assert_eq!(km.get(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)), Some(&Binding::Action(Action::Quit)));
    }

    #[test]
    fn key_parse_roundtrip() {
        let extra = ["ctrl+shift+left", "alt+G", "ctrl+space", "F24", "shift+tab", "ctrl+alt+x", "↑"];
        for s in DEFAULT_BINDINGS.iter().map(|(k, _)| *k).chain(extra) {
            let k = Key::parse(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!(Key::parse(&k.display()), Some(k), "{s} -> {}", k.display());
        }
        assert_eq!(Key::parse("shift+g"), Key::parse("G"));
        assert_eq!(Key::parse("Ctrl+D"), Key::parse("ctrl+d"));
        assert_eq!(Key::parse("shift+tab"), Key::parse("backtab"));
        assert_eq!(Key::parse("←"), Key::parse("left"));
        for bad in ["nope", "", "ctrl+", "hyper+x", "F25", "F0"] {
            assert_eq!(Key::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn terminal_events_match_config_names() {
        let (km, _) = Keymap::new(&BTreeMap::from([("shift+tab".to_string(), "next-tab".to_string())]));
        // crossterm reports shift+tab as BackTab with SHIFT
        assert_eq!(km.get(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)), Some(&Binding::Action(Action::NextTab)));
        // ...and some terminals as Tab with SHIFT
        assert_eq!(km.get(&KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)), Some(&Binding::Action(Action::NextTab)));
        assert_eq!(km.get(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(&Binding::Action(Action::Quit)));
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(km.get(&release), None);
    }

    #[test]
    fn commands_are_checked_and_bare_commands_work() {
        let o = BTreeMap::from([
            ("F2".to_string(), "vol 30".to_string()),
            ("F3".to_string(), ":vol loud".to_string()),
            ("F4".to_string(), "toggle_lyrics".to_string()),
            ("F5".to_string(), "toggle-lyric".to_string()),
            ("n".to_string(), ":nxt".to_string()),
        ]);
        let (km, warnings) = Keymap::new(&o);
        let press = |code| km.get(&KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(press(KeyCode::F(2)), Some(&Binding::Command("vol 30".into())));
        assert_eq!(press(KeyCode::F(3)), None);
        assert_eq!(press(KeyCode::F(4)), Some(&Binding::Action(Action::ToggleLyrics)));
        // a rejected override leaves the default binding alone
        assert_eq!(press(KeyCode::Char('n')), Some(&Binding::Action(Action::Next)));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"F3\": vol: expected a number")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("(did you mean \"toggle-lyrics\"?)")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"n\": unknown command \"nxt\"")), "{warnings:?}");
    }

    #[test]
    fn default_keys_are_unique_and_documented_actions_exist() {
        let mut keys: Vec<Key> = DEFAULT_BINDINGS.iter().map(|(k, _)| Key::parse(k).unwrap()).collect();
        let n = keys.len();
        keys.sort_by_key(|k| k.display());
        keys.dedup();
        assert_eq!(keys.len(), n, "a key is bound twice in DEFAULT_BINDINGS");
        let (km, _) = Keymap::new(&BTreeMap::new());
        assert_eq!(km.keys_for(Action::Quit), ["q", "ctrl+c"]);
        assert!(km.command_bindings().is_empty());
    }

    #[test]
    fn every_action_named_uniquely() {
        let mut names: Vec<_> = Action::ALL.iter().map(|a| a.name()).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
    }
}
