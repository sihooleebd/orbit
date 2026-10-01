//! Audio processing between the decoder and the output. Each playing track runs through
//!
//! decoder -> [`Gain`] (ReplayGain) -> [`Equalizer`] (10 peaking biquads + preamp)
//!         -> [`TrackTap`] (fades, seeking, A-B loop, position, visualizer tap, volume, soft clip)
//!
//! Everything here runs on the audio thread: nothing blocks and nothing allocates per sample. The
//! UI thread steers a playing chain only through atomics ([`EqShared`], [`OutputCtl`],
//! [`TrackCtl`]); the audio thread polls them every few dozen frames and glides to new values so
//! parameter changes never click.

use std::f64::consts::PI;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, SampleRate, Source};

use crate::config::{EqSettings, ReplayGainMode};
use crate::library::Track;

/// Center frequencies of the 10 EQ bands.
pub const EQ_FREQS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
pub const EQ_MAX_DB: f32 = 12.0;

/// Built-in EQ presets (dB per band). Order = cycling order.
pub const EQ_PRESETS: &[(&str, [f32; 10])] = &[
    ("flat", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("bass-boost", [6.0, 5.0, 4.0, 2.5, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("bass-cut", [-6.0, -5.0, -4.0, -2.0, -0.5, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("treble-boost", [0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 1.5, 3.0, 4.5, 5.5]),
    ("vocal", [-2.0, -1.5, -1.0, 1.0, 3.0, 3.5, 3.0, 1.5, 0.0, -1.0]),
    ("loudness", [5.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 1.5, 3.5, 4.5]),
    ("pop", [-1.0, 0.0, 1.5, 3.0, 3.5, 2.5, 1.0, 0.0, -0.5, -1.0]),
    ("rock", [4.0, 3.0, 1.5, 0.0, -1.0, -0.5, 1.0, 2.5, 3.5, 4.0]),
    ("jazz", [3.0, 2.0, 1.0, 2.0, -1.5, -1.5, 0.0, 1.5, 2.5, 3.0]),
    ("classical", [3.5, 3.0, 2.0, 1.0, -1.0, -1.0, 0.0, 2.0, 3.0, 3.5]),
    ("electronic", [4.5, 4.0, 1.5, 0.0, -2.0, 1.5, 0.5, 1.5, 4.0, 5.0]),
    ("hip-hop", [5.0, 4.5, 2.0, 3.0, -1.0, -1.0, 1.5, -0.5, 2.0, 3.0]),
    ("acoustic", [4.0, 3.5, 2.5, 1.0, 1.5, 1.5, 2.5, 3.0, 2.5, 1.5]),
    ("night", [-3.0, -2.0, -1.0, 0.0, 1.0, 1.5, 1.0, 0.0, -2.0, -4.0]),
];

pub fn eq_preset(name: &str) -> Option<[f32; 10]> {
    EQ_PRESETS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, b)| *b)
}

pub fn eq_preset_names() -> Vec<&'static str> {
    EQ_PRESETS.iter().map(|(n, _)| *n).collect()
}

/// Ring buffer of the most recent stereo frames sent to the output, read by the visualizer.
/// Mono sources are duplicated to both channels; >2 channels are folded down to L/R. Frames are
/// taken after the EQ and fades but before the volume, so the visualizer shows the music at any
/// volume (even muted); the reported rate includes the playback speed.
pub struct SampleTap {
    inner: Mutex<TapInner>,
}

struct TapInner {
    ring: Vec<[f32; 2]>,
    pos: usize,
    sample_rate: u32,
}

impl SampleTap {
    pub fn new(capacity_frames: usize) -> Arc<SampleTap> {
        Arc::new(SampleTap { inner: Mutex::new(TapInner { ring: vec![[0.0; 2]; capacity_frames.max(1)], pos: 0, sample_rate: 44100 }) })
    }

    /// Audio thread: append frames (call with small batches, not per sample). Never waits: if the
    /// UI thread holds the lock right now, the batch is dropped.
    pub fn push(&self, frames: &[[f32; 2]], sample_rate: u32) {
        let Ok(mut t) = self.inner.try_lock() else { return };
        t.sample_rate = sample_rate;
        let n = t.ring.len();
        for f in frames {
            let p = t.pos;
            t.ring[p] = *f;
            t.pos = (p + 1) % n;
        }
    }

    /// UI thread: copy the newest `n` frames (oldest first) into `out`; returns the sample rate.
    pub fn latest(&self, n: usize, out: &mut Vec<[f32; 2]>) -> u32 {
        out.clear();
        let Ok(t) = self.inner.lock() else { return 44100 };
        let len = t.ring.len();
        let n = n.min(len);
        for i in 0..n {
            out.push(t.ring[(t.pos + len - n + i) % len]);
        }
        t.sample_rate
    }

    /// Fill with silence (on stop / track change) so the visualizer drops to zero.
    pub fn clear(&self) {
        if let Ok(mut t) = self.inner.lock() {
            t.ring.iter_mut().for_each(|f| *f = [0.0; 2]);
        }
    }
}

/// dB -> linear amplitude.
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Linear gain for a track under the ReplayGain settings. `Track` / `Album` fall back to the other
/// gain when theirs is missing, `Auto` prefers album gain; untagged tracks play at unity (the
/// preamp only applies to tagged ones). `prevent_clip` caps the gain so the tagged peak stays at
/// or below full scale.
pub fn replaygain_gain(track: &Track, mode: ReplayGainMode, preamp_db: f32, prevent_clip: bool) -> f32 {
    let album = track.rg_album_gain.map(|g| (g, track.rg_album_peak));
    let single = track.rg_track_gain.map(|g| (g, track.rg_track_peak));
    let chosen = match mode {
        ReplayGainMode::Off => None,
        ReplayGainMode::Track => single.or(album),
        ReplayGainMode::Album | ReplayGainMode::Auto => album.or(single),
    };
    let Some((gain_db, peak)) = chosen.filter(|(g, _)| g.is_finite()) else { return 1.0 };
    let gain = db_to_gain(gain_db + if preamp_db.is_finite() { preamp_db } else { 0.0 });
    match peak.filter(|p| prevent_clip && p.is_finite() && *p > 0.0) {
        Some(peak) => gain.min(1.0 / peak),
        None => gain,
    }
}

/// Soft clipper for the final output: identity up to 0.95, then a tanh knee that levels off at
/// full scale, so EQ boosts, ReplayGain and volumes above 100 % saturate gently instead of
/// hard-clipping in the device.
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.95;
    let a = x.abs();
    if a <= KNEE { x } else { (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh()).copysign(x) }
}

/// An f32 shared between threads (bit-cast into an `AtomicU32`).
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> AtomicF32 {
        AtomicF32(AtomicU32::new(v.to_bits()))
    }

    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }

    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Relaxed);
    }
}

