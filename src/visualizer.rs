//! Audio analysis (FFT spectrum, waveform, VU levels) and the visualizer widget.
//!
//! [`Analyzer::update`] runs once per UI tick on the newest frames of the engine's [`SampleTap`].
//! One complex FFT carries both channels (left = real part, right = imaginary part), which yields
//! the mono, left and right spectra from a single transform. Bins are grouped into log-spaced
//! bands, tilted +3 dB/octave, scaled from `db_floor`..0 dB to 0..1 and animated with frame-rate
//! independent attack/release smoothing, gravity and peak hold. [`Visualizer`] draws the result in
//! one of the [`VisMode`] styles; [`render_mini`] draws a compact spectrum for the player bar.

use std::borrow::Cow;
use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};

use crate::config::VisualizerConfig;
use crate::dsp::SampleTap;
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VisMode {
    /// Classic spectrum bars rising from the bottom.
    #[default]
    Bars,
    /// Spectrum mirrored around the horizontal center (left channel up, right down when stereo).
    Mirror,
    /// Stacked blocks with gaps (LED meter look).
    Blocks,
    /// Oscilloscope waveform drawn with braille dots.
    Wave,
    /// Stereo VU meters with peak hold.
    Vu,
    /// A cassette whose reels turn while music plays.
    Cassette,
}

