//! Color themes (built-in presets + [colors] overrides), icon sets and border styles.

use std::collections::BTreeMap;

use ratatui::style::Color;
use ratatui::widgets::BorderType;

use crate::config::{BorderStyle, IconSet, did_you_mean};

/// Every color slot the UI uses. Override any of them in `[colors]` by field name
/// (e.g. `accent = "#ff79c6"`, `sel_bg = "darkgray"`, `gradient = ["#00ff00", "#ffff00", "#ff0000"]` as a
/// comma-separated string `"#00ff00,#ffff00,#ff0000"`).
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    /// Background of the whole app (Color::Reset = terminal background).
    pub bg: Color,
    pub fg: Color,
    /// Secondary text: durations, hints, inactive items.
    pub dim: Color,
    /// Primary accent: focused borders, active tab, progress.
    pub accent: Color,
    /// Secondary accent: artist names, icons.
    pub accent2: Color,
    pub border: Color,
    pub border_focus: Color,
    pub title: Color,
    /// Selected row.
    pub sel_bg: Color,
    pub sel_fg: Color,
    /// The row of the track that is playing.
    pub playing: Color,
    pub progress: Color,
    pub progress_bg: Color,
    pub error: Color,
    pub warn: Color,
    pub ok: Color,
    pub lyric_active: Color,
    pub lyric: Color,
    /// Visualizer colors from low to high (at least two).
    pub gradient: Vec<Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            name: "default".into(),
            bg: Color::Reset,
            fg: Color::Reset,
            dim: Color::DarkGray,
            accent: Color::Cyan,
            accent2: Color::Magenta,
            border: Color::DarkGray,
            border_focus: Color::Cyan,
            title: Color::White,
            sel_bg: Color::Cyan,
            sel_fg: Color::Black,
            playing: Color::Green,
            progress: Color::Cyan,
            progress_bg: Color::DarkGray,
            error: Color::Red,
            warn: Color::Yellow,
            ok: Color::Green,
            lyric_active: Color::White,
            lyric: Color::DarkGray,
            gradient: vec![Color::Blue, Color::Cyan, Color::Green, Color::Yellow, Color::Red],
        }
    }
}

/// Names accepted in `[colors]` (every Theme field except `name`).
pub const SLOTS: &[&str] = &[
    "bg",
    "fg",
    "dim",
    "accent",
    "accent2",
    "border",
    "border_focus",
    "title",
    "sel_bg",
    "sel_fg",
    "playing",
    "progress",
    "progress_bg",
    "error",
    "warn",
    "ok",
    "lyric_active",
    "lyric",
    "gradient",
];

impl Theme {
    /// Built-in preset `name` (falls back to "default") with `overrides` applied.
    /// Returns warnings for unknown theme names, unknown slots and unparsable colors.
    pub fn from_config(name: &str, overrides: &BTreeMap<String, String>) -> (Theme, Vec<String>) {
        let mut warnings = Vec::new();
        let mut theme = builtin(name).unwrap_or_else(|| {
            let hint = did_you_mean(name, names()).map(|n| format!(" (did you mean \"{n}\"?)")).unwrap_or_default();
            warnings.push(format!("unknown theme \"{name}\"{hint}, using default"));
            Theme::default()
        });
        for (slot, value) in overrides {
            let key = slot.trim().to_ascii_lowercase().replace('-', "_");
            if key == "gradient" {
                match split_colors(value).into_iter().map(|s| parse_color(s).ok_or(s)).collect::<Result<Vec<Color>, &str>>() {
                    Ok(colors) if colors.len() == 1 => theme.gradient = vec![colors[0]; 2],
                    Ok(colors) if !colors.is_empty() => theme.gradient = colors,
                    Ok(_) => warnings.push("[colors] gradient: expected colors separated by commas".into()),
                    Err(bad) => warnings.push(format!("[colors] gradient: can't parse color \"{bad}\"")),
                }
                continue;
            }
            let Some(target) = theme.slot_mut(&key) else {
                let hint = did_you_mean(&key, SLOTS.iter().copied()).map(|s| format!(" (did you mean \"{s}\"?)")).unwrap_or_default();
                warnings.push(format!("[colors] unknown slot \"{slot}\"{hint}"));
                continue;
            };
            match parse_color(value) {
                Some(c) => *target = c,
                None => warnings.push(format!(
                    "[colors] {slot}: can't parse color \"{value}\" (use \"#rrggbb\", \"#rgb\", \"rgb(r, g, b)\", a color name or 0-255)"
                )),
            }
        }
        (theme, warnings)
    }

    /// Color at position `t` (0..=1) along the gradient, interpolated in RGB where possible.
    /// Named/indexed colors are interpolated through their usual xterm RGB values; a stop that is
    /// `Reset` can't be blended, so the nearest stop is used. Exact stops return the stop itself.
    pub fn gradient_at(&self, t: f32) -> Color {
        let stops = &self.gradient;
        match stops.len() {
            0 => return self.accent,
            1 => return stops[0],
            _ => {}
        }
        let x = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) } * (stops.len() - 1) as f32;
        let i = (x as usize).min(stops.len() - 2);
        let f = x - i as f32;
        let (a, b) = (stops[i], stops[i + 1]);
        match (to_rgb(a), to_rgb(b)) {
            _ if f < 1e-3 => a,
            _ if f > 1.0 - 1e-3 => b,
            (Some(p), Some(q)) => Color::Rgb(lerp(p.0, q.0, f), lerp(p.1, q.1, f), lerp(p.2, q.2, f)),
            _ if f < 0.5 => a,
            _ => b,
        }
    }

    fn slot_mut(&mut self, slot: &str) -> Option<&mut Color> {
        Some(match slot {
            "bg" => &mut self.bg,
            "fg" => &mut self.fg,
            "dim" => &mut self.dim,
            "accent" => &mut self.accent,
            "accent2" => &mut self.accent2,
            "border" => &mut self.border,
            "border_focus" => &mut self.border_focus,
            "title" => &mut self.title,
            "sel_bg" => &mut self.sel_bg,
            "sel_fg" => &mut self.sel_fg,
            "playing" => &mut self.playing,
            "progress" => &mut self.progress,
            "progress_bg" => &mut self.progress_bg,
            "error" => &mut self.error,
            "warn" => &mut self.warn,
            "ok" => &mut self.ok,
            "lyric_active" => &mut self.lyric_active,
            "lyric" => &mut self.lyric,
            _ => return None,
        })
    }
}