/// Time constant of every parameter glide (EQ gains, ReplayGain, volume).
const GLIDE_SECS: f64 = 0.03;

/// One exponential smoothing step that lands exactly on `target` once within `snap`, so settled
/// values compare equal and the exact-bypass paths kick back in.
fn glide(cur: f64, target: f64, k: f64, snap: f64) -> f64 {
    let next = cur + (target - cur) * k;
    if (target - next).abs() <= snap { target } else { next }
}

/// Smoothing coefficient for one step of `step_secs` with time constant `GLIDE_SECS`.
fn glide_k(step_secs: f64) -> f64 {
    1.0 - (-step_secs / GLIDE_SECS).exp()
}

fn to_frames(d: Duration, rate: u32) -> u64 {
    (d.as_nanos() * rate as u128 / 1_000_000_000) as u64
}

fn to_nanos(d: Duration) -> u64 {
    d.as_nanos().min(NO_TIME as u128 - 1) as u64
}

// ---- ReplayGain -------------------------------------------------------------------------------

/// Samples between checks of the gain target.
const GAIN_BLOCK: u32 = 64;

/// Multiplies by a gain the UI thread may change at any time (ReplayGain). Starts at the gain
/// current when it begins playing, then glides to changes over ~30 ms; unity gain passes samples
/// through untouched.
pub struct Gain<S> {
    inner: S,
    target: Arc<AtomicF32>,
    /// NaN until the first sample.
    current: f64,
    k: f64,
    countdown: u32,
}

impl<S: Source> Gain<S> {
    pub fn new(inner: S, target: Arc<AtomicF32>) -> Gain<S> {
        let samples_per_sec = inner.sample_rate().get() as f64 * inner.channels().get() as f64;
        Gain { k: glide_k(GAIN_BLOCK as f64 / samples_per_sec), current: f64::NAN, inner, target, countdown: 1 }
    }
}

impl<S: Source> Iterator for Gain<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let x = self.inner.next()?;
        self.countdown -= 1;
        if self.countdown == 0 {
            self.countdown = GAIN_BLOCK;
            let target = self.target.load() as f64;
            if self.current.is_nan() {
                self.current = target;
            } else if target != self.current {
                self.current = glide(self.current, target, self.k, 1e-5);
            }
        }
        Some(if self.current == 1.0 { x } else { (x as f64 * self.current) as f32 })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Gain<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)
    }
}

// ---- Equalizer --------------------------------------------------------------------------------

/// Bandwidth of every band: Q = √2, about one octave, matching the octave-spaced centers.
const EQ_Q: f64 = std::f64::consts::SQRT_2;
/// Frames between parameter checks and smoothing steps.
const EQ_BLOCK: u32 = 32;
/// Keeps filter state out of the denormal range during digital silence (-360 dB).
const ANTI_DENORMAL: f64 = 1e-18;

/// Normalized biquad coefficients (a0 = 1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Biquad {
    /// RBJ audio-EQ-cookbook peaking filter, from the band's precomputed cos(w0) and alpha.
    fn peaking((cos_w0, alpha): (f64, f64), gain_db: f64) -> Biquad {
        let a = 10f64.powf(gain_db / 40.0);
        let a0 = 1.0 + alpha / a;
        Biquad { b0: (1.0 + alpha * a) / a0, b1: -2.0 * cos_w0 / a0, b2: (1.0 - alpha * a) / a0, a1: -2.0 * cos_w0 / a0, a2: (1.0 - alpha / a) / a0 }
    }

    /// |H(e^jw)| at normalized angular frequency `w`.
    fn magnitude(&self, w: f64) -> f64 {
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num = (self.b0 + self.b1 * c1 + self.b2 * c2).hypot(self.b1 * s1 + self.b2 * s2);
        let den = (1.0 + self.a1 * c1 + self.a2 * c2).hypot(self.a1 * s1 + self.a2 * s2);
        num / den
    }
}

/// cos(w0) and alpha of band `i` at `rate`, or None when its center is too close to Nyquist to
/// exist in the signal (e.g. the 16 kHz band at 32 kHz).
fn band_shape(i: usize, rate: u32) -> Option<(f64, f64)> {
    let (f, rate) = (EQ_FREQS[i] as f64, rate as f64);
    (f < 0.45 * rate).then(|| {
        let (sin, cos) = (2.0 * PI * f / rate).sin_cos();
        (cos, sin / (2.0 * EQ_Q))
    })
}