impl VisMode {
    pub const ALL: [VisMode; 6] = [VisMode::Bars, VisMode::Mirror, VisMode::Blocks, VisMode::Wave, VisMode::Vu, VisMode::Cassette];
    pub fn label(self) -> &'static str {
        match self {
            VisMode::Bars => "bars",
            VisMode::Mirror => "mirror",
            VisMode::Blocks => "blocks",
            VisMode::Wave => "wave",
            VisMode::Vu => "vu",
            VisMode::Cassette => "cassette",
        }
    }
    pub fn next(self) -> VisMode {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

/// How many bars fit in `width` columns with the configured bar width / gap (or `cfg.bars` if set).
pub fn band_count(width: u16, cfg: &VisualizerConfig) -> usize {
    if cfg.bars > 0 {
        return cfg.bars as usize;
    }
    let gap = cfg.bar_gap as usize;
    ((width as usize + gap) / (cfg.bar_width.max(1) as usize + gap)).max(1)
}

/// Most spectrum bands computed (more than any terminal can show; larger requests are clamped).
const MAX_BANDS: usize = 1024;
/// Points per channel in [`Analyzer::wave`].
const WAVE_LEN: usize = 512;
/// Frames the oscilloscope shows (averaged in pairs down to `WAVE_LEN`).
const WAVE_FRAMES: usize = 2 * WAVE_LEN;
/// Frames searched before the shown window for a trigger point that steadies the trace.
const TRIGGER_FRAMES: usize = 1024;
/// The waveform halves every this many seconds once playback stops.
const WAVE_HALF_LIFE: f32 = 0.06;
/// RMS integration time of the VU meters.
const VU_WINDOW_SECS: f32 = 0.05;
/// Level drawn as an empty VU meter.
const VU_FLOOR_DB: f32 = -60.0;
/// VU scale labels in placement priority: when crowded, later labels give way.
const VU_MARKS: [i32; 7] = [0, -60, -20, -40, -10, -6, -3];
/// Spectrum tilt around the pivot, so music's natural treble roll-off doesn't look bass-heavy.
const TILT_DB_PER_OCTAVE: f32 = 3.0;
const TILT_PIVOT_HZ: f32 = 1000.0;
/// `smoothing` is a per-frame factor at this rate; other frame rates get the equivalent curve.
const REF_FPS: f32 = 60.0;
/// Longest step animated at once (the first tick after an idle stretch).
const MAX_DT: f32 = 0.25;
/// Values below this snap to zero once their target is zero, so the analyzer settles.
const SETTLE: f32 = 1e-3;

const LOWER: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const LEFT: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
/// Braille dot bits by [row][column] within a cell (2x4 dots).
const BRAILLE: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

pub struct Analyzer {
    /// Spectrum of the mono mix, and of each channel for the stereo styles.
    mono: Meters,
    left: Meters,
    right: Meters,
    /// Two meters: left and right level.
    vu: Meters,
    /// Oscilloscope trace per channel, `WAVE_LEN` points in -1..=1.
    wave: [Vec<f32>; 2],
    last: Option<Instant>,
    /// Something was on screen before the last step: the UI must draw once more to clear it.
    settling: bool,
    transform: Transform,
    /// Frames copied from the tap (reused every tick).
    frames: Vec<[f32; 2]>,
    /// Power per FFT bin (0..=size/2) of the mono mix, left and right.
    power: [Vec<f32>; 3],
    /// This tick's band levels (0..=1) for mono, left and right, before animation.
    targets: [Vec<f32>; 3],
    bands_map: Vec<Band>,
    /// (bands, min_freq, max_freq, fft size, sample rate) that `bands_map` was built for.
    map_key: (usize, f32, f32, usize, u32),
    /// Seconds animated while playing: turns the cassette's reels.
    spin: f32,
}

impl Default for Analyzer {
    fn default() -> Self {
        Analyzer::new()
    }
}

impl Analyzer {
    pub fn new() -> Analyzer {
        Analyzer {
            mono: Meters::default(),
            left: Meters::default(),
            right: Meters::default(),
            vu: Meters::new(2),
            wave: [vec![0.0; WAVE_LEN], vec![0.0; WAVE_LEN]],
            last: None,
            settling: false,
            transform: Transform::new(VisualizerConfig::default().fft_size),
            frames: Vec::new(),
            power: Default::default(),
            targets: Default::default(),
            bands_map: Vec::new(),
            map_key: (0, 0.0, 0.0, 0, 0),
            spin: 0.0,
        }
    }

    /// Read the newest samples from `tap` and update bands / waveform / VU. `bands` = number of
    /// spectrum bands wanted (from `band_count`). When `playing` is false everything decays to 0.
    /// Uses its own clock for frame-rate-independent smoothing and falloff.
    pub fn update(&mut self, tap: Option<&SampleTap>, cfg: &VisualizerConfig, bands: usize, playing: bool) {
        let now = Instant::now();
        let dt = self.last.map_or(0.0, |t| now.duration_since(t).as_secs_f32().min(MAX_DT));
        self.last = Some(now);
        self.advance(tap.filter(|_| playing), cfg, bands, dt);
    }

    /// One analysis + animation step of `dt` seconds; no tap = silence.
    fn advance(&mut self, tap: Option<&SampleTap>, cfg: &VisualizerConfig, bands: usize, dt: f32) {
        self.settling = self.visible();
        if tap.is_some() {
            self.spin += dt;
        }
        let bands = bands.clamp(1, MAX_BANDS);
        for t in &mut self.targets {
            t.clear();
            t.resize(bands, 0.0);
        }
        let vu = match tap.and_then(|tap| self.analyze(tap, cfg, bands)) {
            Some(vu) => vu,
            None => {
                self.decay_wave(dt);
                [0.0; 2]
            }
        };
        let motion = Motion::new(cfg, dt);
        for (meters, targets) in [&mut self.mono, &mut self.left, &mut self.right].into_iter().zip(&self.targets) {
            meters.resize(bands);
            meters.step(targets, &motion);
        }
        self.vu.step(&vu, &motion);
    }

    /// Analyze the newest frames into `targets` and the waveform; returns the VU targets.
    /// None if the tap had nothing to read.
    fn analyze(&mut self, tap: &SampleTap, cfg: &VisualizerConfig, bands: usize) -> Option<[f32; 2]> {
        let n = cfg.fft_size.clamp(512, 16384).next_power_of_two();
        if self.transform.len() != n {
            self.transform = Transform::new(n);
        }
        let rate = match tap.latest(n.max(WAVE_FRAMES + TRIGGER_FRAMES), &mut self.frames) {
            0 => 44100,
            rate => rate,
        };
        if self.frames.is_empty() {
            return None;
        }
        self.spectrum();
        self.band_targets(cfg, bands, rate);
        self.capture_wave();
        Some(self.vu_levels(rate))
    }

    /// Windowed FFT of the newest frames, left in the real and right in the imaginary part, split
    /// into per-bin power of the mono mix, left and right. Two real signals packed into one
    /// complex one separate by conjugate symmetry: L[k] = (Z[k] + Z*[N-k]) / 2 and
    /// R[k] = (Z[k] - Z*[N-k]) / 2i.
    fn spectrum(&mut self) {
        let t = &mut self.transform;
        let n = t.len();
        let recent = &self.frames[self.frames.len().saturating_sub(n)..];
        let pad = n - recent.len(); // a tap smaller than the FFT: zeros before the data
        for (i, z) in t.buf.iter_mut().enumerate() {
            *z = match i.checked_sub(pad) {
                Some(j) => Complex::new(recent[j][0] * t.window[i], recent[j][1] * t.window[i]),
                None => Complex::default(),
            };
        }
        t.fft.process_with_scratch(&mut t.buf, &mut t.scratch);
        let half = n / 2;
        for p in &mut self.power {
            p.resize(half + 1, 0.0);
        }
        let [mono, left, right] = &mut self.power;
        for k in 0..=half {
            let a = t.buf[k];
            let b = t.buf[(n - k) % n].conj();
            let l = (a + b) * 0.5;
            let d = (a - b) * 0.5;
            let r = Complex::new(d.im, -d.re);
            mono[k] = ((l + r) * 0.5).norm_sqr() * t.norm;
            left[k] = l.norm_sqr() * t.norm;
            right[k] = r.norm_sqr() * t.norm;
        }
    }

    fn band_targets(&mut self, cfg: &VisualizerConfig, bands: usize, rate: u32) {
        let n = self.transform.len();
        let key = (bands, cfg.min_freq, cfg.max_freq, n, rate);
        if key != self.map_key {
            self.bands_map = band_map(bands, cfg.min_freq, cfg.max_freq, n, rate);
            self.map_key = key;
        }
        let floor = cfg.db_floor.min(-1.0);
        for (targets, power) in self.targets.iter_mut().zip(&self.power) {
            for (t, band) in targets.iter_mut().zip(&self.bands_map) {
                let db = 10.0 * band.power(power).max(1e-20).log10() + band.tilt_db;
                *t = (1.0 - db / floor).clamp(0.0, 1.0);
            }
        }
    }

    /// Copy the most recent `WAVE_FRAMES` frames into the waveform, starting at a trigger point
    /// when there is one so periodic sounds stand still on screen.
    fn capture_wave(&mut self) {
        let f = &self.frames;
        let shown = WAVE_FRAMES.min(f.len());
        let newest = f.len() - shown;
        let start = trigger(f, newest.saturating_sub(TRIGGER_FRAMES), newest).unwrap_or(newest);
        let src = &f[start..start + shown];
        let [left, right] = &mut self.wave;
        for j in 0..WAVE_LEN {
            let a = j * shown / WAVE_LEN;
            let chunk = &src[a..((j + 1) * shown / WAVE_LEN).max(a + 1)];
            let n = chunk.len() as f32;
            left[j] = chunk.iter().map(|x| x[0]).sum::<f32>() / n;
            right[j] = chunk.iter().map(|x| x[1]).sum::<f32>() / n;
        }
    }

    /// RMS level per channel over the last `VU_WINDOW_SECS`, dB-scaled to 0..=1.
    fn vu_levels(&self, rate: u32) -> [f32; 2] {
        let f = &self.frames;
        let win = ((rate as f32 * VU_WINDOW_SECS) as usize).clamp(1, f.len());
        let mut sum = [0.0f32; 2];
        for [l, r] in &f[f.len() - win..] {
            sum[0] += l * l;
            sum[1] += r * r;
        }
        sum.map(|s| {
            let db = 10.0 * (s / win as f32).max(1e-12).log10();
            (1.0 - db / VU_FLOOR_DB).clamp(0.0, 1.0)
        })
    }

    fn decay_wave(&mut self, dt: f32) {
        let keep = 0.5f32.powf(dt / WAVE_HALF_LIFE);
        for v in self.wave.iter_mut().flatten() {
            *v = if (*v * keep).abs() < SETTLE { 0.0 } else { *v * keep };
        }
    }

    /// Spectrum bands, 0..=1, low to high frequency.
    pub fn bands(&self) -> &[f32] {
        &self.mono.level
    }

    /// Peak markers per band, 0..=1.
    pub fn peaks(&self) -> &[f32] {
        &self.mono.peak
    }

    /// Per-channel spectra (left, right), same length as `bands`.
    pub fn bands_lr(&self) -> (&[f32], &[f32]) {
        (&self.left.level, &self.right.level)
    }

    /// Per-channel peak markers (left, right), same length as `bands`.
    pub fn peaks_lr(&self) -> (&[f32], &[f32]) {
        (&self.left.peak, &self.right.peak)
    }

    /// Recent waveform per channel, -1..=1 (same length).
    pub fn wave(&self) -> (&[f32], &[f32]) {
        (&self.wave[0], &self.wave[1])
    }

    /// Left/right level, 0..=1 (dB-scaled).
    pub fn vu(&self) -> [f32; 2] {
        [self.vu.level[0], self.vu.level[1]]
    }

    pub fn vu_peaks(&self) -> [f32; 2] {
        [self.vu.peak[0], self.vu.peak[1]]
    }

    /// True while anything is still moving (so the UI keeps redrawing while bars fall), including
    /// the step that settled everything, so the final empty frame gets drawn too.
    pub fn active(&self) -> bool {
        self.settling || self.visible()
    }

    fn visible(&self) -> bool {
        [&self.mono, &self.left, &self.right, &self.vu].iter().any(|m| m.visible())
            || self.wave.iter().flatten().any(|v| *v != 0.0)
    }
}

/// A planned FFT with its window and buffers (rebuilt when `fft_size` changes).
struct Transform {
    fft: Arc<dyn Fft<f32>>,
    /// Hann window, one coefficient per input.
    window: Vec<f32>,
    /// Scales |X|² so a full-scale sine reads 1.0 (0 dB).
    norm: f32,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
}

impl Transform {
    fn new(n: usize) -> Transform {
        let fft = FftPlanner::new().plan_fft_forward(n);
        let window: Vec<f32> = (0..n).map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos()).collect();
        // A sine of amplitude A peaks at |X| = A/2 * sum(window).
        let norm = (2.0 / window.iter().sum::<f32>()).powi(2);
        Transform { scratch: vec![Complex::default(); fft.get_inplace_scratch_len()], buf: vec![Complex::default(); n], fft, window, norm }
    }

    fn len(&self) -> usize {
        self.window.len()
    }
}

/// Animated meters (spectrum bands or VU channels): smoothed levels plus falling peak markers.
#[derive(Default)]
struct Meters {
    level: Vec<f32>,
    peak: Vec<f32>,
    /// Seconds each peak still holds before it starts to fall.
    hold: Vec<f32>,
    /// Current fall speed of each peak (heights/second); gravity accelerates it.
    fall: Vec<f32>,
}

