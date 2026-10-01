//! Album art: embedded pictures or cover images in the track's folder, drawn with truecolor
//! half-block characters (each cell = 2 vertical pixels), with a generated placeholder otherwise.

use std::f32::consts::{FRAC_PI_4, TAU};
use std::io::{BufRead, Cursor, Seek};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use image::imageops::{self, FilterType};
use image::{DynamicImage, ImageReader, Limits, Rgb, RgbImage};
use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use crate::library::{Track, TrackId};
use crate::lyrics::find_files;
use crate::theme::Theme;

/// Covers are downscaled to fit in this many pixels once, on load; per-size resizes start from there.
const MAX_SIDE: u32 = 600;
/// Bigger images aren't decoded at all.
const MAX_DECODE: u32 = 8192;
const IMAGE_EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "bmp"];
/// Folder cover names, best first (any case, any of `IMAGE_EXTS`).
const COVER_NAMES: [&str; 6] = ["cover", "folder", "front", "album", "artwork", "albumart"];
/// Subfolders that often hold an album's scans.
const ART_DIRS: [&str; 7] = ["covers", "cover", "artwork", "art", "scans", "images", "pictures"];
/// Transparent covers are flattened onto this.
const MATTE: [u32; 3] = [24, 24, 28];

/// Embedded front cover (else any embedded picture), else cover/folder/front/album.{jpg,jpeg,png,webp}
/// in the track's folder (case-insensitive). Decoded to RGB.
///
/// Also: pictures of every tag are considered, and corrupt images are skipped in favor of the next
/// candidate; the folder lookup is described at [`cover_files`]. The result is downscaled to fit
/// [`MAX_SIDE`] and solid letterbox / pillarbox bars are trimmed (see [`trim_bars`]), so a square
/// cover padded to a 16:9 video thumbnail comes back square.
pub fn load_cover(track: &Track) -> Option<RgbImage> {
    let img = embedded_cover(&track.path).or_else(|| cover_files(track).iter().find_map(|p| decode_file(p)))?;
    Some(prepare(img))
}

fn embedded_cover(path: &Path) -> Option<DynamicImage> {
    let options = ParseOptions::new().read_properties(false);
    let file = Probe::open(path).ok()?.options(options).guess_file_type().ok()?.read().ok()?;
    let mut pictures: Vec<_> = file.tags().iter().flat_map(|t| t.pictures()).collect();
    pictures.sort_by_key(|p| p.pic_type() != PictureType::CoverFront);
    pictures.into_iter().find_map(|p| decode(ImageReader::new(Cursor::new(p.data()))))
}

fn decode_file(path: &Path) -> Option<DynamicImage> {
    decode(ImageReader::open(path).ok()?)
}

/// Decode by content (not extension). Refuses empty images and ones over [`MAX_DECODE`] pixels
/// per side, so a pathological file can't stall a track change.
fn decode<R: BufRead + Seek>(reader: ImageReader<R>) -> Option<DynamicImage> {
    let mut reader = reader.with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE);
    limits.max_image_height = Some(MAX_DECODE);
    reader.limits(limits);
    reader.decode().ok().filter(|img| img.width() > 0 && img.height() > 0)
}

/// Cover image files for `track`, best first:
/// 1. in its folder: "<file stem>.<ext>" (a per-track thumbnail, as yt-dlp writes them),
///    "<album>.<ext>", cover/folder/front/album/artwork/albumart (any case, jpg/jpeg/png/webp/gif/bmp)
/// 2. the folder's only image, unless it is named after another file there (that file's thumbnail)
/// 3. the same names, or the only image, in an art subfolder (Covers/, Artwork/, Scans/ ...)
/// 4. for disc folders (CD1/, Disc 2/ ...): steps 1-3 in the album folder above
fn cover_files(track: &Track) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (track.path.parent(), track.path.file_stem()) else { return Vec::new() };
    let stem = stem.to_string_lossy();
    let mut names = vec![stem.as_ref(), track.album.as_str()];
    names.extend(COVER_NAMES);
    let mut files = folder_images(dir, &names);
    if let Some(album_dir) = dir.parent().filter(|_| is_disc_folder(dir)) {
        files.extend(folder_images(album_dir, &names[1..]));
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|p| seen.insert(p.clone()));
    files
}

/// Steps 1-3 of [`cover_files`] for one folder.
fn folder_images(dir: &Path, names: &[&str]) -> Vec<PathBuf> {
    let mut files = find_files(dir, names, &IMAGE_EXTS);
    let entries = list_dir(dir);
    files.extend(lone_image(&entries));
    for (sub, _) in entries.iter().filter(|(p, is_dir)| *is_dir && lower_name(p).is_some_and(|n| ART_DIRS.contains(&n.as_str()))) {
        files.extend(find_files(sub, names, &IMAGE_EXTS));
        files.extend(lone_image(&list_dir(sub)));
    }
    files
}

/// (path, is_dir) of the visible entries of `dir`.
fn list_dir(dir: &Path) -> Vec<(PathBuf, bool)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| (e.path(), e.file_type().is_ok_and(|t| t.is_dir())))
        .collect()
}

fn lower_name(path: &Path) -> Option<String> {
    Some(path.file_name()?.to_string_lossy().to_lowercase())
}