/// What the equalizer is asked to do: [preamp, band 0..10] in dB, all zero when disabled.
fn eq_targets(eq: &EqSettings) -> [f32; 11] {
    let clamp = |db: f32| if db.is_finite() { db.clamp(-EQ_MAX_DB, EQ_MAX_DB) } else { 0.0 };
    let mut t = [0.0; 11];
    if eq.enabled {
        t[0] = clamp(eq.preamp_db);
        for (slot, db) in t[1..].iter_mut().zip(eq.bands) {
            *slot = clamp(db);
        }
    }
    t
}

/// The equalizer's overall magnitude response (preamp included) at `freq`, in dB: exactly what
/// the audio thread applies once its smoothing settles. For drawing the EQ curve.
pub fn eq_response_db(eq: &EqSettings, freq: f32, sample_rate: u32) -> f32 {
    let t = eq_targets(eq);
    let w = 2.0 * PI * freq as f64 / sample_rate as f64;
    let bands: f64 = (0..10)
        .filter(|&i| t[i + 1] != 0.0)
        .filter_map(|i| band_shape(i, sample_rate).map(|shape| 20.0 * Biquad::peaking(shape, t[i + 1] as f64).magnitude(w).log10()))
        .sum();
    t[0] + bands as f32
}

/// EQ parameters shared by the UI thread (writer) and every playing chain (readers). Lock-free:
/// the writer stores the values, then bumps the version; readers re-read when the version moved.
#[derive(Default)]
pub struct EqShared {
    version: AtomicU64,
    /// [preamp, bands] in dB, already zeroed when the EQ is disabled.
    db: [AtomicF32; 11],
}

impl EqShared {
    pub fn new() -> Arc<EqShared> {
        Arc::default()
    }

    /// Applies to everything playing (smoothly) and to chains built later.
    pub fn set(&self, eq: &EqSettings) {
        for (slot, db) in self.db.iter().zip(eq_targets(eq)) {
            slot.store(db);
        }
        self.version.fetch_add(1, Release);
    }

    /// The targets if they changed since version `seen`.
    fn read(&self, seen: u64) -> Option<(u64, [f64; 11])> {
        let v = self.version.load(Acquire);
        (v != seen).then(|| (v, std::array::from_fn(|i| self.db[i].load() as f64)))
    }
}

/// 10-band graphic equalizer: RBJ peaking biquads (Direct Form I, f64, per-channel state) at
/// [`EQ_FREQS`] plus a preamp. Gains glide toward new settings (coefficients recomputed every
/// 32 frames), bands at 0 dB are skipped, and with everything at 0 dB (e.g. disabled) samples pass
/// through bit-identical.
pub struct Equalizer<S> {
    inner: S,
    shared: Arc<EqShared>,
    /// Version of `shared` last read; `u64::MAX` before the first read.
    seen: u64,
    target: [f64; 11],
    current: [f64; 11],
    preamp: f64,
    /// Per-frame preamp change while it glides (a step per block would click).
    preamp_step: f64,
    shape: [Option<(f64, f64)>; 10],
    coeffs: [Biquad; 10],
    active: [bool; 10],
    /// [channel][band] = [x1, x2, y1, y2]
    state: Vec<[[f64; 4]; 10]>,
    channels: usize,
    rate: u32,
    k: f64,
    ch: usize,
    countdown: u32,
    bypass: bool,
}

impl<S: Source> Equalizer<S> {
    /// Starts at the parameters current when it begins playing (a preloaded track doesn't glide
    /// from settings that changed while it waited).
    pub fn new(inner: S, shared: Arc<EqShared>) -> Equalizer<S> {
        Equalizer {
            inner,
            shared,
            seen: u64::MAX,
            target: [0.0; 11],
            current: [0.0; 11],
            preamp: 1.0,
            preamp_step: 0.0,
            shape: [None; 10],
            coeffs: [Biquad::default(); 10],
            active: [false; 10],
            state: Vec::new(),
            channels: 0,
            rate: 0,
            k: 0.0,
            ch: 0,
            countdown: 0,
            bypass: true,
        }
    }

    /// At a frame boundary: follow format changes, pick up new parameters, advance the glide.
    fn update(&mut self) {
        self.countdown = EQ_BLOCK;
        let (channels, rate) = (self.inner.channels().get() as usize, self.inner.sample_rate().get());
        let reshaped = (channels, rate) != (self.channels, self.rate);
        if reshaped {
            (self.channels, self.rate) = (channels, rate);
            self.state = vec![[[0.0; 4]; 10]; channels];
            self.shape = std::array::from_fn(|i| band_shape(i, rate));
            self.k = glide_k(EQ_BLOCK as f64 / rate as f64);
        }
        if let Some((version, target)) = self.shared.read(self.seen) {
            if self.seen == u64::MAX {
                self.current = target;
            }
            (self.seen, self.target) = (version, target);
        }
        let mut changed = [reshaped; 11];
        for (i, changed) in changed.iter_mut().enumerate() {
            if self.current[i] != self.target[i] {
                self.current[i] = glide(self.current[i], self.target[i], self.k, 0.01);
                *changed = true;
            }
        }
        // the preamp ramps to this block's gain frame by frame (and lands on it exactly)
        let gain = 10f64.powf(self.current[0] / 20.0);
        self.preamp_step = if changed[0] && !reshaped { (gain - self.preamp) / EQ_BLOCK as f64 } else { 0.0 };
        if self.preamp_step == 0.0 {
            self.preamp = gain;
        }
        for b in (0..10).filter(|b| changed[b + 1]) {
            let was = self.active[b];
            match self.shape[b].filter(|_| self.current[b + 1] != 0.0) {
                Some(shape) => {
                    self.coeffs[b] = Biquad::peaking(shape, self.current[b + 1]);
                    self.active[b] = true;
                }
                None => self.active[b] = false,
            }
            // A band that switches back on must start from silence to be transient-free.
            if was && !self.active[b] {
                self.state.iter_mut().for_each(|st| st[b] = [0.0; 4]);
            }
        }
        self.bypass = self.preamp_step == 0.0 && self.preamp == 1.0 && !self.active.contains(&true);
    }
}

