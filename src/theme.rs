//! Orbit's colour system — multiple swappable palettes.
//!
//! Colours are looked up at render time via accessor functions (e.g. `accent()`)
//! that read the globally-active palette, so the theme can be switched live.

use std::sync::RwLock;

use ratatui::style::Color;

#[derive(Clone, Copy)]
pub struct Palette {
    pub name: &'static str,
    pub bg: Color,
    pub panel_bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub faint: Color,
    pub border: Color,
    pub border_focus: Color,
    pub accent: Color,
    pub accent2: Color,
    pub violet: Color,
    pub gold: Color,
    pub green: Color,
    pub error: Color,
    pub select_bg: Color,
}

pub const PALETTES: &[Palette] = &[
    // 0 — Synthwave (default): deep space, cyan→magenta.
    Palette {
        name: "Synthwave",
        bg: Color::Rgb(14, 14, 22),
        panel_bg: Color::Rgb(19, 19, 30),
        fg: Color::Rgb(208, 211, 225),
        dim: Color::Rgb(96, 99, 122),
        faint: Color::Rgb(60, 62, 84),
        border: Color::Rgb(46, 48, 70),
        border_focus: Color::Rgb(122, 222, 235),
        accent: Color::Rgb(122, 222, 235),
        accent2: Color::Rgb(255, 106, 193),
        violet: Color::Rgb(167, 139, 250),
        gold: Color::Rgb(247, 201, 72),
        green: Color::Rgb(126, 231, 135),
        error: Color::Rgb(255, 107, 107),
        select_bg: Color::Rgb(34, 35, 55),
    },
    // 1 — Nord: cool, muted arctic blues.
    Palette {
        name: "Nord",
        bg: Color::Rgb(36, 42, 54),
        panel_bg: Color::Rgb(46, 52, 64),
        fg: Color::Rgb(216, 222, 233),
        dim: Color::Rgb(118, 128, 146),
        faint: Color::Rgb(76, 86, 106),
        border: Color::Rgb(67, 76, 94),
        border_focus: Color::Rgb(136, 192, 208),
        accent: Color::Rgb(136, 192, 208),
        accent2: Color::Rgb(180, 142, 173),
        violet: Color::Rgb(129, 161, 193),
        gold: Color::Rgb(235, 203, 139),
        green: Color::Rgb(163, 190, 140),
        error: Color::Rgb(191, 97, 106),
        select_bg: Color::Rgb(59, 66, 82),
    },
    // 2 — Matrix: green phosphor on black.
    Palette {
        name: "Matrix",
        bg: Color::Rgb(2, 8, 2),
        panel_bg: Color::Rgb(6, 16, 8),
        fg: Color::Rgb(120, 230, 120),
        dim: Color::Rgb(60, 130, 70),
        faint: Color::Rgb(30, 70, 38),
        border: Color::Rgb(28, 70, 34),
        border_focus: Color::Rgb(96, 245, 110),
        accent: Color::Rgb(96, 245, 110),
        accent2: Color::Rgb(180, 255, 120),
        violet: Color::Rgb(120, 220, 160),
        gold: Color::Rgb(200, 255, 140),
        green: Color::Rgb(96, 245, 110),
        error: Color::Rgb(255, 120, 90),
        select_bg: Color::Rgb(16, 40, 20),
    },
    // 3 — Solarized Dark.
    Palette {
        name: "Solarized",
        bg: Color::Rgb(0, 43, 54),
        panel_bg: Color::Rgb(7, 54, 66),
        fg: Color::Rgb(147, 161, 161),
        dim: Color::Rgb(88, 110, 117),
        faint: Color::Rgb(40, 74, 84),
        border: Color::Rgb(40, 74, 84),
        border_focus: Color::Rgb(42, 161, 152),
        accent: Color::Rgb(42, 161, 152),
        accent2: Color::Rgb(211, 54, 130),
        violet: Color::Rgb(108, 113, 196),
        gold: Color::Rgb(181, 137, 0),
        green: Color::Rgb(133, 153, 0),
        error: Color::Rgb(220, 50, 47),
        select_bg: Color::Rgb(20, 64, 76),
    },
    // 4 — Ember: warm dark, amber & rose.
    Palette {
        name: "Ember",
        bg: Color::Rgb(22, 16, 14),
        panel_bg: Color::Rgb(30, 22, 19),
        fg: Color::Rgb(232, 218, 205),
        dim: Color::Rgb(140, 112, 98),
        faint: Color::Rgb(84, 64, 54),
        border: Color::Rgb(78, 58, 48),
        border_focus: Color::Rgb(245, 158, 88),
        accent: Color::Rgb(245, 158, 88),
        accent2: Color::Rgb(240, 110, 120),
        violet: Color::Rgb(214, 140, 160),
        gold: Color::Rgb(247, 201, 72),
        green: Color::Rgb(180, 200, 120),
        error: Color::Rgb(240, 100, 90),
        select_bg: Color::Rgb(48, 34, 28),
    },
    // 5 — Dracula.
    Palette {
        name: "Dracula",
        bg: Color::Rgb(40, 42, 54),
        panel_bg: Color::Rgb(33, 34, 44),
        fg: Color::Rgb(248, 248, 242),
        dim: Color::Rgb(98, 114, 164),
        faint: Color::Rgb(68, 71, 90),
        border: Color::Rgb(68, 71, 90),
        border_focus: Color::Rgb(139, 233, 253),
        accent: Color::Rgb(139, 233, 253),
        accent2: Color::Rgb(255, 121, 198),
        violet: Color::Rgb(189, 147, 249),
        gold: Color::Rgb(241, 250, 140),
        green: Color::Rgb(80, 250, 123),
        error: Color::Rgb(255, 85, 85),
        select_bg: Color::Rgb(68, 71, 90),
    },
    // 6 — Tokyo Night.
    Palette {
        name: "Tokyo Night",
        bg: Color::Rgb(26, 27, 38),
        panel_bg: Color::Rgb(31, 32, 46),
        fg: Color::Rgb(192, 202, 245),
        dim: Color::Rgb(86, 95, 137),
        faint: Color::Rgb(54, 58, 79),
        border: Color::Rgb(41, 46, 66),
        border_focus: Color::Rgb(122, 162, 247),
        accent: Color::Rgb(125, 207, 255),
        accent2: Color::Rgb(247, 118, 142),
        violet: Color::Rgb(187, 154, 247),
        gold: Color::Rgb(224, 175, 104),
        green: Color::Rgb(158, 206, 106),
        error: Color::Rgb(247, 118, 142),
        select_bg: Color::Rgb(41, 46, 66),
    },
    // 7 — Catppuccin Mocha.
    Palette {
        name: "Catppuccin",
        bg: Color::Rgb(30, 30, 46),
        panel_bg: Color::Rgb(24, 24, 37),
        fg: Color::Rgb(205, 214, 244),
        dim: Color::Rgb(127, 132, 156),
        faint: Color::Rgb(69, 71, 90),
        border: Color::Rgb(49, 50, 68),
        border_focus: Color::Rgb(137, 180, 250),
        accent: Color::Rgb(137, 220, 235),
        accent2: Color::Rgb(245, 194, 231),
        violet: Color::Rgb(203, 166, 247),
        gold: Color::Rgb(249, 226, 175),
        green: Color::Rgb(166, 227, 161),
        error: Color::Rgb(243, 139, 168),
        select_bg: Color::Rgb(49, 50, 68),
    },
    // 8 — Gruvbox.
    Palette {
        name: "Gruvbox",
        bg: Color::Rgb(40, 40, 40),
        panel_bg: Color::Rgb(50, 48, 47),
        fg: Color::Rgb(235, 219, 178),
        dim: Color::Rgb(168, 153, 132),
        faint: Color::Rgb(102, 92, 84),
        border: Color::Rgb(80, 73, 69),
        border_focus: Color::Rgb(250, 189, 47),
        accent: Color::Rgb(142, 192, 124),
        accent2: Color::Rgb(211, 134, 155),
        violet: Color::Rgb(211, 134, 155),
        gold: Color::Rgb(250, 189, 47),
        green: Color::Rgb(184, 187, 38),
        error: Color::Rgb(251, 73, 52),
        select_bg: Color::Rgb(60, 56, 54),
    },
    // 9 — Rosé Pine.
    Palette {
        name: "Rosé Pine",
        bg: Color::Rgb(25, 23, 36),
        panel_bg: Color::Rgb(31, 29, 46),
        fg: Color::Rgb(224, 222, 244),
        dim: Color::Rgb(110, 106, 134),
        faint: Color::Rgb(64, 61, 82),
        border: Color::Rgb(38, 35, 58),
        border_focus: Color::Rgb(156, 207, 216),
        accent: Color::Rgb(156, 207, 216),
        accent2: Color::Rgb(235, 188, 186),
        violet: Color::Rgb(196, 167, 231),
        gold: Color::Rgb(246, 193, 119),
        green: Color::Rgb(62, 143, 176),
        error: Color::Rgb(235, 111, 146),
        select_bg: Color::Rgb(33, 32, 46),
    },
];