/// Split a color list on the commas outside parentheses: "rgb(1, 2, 3), #fff" -> ["rgb(1, 2, 3)", "#fff"].
fn split_colors(list: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0i32, 0);
    for (i, c) in list.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(list[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(list[start..].trim());
    out.retain(|s| !s.is_empty());
    out
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Approximate RGB of any color (xterm's defaults for the 16 ANSI colors); None for Reset.
fn to_rgb(c: Color) -> Option<(u8, u8, u8)> {
    Some(match c {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black | Color::Indexed(0) => (0, 0, 0),
        Color::Red | Color::Indexed(1) => (205, 0, 0),
        Color::Green | Color::Indexed(2) => (0, 205, 0),
        Color::Yellow | Color::Indexed(3) => (205, 205, 0),
        Color::Blue | Color::Indexed(4) => (0, 0, 238),
        Color::Magenta | Color::Indexed(5) => (205, 0, 205),
        Color::Cyan | Color::Indexed(6) => (0, 205, 205),
        Color::Gray | Color::Indexed(7) => (229, 229, 229),
        Color::DarkGray | Color::Indexed(8) => (127, 127, 127),
        Color::LightRed | Color::Indexed(9) => (255, 0, 0),
        Color::LightGreen | Color::Indexed(10) => (0, 255, 0),
        Color::LightYellow | Color::Indexed(11) => (255, 255, 0),
        Color::LightBlue | Color::Indexed(12) => (92, 92, 255),
        Color::LightMagenta | Color::Indexed(13) => (255, 0, 255),
        Color::LightCyan | Color::Indexed(14) => (0, 255, 255),
        Color::White | Color::Indexed(15) => (255, 255, 255),
        Color::Indexed(i @ 16..=231) => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            let i = i - 16;
            (level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        Color::Indexed(i) => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    })
}

/// Defines the truecolor presets. The struct literal makes a missing or misspelled slot a compile error.
macro_rules! presets {
    ($($name:literal => { $($slot:ident: $hex:literal),+ $(,)? ; gradient: [$($stop:literal),+ $(,)?] })+) => {
        /// Truecolor presets in cycling order (after "default").
        const PRESET_NAMES: &[&str] = &[$($name),+];

        fn preset(name: &str) -> Option<Theme> {
            match name {
                $($name => Some(Theme {
                    name: $name.into(),
                    $($slot: Color::from_u32($hex),)+
                    gradient: vec![$(Color::from_u32($stop)),+],
                }),)+
                _ => None,
            }
        }
    };
}

presets! {
    // deep space, cyan -> violet -> magenta
    "orbit" => {
        bg: 0x0e0e16, fg: 0xd0d3e1, dim: 0x6b6e88, accent: 0x7adeeb, accent2: 0xff6ac1,
        border: 0x2e3046, border_focus: 0x7adeeb, title: 0xa78bfa, sel_bg: 0x7adeeb, sel_fg: 0x0e0e16,
        playing: 0x7ee787, progress: 0x7adeeb, progress_bg: 0x222337, error: 0xff6b6b, warn: 0xf7c948,
        ok: 0x7ee787, lyric_active: 0xff6ac1, lyric: 0x6b6e88;
        gradient: [0x7adeeb, 0xa78bfa, 0xff6ac1]
    }
    // warm dark, amber and rose
    "ember" => {
        bg: 0x16100e, fg: 0xe8dacd, dim: 0x8c7062, accent: 0xf59e58, accent2: 0xf06e78,
        border: 0x4e3a30, border_focus: 0xf59e58, title: 0xf7c948, sel_bg: 0xf59e58, sel_fg: 0x16100e,
        playing: 0xb4c878, progress: 0xf59e58, progress_bg: 0x30221c, error: 0xf0645a, warn: 0xf7c948,
        ok: 0xb4c878, lyric_active: 0xf7c948, lyric: 0x8c7062;
        gradient: [0xf59e58, 0xd68ca0, 0xf06e78]
    }
    "catppuccin-mocha" => {
        bg: 0x1e1e2e, fg: 0xcdd6f4, dim: 0x7f849c, accent: 0xcba6f7, accent2: 0x89b4fa,
        border: 0x45475a, border_focus: 0xcba6f7, title: 0xb4befe, sel_bg: 0xcba6f7, sel_fg: 0x1e1e2e,
        playing: 0xa6e3a1, progress: 0xcba6f7, progress_bg: 0x313244, error: 0xf38ba8, warn: 0xf9e2af,
        ok: 0xa6e3a1, lyric_active: 0xf5c2e7, lyric: 0x7f849c;
        gradient: [0x89b4fa, 0x74c7ec, 0x94e2d5, 0xa6e3a1, 0xf9e2af, 0xf38ba8]
    }
    "catppuccin-macchiato" => {
        bg: 0x24273a, fg: 0xcad3f5, dim: 0x8087a2, accent: 0xc6a0f6, accent2: 0x8aadf4,
        border: 0x494d64, border_focus: 0xc6a0f6, title: 0xb7bdf8, sel_bg: 0xc6a0f6, sel_fg: 0x24273a,
        playing: 0xa6da95, progress: 0xc6a0f6, progress_bg: 0x363a4f, error: 0xed8796, warn: 0xeed49f,
        ok: 0xa6da95, lyric_active: 0xf5bde6, lyric: 0x8087a2;
        gradient: [0x8aadf4, 0x7dc4e4, 0x8bd5ca, 0xa6da95, 0xeed49f, 0xed8796]
    }
    "catppuccin-frappe" => {
        bg: 0x303446, fg: 0xc6d0f5, dim: 0x838ba7, accent: 0xca9ee6, accent2: 0x8caaee,
        border: 0x51576d, border_focus: 0xca9ee6, title: 0xbabbf1, sel_bg: 0xca9ee6, sel_fg: 0x303446,
        playing: 0xa6d189, progress: 0xca9ee6, progress_bg: 0x414559, error: 0xe78284, warn: 0xe5c890,
        ok: 0xa6d189, lyric_active: 0xf4b8e4, lyric: 0x838ba7;
        gradient: [0x8caaee, 0x85c1dc, 0x81c8be, 0xa6d189, 0xe5c890, 0xe78284]
    }
    "tokyo-night" => {
        bg: 0x1a1b26, fg: 0xc0caf5, dim: 0x565f89, accent: 0x7aa2f7, accent2: 0xbb9af7,
        border: 0x3b4261, border_focus: 0x7aa2f7, title: 0x7dcfff, sel_bg: 0x7aa2f7, sel_fg: 0x1a1b26,
        playing: 0x9ece6a, progress: 0x7aa2f7, progress_bg: 0x292e42, error: 0xf7768e, warn: 0xe0af68,
        ok: 0x9ece6a, lyric_active: 0xbb9af7, lyric: 0x565f89;
        gradient: [0x7aa2f7, 0x7dcfff, 0x73daca, 0x9ece6a, 0xe0af68, 0xf7768e]
    }
    "dracula" => {
        bg: 0x282a36, fg: 0xf8f8f2, dim: 0x6272a4, accent: 0xbd93f9, accent2: 0xff79c6,
        border: 0x44475a, border_focus: 0xbd93f9, title: 0xff79c6, sel_bg: 0xbd93f9, sel_fg: 0x282a36,
        playing: 0x50fa7b, progress: 0xbd93f9, progress_bg: 0x44475a, error: 0xff5555, warn: 0xf1fa8c,
        ok: 0x50fa7b, lyric_active: 0xff79c6, lyric: 0x6272a4;
        gradient: [0x8be9fd, 0xbd93f9, 0xff79c6, 0xffb86c, 0xf1fa8c]
    }
    "nord" => {
        bg: 0x2e3440, fg: 0xd8dee9, dim: 0x7b88a1, accent: 0x88c0d0, accent2: 0x81a1c1,
        border: 0x4c566a, border_focus: 0x88c0d0, title: 0x8fbcbb, sel_bg: 0x88c0d0, sel_fg: 0x2e3440,
        playing: 0xa3be8c, progress: 0x88c0d0, progress_bg: 0x3b4252, error: 0xbf616a, warn: 0xebcb8b,
        ok: 0xa3be8c, lyric_active: 0xeceff4, lyric: 0x7b88a1;
        gradient: [0x5e81ac, 0x81a1c1, 0x88c0d0, 0x8fbcbb, 0xa3be8c, 0xebcb8b]
    }
    "gruvbox-dark" => {
        bg: 0x282828, fg: 0xebdbb2, dim: 0x928374, accent: 0xfabd2f, accent2: 0x83a598,
        border: 0x504945, border_focus: 0xfabd2f, title: 0xfe8019, sel_bg: 0xfabd2f, sel_fg: 0x282828,
        playing: 0xb8bb26, progress: 0xfabd2f, progress_bg: 0x3c3836, error: 0xfb4934, warn: 0xfe8019,
        ok: 0xb8bb26, lyric_active: 0xfbf1c7, lyric: 0x928374;
        gradient: [0x83a598, 0x8ec07c, 0xb8bb26, 0xfabd2f, 0xfe8019, 0xfb4934]
    }
    "rose-pine" => {
        bg: 0x191724, fg: 0xe0def4, dim: 0x6e6a86, accent: 0xebbcba, accent2: 0xc4a7e7,
        border: 0x403d52, border_focus: 0xebbcba, title: 0xf6c177, sel_bg: 0xebbcba, sel_fg: 0x191724,
        playing: 0x9ccfd8, progress: 0xebbcba, progress_bg: 0x26233a, error: 0xeb6f92, warn: 0xf6c177,
        ok: 0x9ccfd8, lyric_active: 0xf6c177, lyric: 0x6e6a86;
        gradient: [0x31748f, 0x9ccfd8, 0xc4a7e7, 0xebbcba, 0xf6c177, 0xeb6f92]
    }
    "rose-pine-moon" => {
        bg: 0x232136, fg: 0xe0def4, dim: 0x6e6a86, accent: 0xea9a97, accent2: 0xc4a7e7,
        border: 0x44415a, border_focus: 0xea9a97, title: 0xf6c177, sel_bg: 0xea9a97, sel_fg: 0x232136,
        playing: 0x9ccfd8, progress: 0xea9a97, progress_bg: 0x393552, error: 0xeb6f92, warn: 0xf6c177,
        ok: 0x9ccfd8, lyric_active: 0xf6c177, lyric: 0x6e6a86;
        gradient: [0x3e8fb0, 0x9ccfd8, 0xc4a7e7, 0xea9a97, 0xf6c177, 0xeb6f92]
    }
    "kanagawa" => {
        bg: 0x1f1f28, fg: 0xdcd7ba, dim: 0x727169, accent: 0x7e9cd8, accent2: 0x957fb8,
        border: 0x54546d, border_focus: 0x7e9cd8, title: 0xe6c384, sel_bg: 0x7e9cd8, sel_fg: 0x1f1f28,
        playing: 0x98bb6c, progress: 0x7e9cd8, progress_bg: 0x2a2a37, error: 0xff5d62, warn: 0xff9e3b,
        ok: 0x98bb6c, lyric_active: 0xd27e99, lyric: 0x727169;
        gradient: [0x7e9cd8, 0x7fb4ca, 0x98bb6c, 0xe6c384, 0xffa066, 0xff5d62]
    }
    "everforest" => {
        bg: 0x2d353b, fg: 0xd3c6aa, dim: 0x859289, accent: 0xa7c080, accent2: 0x7fbbb3,
        border: 0x475258, border_focus: 0xa7c080, title: 0xdbbc7f, sel_bg: 0xa7c080, sel_fg: 0x2d353b,
        playing: 0x83c092, progress: 0xa7c080, progress_bg: 0x3d484d, error: 0xe67e80, warn: 0xdbbc7f,
        ok: 0xa7c080, lyric_active: 0xe69875, lyric: 0x859289;
        gradient: [0x7fbbb3, 0x83c092, 0xa7c080, 0xdbbc7f, 0xe69875, 0xe67e80]
    }
    "one-dark" => {
        bg: 0x282c34, fg: 0xabb2bf, dim: 0x7f848e, accent: 0x61afef, accent2: 0xc678dd,
        border: 0x3e4451, border_focus: 0x61afef, title: 0x56b6c2, sel_bg: 0x61afef, sel_fg: 0x282c34,
        playing: 0x98c379, progress: 0x61afef, progress_bg: 0x3e4451, error: 0xe06c75, warn: 0xd19a66,
        ok: 0x98c379, lyric_active: 0xe5c07b, lyric: 0x7f848e;
        gradient: [0x61afef, 0x56b6c2, 0x98c379, 0xe5c07b, 0xd19a66, 0xe06c75]
    }
    "monokai" => {
        bg: 0x272822, fg: 0xf8f8f2, dim: 0x75715e, accent: 0xf92672, accent2: 0x66d9ef,
        border: 0x49483e, border_focus: 0xf92672, title: 0xe6db74, sel_bg: 0xf92672, sel_fg: 0x272822,
        playing: 0xa6e22e, progress: 0xf92672, progress_bg: 0x3e3d32, error: 0xf92672, warn: 0xfd971f,
        ok: 0xa6e22e, lyric_active: 0xe6db74, lyric: 0x75715e;
        gradient: [0x66d9ef, 0xa6e22e, 0xe6db74, 0xfd971f, 0xf92672]
    }
    "ayu-dark" => {
        bg: 0x0d1017, fg: 0xbfbdb6, dim: 0x6c7380, accent: 0xe6b450, accent2: 0x59c2ff,
        border: 0x2d3640, border_focus: 0xe6b450, title: 0xff8f40, sel_bg: 0xe6b450, sel_fg: 0x0d1017,
        playing: 0xaad94c, progress: 0xe6b450, progress_bg: 0x1e232b, error: 0xf07178, warn: 0xffb454,
        ok: 0xaad94c, lyric_active: 0xd2a6ff, lyric: 0x6c7380;
        gradient: [0x59c2ff, 0x95e6cb, 0xaad94c, 0xe6b450, 0xff8f40, 0xf07178]
    }
    "github-dark" => {
        bg: 0x0d1117, fg: 0xe6edf3, dim: 0x7d8590, accent: 0x58a6ff, accent2: 0xbc8cff,
        border: 0x30363d, border_focus: 0x58a6ff, title: 0x79c0ff, sel_bg: 0x58a6ff, sel_fg: 0x0d1117,
        playing: 0x3fb950, progress: 0x58a6ff, progress_bg: 0x21262d, error: 0xf85149, warn: 0xd29922,
        ok: 0x3fb950, lyric_active: 0xffa657, lyric: 0x7d8590;
        gradient: [0x58a6ff, 0x39c5cf, 0x3fb950, 0xd29922, 0xdb6d28, 0xf85149]
    }
    "nightfox" => {
        bg: 0x192330, fg: 0xcdcecf, dim: 0x738091, accent: 0x719cd6, accent2: 0x9d79d6,
        border: 0x39506d, border_focus: 0x719cd6, title: 0x63cdcf, sel_bg: 0x719cd6, sel_fg: 0x192330,
        playing: 0x81b29a, progress: 0x719cd6, progress_bg: 0x29394f, error: 0xc94f6d, warn: 0xdbc074,
        ok: 0x81b29a, lyric_active: 0xf4a261, lyric: 0x738091;
        gradient: [0x719cd6, 0x63cdcf, 0x81b29a, 0xdbc074, 0xf4a261, 0xc94f6d]
    }
    "solarized-dark" => {
        bg: 0x002b36, fg: 0x93a1a1, dim: 0x657b83, accent: 0x268bd2, accent2: 0x2aa198,
        border: 0x2a4f58, border_focus: 0x268bd2, title: 0xb58900, sel_bg: 0x268bd2, sel_fg: 0x002b36,
        playing: 0x859900, progress: 0x268bd2, progress_bg: 0x073642, error: 0xdc322f, warn: 0xb58900,
        ok: 0x859900, lyric_active: 0xeee8d5, lyric: 0x657b83;
        gradient: [0x268bd2, 0x2aa198, 0x859900, 0xb58900, 0xcb4b16, 0xdc322f]
    }
    "synthwave" => {
        bg: 0x262335, fg: 0xf0eff1, dim: 0x848bbd, accent: 0xff7edb, accent2: 0x36f9f6,
        border: 0x495495, border_focus: 0xff7edb, title: 0xfede5d, sel_bg: 0xff7edb, sel_fg: 0x262335,
        playing: 0x72f1b8, progress: 0xff7edb, progress_bg: 0x34294f, error: 0xfe4450, warn: 0xfede5d,
        ok: 0x72f1b8, lyric_active: 0x36f9f6, lyric: 0x848bbd;
        gradient: [0x36f9f6, 0xb893ce, 0xff7edb, 0xf97e72, 0xfede5d]
    }
    "sakura" => {
        bg: 0x1d1720, fg: 0xf3e4ec, dim: 0x8c7a89, accent: 0xff9ec4, accent2: 0xc9a7ff,
        border: 0x4a3a4f, border_focus: 0xff9ec4, title: 0xffc2d9, sel_bg: 0xff9ec4, sel_fg: 0x1d1720,
        playing: 0xa8e6cf, progress: 0xff9ec4, progress_bg: 0x33263a, error: 0xff6b81, warn: 0xffd6a5,
        ok: 0xa8e6cf, lyric_active: 0xffc2d9, lyric: 0x8c7a89;
        gradient: [0xc9a7ff, 0xe3a6f0, 0xff9ec4, 0xffc2d9, 0xfff0f6]
    }
    "matrix" => {
        bg: 0x0d0208, fg: 0x00ff41, dim: 0x008f11, accent: 0x00ff41, accent2: 0x7dff9b,
        border: 0x0b5d1e, border_focus: 0x00ff41, title: 0xb6ffb6, sel_bg: 0x00ff41, sel_fg: 0x0d0208,
        playing: 0xccffcc, progress: 0x00ff41, progress_bg: 0x06330f, error: 0xff4545, warn: 0xd4ff00,
        ok: 0x00ff41, lyric_active: 0xccffcc, lyric: 0x008f11;
        gradient: [0x00561b, 0x008f11, 0x00c832, 0x00ff41, 0xb6ffb6]
    }
    "catppuccin-latte" => {
        bg: 0xeff1f5, fg: 0x4c4f69, dim: 0x7c7f93, accent: 0x8839ef, accent2: 0x1e66f5,
        border: 0xacb0be, border_focus: 0x8839ef, title: 0x1e66f5, sel_bg: 0x8839ef, sel_fg: 0xeff1f5,
        playing: 0x40a02b, progress: 0x8839ef, progress_bg: 0xccd0da, error: 0xd20f39, warn: 0xdf8e1d,
        ok: 0x40a02b, lyric_active: 0x8839ef, lyric: 0x8c8fa1;
        gradient: [0x1e66f5, 0x209fb5, 0x179299, 0x40a02b, 0xdf8e1d, 0xd20f39]
    }
    "tokyo-night-day" => {
        bg: 0xe1e2e7, fg: 0x3760bf, dim: 0x6172b0, accent: 0x2e7de9, accent2: 0x9854f1,
        border: 0xa8aecb, border_focus: 0x2e7de9, title: 0x007197, sel_bg: 0x2e7de9, sel_fg: 0xe1e2e7,
        playing: 0x587539, progress: 0x2e7de9, progress_bg: 0xc4c8da, error: 0xf52a65, warn: 0x8c6c3e,
        ok: 0x587539, lyric_active: 0x9854f1, lyric: 0x848cb5;
        gradient: [0x2e7de9, 0x007197, 0x118c74, 0x587539, 0x8c6c3e, 0xf52a65]
    }
    "rose-pine-dawn" => {
        bg: 0xfaf4ed, fg: 0x575279, dim: 0x797593, accent: 0x286983, accent2: 0x907aa9,
        border: 0xcecacd, border_focus: 0x286983, title: 0xb4637a, sel_bg: 0x286983, sel_fg: 0xfaf4ed,
        playing: 0x56949f, progress: 0x286983, progress_bg: 0xf2e9e1, error: 0xb4637a, warn: 0xea9d34,
        ok: 0x56949f, lyric_active: 0xb4637a, lyric: 0x9893a5;
        gradient: [0x286983, 0x56949f, 0x907aa9, 0xd7827e, 0xea9d34, 0xb4637a]
    }
    "gruvbox-light" => {
        bg: 0xfbf1c7, fg: 0x3c3836, dim: 0x7c6f64, accent: 0xb57614, accent2: 0x076678,
        border: 0xbdae93, border_focus: 0xb57614, title: 0xaf3a03, sel_bg: 0xb57614, sel_fg: 0xfbf1c7,
        playing: 0x79740e, progress: 0xb57614, progress_bg: 0xebdbb2, error: 0x9d0006, warn: 0xaf3a03,
        ok: 0x79740e, lyric_active: 0x076678, lyric: 0x928374;
        gradient: [0x076678, 0x427b58, 0x79740e, 0xb57614, 0xaf3a03, 0x9d0006]
    }
    "solarized-light" => {
        bg: 0xfdf6e3, fg: 0x586e75, dim: 0x839496, accent: 0x268bd2, accent2: 0x6c71c4,
        border: 0x93a1a1, border_focus: 0x268bd2, title: 0xcb4b16, sel_bg: 0x268bd2, sel_fg: 0xfdf6e3,
        playing: 0x859900, progress: 0x268bd2, progress_bg: 0xeee8d5, error: 0xdc322f, warn: 0xcb4b16,
        ok: 0x859900, lyric_active: 0x073642, lyric: 0x93a1a1;
        gradient: [0x268bd2, 0x2aa198, 0x859900, 0xb58900, 0xcb4b16, 0xdc322f]
    }
    "ayu-light" => {
        bg: 0xfcfcfc, fg: 0x5c6166, dim: 0x8a9199, accent: 0xf29718, accent2: 0xa37acc,
        border: 0xcfd1d2, border_focus: 0xf29718, title: 0x2a82c4, sel_bg: 0xf29718, sel_fg: 0x1f2430,
        playing: 0x6c9a00, progress: 0xe08600, progress_bg: 0xe7eaed, error: 0xe65050, warn: 0xfa8d3e,
        ok: 0x6c9a00, lyric_active: 0xa37acc, lyric: 0x8a9199;
        gradient: [0x399ee6, 0x4cbf99, 0x86b300, 0xf2ae49, 0xfa8d3e, 0xf07171]
    }
    "github-light" => {
        bg: 0xffffff, fg: 0x1f2328, dim: 0x656d76, accent: 0x0969da, accent2: 0x8250df,
        border: 0xd0d7de, border_focus: 0x0969da, title: 0x0550ae, sel_bg: 0x0969da, sel_fg: 0xffffff,
        playing: 0x1a7f37, progress: 0x0969da, progress_bg: 0xeaeef2, error: 0xcf222e, warn: 0x9a6700,
        ok: 0x1a7f37, lyric_active: 0x8250df, lyric: 0x8c959f;
        gradient: [0x0969da, 0x1b7c83, 0x1a7f37, 0x9a6700, 0xbc4c00, 0xcf222e]
    }
    "mono" => {
        bg: 0x121212, fg: 0xd0d0d0, dim: 0x7a7a7a, accent: 0xffffff, accent2: 0xb0b0b0,
        border: 0x3a3a3a, border_focus: 0xd0d0d0, title: 0xffffff, sel_bg: 0xd0d0d0, sel_fg: 0x121212,
        playing: 0xffffff, progress: 0xd0d0d0, progress_bg: 0x2a2a2a, error: 0xffffff, warn: 0xe0e0e0,
        ok: 0xb0b0b0, lyric_active: 0xffffff, lyric: 0x7a7a7a;
        gradient: [0x4a4a4a, 0x8a8a8a, 0xc8c8c8, 0xffffff]
    }
    "high-contrast" => {
        bg: 0x000000, fg: 0xffffff, dim: 0xc0c0c0, accent: 0xffff00, accent2: 0x00ffff,
        border: 0xffffff, border_focus: 0xffff00, title: 0xffffff, sel_bg: 0xffff00, sel_fg: 0x000000,
        playing: 0x00ff00, progress: 0xffff00, progress_bg: 0x555555, error: 0xff5555, warn: 0xffff00,
        ok: 0x00ff00, lyric_active: 0xffff00, lyric: 0xc0c0c0;
        gradient: [0x00ffff, 0x00ff00, 0xffff00, 0xff0000]
    }
}

/// Other spellings accepted for theme names (separators and case never matter).
const THEME_ALIASES: &[(&str, &str)] = &[
    ("terminal", "default"),
    ("ansi", "default"),
    ("catppuccin", "catppuccin-mocha"),
    ("mocha", "catppuccin-mocha"),
    ("macchiato", "catppuccin-macchiato"),
    ("frappe", "catppuccin-frappe"),
    ("latte", "catppuccin-latte"),
    ("gruvbox", "gruvbox-dark"),
    ("solarized", "solarized-dark"),
    ("ayu", "ayu-dark"),
    ("github", "github-dark"),
    ("synthwave84", "synthwave"),
    ("monochrome", "mono"),
    ("contrast", "high-contrast"),
    ("hc", "high-contrast"),
];

/// Built-in preset names, in cycling order.
pub fn names() -> Vec<&'static str> {
    std::iter::once("default").chain(PRESET_NAMES.iter().copied()).collect()
}

/// The canonical built-in name for `name`: case, spaces, '-' and '_' are ignored ("Tokyo Night",
/// "tokyo_night"), and family names pick a variant ("catppuccin" -> "catppuccin-mocha").
pub fn canonical_name(name: &str) -> Option<&'static str> {
    let key = |s: &str| s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    let k = key(name);
    names().into_iter().find(|n| key(n) == k).or_else(|| THEME_ALIASES.iter().find(|(a, _)| key(a) == k).map(|(_, n)| *n))
}

pub fn builtin(name: &str) -> Option<Theme> {
    match canonical_name(name)? {
        "default" => Some(Theme::default()),
        n => preset(n),
    }
}

/// "#rrggbb", "#rgb", "rgb(1,2,3)", named colors ("red", "darkgray", "lightblue", "reset"), or an index "42".
/// Names follow ratatui (also "grey", "bright-red", "light_blue"...); "default"/"none" mean `Reset`.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    if matches!(lower.as_str(), "default" | "none" | "terminal") {
        return Some(Color::Reset);
    }
    if let Some(hex) = lower.strip_prefix('#') {
        let n = u32::from_str_radix(hex, 16).ok().filter(|_| hex.chars().all(|c| c.is_ascii_hexdigit()))?;
        return match hex.len() {
            6 => Some(Color::from_u32(n)),
            // "#f80" = "#ff8800"
            3 => Some(Color::Rgb(((n >> 8) & 0xf) as u8 * 17, ((n >> 4) & 0xf) as u8 * 17, (n & 0xf) as u8 * 17)),
            _ => None,
        };
    }
    if let Some(inner) = lower.strip_prefix("rgb(").and_then(|r| r.strip_suffix(')')) {
        let v = inner.split(',').map(|p| p.trim().parse::<u8>().ok()).collect::<Option<Vec<u8>>>()?;
        return (v.len() == 3).then(|| Color::Rgb(v[0], v[1], v[2]));
    }
    s.parse::<Color>().ok()
}