impl Meters {
    fn new(n: usize) -> Meters {
        Meters { level: vec![0.0; n], peak: vec![0.0; n], hold: vec![0.0; n], fall: vec![0.0; n] }
    }

    /// Change the number of meters, stretching the current picture so a resize doesn't flash.
    fn resize(&mut self, n: usize) {
        if self.level.len() != n {
            *self = Meters { level: resample(&self.level, n), peak: resample(&self.peak, n), ..Meters::new(n) };
        }
    }

    fn step(&mut self, targets: &[f32], m: &Motion) {
        for (i, &target) in targets.iter().enumerate() {
            let old = self.level[i];
            let keep = if target > old { m.attack } else { m.release };
            // smoothing toward the target, but never falling faster than gravity allows
            let mut level = (target + (old - target) * keep).max(old - m.drop);
            if target <= 0.0 && level < SETTLE {
                level = 0.0;
            }
            self.level[i] = level;
            if level >= self.peak[i] {
                self.peak[i] = level;
                self.hold[i] = m.hold;
                self.fall[i] = 0.0;
            } else if self.hold[i] > 0.0 {
                self.hold[i] -= m.dt;
            } else {
                self.fall[i] += m.gravity * m.dt;
                self.peak[i] = (self.peak[i] - self.fall[i] * m.dt).max(level);
            }
        }
    }

    fn visible(&self) -> bool {
        self.level.iter().chain(&self.peak).any(|v| *v > 0.0)
    }
}

/// Per-step animation constants from the config and the elapsed time.
struct Motion {
    dt: f32,
    /// Share of the distance to the target kept this step when rising / falling.
    attack: f32,
    release: f32,
    /// Most a level may fall this step (falloff 0 = no limit).
    drop: f32,
    hold: f32,
    /// Acceleration of falling peaks, heights/s².
    gravity: f32,
}

impl Motion {
    fn new(cfg: &VisualizerConfig, dt: f32) -> Motion {
        let smoothing = cfg.smoothing.max(0.0).min(0.99); // not clamp: NaN must become 0
        let frames = dt * REF_FPS;
        let falloff = cfg.falloff.max(0.0);
        Motion {
            dt,
            attack: smoothing.powi(3).powf(frames),
            release: smoothing.powf(frames),
            drop: if falloff > 0.0 { falloff * dt } else { f32::INFINITY },
            hold: cfg.peak_hold_ms as f32 / 1000.0,
            gravity: 2.0 * falloff.max(0.5),
        }
    }
}

#[derive(Clone, Copy)]
enum Bins {
    /// The loudest bin in lo..=hi.
    Max(usize, usize),
    /// A band narrower than one bin: magnitude interpolated at fractional bin `k + frac`, so low
    /// bands form a smooth curve instead of steps.
    Lerp(usize, f32),
}

struct Band {
    bins: Bins,
    tilt_db: f32,
}

impl Band {
    fn power(&self, p: &[f32]) -> f32 {
        match self.bins {
            Bins::Max(lo, hi) => p[lo..=hi].iter().fold(0.0f32, |m, &v| m.max(v)),
            Bins::Lerp(k, frac) => (p[k].sqrt() * (1.0 - frac) + p[k + 1].sqrt() * frac).powi(2),
        }
    }
}

/// `bands` log-spaced bands between the configured frequencies, for an `n`-point FFT at `rate`.
fn band_map(bands: usize, min_hz: f32, max_hz: f32, n: usize, rate: u32) -> Vec<Band> {
    let bin_hz = rate as f32 / n as f32;
    let half = n / 2;
    let (lo_hz, hi_hz) = freq_limits(min_hz, max_hz, rate);
    (0..bands)
        .map(|i| {
            let (lo, hi) = band_edges(i, bands, lo_hz, hi_hz);
            let center = (lo * hi).sqrt();
            let (a, b) = (lo / bin_hz, hi / bin_hz);
            let bins = if b - a >= 1.0 {
                Bins::Max((a.ceil() as usize).min(half), (b.ceil() as usize - 1).min(half))
            } else {
                let c = center / bin_hz;
                let k = (c as usize).min(half - 1);
                Bins::Lerp(k, (c - k as f32).clamp(0.0, 1.0))
            };
            Band { bins, tilt_db: TILT_DB_PER_OCTAVE * (center / TILT_PIVOT_HZ).log2() }
        })
        .collect()
}

/// The configured range made usable: below Nyquist, low < high (NaN falls back to the limits).
fn freq_limits(min_hz: f32, max_hz: f32, rate: u32) -> (f32, f32) {
    let hi = max_hz.min(rate as f32 / 2.0).max(2.0);
    (min_hz.max(1.0).min(hi / 2.0), hi)
}

/// Frequency range of band `i` of `bands` log-spaced bands spanning lo_hz..hi_hz.
fn band_edges(i: usize, bands: usize, lo_hz: f32, hi_hz: f32) -> (f32, f32) {
    let at = |j: usize| lo_hz * (hi_hz / lo_hz).powf(j as f32 / bands as f32);
    (at(i), at(i + 1))
}

/// The last rising zero crossing of the mono mix at a frame in from..=to, counting only crossings
/// that follow a dip below -10% of the local peak (so noise around zero doesn't trigger).
fn trigger(f: &[[f32; 2]], from: usize, to: usize) -> Option<usize> {
    let mono = |i: usize| f[i][0] + f[i][1];
    let peak = (from..=to).fold(0.0f32, |m, i| m.max(mono(i).abs()));
    if peak < 1e-4 {
        return None;
    }
    let (mut armed, mut found) = (false, None);
    for i in from.max(1)..=to {
        let v = mono(i);
        if v < -0.1 * peak {
            armed = true;
        } else if armed && v >= 0.0 && mono(i - 1) < 0.0 {
            found = Some(i);
            armed = false;
        }
    }
    found
}

/// Stretch or shrink `v` to `n` values: linear interpolation when growing, the maximum of each
/// span when shrinking (so narrow peaks survive).
fn resample(v: &[f32], n: usize) -> Vec<f32> {
    let len = v.len();
    if len == 0 {
        return vec![0.0; n];
    }
    if n <= len {
        return (0..n).map(|i| v[i * len / n..(i + 1) * len / n].iter().fold(0.0f32, |m, &x| m.max(x))).collect();
    }
    let scale = (len - 1) as f32 / (n - 1) as f32;
    (0..n)
        .map(|i| {
            let pos = i as f32 * scale;
            let k = (pos as usize).min(len - 1);
            let frac = pos - k as f32;
            v[k] * (1.0 - frac) + v[(k + 1).min(len - 1)] * frac
        })
        .collect()
}

fn fit(v: &[f32], n: usize) -> Cow<'_, [f32]> {
    if v.len() == n { Cow::Borrowed(v) } else { Cow::Owned(resample(v, n)) }
}

/// `v` (0..=1) as a whole number of `total` steps.
fn steps(v: f32, total: usize) -> usize {
    (v.clamp(0.0, 1.0) * total as f32).round() as usize
}

/// Position of step `i` of `n` as 0..=1.
fn frac(i: usize, n: usize) -> f32 {
    i as f32 / n.saturating_sub(1).max(1) as f32
}

/// Glyph filling the top `k` eighths of a cell, and whether it needs reverse video: Unicode only
/// has a few upper blocks, the rest are drawn as the complementary lower block with foreground and
/// background swapped (the glyph part then shows the cell's own background).
fn upper(k: usize) -> (&'static str, bool) {
    match k {
        0 => (" ", false),
        1 => ("▔", false),
        4 => ("▀", false),
        k if k >= 8 => ("█", false),
        k => (LOWER[8 - k], true),
    }
}