impl<S: Source> Iterator for Equalizer<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.ch == 0 {
            if self.countdown == 0 {
                self.update();
            }
            self.countdown -= 1;
            self.preamp += self.preamp_step;
        }
        let x = self.inner.next()?;
        let ch = self.ch;
        self.ch = if ch + 1 == self.channels { 0 } else { ch + 1 };
        if self.bypass {
            return Some(x);
        }
        let mut v = x as f64 * self.preamp + ANTI_DENORMAL;
        for ((c, s), on) in self.coeffs.iter().zip(self.state[ch].iter_mut()).zip(self.active) {
            if on {
                let y = c.b0 * v + c.b1 * s[0] + c.b2 * s[1] - c.a1 * s[2] - c.a2 * s[3];
                *s = [v, s[0], y, s[2]];
                v = y;
            }
        }
        Some(v as f32)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for Equalizer<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    /// Forwards, then clears the filter history: playback restarts cleanly at the new position.
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        self.state.iter_mut().for_each(|st| *st = [[0.0; 4]; 10]);
        Ok(())
    }
}

// ---- Per-track output stage -------------------------------------------------------------------

/// "No seek pending" / "no loop point".
const NO_TIME: u64 = u64::MAX;
const PENDING: u8 = 0;
const PLAYING: u8 = 1;
const ENDED: u8 = 2;
const CANCELLED: u8 = 3;
/// Frames between reads of the control atomics.
const CONTROL_FRAMES: u32 = 64;
/// Frames per push into the [`SampleTap`].
const TAP_BATCH: usize = 256;
/// Upper bound of the fade-out/in around a seek (shorter when `fade_ms` is shorter).
const SEEK_DIP_MS: u32 = 20;

/// Engine-wide output controls, read by every chain's [`TrackTap`].
pub struct OutputCtl {
    pub tap: Arc<SampleTap>,
    /// Linear output gain (volume curve and mute already applied).
    pub volume: AtomicF32,
    /// Playback speed: the tap reports the rate the listener actually hears.
    pub speed: AtomicF32,
    /// Fade length for pause / resume / stop / track switches; seeks dip for at most 20 ms.
    pub fade_ms: AtomicU32,
    /// Fade out and hold (pause) instead of playing.
    pub paused: AtomicBool,
}

impl OutputCtl {
    pub fn new(tap: Arc<SampleTap>) -> Arc<OutputCtl> {
        Arc::new(OutputCtl {
            tap,
            volume: AtomicF32::new(1.0),
            speed: AtomicF32::new(1.0),
            fade_ms: AtomicU32::new(0),
            paused: AtomicBool::new(false),
        })
    }
}

/// State one track's chain shares with the engine: lifecycle, position, requests.
pub struct TrackCtl {
    phase: AtomicU8,
    /// Fade out, then end the chain.
    kill: AtomicBool,
    frames: AtomicU64,
    rate: AtomicU32,
    /// Faded out and holding still because of a pause.
    held: AtomicBool,
    /// Seek target in nanoseconds, `NO_TIME` when none is pending.
    seek: AtomicU64,
    loop_a: AtomicU64,
    loop_b: AtomicU64,
    error: Mutex<Option<String>>,
}

impl TrackCtl {
    pub fn new() -> Arc<TrackCtl> {
        Arc::new(TrackCtl {
            phase: AtomicU8::new(PENDING),
            kill: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            held: AtomicBool::new(false),
            seek: AtomicU64::new(NO_TIME),
            loop_a: AtomicU64::new(NO_TIME),
            loop_b: AtomicU64::new(NO_TIME),
            error: Mutex::new(None),
        })
    }

    /// Position in the track's own timeline: frames played / sample rate (independent of speed),
    /// or the target of a seek the audio thread hasn't performed yet.
    pub fn position(&self) -> Duration {
        let seek = self.seek.load(Acquire);
        if seek != NO_TIME {
            return Duration::from_nanos(seek);
        }
        match self.rate.load(Relaxed) {
            0 => Duration::ZERO,
            rate => Duration::from_nanos((self.frames.load(Relaxed) as u128 * 1_000_000_000 / rate as u128) as u64),
        }
    }

    /// Performed on the audio thread with a short dip (or when playback resumes, if paused).
    pub fn seek(&self, pos: Duration) {
        self.seek.store(to_nanos(pos), Release);
    }

    /// Keep playback between A and B once both are set and A < B: reaching (or being past) B jumps
    /// back to A.
    pub fn set_loop(&self, a: Option<Duration>, b: Option<Duration>) {
        self.loop_a.store(a.map_or(NO_TIME, to_nanos), Relaxed);
        self.loop_b.store(b.map_or(NO_TIME, to_nanos), Relaxed);
    }

    pub fn loop_points(&self) -> Option<(Duration, Duration)> {
        let (a, b) = (self.loop_a.load(Relaxed), self.loop_b.load(Relaxed));
        (a != NO_TIME && b != NO_TIME && a < b).then(|| (Duration::from_nanos(a), Duration::from_nanos(b)))
    }

    /// The audio thread has begun playing it.
    pub fn started(&self) -> bool {
        matches!(self.phase.load(Acquire), PLAYING | ENDED)
    }

    /// It played to its end (or was killed and faded out).
    pub fn ended(&self) -> bool {
        self.phase.load(Acquire) == ENDED
    }

    pub fn held(&self) -> bool {
        self.held.load(Acquire)
    }