/// Glyphs used throughout the UI. Nerd and Unicode glyphs are one cell wide (`repeat_one` may be
/// two); ASCII uses up to two characters for transport buttons ("||", ">|"), so measure widths.
#[derive(Clone, Debug)]
pub struct Icons {
    pub play: &'static str,
    pub pause: &'static str,
    pub stop: &'static str,
    pub next: &'static str,
    pub prev: &'static str,
    pub shuffle: &'static str,
    pub repeat: &'static str,
    pub repeat_one: &'static str,
    pub stop_after: &'static str,
    pub sleep: &'static str,
    pub ab_loop: &'static str,
    pub speed: &'static str,
    pub volume: &'static str,
    pub muted: &'static str,
    pub favorite: &'static str,
    pub folder: &'static str,
    pub artist: &'static str,
    pub album: &'static str,
    pub genre: &'static str,
    pub year: &'static str,
    pub track: &'static str,
    pub playlist: &'static str,
    pub smart: &'static str,
    pub queue: &'static str,
    pub lyrics: &'static str,
    pub eq: &'static str,
    pub search: &'static str,
    pub playing_marker: &'static str,
    pub selected_marker: &'static str,
}

pub fn icons(set: IconSet) -> Icons {
    match set {
        // Nerd Fonts v3 codepoints (glyph names checked against a patched font): Material Design
        // (nf-md-*, U+F0001..) for a consistent look, plus Font Awesome's moon.
        IconSet::Nerd => Icons {
            play: "\u{f040a}",            // nf-md-play
            pause: "\u{f03e4}",           // nf-md-pause
            stop: "\u{f04db}",            // nf-md-stop
            next: "\u{f04ad}",            // nf-md-skip_next
            prev: "\u{f04ae}",            // nf-md-skip_previous
            shuffle: "\u{f049d}",         // nf-md-shuffle
            repeat: "\u{f0456}",          // nf-md-repeat
            repeat_one: "\u{f0458}",      // nf-md-repeat_once
            stop_after: "\u{f0667}",      // nf-md-stop_circle_outline
            sleep: "\u{f186}",            // nf-fa-moon_o
            ab_loop: "\u{f01c9}",         // nf-md-ab_testing
            speed: "\u{f08ff}",           // nf-md-play_speed
            volume: "\u{f057e}",          // nf-md-volume_high
            muted: "\u{f075f}",           // nf-md-volume_mute
            favorite: "\u{f02d1}",        // nf-md-heart
            folder: "\u{f024b}",          // nf-md-folder
            artist: "\u{f0803}",          // nf-md-account_music
            album: "\u{f0025}",           // nf-md-album
            genre: "\u{f04f9}",           // nf-md-tag
            year: "\u{f00ed}",            // nf-md-calendar
            track: "\u{f0387}",           // nf-md-music_note
            playlist: "\u{f0cb8}",        // nf-md-playlist_music
            smart: "\u{f0ae2}",           // nf-md-star_four_points
            queue: "\u{f0411}",           // nf-md-playlist_play
            lyrics: "\u{f0370}",          // nf-md-microphone_variant
            eq: "\u{f066a}",              // nf-md-tune_vertical
            search: "\u{f0349}",          // nf-md-magnify
            playing_marker: "\u{f040a}",  // nf-md-play
            selected_marker: "\u{f0142}", // nf-md-chevron_right
        },
        IconSet::Unicode => Icons {
            play: "▶",
            pause: "⏸",
            stop: "■",
            next: "⏭",
            prev: "⏮",
            shuffle: "⤮",
            repeat: "↻",
            repeat_one: "↻1",
            stop_after: "⏏",
            sleep: "☾",
            ab_loop: "⟲",
            speed: "»",
            volume: "♪",
            muted: "∅",
            favorite: "♥",
            folder: "▸",
            artist: "☺",
            album: "◉",
            genre: "◆",
            year: "◷",
            track: "♫",
            playlist: "≡",
            smart: "✦",
            queue: "▤",
            lyrics: "¶",
            eq: "≋",
            search: "⌕",
            playing_marker: "▶",
            selected_marker: "›",
        },
        IconSet::Ascii => Icons {
            play: ">",
            pause: "||",
            stop: "[]",
            next: ">|",
            prev: "|<",
            shuffle: "~",
            repeat: "R",
            repeat_one: "R1",
            stop_after: "!",
            sleep: "z",
            ab_loop: "AB",
            speed: "x",
            volume: "v",
            muted: "m",
            favorite: "*",
            folder: "/",
            artist: "@",
            album: "o",
            genre: "#",
            year: "y",
            track: "-",
            playlist: "=",
            smart: "+",
            queue: "Q",
            lyrics: "\"",
            eq: "E",
            search: "?",
            playing_marker: ">",
            selected_marker: "|",
        },
    }
}