/// Row (from the top) and glyph of a thin peak line `y8` eighths below the top of an `h`-row area.
fn peak_mark(y8: f32, h: usize) -> (usize, &'static str) {
    let row = ((y8.max(0.0) / 8.0) as usize).min(h - 1);
    (row, if y8 - ((row * 8) as f32) < 4.0 { "▔" } else { "▁" })
}

/// Draw `symbol` into `width` cells from (x, y). Only the foreground (and modifiers) is set.
fn span(buf: &mut Buffer, x: u16, width: u16, y: u16, symbol: &str, style: Style) {
    for x in x..x.saturating_add(width) {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_symbol(symbol).set_style(style);
        }
    }
}

/// Turn on braille dot (x, y) of `area` (2x4 dots per cell), keeping the cell's other dots.
fn set_dot(buf: &mut Buffer, area: Rect, x: usize, y: usize, color: Color) {
    let Some(cell) = buf.cell_mut((area.x + (x / 2) as u16, area.y + (y / 4) as u16)) else { return };
    let dots = cell.symbol().chars().next().map(|c| c as u32).filter(|c| (0x2800..=0x28ff).contains(c)).map_or(0, |c| c - 0x2800);
    let ch = char::from_u32(0x2800 | dots | BRAILLE[y % 4][x % 2]).unwrap_or(' ');
    cell.set_char(ch).set_fg(color);
}

/// Horizontal placement of bars: `n` bars `width` columns wide, `gap` apart, centered.
struct Columns {
    left: u16,
    n: usize,
    width: usize,
    gap: usize,
}

impl Columns {
    /// The configured bar width and gap when they fit; otherwise the gap shrinks first, then the
    /// bars, and finally neighboring bands merge so every bar keeps at least one column.
    fn new(area: Rect, bands: usize, cfg: &VisualizerConfig) -> Option<Columns> {
        let avail = area.width as usize;
        if bands == 0 || avail == 0 {
            return None;
        }
        let (mut n, mut width, mut gap) = (bands, cfg.bar_width.max(1) as usize, cfg.bar_gap as usize);
        if n * width + (n - 1) * gap > avail {
            gap = if n > 1 { avail.saturating_sub(n * width) / (n - 1) } else { 0 };
            if n * width > avail {
                width = (avail / n).max(1);
            }
            n = n.min(avail);
        }
        let used = n * width + (n - 1) * gap;
        Some(Columns { left: area.x + ((avail - used) / 2) as u16, n, width, gap })
    }

    fn x(&self, i: usize) -> u16 {
        self.left + (i * (self.width + self.gap)) as u16
    }
}

/// Renders the analyzer in `mode`. Transparent background (draws only foreground cells).
pub struct Visualizer<'a> {
    pub analyzer: &'a Analyzer,
    pub cfg: &'a VisualizerConfig,
    pub theme: &'a Theme,
    pub mode: VisMode,
}

impl Widget for Visualizer<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        if area.is_empty() {
            return;
        }
        match self.mode {
            VisMode::Bars => self.bars(area, buf),
            VisMode::Mirror => self.mirror(area, buf),
            VisMode::Blocks => self.blocks(area, buf),
            VisMode::Wave => self.wave(area, buf),
            VisMode::Vu => self.vu(area, buf),
            VisMode::Cassette => self.cassette(area, buf),
        }
    }
}