    /// Sample rate of the track's audio (0 before the chain knows it).
    pub fn sample_rate(&self) -> u32 {
        self.rate.load(Relaxed)
    }

    /// Withdraw a chain the audio thread hasn't started; false if it already has.
    pub fn cancel(&self) -> bool {
        self.phase.compare_exchange(PENDING, CANCELLED, AcqRel, Acquire).is_ok()
    }

    /// Get rid of the chain: withdrawn at once if it hasn't started (true), else faded out and ended.
    pub fn discard(&self) -> bool {
        let cancelled = self.cancel();
        if !cancelled {
            self.kill.store(true, Release);
        }
        cancelled
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.try_lock().ok()?.take()
    }
}

enum Step {
    Play,
    Hold,
    End,
}

/// The last stage of a track's chain, in processing order:
/// - fade envelope (smoothstep): pause fades out then holds without consuming the decoder, resume
///   fades in, `discard` fades out then ends, seeks dip briefly around the jump;
/// - executes seeks and the A-B loop on the audio thread (never blocking the UI thread);
/// - counts frames for an exact position in the track's timeline and marks start / end;
/// - feeds the visualizer tap in batches (post-EQ and fade, pre-volume);
/// - applies the (smoothed) volume and the soft clipper.
pub struct TrackTap<S> {
    inner: S,
    track: Arc<TrackCtl>,
    out: Arc<OutputCtl>,
    channels: usize,
    rate: u32,
    /// Channel of the next sample.
    ch: usize,
    frames: u64,
    /// Zero samples still owed for the frame being held.
    zeros: usize,
    begun: bool,
    done: bool,
    fade_in: bool,
    held: bool,
    /// Fade phase 0..=1 (gain = smoothstep(env)) and the gain of the current frame.
    env: f64,
    gain: f32,
    /// Ramping around a seek (at the short dip speed).
    dipping: bool,
    vol: f64,
    vol_k: f64,
    countdown: u32,
    // Controls as of the last read:
    paused: bool,
    kill: bool,
    seek: u64,
    vol_target: f64,
    fade_step: f64,
    dip_step: f64,
    /// (seek target of the loop, frame that triggers it)
    loop_to: Option<(u64, u64)>,
    acc: [f32; 2],
    batch: [[f32; 2]; TAP_BATCH],
    batch_len: usize,
}

impl<S: Source> TrackTap<S> {
    /// `start` is where `inner` already is. `fade_in` starts silent and fades in (for starts
    /// mid-track).
    pub fn new(inner: S, track: Arc<TrackCtl>, out: Arc<OutputCtl>, start: Duration, fade_in: bool) -> TrackTap<S> {
        let mut tap = TrackTap {
            inner,
            track,
            out,
            channels: 0,
            rate: 0,
            ch: 0,
            frames: 0,
            zeros: 0,
            begun: false,
            done: false,
            fade_in,
            held: false,
            env: 1.0,
            gain: 1.0,
            dipping: false,
            vol: 1.0,
            vol_k: 0.0,
            countdown: 0,
            paused: false,
            kill: false,
            seek: NO_TIME,
            vol_target: 1.0,
            fade_step: 1.0,
            dip_step: 1.0,
            loop_to: None,
            acc: [0.0; 2],
            batch: [[0.0; 2]; TAP_BATCH],
            batch_len: 0,
        };
        tap.configure();
        tap.frames = to_frames(start, tap.rate);
        tap.track.frames.store(tap.frames, Relaxed);
        tap
    }

    /// Follow the inner format (at a frame boundary).
    fn configure(&mut self) {
        let (channels, rate) = (self.inner.channels().get() as usize, self.inner.sample_rate().get());
        if self.rate != 0 && rate != self.rate {
            self.frames = (self.frames as u128 * rate as u128 / self.rate as u128) as u64;
        }
        (self.channels, self.rate) = (channels, rate);
        self.vol_k = glide_k(1.0 / rate as f64);
        self.track.frames.store(self.frames, Relaxed);
        self.track.rate.store(rate, Relaxed);
    }

    fn read_controls(&mut self) {
        self.countdown = CONTROL_FRAMES;
        if (self.inner.channels().get() as usize, self.inner.sample_rate().get()) != (self.channels, self.rate) {
            self.configure();
        }
        let ramp = |ms: u32, rate: u32| if ms == 0 { 1.0 } else { 1000.0 / (ms as f64 * rate as f64) };
        let fade_ms = self.out.fade_ms.load(Relaxed);
        let dip_ms = fade_ms.min(SEEK_DIP_MS);
        self.paused = self.out.paused.load(Relaxed);
        self.vol_target = self.out.volume.load() as f64;
        self.fade_step = ramp(fade_ms, self.rate);
        self.dip_step = ramp(dip_ms, self.rate);
        self.kill = self.track.kill.load(Acquire);
        let seek = self.track.seek.load(Acquire);
        if seek != NO_TIME && seek != self.seek {
            self.dipping = true;
        }
        self.seek = seek;
        // Trigger the loop one dip early so the jump lands on B.
        self.loop_to = self.track.loop_points().map(|(a, b)| {
            let (a_frame, dip) = (to_frames(a, self.rate), dip_ms as u64 * self.rate as u64 / 1000);
            (to_nanos(a), to_frames(b, self.rate).saturating_sub(dip).max(a_frame + 1))
        });
    }