fn is_image(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGE_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// The only image among `entries`, unless another file there is named like it ("07 - Song.png"
/// next to "07 - Song.webm" is that track's thumbnail, not the folder's cover).
fn lone_image(entries: &[(PathBuf, bool)]) -> Option<PathBuf> {
    let mut images = entries.iter().filter(|(p, is_dir)| !is_dir && is_image(p));
    let (image, _) = images.next()?;
    if images.next().is_some() {
        return None;
    }
    let prefix = format!("{}.", image.file_stem()?.to_string_lossy().to_lowercase());
    let owned = entries.iter().any(|(p, _)| p != image && lower_name(p).is_some_and(|n| n.starts_with(&prefix)));
    (!owned).then(|| image.clone())
}

/// "CD1", "cd 2", "Disc 1", "disk_03" ...
fn is_disc_folder(dir: &Path) -> bool {
    let name = lower_name(dir).unwrap_or_default();
    ["cd", "disc", "disk"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|rest| {
            let number = rest.trim_start_matches([' ', '_', '-', '.']);
            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
        })
    })
}

/// Downscale to fit `MAX_SIDE`, flatten transparency, trim letterbox bars.
fn prepare(img: DynamicImage) -> RgbImage {
    let img = if img.width().max(img.height()) > MAX_SIDE { img.thumbnail(MAX_SIDE, MAX_SIDE) } else { img };
    let rgb = if img.color().has_alpha() {
        let rgba = img.into_rgba8();
        RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            let p = rgba.get_pixel(x, y).0;
            let a = u32::from(p[3]);
            Rgb([0, 1, 2].map(|i| ((u32::from(p[i]) * a + MATTE[i] * (255 - a)) / 255) as u8))
        })
    } else {
        img.into_rgb8()
    };
    trim_bars(rgb)
}

/// Strip solid-color bars that pad a picture into a video frame: a square cover pillarboxed in a
/// wider frame (YouTube "Topic" uploads), a 16:9 picture letterboxed in a 4:3 thumbnail, or both.
/// A crop is made only when it yields that shape (near-square; 16:9 from 4:3) and both bars of the
/// pair are present and about equal, so ordinary covers and video frames are left alone.
fn trim_bars(img: RgbImage) -> RgbImage {
    let aspect = |w: u32, h: u32| w * 100 / h.max(1); // in hundredths
    let even = |a: u32, b: u32, n: u32| a * 50 >= n && b * 50 >= n && a.abs_diff(b) * 20 <= n;
    let (w, h) = img.dimensions();
    let img = match (125..=145).contains(&aspect(w, h)).then(|| (bar(&img, true, false), bar(&img, true, true))) {
        Some((top, bottom)) if even(top, bottom, h) && (160..=195).contains(&aspect(w, h - top - bottom)) => {
            imageops::crop_imm(&img, 0, top, w, h - top - bottom).to_image()
        }
        _ => img,
    };
    let (w, h) = img.dimensions();
    match (aspect(w, h) >= 120).then(|| (bar(&img, false, false), bar(&img, false, true))) {
        Some((left, right)) if even(left, right, w) && (85..=115).contains(&aspect(w - left - right, h)) => {
            imageops::crop_imm(&img, left, 0, w - left - right, h).to_image()
        }
        _ => img,
    }
}

/// How many rows (`rows`) or columns, from the top/left (or the bottom/right with `from_end`), are
/// one solid color, up to half the image. Tolerates compression noise and a few stray pixels.
fn bar(img: &RgbImage, rows: bool, from_end: bool) -> u32 {
    let (w, h) = img.dimensions();
    let (lines, len) = if rows { (h, w) } else { (w, h) };
    let pixel = |line: u32, i: u32| {
        let line = if from_end { lines - 1 - line } else { line };
        if rows { img.get_pixel(i, line).0 } else { img.get_pixel(line, i).0 }
    };
    let color = pixel(0, 0);
    let close = |p: [u8; 3]| p.iter().zip(color).all(|(a, b)| a.abs_diff(b) <= 24);
    (0..lines / 2).take_while(|&line| (0..len).filter(|&i| !close(pixel(line, i))).count() as u32 * 32 <= len).count() as u32
}