impl Visualizer<'_> {
    /// Color at height / position `t` (0..=1).
    fn color(&self, t: f32) -> Style {
        Style::new().fg(if self.cfg.gradient { self.theme.gradient_at(t) } else { self.theme.accent })
    }

    /// orbit's tape deck: two reels joined by the tape over the capstan, in a shell when there's
    /// room, down to a one-line "(╲)─▓▓─(│)".
    fn cassette(&self, area: Rect, buf: &mut Buffer) {
        const SPOKES: [[&str; 3]; 4] = [
            ["       ", " ──o── ", "       "],
            [" ╲     ", "   o   ", "     ╲ "],
            ["   |   ", "   o   ", "   |   "],
            ["     ╱ ", "   o   ", " ╱     "],
        ];
        const TINY: [&str; 4] = ["─", "╲", "│", "╱"];
        let th = self.theme;
        let outline = Style::new().fg(th.dim);
        let spoke = Style::new().fg(th.accent).add_modifier(Modifier::BOLD);
        let tape = Style::new().fg(th.accent2);
        let phase = (self.analyzer.spin / 0.14) as usize;
        let (left, right) = (&SPOKES[phase % 4], &SPOKES[(phase + 2) % 4]);
        // a line of spans centered in `area` at row `y`, clipped to it
        let put = |buf: &mut Buffer, y: u16, spans: Vec<Span<'static>>| {
            let w = spans.iter().map(Span::width).sum::<usize>() as u16;
            let x = area.x + area.width.saturating_sub(w) / 2;
            buf.set_line(x, y, &Line::from(spans), area.right() - x);
        };
        if area.width < 37 || area.height < 5 {
            let s = |i: usize| Span::styled(TINY[i % 4], spoke);
            let o = |t: &'static str| Span::styled(t, outline);
            put(buf, area.y + area.height / 2, vec![o("("), s(phase), o(")─"), Span::styled("▓▓", tape), o("─("), s(phase + 2), o(")")]);
            return;
        }
        let reel = |r: usize, s: &[&'static str; 3]| match r {
            0 | 4 => vec![Span::styled("  ·─────·  ", outline)],
            1 => vec![Span::styled(" ╱", outline), Span::styled(s[0], spoke), Span::styled("╲ ", outline)],
            2 => vec![Span::styled(" ⎸", outline), Span::styled(s[1], spoke), Span::styled(" ⎸", outline)],
            _ => vec![Span::styled(" ╲", outline), Span::styled(s[2], spoke), Span::styled("╱ ", outline)],
        };
        let capstan = |r: usize| match r {
            1 => vec![Span::styled("     ┌───┐     ", outline)],
            2 => vec![
                Span::styled("── ", tape),
                Span::styled("◎", Style::new().fg(th.accent)),
                Span::styled("─", tape),
                Span::styled("┤", outline),
                Span::styled("▓▒▓", tape),
                Span::styled("├", outline),
                Span::styled("─", tape),
                Span::styled("◎", Style::new().fg(th.accent)),
                Span::styled(" ──", tape),
            ],
            3 => vec![Span::styled("     └───┘     ", outline)],
            _ => vec![Span::raw("               ")],
        };
        let mut top = area.y + (area.height - 5) / 2;
        if area.width >= 45 && area.height >= 9 {
            let shell = Rect::new(area.x + (area.width - 45) / 2, area.y + (area.height - 9) / 2, 45, 9);
            let title = Line::from(vec![Span::styled(" ◈ ", tape), Span::styled("ORBIT · C-90 ", spoke)]);
            Block::bordered().border_type(BorderType::Rounded).border_style(outline).title(title).render(shell, buf);
            let label = vec![Span::styled("TYPE I  ·  STEREO  ·  SIDE A", outline)];
            put(buf, shell.y + 1, label);
            top = shell.y + 3;
        }
        for r in 0..5 {
            put(buf, top + r as u16, [reel(r, left), capstan(r), reel(r, right)].concat());
        }
    }

    fn peak_color(&self, t: f32) -> Style {
        Style::new().fg(if self.cfg.gradient { self.theme.gradient_at(t) } else { self.theme.accent2 })
    }

    fn bars(&self, area: Rect, buf: &mut Buffer) {
        let a = self.analyzer;
        let Some(cols) = Columns::new(area, a.bands().len(), self.cfg) else { return };
        let (levels, peaks) = (fit(a.bands(), cols.n), fit(a.peaks(), cols.n));
        let h = area.height as usize;
        for (i, (&v, &p)) in levels.iter().zip(peaks.iter()).enumerate() {
            let x = cols.x(i);
            let e = steps(v, h * 8);
            let top = e.div_ceil(8).min(h); // rows the bar touches
            for row in 0..top {
                span(buf, x, cols.width as u16, area.bottom() - 1 - row as u16, LOWER[(e - row * 8).min(8)], self.color(frac(row, h)));
            }
            let pe = p.clamp(0.0, 1.0) * (h * 8) as f32;
            if self.cfg.peaks && pe >= 0.5 {
                let (r, glyph) = peak_mark((h * 8) as f32 - pe, h);
                if r < h - top {
                    span(buf, x, cols.width as u16, area.y + r as u16, glyph, self.peak_color(frac(h - 1 - r, h)));
                }
            }
        }
    }

    fn mirror(&self, area: Rect, buf: &mut Buffer) {
        let a = self.analyzer;
        let ((up, down), (up_peaks, down_peaks)) =
            if self.cfg.stereo { (a.bands_lr(), a.peaks_lr()) } else { ((a.bands(), a.bands()), (a.peaks(), a.peaks())) };
        let Some(cols) = Columns::new(area, up.len(), self.cfg) else { return };
        let n = cols.n;
        let (up, down, up_peaks, down_peaks) = (fit(up, n), fit(down, n), fit(up_peaks, n), fit(down_peaks, n));
        let h = area.height as usize;
        let mid = h * 4; // the center line, in eighths from the top
        // color by rows away from the center line
        let away = |row: usize| frac((2 * row + 1).abs_diff(h) / 2, (h - 1) / 2 + 1);
        let width = cols.width as u16;
        for i in 0..n {
            let x = cols.x(i);
            let (u, d) = (steps(up[i], mid), steps(down[i], mid));
            for row in 0..h {
                let (top, bottom) = (row * 8, row * 8 + 8);
                let (glyph, reversed) = if bottom <= mid {
                    (LOWER[(bottom + u).saturating_sub(mid).min(8)], false)
                } else if top >= mid {
                    upper((mid + d).saturating_sub(top).min(8))
                } else {
                    // middle row of an odd height: upper half belongs to `up`, lower half to `down`
                    let glyph = match (u >= 2, d >= 2) {
                        (true, true) => "█",
                        (true, false) => "▀",
                        (false, true) => "▄",
                        _ => " ",
                    };
                    (glyph, false)
                };
                if glyph != " " {
                    let style = self.color(away(row));
                    let style = if reversed { style.add_modifier(Modifier::REVERSED) } else { style };
                    span(buf, x, width, area.y + row as u16, glyph, style);
                }
            }
            if !self.cfg.peaks {
                continue;
            }
            let pe = up_peaks[i].clamp(0.0, 1.0) * mid as f32;
            if pe >= 0.5 {
                let (r, glyph) = peak_mark(mid as f32 - pe, h);
                if r < (mid - u) / 8 {
                    span(buf, x, width, area.y + r as u16, glyph, self.peak_color(away(r)));
                }
            }
            let pe = down_peaks[i].clamp(0.0, 1.0) * mid as f32;
            if pe >= 0.5 {
                let (r, glyph) = peak_mark(mid as f32 + pe, h);
                if r >= (mid + d).div_ceil(8) {
                    span(buf, x, width, area.y + r as u16, glyph, self.peak_color(away(r)));
                }
            }
        }
    }

    fn blocks(&self, area: Rect, buf: &mut Buffer) {
        let a = self.analyzer;
        let Some(cols) = Columns::new(area, a.bands().len(), self.cfg) else { return };
        let (levels, peaks) = (fit(a.bands(), cols.n), fit(a.peaks(), cols.n));
        let h = area.height as usize;
        // LED segments: a row of full blocks plus a gap row; short areas use every row, with the
        // gap built into the glyph
        let (pitch, glyph) = if h >= 4 { (2, "█") } else { (1, "▆") };
        let segments = h.div_ceil(pitch);
        for (i, (&v, &p)) in levels.iter().zip(peaks.iter()).enumerate() {
            let x = cols.x(i);
            let mut segment = |s: usize, style| span(buf, x, cols.width as u16, area.bottom() - 1 - (s * pitch) as u16, glyph, style);
            let lit = steps(v, segments);
            for s in 0..lit {
                segment(s, self.color(frac(s, segments)));
            }
            let peak = steps(p, segments);
            if self.cfg.peaks && peak > lit {
                segment(peak - 1, self.peak_color(frac(peak - 1, segments)));
            }
        }
    }

    fn wave(&self, area: Rect, buf: &mut Buffer) {
        let (left, right) = self.analyzer.wave();
        if self.cfg.stereo {
            // right first, so the left channel wins cells both touch
            trace(area, buf, right.len(), |i| right[i], |_| self.theme.accent2);
            trace(area, buf, left.len(), |i| left[i], |_| self.theme.accent);
        } else {
            // mono: with the gradient on, color by distance from the center line (loud = hot)
            let mid = (area.height as f32 * 4.0 - 1.0) / 2.0;
            let color = |row: usize| {
                if self.cfg.gradient { self.theme.gradient_at(((row * 4) as f32 + 1.5 - mid).abs() / mid.max(1.0)) } else { self.theme.accent }
            };
            trace(area, buf, left.len().min(right.len()), |i| (left[i] + right[i]) * 0.5, color);
        }
    }

    fn vu(&self, area: Rect, buf: &mut Buffer) {
        let (levels, peaks) = (self.analyzer.vu(), self.analyzer.vu_peaks());
        let (w, h) = (area.width as usize, area.height as usize);
        if h == 1 {
            // one row: left fills the upper half of each cell, right the lower half
            for x in 0..w {
                let on = |v: f32| v * w as f32 >= x as f32 + 0.5;
                let glyph = match (on(levels[0]), on(levels[1])) {
                    (true, true) => "█",
                    (true, false) => "▀",
                    (false, true) => "▄",
                    _ => continue,
                };
                span(buf, area.x + x as u16, 1, area.y, glyph, self.color(frac(x, w)));
            }
            return;
        }
        let label = if w >= 8 { 2 } else { 0 };
        let readout = if w >= 40 { 7 } else { 0 };
        let meter = Rect::new(area.x + label, area.y, (w - label as usize - readout as usize) as u16, area.height);
        // two meters of `thick` rows with the scale between them, centered vertically
        let (thick, top) = if h == 2 { (1, 0) } else { let t = (h / 4).max(1).min((h - 1) / 2); (t, (h - 2 * t - 1) / 2) };
        let firsts = [top, if h == 2 { 1 } else { top + thick + 1 }];
        for ch in 0..2 {
            let y = area.y + firsts[ch] as u16;
            for dy in 0..thick as u16 {
                self.meter_row(buf, meter, y + dy, levels[ch], peaks[ch]);
            }
            let mid = y + (thick / 2) as u16;
            if label > 0 {
                buf.set_string(area.x, mid, ["L", "R"][ch], Style::new().fg(self.theme.accent));
            }
            if readout > 0 {
                let db = (levels[ch] - 1.0) * -VU_FLOOR_DB;
                let text = if levels[ch] > 0.0 { format!("{db:>6.1}") } else { format!("{:>6}", "-inf") };
                buf.set_string(meter.right() + 1, mid, text, Style::new().fg(self.theme.fg));
            }
        }
        if h >= 3 {
            let y = area.y + (top + thick) as u16;
            self.scale(buf, meter, y);
            if readout > 0 {
                buf.set_string(meter.right() + 1, y, format!("{:>6}", "dB"), Style::new().fg(self.theme.dim));
            }
        }
    }

    /// One row of a horizontal meter: gradient fill with eighth-cell precision plus a peak tick.
    fn meter_row(&self, buf: &mut Buffer, meter: Rect, y: u16, level: f32, peak: f32) {
        let w = meter.width as usize;
        let e = steps(level, w * 8);
        for x in 0..e.div_ceil(8).min(w) {
            span(buf, meter.x + x as u16, 1, y, LEFT[(e - x * 8).min(8)], self.color(frac(x, w)));
        }
        let pe = peak.clamp(0.0, 1.0) * (w * 8) as f32;
        if self.cfg.peaks && pe >= 0.5 {
            let cell = ((pe / 8.0) as usize).min(w - 1);
            if e <= cell * 8 {
                let tick = if pe - ((cell * 8) as f32) < 4.0 { "▏" } else { "▕" };
                span(buf, meter.x + cell as u16, 1, y, tick, self.peak_color(frac(cell, w)));
            }
        }
    }

    /// dB labels under their meter positions; labels that would collide are dropped by priority.
    fn scale(&self, buf: &mut Buffer, meter: Rect, y: u16) {
        let w = meter.width as usize;
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for db in VU_MARKS {
            let text = db.to_string();
            let len = text.len();
            if len > w {
                continue;
            }
            let at = (vu_pos(db) * (w - 1) as f32).round() as usize;
            let start = at.saturating_sub(len / 2).min(w - len);
            if taken.iter().any(|&(a, b)| start <= b && a <= start + len) {
                continue;
            }
            taken.push((start, start + len));
            buf.set_string(meter.x + start as u16, y, text, Style::new().fg(self.theme.dim));
        }
    }
}