    /// Control and envelope work at the start of every frame.
    fn begin_frame(&mut self) -> Step {
        if !self.begun {
            self.begun = true;
            if self.track.phase.compare_exchange(PENDING, PLAYING, AcqRel, Acquire).is_err() {
                return Step::End;
            }
            self.read_controls();
            self.vol = self.vol_target;
            self.env = if self.paused || self.fade_in || self.seek != NO_TIME { 0.0 } else { 1.0 };
        } else if self.countdown == 0 {
            self.read_controls();
        }
        self.countdown -= 1;
        let seeking = self.seek != NO_TIME;
        let up = !(self.paused || self.kill || seeking);
        let step = if self.dipping { self.dip_step } else { self.fade_step };
        self.env = if up { (self.env + step).min(1.0) } else { (self.env - step).max(0.0) };
        if self.env == 0.0 {
            if self.kill {
                return Step::End;
            }
            if seeking {
                self.seek_now();
                if !self.paused {
                    // Start coming back right away (no dip at all when fades are off).
                    self.env = self.dip_step.min(1.0);
                }
            }
        }
        if self.env == 1.0 {
            self.dipping = false;
        }
        let hold = self.env == 0.0 && self.paused;
        if hold != self.held {
            self.held = hold;
            self.track.held.store(hold, Release);
            self.flush_tap();
        }
        if hold {
            return Step::Hold;
        }
        self.gain = (self.env * self.env * (3.0 - 2.0 * self.env)) as f32;
        self.vol = glide(self.vol, self.vol_target, self.vol_k, 1e-6);
        Step::Play
    }

    fn end_frame(&mut self) {
        self.ch = 0;
        self.frames += 1;
        self.track.frames.store(self.frames, Relaxed);
        let [l, r] = self.acc;
        self.batch[self.batch_len] = match self.channels {
            1 => [l, l],
            2 => [l, r],
            n => [l / n.div_ceil(2) as f32, r / (n / 2) as f32],
        };
        self.acc = [0.0; 2];
        self.batch_len += 1;
        if self.batch_len == TAP_BATCH {
            self.flush_tap();
        }
        if let Some((a, trigger)) = self.loop_to
            && self.frames >= trigger
            && self.seek == NO_TIME
            && self.track.seek.compare_exchange(NO_TIME, a, AcqRel, Relaxed).is_ok()
        {
            (self.seek, self.dipping) = (a, true);
        }
    }

    fn seek_now(&mut self) {
        let target = self.seek;
        if let Err(e) = self.try_seek(Duration::from_nanos(target))
            && let Ok(mut slot) = self.track.error.try_lock()
        {
            *slot = Some(format!("seek failed: {e}"));
        }
        // A newer request that arrived meanwhile stays pending.
        let _ = self.track.seek.compare_exchange(target, NO_TIME, AcqRel, Relaxed);
        self.seek = NO_TIME;
    }

    fn flush_tap(&mut self) {
        if self.batch_len > 0 {
            let rate = (self.rate as f32 * self.out.speed.load()).round().max(1.0) as u32;
            self.out.tap.push(&self.batch[..self.batch_len], rate);
            self.batch_len = 0;
        }
    }

    fn end(&mut self) -> Option<f32> {
        self.done = true;
        self.flush_tap();
        self.track.held.store(false, Release);
        let _ = self.track.phase.compare_exchange(PLAYING, ENDED, AcqRel, Acquire);
        None
    }
}