/// Index of the active palette.
static ACTIVE: RwLock<usize> = RwLock::new(0);

pub fn set_palette(idx: usize) {
    *ACTIVE.write().unwrap() = idx % PALETTES.len();
}

pub fn active_index() -> usize {
    *ACTIVE.read().unwrap()
}

pub fn current() -> Palette {
    PALETTES[active_index() % PALETTES.len()]
}

pub fn palette_name() -> &'static str {
    current().name
}

pub fn palette_count() -> usize {
    PALETTES.len()
}

pub fn palette_at(idx: usize) -> Palette {
    PALETTES[idx % PALETTES.len()]
}

// -- accessors ---------------------------------------------------------------

pub fn bg() -> Color {
    current().bg
}
pub fn panel_bg() -> Color {
    current().panel_bg
}
pub fn fg() -> Color {
    current().fg
}
pub fn dim() -> Color {
    current().dim
}
pub fn faint() -> Color {
    current().faint
}
pub fn border() -> Color {
    current().border
}
pub fn border_focus() -> Color {
    current().border_focus
}
pub fn accent() -> Color {
    current().accent
}
pub fn accent2() -> Color {
    current().accent2
}
pub fn violet() -> Color {
    current().violet
}
pub fn gold() -> Color {
    current().gold
}
pub fn green() -> Color {
    current().green
}
pub fn error() -> Color {
    current().error
}
pub fn select_bg() -> Color {
    current().select_bg
}