/// Position of `db` on a VU meter, 0..=1.
fn vu_pos(db: i32) -> f32 {
    1.0 - db as f32 / VU_FLOOR_DB
}

/// Draw one waveform of `len` points as a connected braille line across `area`. Each dot column
/// covers every value the signal takes between the column's edges, so peaks don't alias away and
/// steep edges stay connected. `color` maps a cell row to its color.
fn trace(area: Rect, buf: &mut Buffer, len: usize, sample: impl Fn(usize) -> f32, color: impl Fn(usize) -> Color) {
    if len == 0 {
        return;
    }
    let (cols, rows) = (area.width as usize * 2, area.height as usize * 4);
    let at = |pos: f32| {
        let i = (pos as usize).min(len - 1);
        let f = pos - i as f32;
        sample(i) * (1.0 - f) + sample((i + 1).min(len - 1)) * f
    };
    let dot = |v: f32| ((1.0 - v.clamp(-1.0, 1.0)) * 0.5 * (rows - 1) as f32).round() as usize;
    let step = (len - 1) as f32 / cols as f32;
    for x in 0..cols {
        let (a, b) = (x as f32 * step, (x + 1) as f32 * step);
        let (va, vb) = (at(a), at(b));
        let (mut lo, mut hi) = (va.min(vb), va.max(vb));
        for i in a as usize + 1..(b.ceil() as usize).min(len) {
            lo = lo.min(sample(i));
            hi = hi.max(sample(i));
        }
        for y in dot(hi)..=dot(lo) {
            set_dot(buf, area, x, y, color(y / 4));
        }
    }
}