/// ratatui border type for a style; `None` means draw no borders at all.
pub fn border_type(style: BorderStyle) -> Option<BorderType> {
    match style {
        BorderStyle::Rounded => Some(BorderType::Rounded),
        BorderStyle::Plain => Some(BorderType::Plain),
        BorderStyle::Double => Some(BorderType::Double),
        BorderStyle::Thick => Some(BorderType::Thick),
        BorderStyle::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// WCAG contrast ratio between two RGB colors (1.0 ..= 21.0).
    fn contrast(a: Color, b: Color) -> f32 {
        let lum = |c: Color| {
            let (r, g, b) = to_rgb(c).expect("preset colors are RGB");
            let ch = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn presets_are_complete_and_readable() {
        let names = names();
        assert_eq!(names[0], "default");
        assert!(names.len() >= 16);
        for required in [
            "default",
            "catppuccin-mocha",
            "catppuccin-latte",
            "dracula",
            "nord",
            "gruvbox-dark",
            "gruvbox-light",
            "tokyo-night",
            "rose-pine",
            "solarized-dark",
            "solarized-light",
            "everforest",
            "kanagawa",
            "one-dark",
            "monokai",
            "synthwave",
            "matrix",
            "mono",
            "high-contrast",
        ] {
            assert!(names.contains(&required), "{required}");
        }
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len());

        let mut problems = Vec::new();
        for name in &names[1..] {
            let t = builtin(name).unwrap();
            assert_eq!(t.name, *name);
            assert!((3..=6).contains(&t.gradient.len()), "{name}: gradient stops");
            let mut check = |what: &str, a: Color, b: Color, min: f32| {
                let c = contrast(a, b);
                if c < min {
                    problems.push(format!("{name}: {what} contrast {c:.2} < {min}"));
                }
            };
            check("fg/bg", t.fg, t.bg, 4.5);
            check("sel_fg/sel_bg", t.sel_fg, t.sel_bg, 3.0);
            check("sel_fg/accent (active tab)", t.sel_fg, t.accent, 3.0);
            check("dim/bg", t.dim, t.bg, 2.5);
            check("lyric_active/bg", t.lyric_active, t.bg, 3.0);
            check("playing/bg", t.playing, t.bg, 2.5);
            check("accent/bg", t.accent, t.bg, 2.0);
            check("accent2/bg (artist names)", t.accent2, t.bg, 3.0);
            check("title/bg", t.title, t.bg, 3.0);
            check("warn/bg", t.warn, t.bg, 2.0);
            check("ok/bg", t.ok, t.bg, 2.0);
            check("error/bg", t.error, t.bg, 3.0);
            check("border/bg", t.border, t.bg, 1.4);
            check("progress/progress_bg", t.progress, t.progress_bg, 2.0);
        }
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn theme_names_are_forgiving() {
        assert_eq!(canonical_name("Tokyo Night"), Some("tokyo-night"));
        assert_eq!(canonical_name("tokyo_night"), Some("tokyo-night"));
        assert_eq!(canonical_name("ONEDARK"), Some("one-dark"));
        assert_eq!(canonical_name("catppuccin"), Some("catppuccin-mocha"));
        assert_eq!(canonical_name("gruvbox"), Some("gruvbox-dark"));
        assert_eq!(canonical_name("nope"), None);
        assert_eq!(builtin("Dracula").unwrap().name, "dracula");
        assert_eq!(builtin("default"), Some(Theme::default()));
    }

    #[test]
    fn parses_every_color_format() {
        assert_eq!(parse_color("#ff79c6"), Some(Color::Rgb(0xff, 0x79, 0xc6)));
        assert_eq!(parse_color("#FF79C6"), Some(Color::Rgb(0xff, 0x79, 0xc6)));
        assert_eq!(parse_color("#f80"), Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_color(" rgb(1, 2, 3) "), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(parse_color("RGB(255,0,10)"), Some(Color::Rgb(255, 0, 10)));
        assert_eq!(parse_color("darkgray"), Some(Color::DarkGray));
        assert_eq!(parse_color("dark-grey"), Some(Color::DarkGray));
        assert_eq!(parse_color("grey"), Some(Color::Gray));
        assert_eq!(parse_color("Light Blue"), Some(Color::LightBlue));
        assert_eq!(parse_color("bright_red"), Some(Color::LightRed));
        assert_eq!(parse_color("42"), Some(Color::Indexed(42)));
        assert_eq!(parse_color("255"), Some(Color::Indexed(255)));
        for reset in ["reset", "default", "none", "Reset"] {
            assert_eq!(parse_color(reset), Some(Color::Reset), "{reset}");
        }
        for bad in ["", "#ff79c", "#gg0000", "#+12345", "rgb(1,2)", "rgb(1,2,300)", "256", "purplish"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn overrides_apply_and_warn() {
        let mut o = BTreeMap::new();
        o.insert("accent".to_string(), "#ff0000".to_string());
        o.insert("sel-bg".to_string(), "rgb(1,2,3)".to_string());
        o.insert("gradient".to_string(), "#000000, #ffffff".to_string());
        o.insert("acent".to_string(), "#ffffff".to_string());
        o.insert("fg".to_string(), "blurple".to_string());
        let (t, w) = Theme::from_config("nord", &o);
        assert_eq!(t.name, "nord");
        assert_eq!(t.accent, Color::Rgb(255, 0, 0));
        assert_eq!(t.sel_bg, Color::Rgb(1, 2, 3));
        assert_eq!(t.gradient, vec![Color::Rgb(0, 0, 0), Color::Rgb(255, 255, 255)]);
        assert_eq!(t.fg, builtin("nord").unwrap().fg, "bad colors keep the preset");
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w.iter().any(|m| m.contains("unknown slot \"acent\"") && m.contains("\"accent\"")), "{w:?}");
        assert!(w.iter().any(|m| m.contains("fg") && m.contains("blurple")), "{w:?}");

        let (t, w) = Theme::from_config("nordd", &BTreeMap::new());
        assert_eq!(t, Theme::default());
        assert!(w[0].contains("unknown theme \"nordd\"") && w[0].contains("\"nord\""), "{w:?}");

        let rgb = BTreeMap::from([("gradient".to_string(), "rgb(1, 2, 3), #ffffff,, 42".to_string())]);
        let (t, w) = Theme::from_config("default", &rgb);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(t.gradient, vec![Color::Rgb(1, 2, 3), Color::Rgb(255, 255, 255), Color::Indexed(42)]);
        let one = BTreeMap::from([("gradient".to_string(), "red".to_string())]);
        assert_eq!(Theme::from_config("default", &one).0.gradient, vec![Color::Red, Color::Red]);
        let bad = BTreeMap::from([("gradient".to_string(), "red, nope".to_string())]);
        let (t, w) = Theme::from_config("default", &bad);
        assert_eq!(t.gradient, Theme::default().gradient);
        assert!(w[0].contains("nope"));
        // every slot is overridable by name
        for slot in SLOTS {
            let o = BTreeMap::from([(slot.to_string(), "#123456".to_string())]);
            assert!(Theme::from_config("default", &o).1.is_empty(), "{slot}");
        }
    }

    #[test]
    fn gradient_interpolates() {
        let t = Theme { gradient: vec![Color::Rgb(0, 0, 0), Color::Rgb(200, 100, 50)], ..Theme::default() };
        assert_eq!(t.gradient_at(0.0), Color::Rgb(0, 0, 0));
        assert_eq!(t.gradient_at(1.0), Color::Rgb(200, 100, 50));
        assert_eq!(t.gradient_at(0.5), Color::Rgb(100, 50, 25));
        assert_eq!(t.gradient_at(-3.0), Color::Rgb(0, 0, 0));
        assert_eq!(t.gradient_at(7.0), Color::Rgb(200, 100, 50));
        assert_eq!(t.gradient_at(f32::NAN), Color::Rgb(0, 0, 0));

        // exact stops keep named colors; in between blends their approximate RGB
        let d = Theme::default();
        assert_eq!(d.gradient_at(0.0), Color::Blue);
        assert_eq!(d.gradient_at(0.25), Color::Cyan);
        assert_eq!(d.gradient_at(1.0), Color::Red);
        assert_eq!(d.gradient_at(0.125), Color::Rgb(0, 103, 222));

        // Reset can't be blended: step to the nearest stop
        let r = Theme { gradient: vec![Color::Reset, Color::Rgb(255, 255, 255)], ..Theme::default() };
        assert_eq!(r.gradient_at(0.4), Color::Reset);
        assert_eq!(r.gradient_at(0.6), Color::Rgb(255, 255, 255));

        let single = Theme { gradient: vec![Color::Green], ..Theme::default() };
        assert_eq!(single.gradient_at(0.7), Color::Green);
        let empty = Theme { gradient: vec![], ..Theme::default() };
        assert_eq!(empty.gradient_at(0.7), empty.accent);
    }

    #[test]
    fn indexed_colors_blend() {
        assert_eq!(to_rgb(Color::Indexed(16)), Some((0, 0, 0)));
        assert_eq!(to_rgb(Color::Indexed(196)), Some((255, 0, 0)));
        assert_eq!(to_rgb(Color::Indexed(232)), Some((8, 8, 8)));
        assert_eq!(to_rgb(Color::Indexed(255)), Some((238, 238, 238)));
        assert_eq!(to_rgb(Color::Indexed(9)), to_rgb(Color::LightRed));
    }

    fn all(i: &Icons) -> [&'static str; 29] {
        [
            i.play, i.pause, i.stop, i.next, i.prev, i.shuffle, i.repeat, i.repeat_one, i.stop_after, i.sleep, i.ab_loop,
            i.speed, i.volume, i.muted, i.favorite, i.folder, i.artist, i.album, i.genre, i.year, i.track, i.playlist,
            i.smart, i.queue, i.lyrics, i.eq, i.search, i.playing_marker, i.selected_marker,
        ]
    }

    #[test]
    fn icons_fit_their_cells() {
        for set in [IconSet::Nerd, IconSet::Unicode] {
            let i = icons(set);
            for glyph in all(&i) {
                let max = if glyph == i.repeat_one { 2 } else { 1 };
                assert!((1..=max).contains(&glyph.width()), "{set:?} {glyph:?} is {} cells", glyph.width());
            }
        }
        // Nerd glyphs live in the private use areas (BMP or plane 15)
        for glyph in all(&icons(IconSet::Nerd)) {
            let c = glyph.chars().next().unwrap() as u32;
            assert!((0xe000..=0xf8ff).contains(&c) || (0xf0000..=0xffffd).contains(&c), "{c:x}");
        }
        for glyph in all(&icons(IconSet::Ascii)) {
            assert!(glyph.is_ascii() && (1..=2).contains(&glyph.len()), "{glyph:?}");
        }
    }
}