/// Number of distinct accent colours buckets cycle through.
pub const BUCKET_COLORS: usize = 5;

/// Accent colour for a bucket, by its stored colour index.
pub fn bucket_color(idx: u8) -> Color {
    let p = current();
    let palette = [p.accent, p.accent2, p.violet, p.gold, p.green];
    palette[idx as usize % BUCKET_COLORS]
}

/// Three-stop gradient accent → violet → accent2. `t` in 0.0..=1.0.
pub fn gradient(t: f32) -> Color {
    let p = current();
    let t = t.clamp(0.0, 1.0);
    let (a, b, local) = if t < 0.5 {
        (p.accent, p.violet, t / 0.5)
    } else {
        (p.violet, p.accent2, (t - 0.5) / 0.5)
    };
    lerp(a, b, local)
}

/// Blend a colour toward the background by `t` (0 = unchanged, 1 = background).
pub fn toward_bg(c: Color, t: f32) -> Color {
    lerp(c, current().bg, t)
}

fn lerp(a: Color, b: Color, t: f32) -> Color {
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    Color::Rgb(
        (ar as f32 + (br as f32 - ar as f32) * t) as u8,
        (ag as f32 + (bg as f32 - ag as f32) * t) as u8,
        (ab as f32 + (bb as f32 - ab as f32) * t) as u8,
    )
}

fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (255, 255, 255),
    }
}