/// A spectrum across all of `area` (one row in the player bar, any width): one ▁..█ column per
/// cell, colored by its height along the theme gradient.
#[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
pub fn render_mini(analyzer: &Analyzer, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let area = area.intersection(buf.area);
    if area.is_empty() || analyzer.bands().is_empty() {
        return;
    }
    let h = area.height as usize;
    for (x, &v) in resample(analyzer.bands(), area.width as usize).iter().enumerate() {
        let e = steps(v, h * 8);
        let style = Style::new().fg(theme.gradient_at(v));
        for row in 0..e.div_ceil(8).min(h) {
            span(buf, area.x + x as u16, 1, area.bottom() - 1 - row as u16, LOWER[(e - row * 8).min(8)], style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const RATE: u32 = 44100;
    const DT: f32 = 1.0 / 60.0;

    /// A full tap of `f(seconds) -> [left, right]`.
    fn tap(f: impl Fn(f32) -> [f32; 2]) -> Arc<SampleTap> {
        let tap = SampleTap::new(16384);
        let frames: Vec<[f32; 2]> = (0..16384).map(|i| f(i as f32 / RATE as f32)).collect();
        tap.push(&frames, RATE);
        tap
    }

    fn sine(hz: f32, amp: f32, t: f32) -> f32 {
        amp * (TAU * hz * t).sin()
    }

    fn mono(f: impl Fn(f32) -> f32) -> Arc<SampleTap> {
        tap(|t| [f(t), f(t)])
    }

    /// Broadband stereo test signal: tones across the spectrum, right a bit quieter.
    fn music() -> Arc<SampleTap> {
        tap(|t| {
            let v: f32 = [60.0, 100.0, 250.0, 440.0, 1500.0, 3000.0, 5000.0, 8000.0, 12000.0].iter().map(|&hz| sine(hz, 0.2, t)).sum();
            [v, v * 0.7]
        })
    }

    /// `secs` of 60 fps ticks.
    fn run(a: &mut Analyzer, tap: Option<&SampleTap>, cfg: &VisualizerConfig, bands: usize, secs: f32) {
        for _ in 0..(secs / DT).round() as usize {
            a.advance(tap, cfg, bands, DT);
        }
    }

    fn argmax(v: &[f32]) -> usize {
        (0..v.len()).max_by(|&a, &b| v[a].total_cmp(&v[b])).unwrap()
    }

    fn band_of(hz: f32, bands: usize, cfg: &VisualizerConfig) -> usize {
        let (lo, hi) = freq_limits(cfg.min_freq, cfg.max_freq, RATE);
        (0..bands).find(|&i| (band_edges(i, bands, lo, hi).0..band_edges(i, bands, lo, hi).1).contains(&hz)).unwrap()
    }

    fn all_zero(a: &Analyzer) -> bool {
        let (l, r) = a.bands_lr();
        let (w0, w1) = a.wave();
        a.bands().iter().chain(a.peaks()).chain(l).chain(r).chain(w0).chain(w1).all(|v| *v == 0.0)
            && a.vu() == [0.0; 2]
            && a.vu_peaks() == [0.0; 2]
    }

    #[test]
    fn sine_lands_in_its_band_at_full_scale() {
        let cfg = VisualizerConfig::default();
        for hz in [100.0, 1000.0, 7000.0] {
            let mut a = Analyzer::new();
            run(&mut a, Some(&mono(|t| sine(hz, 1.0, t))), &cfg, 64, 0.5);
            assert_eq!(argmax(a.bands()), band_of(hz, 64, &cfg), "{hz} Hz");
        }
        let mut a = Analyzer::new();
        run(&mut a, Some(&mono(|t| sine(1000.0, 1.0, t))), &cfg, 64, 0.5);
        assert!(a.bands()[band_of(1000.0, 64, &cfg)] > 0.97, "a full-scale 1 kHz sine reads ~0 dB");
    }

    #[test]
    fn two_tones_make_two_peaks() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        run(&mut a, Some(&mono(|t| sine(200.0, 0.5, t) + sine(4000.0, 0.5, t))), &cfg, 64, 0.5);
        let b = a.bands();
        let peaks: Vec<usize> = (1..b.len() - 1).filter(|&i| b[i] > 0.4 && b[i] > b[i - 1] && b[i] >= b[i + 1]).collect();
        assert_eq!(peaks, [band_of(200.0, 64, &cfg), band_of(4000.0, 64, &cfg)]);
    }

    #[test]
    fn channels_separate_and_mix() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        run(&mut a, Some(&tap(|t| [sine(300.0, 1.0, t), sine(6000.0, 1.0, t)])), &cfg, 48, 0.5);
        let (l, r) = a.bands_lr();
        let (b300, b6k) = (band_of(300.0, 48, &cfg), band_of(6000.0, 48, &cfg));
        assert_eq!((argmax(l), argmax(r)), (b300, b6k));
        assert!(l[b6k] < 0.05 && r[b300] < 0.05, "no crosstalk");
        assert!(a.bands()[b300] > 0.5 && a.bands()[b6k] > 0.5, "mono mix has both");
    }

    #[test]
    fn silence_decays_smoothly_to_zero() {
        let cfg = VisualizerConfig::default();
        for silence in [None, Some(mono(|_| 0.0))] {
            let mut a = Analyzer::new();
            run(&mut a, Some(&music()), &cfg, 40, 0.5);
            assert!(a.active());
            let before = a.bands().to_vec();
            a.advance(silence.as_deref(), &cfg, 40, DT);
            assert!(a.bands().iter().zip(&before).all(|(now, was)| now <= was));
            assert!(a.bands().iter().any(|v| *v > 0.5), "falls, doesn't vanish");
            let mut steps_after_zero = 0;
            for _ in 0..(3.0 / DT) as usize {
                a.advance(silence.as_deref(), &cfg, 40, DT);
                if all_zero(&a) {
                    if steps_after_zero == 0 {
                        assert!(a.active(), "the step that settles still asks for a redraw");
                    }
                    steps_after_zero += 1;
                }
            }
            assert!(steps_after_zero > 1 && all_zero(&a) && !a.active());
        }
    }

    #[test]
    fn stopping_ignores_the_tap() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        let t = music();
        run(&mut a, Some(&t), &cfg, 40, 0.5);
        for _ in 0..200 {
            a.update(Some(&t), &cfg, 40, false);
            a.last = a.last.map(|l| l - std::time::Duration::from_millis(20)); // pretend 20 ms passed
        }
        assert!(all_zero(&a) && !a.active());
    }

    #[test]
    fn smoothing_is_frame_rate_independent_and_asymmetric() {
        let cfg = VisualizerConfig { smoothing: 0.5, falloff: 0.0, ..Default::default() };
        let (mut once, mut twice) = (Meters::new(1), Meters::new(1));
        once.step(&[1.0], &Motion::new(&cfg, 1.0 / 30.0));
        twice.step(&[1.0], &Motion::new(&cfg, 1.0 / 60.0));
        twice.step(&[1.0], &Motion::new(&cfg, 1.0 / 60.0));
        assert!((once.level[0] - twice.level[0]).abs() < 1e-6);
        let mut fall = Meters { level: vec![1.0], peak: vec![1.0], ..Meters::new(1) };
        fall.step(&[0.0], &Motion::new(&cfg, 1.0 / 30.0));
        assert!(once.level[0] > 1.0 - fall.level[0], "attack is faster than release");
        let mut raw = Meters::new(1);
        raw.step(&[0.7], &Motion::new(&VisualizerConfig { smoothing: 0.0, ..cfg }, DT));
        assert_eq!(raw.level[0], 0.7, "smoothing 0 is raw");
    }

    #[test]
    fn gravity_and_peak_hold_follow_real_time() {
        let cfg = VisualizerConfig { smoothing: 0.0, falloff: 1.0, peak_hold_ms: 500, ..Default::default() };
        let mut m = Meters::new(1);
        m.step(&[1.0], &Motion::new(&cfg, DT));
        m.step(&[0.0], &Motion::new(&cfg, 0.25));
        assert!((m.level[0] - 0.75).abs() < 1e-5, "falls at 1 height/s");
        m.step(&[0.0], &Motion::new(&cfg, 0.125));
        m.step(&[0.0], &Motion::new(&cfg, 0.125));
        assert!((m.level[0] - 0.5).abs() < 1e-5, "same distance in smaller steps");
        assert_eq!(m.peak[0], 1.0, "peak held for peak_hold_ms");
        m.step(&[0.0], &Motion::new(&cfg, 0.1));
        assert!(m.peak[0] < 1.0 && m.peak[0] > m.level[0], "then it falls");
        for _ in 0..100 {
            m.step(&[0.0], &Motion::new(&cfg, DT));
        }
        assert!(!m.visible());
    }

    #[test]
    fn vu_reads_rms_per_channel() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        run(&mut a, Some(&tap(|t| [sine(1000.0, 1.0, t), 0.0])), &cfg, 16, 1.0);
        let [l, r] = a.vu();
        assert!((l - (1.0 - 3.01 / 60.0)).abs() < 0.01, "full-scale sine is -3 dB RMS, got {l}");
        assert_eq!(r, 0.0);
        assert!(a.vu_peaks()[0] >= l);
    }

    #[test]
    fn wave_starts_at_a_rising_zero_crossing() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        a.advance(Some(&mono(|t| sine(441.0, 0.5, t + 0.0013))), &cfg, 8, DT);
        let (l, r) = a.wave();
        assert_eq!((l.len(), r.len()), (WAVE_LEN, WAVE_LEN));
        assert!(l[0].abs() < 0.05 && l[1] > l[0], "triggered: {} {}", l[0], l[1]);
        let peak = l.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.02, "keeps the amplitude: {peak}");
    }

    #[test]
    fn band_count_and_config_changes_are_handled() {
        let t = music();
        let mut a = Analyzer::new();
        for (bands, fft_size) in [(32, 4096), (80, 512), (1, 16384), (5000, 1000), (7, 0), (64, usize::MAX)] {
            let cfg = VisualizerConfig { fft_size, ..Default::default() };
            run(&mut a, Some(&t), &cfg, bands, 0.1);
            let n = bands.min(MAX_BANDS);
            assert_eq!((a.bands().len(), a.peaks().len(), a.bands_lr().0.len(), a.peaks_lr().1.len()), (n, n, n, n));
            assert!(a.bands().iter().any(|v| *v > 0.3), "{bands} bands, fft {fft_size}");
        }
    }

    #[test]
    fn nonsense_config_values_stay_finite() {
        let t = music();
        let nan = f32::NAN;
        for cfg in [
            VisualizerConfig { min_freq: 20000.0, max_freq: 10.0, db_floor: 5.0, smoothing: 7.0, falloff: -1.0, ..Default::default() },
            VisualizerConfig { min_freq: nan, max_freq: nan, db_floor: nan, smoothing: nan, falloff: nan, ..Default::default() },
            VisualizerConfig { min_freq: -5.0, max_freq: 1e9, db_floor: -1e9, ..Default::default() },
        ] {
            let mut a = Analyzer::new();
            run(&mut a, Some(&t), &cfg, 50, 0.2);
            let ok = |v: &f32| v.is_finite() && (0.0..=1.0).contains(v);
            assert!(a.bands().iter().chain(a.peaks()).all(ok), "{cfg:?}");
            assert!(a.vu().iter().chain(&a.vu_peaks()).all(ok));
        }
    }

    fn draw(a: &Analyzer, cfg: &VisualizerConfig, mode: VisMode, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let theme = Theme::default();
        term.draw(|f| f.render_widget(Visualizer { analyzer: a, cfg, theme: &theme, mode }, f.area())).unwrap();
        term.backend().buffer().clone()
    }

    fn ink(buf: &Buffer) -> usize {
        buf.content().iter().filter(|c| c.symbol() != " ").count()
    }

    fn text(buf: &Buffer) -> String {
        let w = buf.area.width as usize;
        buf.content().chunks(w).map(|row| row.iter().map(|c| c.symbol()).collect::<String>() + "\n").collect()
    }

    #[test]
    fn every_mode_renders_at_every_size() {
        let t = music();
        for stereo in [true, false] {
            let cfg = VisualizerConfig { stereo, ..Default::default() };
            for (w, h) in [(1, 1), (2, 1), (3, 2), (10, 2), (40, 10), (7, 5), (200, 40)] {
                let mut a = Analyzer::new();
                for mode in VisMode::ALL {
                    draw(&a, &cfg, mode, w, h); // never updated: nothing to show, must not panic
                }
                run(&mut a, Some(&t), &cfg, band_count(w, &cfg), 0.5);
                for mode in VisMode::ALL {
                    let buf = draw(&a, &cfg, mode, w, h);
                    assert!(ink(&buf) > 0, "{mode:?} {w}x{h} stereo={stereo} drew nothing");
                    assert!(buf.content().iter().all(|c| c.bg == Color::Reset), "{mode:?} painted a background");
                }
            }
        }
    }

    #[test]
    fn cassette_reels_turn_only_while_playing() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        let still = text(&draw(&a, &cfg, VisMode::Cassette, 60, 12));
        assert!(still.contains("ORBIT · C-90") && still.contains("▓▒▓"), "{still}");
        run(&mut a, Some(&music()), &cfg, 8, 0.2);
        let turned = text(&draw(&a, &cfg, VisMode::Cassette, 60, 12));
        assert_ne!(still, turned, "the reels turn while playing");
        run(&mut a, None, &cfg, 8, 1.0);
        assert_eq!(text(&draw(&a, &cfg, VisMode::Cassette, 60, 12)), turned, "and stop when it doesn't");
    }

    #[test]
    fn renders_only_inside_its_area() {
        let mut a = Analyzer::new();
        let cfg = VisualizerConfig { bars: 300, ..Default::default() };
        run(&mut a, Some(&music()), &cfg, 300, 0.3);
        let area = Rect::new(5, 3, 20, 6);
        for mode in VisMode::ALL {
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
            Visualizer { analyzer: &a, cfg: &cfg, theme: &Theme::default(), mode }.render(area, &mut buf);
            for y in 0..12 {
                for x in 0..40 {
                    assert!(area.contains((x, y).into()) || buf[(x, y)].symbol() == " ", "{mode:?} drew at {x},{y}");
                }
            }
            // an area hanging off the buffer is clipped, not a panic
            Visualizer { analyzer: &a, cfg: &cfg, theme: &Theme::default(), mode }.render(Rect::new(30, 8, 30, 30), &mut buf);
        }
    }

    #[test]
    fn bars_are_centered_with_configured_size() {
        let cfg = VisualizerConfig { bars: 4, ..Default::default() }; // 4 bars x 2 + 3 gaps = 11 columns
        let mut a = Analyzer::new();
        run(&mut a, Some(&music()), &cfg, band_count(41, &cfg), 0.5);
        let buf = draw(&a, &cfg, VisMode::Bars, 41, 8);
        let used: Vec<u16> = (0..41).filter(|&x| (0..8).any(|y| buf[(x, y)].symbol() != " ")).collect();
        assert_eq!(used, [15, 16, 18, 19, 21, 22, 24, 25]);
        assert_eq!(band_count(41, &VisualizerConfig::default()), 14);
        assert_eq!(band_count(1, &VisualizerConfig::default()), 1);
    }

    #[test]
    fn peak_markers_hover_above_falling_bars() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        run(&mut a, Some(&music()), &cfg, 13, 0.5);
        run(&mut a, None, &cfg, 13, 0.3); // bars fall, peaks still held
        for mode in [VisMode::Bars, VisMode::Mirror, VisMode::Blocks, VisMode::Vu] {
            let with = ink(&draw(&a, &cfg, mode, 40, 10));
            let without = ink(&draw(&a, &VisualizerConfig { peaks: false, ..cfg.clone() }, mode, 40, 10));
            assert!(with > without, "{mode:?}: {with} vs {without}");
        }
    }

    #[test]
    fn mirror_sends_left_up_and_right_down() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        run(&mut a, Some(&tap(|t| [sine(1000.0, 1.0, t) + sine(150.0, 1.0, t), 0.0])), &cfg, 13, 0.5);
        let buf = draw(&a, &cfg, VisMode::Mirror, 40, 10);
        let rows_used: Vec<u16> = (0..10).filter(|&y| (0..40).any(|x| buf[(x, y)].symbol() != " ")).collect();
        assert!(!rows_used.is_empty() && rows_used.iter().all(|&y| y < 5), "left only: {rows_used:?}");
        let mono = draw(&a, &VisualizerConfig { stereo: false, ..cfg }, VisMode::Mirror, 40, 10);
        assert!((5..10).any(|y| (0..40).any(|x| mono[(x, y)].symbol() != " ")), "mono mirrors down too");
    }

    #[test]
    fn vu_shows_labels_scale_and_readouts() {
        let cfg = VisualizerConfig::default();
        let mut a = Analyzer::new();
        run(&mut a, Some(&music()), &cfg, 8, 0.5);
        let t = text(&draw(&a, &cfg, VisMode::Vu, 80, 5));
        for s in ["L ", "R ", "-60", "-40", "-20", "-10", "-6", "-3", " 0", "dB"] {
            assert!(t.contains(s), "missing {s:?} in\n{t}");
        }
        let narrow = text(&draw(&a, &cfg, VisMode::Vu, 20, 3));
        assert!(narrow.contains("-60") && narrow.contains('0') && !narrow.contains("-3 "), "crowded labels give way:\n{narrow}");
    }

    #[test]
    fn mini_fills_any_width() {
        let mut a = Analyzer::new();
        let theme = Theme::default();
        let mut empty = Buffer::empty(Rect::new(0, 0, 10, 1));
        render_mini(&a, empty.area, &mut empty, &theme);
        assert_eq!(ink(&empty), 0);
        run(&mut a, Some(&music()), &VisualizerConfig::default(), 20, 0.5);
        for (w, h) in [(1, 1), (5, 1), (33, 1), (300, 1), (12, 3)] {
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
            render_mini(&a, buf.area, &mut buf, &theme);
            assert!(ink(&buf) > 0, "{w}x{h}");
        }
    }

    #[test]
    fn resample_keeps_peaks_and_interpolates() {
        assert_eq!(resample(&[0.0, 1.0, 0.0, 0.0], 2), [1.0, 0.0]);
        assert_eq!(resample(&[0.0, 1.0], 3), [0.0, 0.5, 1.0]);
        assert_eq!(resample(&[0.4], 3), [0.4, 0.4, 0.4]);
        assert_eq!(resample(&[], 2), [0.0, 0.0]);
    }

    /// Run with `cargo test --release update_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing"]
    fn update_cost() {
        let t = music();
        for fft_size in [4096, 16384] {
            let cfg = VisualizerConfig { fft_size, ..Default::default() };
            let mut a = Analyzer::new();
            run(&mut a, Some(&t), &cfg, 100, 0.1);
            let n = 2000;
            let start = Instant::now();
            for _ in 0..n {
                a.advance(Some(&t), &cfg, 100, DT);
            }
            eprintln!("fft {fft_size}: {:.1} µs per update", start.elapsed().as_secs_f64() * 1e6 / n as f64);
        }
    }
}