impl<S: Source> Iterator for TrackTap<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.zeros > 0 {
            self.zeros -= 1;
            return Some(0.0);
        }
        if self.done {
            return None;
        }
        if self.ch == 0 {
            match self.begin_frame() {
                Step::Play => {}
                Step::Hold => {
                    self.zeros = self.channels - 1;
                    return Some(0.0);
                }
                Step::End => return self.end(),
            }
        }
        let Some(x) = self.inner.next() else { return self.end() };
        let y = x * self.gain;
        self.acc[self.ch & 1] += y;
        self.ch += 1;
        if self.ch == self.channels {
            self.end_frame();
        }
        Some(soft_clip(y * self.vol as f32))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source> Source for TrackTap<S> {
    fn current_span_len(&self) -> Option<usize> {
        if self.done { Some(0) } else { self.inner.current_span_len() }
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    /// Forwards, then resets the frame counter to the target.
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        self.frames = to_frames(pos, self.rate);
        self.track.frames.store(self.frames, Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    fn buffer(channels: u16, rate: u32, samples: Vec<f32>) -> SamplesBuffer {
        SamplesBuffer::new(ChannelCount::new(channels).unwrap(), SampleRate::new(rate).unwrap(), samples)
    }

    fn sine(freq: f64, rate: u32, secs: f64, amp: f32) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize).map(|i| amp * (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn eq(bands: [f32; 10], preamp_db: f32) -> EqSettings {
        EqSettings { enabled: true, preset: "custom".into(), preamp_db, bands }
    }

    fn shared(settings: &EqSettings) -> Arc<EqShared> {
        let s = EqShared::new();
        s.set(settings);
        s
    }

    /// Gain (dB) the equalizer applies to a sine at `freq`, once settled.
    fn measured_db(settings: &EqSettings, freq: f64, rate: u32) -> f64 {
        let input = sine(freq, rate, 0.6, 0.25);
        let out: Vec<f32> = Equalizer::new(buffer(1, rate, input.clone()), shared(settings)).collect();
        let settle = rate as usize / 5;
        20.0 * (rms(&out[settle..]) / rms(&input[settle..])).log10()
    }

    #[test]
    fn band_center_gets_the_band_gain() {
        for rate in [44100, 48000, 96000] {
            for (i, &f) in EQ_FREQS.iter().enumerate().filter(|(_, f)| (**f as f64) < 0.45 * rate as f64) {
                for gain in [6.0, -9.0] {
                    let mut bands = [0.0; 10];
                    bands[i] = gain;
                    let db = measured_db(&eq(bands, 0.0), f as f64, rate);
                    assert!((db - gain as f64).abs() < 0.1, "{f} Hz @ {rate}: {db:.3} dB, want {gain}");
                }
            }
        }
    }

    #[test]
    fn processing_matches_the_designed_response() {
        let settings = eq(eq_preset("rock").unwrap(), -3.0);
        for rate in [22050, 44100, 96000] {
            for f in [40.0, 90.0, 180.0, 700.0, 1500.0, 3000.0, 6000.0, 10000.0] {
                if f >= 0.45 * rate as f64 {
                    continue;
                }
                let (got, want) = (measured_db(&settings, f, rate), eq_response_db(&settings, f as f32, rate) as f64);
                assert!((got - want).abs() < 0.1, "{f} Hz @ {rate}: measured {got:.3}, designed {want:.3}");
            }
        }
        // The band above Nyquist is skipped rather than aliased.
        let mut bands = [0.0; 10];
        bands[9] = 12.0;
        assert_eq!(eq_response_db(&eq(bands, 0.0), 1000.0, 22050), 0.0);
    }

    #[test]
    fn preamp_changes_glide_without_steps() {
        // largest |second difference| of a 220 Hz sine, steady vs right after the change
        let d2 = |x: &[f32]| x.windows(3).map(|w| (w[0] as f64 - 2.0 * w[1] as f64 + w[2] as f64).abs()).fold(0.0, f64::max);
        let params = shared(&eq([0.0; 10], 0.0));
        let mut e = Equalizer::new(buffer(1, 44100, sine(220.0, 44100, 1.0, 0.5)), params.clone());
        let steady = d2(&e.by_ref().take(11025).collect::<Vec<f32>>());
        params.set(&eq([0.0; 10], -12.0));
        let change = d2(&e.take(11025).collect::<Vec<f32>>());
        assert!(change < 2.0 * steady, "steps while the preamp glides: {change} vs {steady}");
    }

    #[test]
    fn disabled_or_flat_is_bit_identical() {
        let input: Vec<f32> = sine(440.0, 44100, 0.2, 0.5).iter().zip(sine(3000.0, 44100, 0.2, 0.3)).flat_map(|(l, r)| [*l, r]).collect();
        let mut disabled = eq([5.0; 10], 3.0);
        disabled.enabled = false;
        for settings in [disabled, eq([0.0; 10], 0.0)] {
            let out: Vec<f32> = Equalizer::new(buffer(2, 44100, input.clone()), shared(&settings)).collect();
            assert_eq!(out, input);
        }
    }

    #[test]
    fn disabling_glides_back_to_exact_bypass() {
        let input = sine(1000.0, 48000, 1.0, 0.25);
        let params = shared(&eq([6.0; 10], -6.0));
        let mut e = Equalizer::new(buffer(1, 48000, input.clone()), params.clone());
        let boosted: Vec<f32> = e.by_ref().take(4800).collect();
        assert_ne!(boosted, input[..4800]);
        params.set(&EqSettings::default());
        let rest: Vec<f32> = e.collect();
        let tail = rest.len() / 2; // after ~0.45 s of gliding
        assert_eq!(rest[tail..], input[4800 + tail..]);
    }

    #[test]
    fn live_changes_glide_without_clicks() {
        // Flip the 500 Hz band between -12 and +12 dB every 50 ms under a 440 Hz tone. A pure
        // sine's second difference is at most A·w²; gliding keeps the output close to that, while
        // a coefficient jump would spike it.
        let rate = 44100;
        let params = shared(&eq([0.0; 10], 0.0));
        let mut e = Equalizer::new(buffer(1, rate, sine(440.0, rate, 0.6, 0.2)), params.clone());
        let mut out = Vec::new();
        for i in 0..12 {
            let mut bands = [0.0; 10];
            bands[4] = if i % 2 == 0 { 12.0 } else { -12.0 };
            params.set(&eq(bands, 0.0));
            out.extend(e.by_ref().take(rate as usize / 20));
        }
        let w = 2.0 * PI * 440.0 / rate as f64;
        let peak = out.iter().fold(0.0f64, |m, v| m.max(v.abs() as f64));
        let curvature = out.windows(3).map(|s| (s[0] as f64 - 2.0 * s[1] as f64 + s[2] as f64).abs()).fold(0.0, f64::max);
        assert!(curvature < 1.5 * peak * w * w, "second difference {curvature:.5} vs sine bound {:.5}", peak * w * w);
    }

    #[test]
    fn channels_are_filtered_independently() {
        let input: Vec<f32> = sine(1000.0, 44100, 0.3, 0.5).into_iter().flat_map(|l| [l, 0.0]).collect();
        let out: Vec<f32> = Equalizer::new(buffer(2, 44100, input), shared(&eq([9.0; 10], 0.0))).collect();
        assert!(out.iter().skip(1).step_by(2).all(|r| r.abs() < 1e-12));
        assert!(rms(&out.iter().step_by(2).copied().collect::<Vec<_>>()) > 0.5);
    }

    #[test]
    fn replaygain_gain_glides_then_passes_through() {
        let target = Arc::new(AtomicF32::new(1.0));
        let mut g = Gain::new(buffer(1, 8000, vec![0.8; 16000]), target.clone());
        assert!(g.by_ref().take(100).all(|x| x == 0.8));
        target.store(0.5);
        let fall: Vec<f32> = g.by_ref().take(4000).collect();
        assert!(fall.windows(2).all(|w| w[1] <= w[0]), "monotonic");
        assert!(fall[0] > 0.79 && fall[3999] == 0.4, "{} .. {}", fall[0], fall[3999]);
        target.store(1.0);
        assert_eq!(g.nth(4000), Some(0.8));
    }

    #[test]
    fn soft_clip_is_transparent_then_bounded() {
        for x in [0.0, 0.3, -0.7, 0.95, -0.95] {
            assert_eq!(soft_clip(x), x);
        }
        let ys: Vec<f32> = (0..400).map(|i| soft_clip(0.9 + i as f32 * 0.01)).collect();
        assert!(ys.windows(2).all(|w| w[1] >= w[0]) && ys.iter().all(|y| *y <= 1.0));
        assert_eq!(soft_clip(-3.0), -soft_clip(3.0));
    }

    #[test]
    fn replaygain_modes_fallbacks_and_clipping() {
        let track =
            Track { rg_track_gain: Some(-6.0), rg_track_peak: Some(1.2), rg_album_gain: Some(3.0), rg_album_peak: Some(0.9), ..Track::default() };
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert_eq!(replaygain_gain(&track, ReplayGainMode::Off, 5.0, true), 1.0);
        assert!(near(replaygain_gain(&track, ReplayGainMode::Track, 0.0, false), db_to_gain(-6.0)));
        // -6 dB keeps the 1.2 peak at 0.6: nothing to cap.
        assert!(near(replaygain_gain(&track, ReplayGainMode::Track, 0.0, true), db_to_gain(-6.0)));
        assert!(near(replaygain_gain(&track, ReplayGainMode::Album, 0.0, false), db_to_gain(3.0)));
        assert!(near(replaygain_gain(&track, ReplayGainMode::Auto, 2.0, false), db_to_gain(5.0)));
        // +3 dB (1.41) on a 0.9 peak would clip: capped at 1/0.9.
        assert!(near(replaygain_gain(&track, ReplayGainMode::Album, 0.0, true), 1.0 / 0.9));
        let track_only = Track { rg_track_gain: Some(4.0), ..Track::default() };
        assert!(near(replaygain_gain(&track_only, ReplayGainMode::Album, 0.0, true), db_to_gain(4.0)));
        assert_eq!(replaygain_gain(&Track::default(), ReplayGainMode::Track, 6.0, true), 1.0);
    }

    #[test]
    fn tap_keeps_the_newest_frames() {
        let tap = SampleTap::new(4);
        tap.push(&[[1.0, -1.0], [2.0, -2.0], [3.0, -3.0]], 48000);
        tap.push(&[[4.0, -4.0], [5.0, -5.0]], 48000);
        let mut out = Vec::new();
        assert_eq!(tap.latest(3, &mut out), 48000);
        assert_eq!(out, vec![[3.0, -3.0], [4.0, -4.0], [5.0, -5.0]]);
        tap.clear();
        tap.latest(10, &mut out);
        assert_eq!(out, vec![[0.0; 2]; 4]);
    }

    fn output_stage(channels: u16, samples: Vec<f32>) -> (TrackTap<SamplesBuffer>, Arc<TrackCtl>, Arc<OutputCtl>) {
        let (track, out) = (TrackCtl::new(), OutputCtl::new(SampleTap::new(1024)));
        (TrackTap::new(buffer(channels, 8000, samples), track.clone(), out.clone(), Duration::ZERO, false), track, out)
    }

    #[test]
    fn tap_folds_surround_to_stereo_before_the_volume() {
        let (stage, _, out) = output_stage(4, [0.2, 0.4, 0.6, 0.8].repeat(100));
        out.volume.store(0.5);
        let played: Vec<f32> = stage.collect();
        assert_eq!(played[..4], [0.1, 0.2, 0.3, 0.4]);
        let mut frames = Vec::new();
        out.tap.latest(1, &mut frames);
        assert!((frames[0][0] - 0.4).abs() < 1e-6 && (frames[0][1] - 0.6).abs() < 1e-6, "{frames:?}");
    }

    #[test]
    fn holding_does_not_consume_the_source() {
        let ramp: Vec<f32> = (0..8000).map(|i| i as f32 / 10000.0).collect();
        let (mut stage, track, out) = output_stage(1, ramp);
        out.fade_ms.store(10, Relaxed); // 80 frames
        assert_eq!(stage.by_ref().take(500).last(), Some(0.0499));
        out.paused.store(true, Relaxed);
        let fading: Vec<f32> = stage.by_ref().take(300).collect();
        assert!(track.held() && fading[250..].iter().all(|x| *x == 0.0));
        let frozen = track.position();
        assert!(stage.by_ref().take(1000).all(|x| x == 0.0));
        assert_eq!(track.position(), frozen);
        out.paused.store(false, Relaxed);
        let back: Vec<f32> = stage.by_ref().take(300).collect();
        assert!(!track.held());
        // Resumes where the fade-out left off: nothing was skipped while held.
        let resumed_at = (frozen.as_secs_f64() * 8000.0).round() as usize;
        assert!((back[299] - (resumed_at + 299 - 64) as f32 / 10000.0).abs() < 0.0065, "{} vs frame {resumed_at}", back[299]);
    }

    #[test]
    fn trackctl_lifecycle_and_loop_points() {
        let t = TrackCtl::new();
        assert!(!t.started() && t.cancel() && !t.cancel());
        let t = TrackCtl::new();
        t.set_loop(Some(Duration::from_secs(2)), Some(Duration::from_secs(1)));
        assert_eq!(t.loop_points(), None);
        t.set_loop(Some(Duration::from_secs(1)), Some(Duration::from_secs(2)));
        assert_eq!(t.loop_points(), Some((Duration::from_secs(1), Duration::from_secs(2))));
        t.seek(Duration::from_millis(1500));
        assert_eq!(t.position(), Duration::from_millis(1500));
        let (mut stage, track, _) = output_stage(1, vec![0.1; 10]);
        assert!(stage.by_ref().count() == 10 && track.ended() && !track.cancel());
    }
}