/// Deterministic art for tracks without a cover (e.g. a gradient/vinyl pattern seeded by the album name).
///
/// A diagonal gradient between two hues derived from a hash of `seed` (mid-tones that sit well on
/// dark and light themes) with one of three motifs: a vinyl record whose label takes the theme's
/// accent, ripples spreading from an off-center dot, or a beamed pair of eighth notes. Anti-aliased by
/// supersampling; shapes stay round in non-square images. Same inputs, same image.
pub fn placeholder(seed: &str, width: u32, height: u32, theme: &Theme) -> RgbImage {
    let (w, h) = (width.max(1), height.max(1));
    // FNV-1a: stable across runs and Rust versions, unlike std's hasher.
    let hash = seed.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |acc, b| (acc ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    let bits = |shift: u32, n: u64| (hash >> shift) % n;
    let sign = |shift: u32| if bits(shift, 2) == 0 { -1.0 } else { 1.0 };
    let hue = bits(0, 360) as f32;
    let hue2 = hue + (40 + bits(10, 60)) as f32 * sign(9);
    let motif = bits(20, 3);
    let dot = (0.28 * sign(24), 0.28 * sign(25));
    let tint = to_rgb(theme.accent).unwrap_or_else(|| from_hsl(hue + 180.0, 0.65, 0.62)).map(f32::from);
    let side = w.min(h) as f32;
    let shade = |u: f32, v: f32, x: f32, y: f32| {
        let t = ((u + v) / 2.0).clamp(0.0, 1.0);
        let vignette = 1.0 - 0.35 * (x * x + y * y);
        let base = from_hsl(hue + (hue2 - hue) * t, 0.50 + 0.08 * t, 0.62 - 0.32 * t).map(|c| f32::from(c) * vignette);
        match motif {
            0 => vinyl(base, x, y, tint),
            1 => ripples(base, x - dot.0, y - dot.1, tint, (4.0 / side).max(0.12)),
            _ => notes(base, x, y),
        }
    };
    let n = if w.max(h) > 64 { 2 } else { 4 };
    RgbImage::from_fn(w, h, |px, py| {
        let mut sum = [0.0f32; 3];
        for i in 0..n * n {
            let fx = px as f32 + ((i % n) as f32 + 0.5) / n as f32;
            let fy = py as f32 + ((i / n) as f32 + 0.5) / n as f32;
            let c = shade(fx / w as f32, fy / h as f32, (fx - w as f32 / 2.0) / side, (fy - h as f32 / 2.0) / side);
            sum.iter_mut().zip(c).for_each(|(s, c)| *s += c);
        }
        Rgb(sum.map(|s| (s / (n * n) as f32).round().clamp(0.0, 255.0) as u8))
    })
}

fn scale(c: [f32; 3], k: f32) -> [f32; 3] {
    c.map(|v| v * k)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

/// A record with grooves, a sheen, the label in `tint` and a drop shadow. (x, y): position from the
/// center, in units of the image's short side.
fn vinyl(bg: [f32; 3], x: f32, y: f32, tint: [f32; 3]) -> [f32; 3] {
    const R: f32 = 0.41;
    const LABEL: f32 = 0.15;
    let r = x.hypot(y);
    let shadow = if (x - 0.02).hypot(y - 0.03) < R { 0.5 } else { 1.0 };
    if r >= R || r < 0.02 {
        return scale(bg, shadow);
    }
    if r < LABEL {
        return if r > LABEL - 0.014 { scale(tint, 0.7) } else { tint };
    }
    let sheen = (2.0 * (y.atan2(x) + FRAC_PI_4)).cos().max(0.0).powi(6) * 36.0;
    let groove = if r > LABEL + 0.03 && r < R - 0.025 { ((r * 170.0).sin() * 0.5 + 0.5) * 9.0 } else { 0.0 };
    let rim = if r > R - 0.012 { 20.0 } else { 0.0 };
    let v = 14.0 + sheen + groove + rim;
    [v, v, v + 4.0]
}

/// Soft rings spreading from a dot in `tint`. (x, y): position relative to the dot; `period`: ring
/// spacing (wider in small images, where fine rings would alias).
fn ripples(bg: [f32; 3], x: f32, y: f32, tint: [f32; 3], period: f32) -> [f32; 3] {
    const DOT: f32 = 0.13;
    let d = x.hypot(y);
    if d < DOT {
        return if d > DOT - 0.024 { scale(tint, 0.75) } else { tint };
    }
    let fade = (1.0 - (d - DOT) / 1.1).max(0.0);
    let wave = 0.5 + 0.5 * (TAU * (d - DOT) / period).cos();
    scale(mix(bg, [255.0; 3], 0.32 * fade * wave * wave), 1.0 - 0.12 * fade * (1.0 - wave))
}

/// A beamed pair of eighth notes (♫) in near-white with a soft shadow.
fn notes(bg: [f32; 3], x: f32, y: f32) -> [f32; 3] {
    let inside = |x: f32, y: f32| {
        let head = |cx: f32, cy: f32| {
            let (dx, dy) = (x - cx, y - cy);
            let (c, s) = (0.94, -0.34); // tilted ~20°
            let (u, v) = (dx * c - dy * s, dx * s + dy * c);
            (u / 0.105).powi(2) + (v / 0.075).powi(2) < 1.0
        };
        let bar = |ax: f32, ay: f32, bx: f32, by: f32, half: f32| {
            let (dx, dy) = (bx - ax, by - ay);
            let len2 = dx * dx + dy * dy;
            let t = ((x - ax) * dx + (y - ay) * dy) / len2;
            (0.0..=1.0).contains(&t) && ((x - ax) * dy - (y - ay) * dx).abs() / len2.sqrt() < half
        };
        head(-0.17, 0.21)
            || head(0.19, 0.13)
            || bar(-0.083, 0.19, -0.083, -0.25, 0.021)
            || bar(0.277, 0.11, 0.277, -0.33, 0.021)
            || bar(-0.104, -0.215, 0.298, -0.295, 0.045)
    };
    if inside(x, y) {
        mix([246.0, 246.0, 250.0], bg, 0.12)
    } else if inside(x - 0.022, y - 0.03) {
        scale(bg, 0.6)
    } else {
        bg
    }
}

/// Loads the cover for the playing track once and caches the scaled cells per area size.
#[derive(Default)]
pub struct ArtCache {
    track: Option<TrackId>,
    cover: Option<image::RgbImage>,
    seed: String,
    /// With `track`: ids are reassigned by a rescan, paths aren't.
    path: Option<PathBuf>,
    dominant: Option<Color>,
    scaled: Option<Scaled>,
    /// The new track's cover and its dominant color, being decoded in the background (see `poll`).
    loading: Option<Receiver<(Option<RgbImage>, Option<Color>)>>,
}

/// The image last drawn.
struct Scaled {
    /// Its pixel size and (placeholders only) the accent it was tinted with.
    key: (u32, u32, Option<Color>),
    img: RgbImage,
}

impl ArtCache {
    pub fn new() -> ArtCache {
        ArtCache::default()
    }

    /// Switch to `track` (no-op if unchanged). Its cover is decoded on a background thread (a big
    /// one takes ~100 ms, too long for the UI thread) and shows once `poll` picks it up; the
    /// previous one stays up until then.
    pub fn set_track(&mut self, track: Option<&Track>) {
        let id = track.map(|t| t.id);
        let path = track.map(|t| t.path.clone());
        if id == self.track && path == self.path {
            return;
        }
        self.track = id;
        self.path = path;
        self.seed = track.map(|t| format!("{}{}", t.album, t.artist)).unwrap_or_default();
        // replacing the receiver discards an earlier track's cover when it arrives
        self.loading = track.cloned().map(|track| {
            let (tx, rx) = mpsc::channel();
            let _ = std::thread::Builder::new().name("cover".into()).spawn(move || {
                let cover = load_cover(&track);
                let accent = cover.as_ref().and_then(dominant);
                let _ = tx.send((cover, accent));
            });
            rx
        });
        if track.is_none() {
            self.cover = None;
            self.dominant = None;
            self.scaled = None;
        }
    }

    /// Take the cover decoded since `set_track`. True when it just arrived: redraw.
    pub fn poll(&mut self) -> bool {
        let Some((cover, dominant)) = self.loading.as_ref().and_then(|rx| rx.try_recv().ok()) else { return false };
        self.loading = None;
        self.cover = cover;
        self.dominant = dominant;
        self.scaled = None;
        true
    }

    #[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
    pub fn has_cover(&self) -> bool {
        self.cover.is_some()
    }

    /// Draw into `area`, preserving aspect ratio (square art = width W cells x W/2 rows), centered.
    /// Uses the placeholder when there's no cover.
    ///
    /// Centered to the half cell vertically (odd pixel rows use "▄"/"▀" over the cell's existing
    /// background). The scaled image is cached until the size, track or (placeholder) accent
    /// changes. Areas under 4x2 cells draw nothing.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let area = area.intersection(buf.area);
        let rect = self.art_rect(area);
        if rect.is_empty() {
            return;
        }
        let (w, h) = self.pixel_size(area);
        let key = (w, h, self.cover.is_none().then_some(theme.accent));
        if self.scaled.as_ref().is_none_or(|s| s.key != key) {
            let img = match &self.cover {
                Some(cover) => imageops::resize(cover, w, h, FilterType::CatmullRom),
                None => placeholder(&self.seed, w, h, theme),
            };
            self.scaled = Some(Scaled { key, img });
        }
        let Some(Scaled { img, .. }) = &self.scaled else { return };
        let pad = i64::from(area.height) * 2 - i64::from(h);
        let rgb = |x: u32, row: i64| {
            let row = u32::try_from(row).ok().filter(|r| *r < h)?;
            let [r, g, b] = img.get_pixel(x, row).0;
            Some(Color::Rgb(r, g, b))
        };
        for y in rect.top()..rect.bottom() {
            let row = i64::from(y - area.y) * 2 - pad / 2;
            for (i, x) in (rect.left()..rect.right()).enumerate() {
                let cell = &mut buf[(x, y)];
                cell.modifier = Modifier::empty();
                match (rgb(i as u32, row), rgb(i as u32, row + 1)) {
                    (Some(top), Some(bottom)) => cell.set_symbol("▀").set_fg(top).set_bg(bottom),
                    (Some(top), None) => cell.set_symbol("▀").set_fg(top),
                    (None, Some(bottom)) => cell.set_symbol("▄").set_fg(bottom),
                    (None, None) => cell,
                };
            }
        }
    }

    /// The cells `render` covers in `area` (empty when nothing would be drawn), e.g. to put the title
    /// right below the art.
    pub fn art_rect(&self, area: Rect) -> Rect {
        if area.width < 4 || area.height < 2 {
            return Rect::new(area.x, area.y, 0, 0);
        }
        let (w, h) = self.pixel_size(area);
        let pad = u32::from(area.height) * 2 - h;
        let (top, bottom) = (pad / 2 / 2, (pad / 2 + h).div_ceil(2));
        Rect::new(area.x + (area.width - w as u16) / 2, area.y + top as u16, w as u16, (bottom - top) as u16)
    }

    /// Size in pixels (1 cell = 1 px wide, 2 px tall) of the art fitted into `area`: the cover's
    /// aspect ratio, square for the placeholder.
    fn pixel_size(&self, area: Rect) -> (u32, u32) {
        let (bw, bh) = (u32::from(area.width), u32::from(area.height) * 2);
        let (iw, ih) = self.cover.as_ref().map_or((1, 1), |c| c.dimensions());
        if bw * ih <= bh * iw {
            (bw, ((bw * ih + iw / 2) / iw).clamp(1, bh))
        } else {
            (((bh * iw + ih / 2) / ih).clamp(1, bw), bh)
        }
    }

    /// Most prominent vivid color of the cover (for ui.dynamic_accent).
    ///
    /// Computed once per track; None without a cover or for a grayscale one.
    pub fn dominant_color(&self) -> Option<Color> {
        self.dominant
    }
}

/// The most prominent vivid color: pixels are binned by hue and weighted by chroma (distance from
/// gray; dull, near-black and near-white pixels are skipped); the heaviest 30° hue range wins and
/// its mean color is lifted to read well on dark backgrounds and under dark text.
fn dominant(img: &RgbImage) -> Option<Color> {
    let small = imageops::thumbnail(img, img.width().min(64), img.height().min(64));
    let mut weight = [0.0f32; 36];
    let mut sum = [[0.0f32; 3]; 36];
    for p in small.pixels() {
        let (hue, s, l) = hsl(p.0);
        let chroma = s * (1.0 - (2.0 * l - 1.0).abs());
        if chroma < 0.15 || !(0.1..=0.92).contains(&l) {
            continue;
        }
        let bin = (hue / 10.0) as usize % 36;
        weight[bin] += chroma;
        sum[bin].iter_mut().zip(p.0).for_each(|(s, c)| *s += f32::from(c) * chroma);
    }
    let window = |i: usize| [35, 0, 1].map(|d| (i + d) % 36);
    let total = |i: usize| window(i).iter().map(|&b| weight[b]).sum::<f32>();
    let best = (0..36).max_by(|&a, &b| total(a).total_cmp(&total(b)))?;
    if total(best) < (small.width() * small.height()) as f32 * 0.005 {
        return None;
    }
    let mean = [0, 1, 2].map(|k| window(best).iter().map(|&b| sum[b][k]).sum::<f32>() / total(best));
    let (hue, s, l) = hsl(mean.map(|c| c.round() as u8));
    let (s, mut l) = (s.clamp(0.5, 0.9), l.clamp(0.55, 0.75));
    while luminance(from_hsl(hue, s, l)) < 0.2 && l < 0.85 {
        l += 0.02;
    }
    let [r, g, b] = from_hsl(hue, s, l);
    Some(Color::Rgb(r, g, b))
}

/// `accent` darkened until it has 3:1 contrast on a light `bg` (`dominant` only lifts colors, for
/// dark backgrounds); dark and unknown (`Reset`) backgrounds keep it as it is.
pub fn readable_on(accent: Color, bg: Color) -> Color {
    let (Some(mut rgb), Some(bg)) = (to_rgb(accent), to_rgb(bg)) else { return accent };
    let lb = luminance(bg);
    if lb <= 0.5 {
        return accent;
    }
    let (hue, s, mut l) = hsl(rgb);
    while (lb + 0.05) / (luminance(rgb) + 0.05) < 3.0 && l > 0.0 {
        l = (l - 0.02).max(0.0);
        rgb = from_hsl(hue, s, l);
    }
    Color::Rgb(rgb[0], rgb[1], rgb[2])
}

/// WCAG relative luminance.
fn luminance(rgb: [u8; 3]) -> f32 {
    let lin = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
}

/// (hue in degrees, saturation, lightness)
fn hsl(rgb: [u8; 3]) -> (f32, f32, f32) {
    let [r, g, b] = rgb.map(|c| f32::from(c) / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let hue = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (hue, s, l)
}

fn from_hsl(hue: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h = hue.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r, g, b].map(|v| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// Typical RGB of a terminal color; None for `Reset` (the terminal's own default).
fn to_rgb(color: Color) -> Option<[u8; 3]> {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [229, 229, 229],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [255, 255, 255],
    ];
    let index = match color {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => return Some([r, g, b]),
        Color::Indexed(i @ 16..=231) => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            let i = i - 16;
            return Some([level(i / 36), level(i / 6 % 6), level(i % 6)]);
        }
        Color::Indexed(i @ 232..) => return Some([8 + (i - 232) * 10; 3]),
        Color::Indexed(i) => i,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
    };
    Some(ANSI[usize::from(index)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::tests::{TempDir, track, wav};
    use image::ImageFormat;
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture};
    use lofty::tag::{Tag, TagExt, TagType};

    fn png(img: &RgbImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb(c))
    }

    /// Busy content that no bar detection mistakes for a solid color.
    fn pattern(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| Rgb([(x * 37 % 256) as u8, (y * 53 % 256) as u8, ((x + y) * 11 % 256) as u8]))
    }

    /// `inner` centered on a `w`x`h` canvas of `bg`.
    fn padded(inner: &RgbImage, w: u32, h: u32, bg: [u8; 3]) -> RgbImage {
        let mut img = solid(w, h, bg);
        imageops::replace(&mut img, inner, i64::from((w - inner.width()) / 2), i64::from((h - inner.height()) / 2));
        img
    }

    fn cell_colors(buf: &Buffer, x: u16, y: u16) -> (&str, Color, Color) {
        let c = &buf[(x, y)];
        (c.symbol(), c.fg, c.bg)
    }

    fn rgb_of(c: Color) -> [u8; 3] {
        match c {
            Color::Rgb(r, g, b) => [r, g, b],
            other => panic!("not rgb: {other:?}"),
        }
    }

    fn near(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 12)
    }

    #[test]
    fn placeholder_is_deterministic_and_sized() {
        let theme = Theme::default();
        let a = placeholder("Album Artist", 40, 40, &theme);
        assert_eq!(a, placeholder("Album Artist", 40, 40, &theme));
        assert_ne!(a, placeholder("Other Album", 40, 40, &theme));
        for (w, h) in [(0, 0), (1, 1), (20, 20), (60, 60), (37, 11), (11, 37), (130, 130)] {
            assert_eq!(placeholder("x", w, h, &theme).dimensions(), (w.max(1), h.max(1)));
        }
        // not a flat fill: it has a gradient and a motif
        let distinct: std::collections::HashSet<_> = a.pixels().map(|p| p.0).collect();
        assert!(distinct.len() > 50, "{}", distinct.len());
    }

    #[test]
    fn placeholder_motifs_use_the_accent() {
        let red = Theme { accent: Color::Rgb(255, 0, 0), ..Theme::default() };
        let blue = Theme { accent: Color::Rgb(0, 0, 255), ..Theme::default() };
        let tinted = (0..12).map(|i| format!("seed {i}")).filter(|s| placeholder(s, 30, 30, &red) != placeholder(s, 30, 30, &blue)).count();
        assert!((3..12).contains(&tinted), "vinyl and ripples take the accent, notes don't: {tinted}");
    }

    #[test]
    fn render_placeholder_at_several_sizes() {
        let theme = Theme::default();
        let mut art = ArtCache::new();
        for (w, h) in [(3, 1), (3, 2), (4, 1), (10, 1)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            art.render(buf.area, &mut buf, &theme);
            assert_eq!(buf, Buffer::empty(buf.area), "{w}x{h} is too small");
        }
        for (w, h) in [(4, 2), (20, 10), (60, 30), (40, 30), (13, 3)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            art.render(buf.area, &mut buf, &theme);
            let side = w.min(h * 2);
            let rect = art.art_rect(buf.area);
            assert_eq!((rect.width, rect.height), (side, side.div_ceil(2)), "{w}x{h}");
            for y in 0..h {
                for x in 0..w {
                    let (symbol, fg, _) = cell_colors(&buf, x, y);
                    let inside = rect.contains((x, y).into());
                    assert_eq!(inside, symbol != " ", "{w}x{h} at {x},{y}");
                    if inside {
                        assert!(matches!(fg, Color::Rgb(..)));
                    }
                }
            }
        }
        // a wide area centers the square horizontally
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 20));
        art.render(buf.area, &mut buf, &theme);
        assert_eq!(art.art_rect(buf.area), Rect::new(20, 0, 40, 20));
        assert_eq!(cell_colors(&buf, 19, 5).0, " ");
        assert_eq!(cell_colors(&buf, 20, 5).0, "▀");
        // an area reaching past the buffer is clipped first: 20x8 cells visible -> 16x16 px at x 12..28
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 12));
        art.render(Rect::new(10, 4, 40, 40), &mut buf, &theme);
        assert_eq!(cell_colors(&buf, 11, 4).0, " ");
        assert_eq!(cell_colors(&buf, 12, 4).0, "▀");
        assert_eq!(cell_colors(&buf, 27, 11).0, "▀");
        assert_eq!(cell_colors(&buf, 28, 11).0, " ");
    }

    #[test]
    fn render_cover_keeps_aspect_and_centers_to_the_half_cell() {
        let theme = Theme::default();
        let (red, blue) = ([220, 30, 30], [30, 30, 220]);
        let mut cover = solid(200, 100, red);
        imageops::replace(&mut cover, &solid(200, 50, blue), 0, 50);
        let mut art = ArtCache { cover: Some(cover), ..ArtCache::default() };

        // 10x5 cells = 10x10 px: the 2:1 cover is 10x5 px, 2 px padding on top -> rows 1..4, last half empty
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 5));
        art.render(buf.area, &mut buf, &theme);
        assert_eq!(art.art_rect(buf.area), Rect::new(0, 1, 10, 3));
        assert_eq!(cell_colors(&buf, 0, 0).0, " ");
        let (symbol, fg, bg) = cell_colors(&buf, 3, 1);
        assert_eq!(symbol, "▀");
        assert!(near(rgb_of(fg), red) && near(rgb_of(bg), red), "{fg:?} {bg:?}");
        let (symbol, fg, bg) = cell_colors(&buf, 3, 3);
        assert_eq!((symbol, bg), ("▀", Color::Reset), "bottom half keeps the cell background");
        assert!(near(rgb_of(fg), blue), "{fg:?}");
        assert_eq!(cell_colors(&buf, 0, 4).0, " ");

        // 10x6 cells = 10x12 px: 3.5 px padding -> the first pixel row lands in a bottom half
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        art.render(buf.area, &mut buf, &theme);
        assert_eq!(art.art_rect(buf.area), Rect::new(0, 1, 10, 3));
        let (symbol, fg, bg) = cell_colors(&buf, 5, 1);
        assert_eq!((symbol, bg), ("▄", Color::Reset));
        assert!(near(rgb_of(fg), red));

        // a tall area: the cover spans the full width
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 40));
        art.render(buf.area, &mut buf, &theme);
        assert_eq!(art.art_rect(buf.area), Rect::new(0, 15, 40, 10));
    }

    #[test]
    fn render_caches_per_size() {
        let theme = Theme::default();
        let mut art = ArtCache { cover: Some(pattern(300, 300)), ..ArtCache::default() };
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        art.render(buf.area, &mut buf, &theme);
        let first = buf.clone();
        assert_eq!(art.scaled.as_ref().map(|s| s.key), Some((20, 20, None)));
        art.render(buf.area, &mut buf, &theme);
        assert_eq!(buf, first);
        art.render(Rect::new(0, 0, 10, 10), &mut buf, &theme);
        assert_eq!(art.scaled.as_ref().map(|s| s.key), Some((10, 10, None)));
    }

    #[test]
    fn dominant_color_of_synthetic_covers() {
        let hue = |c: Color| hsl(rgb_of(c)).0;
        // mostly blue with a red corner: blue wins
        let mut img = solid(100, 100, [30, 60, 200]);
        imageops::replace(&mut img, &solid(30, 30, [220, 20, 20]), 0, 0);
        let c = dominant(&img).unwrap();
        assert!((200.0..250.0).contains(&hue(c)), "{c:?}");
        assert!(luminance(rgb_of(c)) >= 0.2, "readable on dark backgrounds: {c:?}");
        // white cover with a small green logo: the vivid part counts, the white doesn't
        let mut img = solid(100, 100, [250, 250, 250]);
        imageops::replace(&mut img, &solid(20, 20, [20, 160, 40]), 40, 40);
        assert!((100.0..150.0).contains(&hue(dominant(&img).unwrap())));
        // dark red is lifted into a readable red
        let c = dominant(&solid(10, 10, [120, 10, 10])).unwrap();
        assert!(hue(c) < 10.0 || hue(c) > 350.0);
        assert!(luminance(rgb_of(c)) >= 0.2);
        // grayscale, near-black and near-white images have none
        assert_eq!(dominant(&RgbImage::from_fn(64, 64, |x, _| Rgb([(x * 4) as u8; 3]))), None);
        assert_eq!(dominant(&solid(10, 10, [30, 4, 4])), None);
        assert_eq!(dominant(&solid(10, 10, [255, 250, 250])), None);
        assert_eq!(ArtCache::new().dominant_color(), None);
    }

    #[test]
    fn accent_is_darkened_only_on_light_backgrounds() {
        let yellow = Color::Rgb(244, 207, 37);
        let contrast = |a: Color, b: Color| {
            let (x, y) = (luminance(to_rgb(a).unwrap()), luminance(to_rgb(b).unwrap()));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        for light in [Color::Rgb(251, 241, 199), Color::Rgb(239, 241, 245), Color::White] {
            assert!(contrast(readable_on(yellow, light), light) >= 3.0, "{light:?}");
        }
        assert_eq!(readable_on(yellow, Color::Rgb(40, 40, 40)), yellow);
        assert_eq!(readable_on(yellow, Color::Reset), yellow);
    }

    #[test]
    fn trims_video_thumbnail_bars() {
        let cover = pattern(90, 90);
        // YouTube-style: square cover pillarboxed in a 16:9 frame with a solid (not black) color
        assert_eq!(trim_bars(padded(&cover, 160, 90, [90, 40, 30])), cover);
        // 16:9 frame letterboxed in 4:3
        let frame = pattern(120, 68);
        assert_eq!(trim_bars(padded(&frame, 120, 90, [0, 0, 0])), frame);
        // both: a pillarboxed cover inside a letterboxed 4:3 thumbnail
        let inner = padded(&pattern(68, 68), 120, 68, [40, 90, 30]);
        assert_eq!(trim_bars(padded(&inner, 120, 90, [0, 0, 0])).dimensions(), (68, 68));
        // compression noise in the bars is tolerated
        let noisy = RgbImage::from_fn(160, 90, |x, y| {
            if (35..125).contains(&x) { *cover.get_pixel(x - 35, y) } else { Rgb([90 + (x * y % 7) as u8, 40, 30]) }
        });
        assert_eq!(trim_bars(noisy).dimensions(), (90, 90));
        // left alone: crops that wouldn't give a cover shape (cinematic bars in a 16:9 frame, a
        // portrait strip on a flat background), square images, one-sided or off-center bars, solid images
        let cinematic = padded(&pattern(160, 68), 160, 90, [0, 0, 0]);
        assert_eq!(trim_bars(cinematic.clone()), cinematic);
        let portrait = padded(&pattern(60, 90), 160, 90, [20, 80, 90]);
        assert_eq!(trim_bars(portrait.clone()), portrait);
        let bordered = padded(&pattern(80, 80), 100, 100, [255, 255, 255]);
        assert_eq!(trim_bars(bordered.clone()), bordered);
        let mut one_side = pattern(160, 90);
        imageops::replace(&mut one_side, &solid(40, 90, [0, 0, 0]), 0, 0);
        assert_eq!(trim_bars(one_side.clone()), one_side);
        let mut off_center = solid(160, 90, [9, 9, 9]);
        imageops::replace(&mut off_center, &pattern(90, 90), 10, 0);
        assert_eq!(trim_bars(off_center.clone()).dimensions(), (160, 90));
        assert_eq!(trim_bars(solid(160, 90, [5, 5, 5])).dimensions(), (160, 90));
        assert_eq!(trim_bars(solid(1, 1, [5, 5, 5])).dimensions(), (1, 1));
    }

    #[test]
    fn prepare_downscales_and_flattens() {
        let big = prepare(DynamicImage::ImageRgb8(pattern(1600, 1200)));
        assert_eq!(big.dimensions(), (600, 450));
        let small = prepare(DynamicImage::ImageRgb8(pattern(300, 300)));
        assert_eq!(small, pattern(300, 300));
        let clear = prepare(DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 0]))));
        assert_eq!(clear.get_pixel(0, 0).0, MATTE.map(|c| c as u8));
    }

    fn names(files: &[PathBuf], root: &Path) -> Vec<String> {
        files.iter().map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn folder_cover_candidates() {
        let dir = TempDir::new("covers");
        let img = png(&solid(4, 4, [1, 2, 3]));
        let t = |rel: &str| Track { album: "Best Of".into(), ..track(&dir.file(rel, "audio")) };

        let song = t("a/01 - Song.mp3");
        for f in ["a/folder.png", "a/Cover.JPG", "a/best of.webp", "a/01 - Song.jpeg", "a/notes.txt", "a/Front.gif"] {
            dir.file(f, &img);
        }
        assert_eq!(
            names(&cover_files(&song), &dir.0),
            ["a/01 - Song.jpeg", "a/best of.webp", "a/Cover.JPG", "a/folder.png", "a/Front.gif"]
        );

        // the only image, whatever its name (hidden files don't count)
        let b = t("b/track.flac");
        dir.file("b/scan 1.jpg", &img);
        dir.file("b/._scan 1.jpg", "junk");
        assert_eq!(names(&cover_files(&b), &dir.0), ["b/scan 1.jpg"]);

        // ... unless it's another file's thumbnail (yt-dlp: "07 - Matrix.png" + "07 - Matrix.webm.part")
        let c = t("c/01 - Other.mp3");
        dir.file("c/07 - Matrix.png", &img);
        dir.file("c/07 - Matrix.webm.part", "partial download");
        assert!(cover_files(&c).is_empty());

        // art subfolders: named files first, then the only image there
        let d = t("d/t.mp3");
        dir.file("d/Scans/back.jpg", &img);
        dir.file("d/Scans/FRONT.jpg", &img);
        dir.file("d/Artwork/whatever.png", &img);
        dir.file("d/Other/cover.jpg", &img);
        let found = names(&cover_files(&d), &dir.0);
        assert!(found.contains(&"d/Scans/FRONT.jpg".to_string()) && found.contains(&"d/Artwork/whatever.png".to_string()));
        assert!(!found.iter().any(|f| f.contains("back") || f.contains("Other")), "{found:?}");

        // disc folders look in the album folder too
        let e = t("e/Album/CD 2/01.mp3");
        dir.file("e/Album/cover.png", &img);
        assert_eq!(names(&cover_files(&e), &dir.0), ["e/Album/cover.png"]);
        for (name, disc) in [("CD1", true), ("cd 2", true), ("Disc 03", true), ("disk_4", true), ("CD", false), ("Discography", false), ("cd1a", false)] {
            assert_eq!(is_disc_folder(Path::new(name)), disc, "{name}");
        }
    }

    #[test]
    fn load_cover_skips_corrupt_images() {
        let dir = TempDir::new("corrupt");
        let t = track(&dir.file("x/song.mp3", "not audio"));
        dir.file("x/cover.jpg", b"\xff\xd8\xff\xe0 definitely not a jpeg");
        assert_eq!(load_cover(&t), None);
        dir.file("x/folder.png", png(&solid(40, 20, [200, 100, 0])));
        assert_eq!(load_cover(&t).map(|c| c.dimensions()), Some((40, 20)));
        dir.file("x/cover.jpg", png(&solid(30, 30, [0, 100, 200])));
        assert_eq!(load_cover(&t).map(|c| c.dimensions()), Some((30, 30)), "wrong extension, sniffed format");
        dir.file("x/cover.jpg", png(&solid(MAX_DECODE + 1, 1, [0, 100, 200])));
        assert_eq!(load_cover(&t).map(|c| c.dimensions()), Some((40, 20)), "oversized images are skipped");
    }

    fn picture(img: &RgbImage, kind: PictureType) -> Picture {
        Picture::unchecked(png(img)).pic_type(kind).mime_type(MimeType::Png).build()
    }

    #[test]
    fn embedded_cover_prefers_the_front_cover() {
        let dir = TempDir::new("embedded-art");
        let path = dir.file("song.wav", wav());
        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(picture(&solid(8, 8, [250, 0, 0]), PictureType::Artist));
        tag.push_picture(picture(&solid(8, 8, [0, 0, 250]), PictureType::CoverFront));
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        dir.file("folder.jpg", png(&solid(16, 16, [0, 250, 0])));

        let mut art = ArtCache::new();
        art.set_track(Some(&track(&path)));
        let start = std::time::Instant::now();
        while !art.poll() && start.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(art.has_cover());
        assert_eq!(art.cover.as_ref().unwrap().get_pixel(0, 0).0, [0, 0, 250]);
        let accent = art.dominant_color().unwrap();
        assert!((220.0..260.0).contains(&hsl(rgb_of(accent)).0), "{accent:?}");

        art.set_track(None);
        assert!(!art.has_cover());
        assert_eq!(art.dominant_color(), None);

        // a picture that doesn't decode falls back to the folder
        let mut tag = Tag::new(TagType::Id3v2);
        tag.push_picture(Picture::unchecked(b"garbage".to_vec()).pic_type(PictureType::CoverFront).build());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        assert_eq!(load_cover(&track(&path)).unwrap().get_pixel(0, 0).0, [0, 250, 0]);
    }

    #[test]
    fn terminal_colors_to_rgb() {
        assert_eq!(to_rgb(Color::Reset), None);
        assert_eq!(to_rgb(Color::Rgb(1, 2, 3)), Some([1, 2, 3]));
        assert_eq!(to_rgb(Color::Indexed(16)), Some([0, 0, 0]));
        assert_eq!(to_rgb(Color::Indexed(196)), Some([255, 0, 0]));
        assert_eq!(to_rgb(Color::Indexed(231)), Some([255, 255, 255]));
        assert_eq!(to_rgb(Color::Indexed(232)), Some([8, 8, 8]));
        assert_eq!(to_rgb(Color::Indexed(255)), Some([238, 238, 238]));
        assert_eq!(to_rgb(Color::Indexed(1)), to_rgb(Color::Red));
        assert_eq!(to_rgb(Color::White), Some([255, 255, 255]));
    }

    #[test]
    fn hsl_round_trips() {
        for rgb in [[0, 0, 0], [255, 255, 255], [255, 0, 0], [12, 200, 90], [90, 12, 200], [128, 128, 64]] {
            let (h, s, l) = hsl(rgb);
            assert!(near(from_hsl(h, s, l), rgb), "{rgb:?}");
        }
    }
}
