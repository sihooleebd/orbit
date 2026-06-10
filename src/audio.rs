//! Audio playback engine: rodio output + a real-time 10-band graphic equalizer.
//!
//! Decoded f32 samples are streamed through a cascade of RBJ peaking biquad
//! filters (one per band, per channel) before reaching the output device. Band
//! gains are shared atomically so the UI can move sliders while audio plays.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use rodio::source::SeekError;
use rodio::{ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source};

/// Outcome of inspecting a `cpal::StreamError`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthEvent {
    /// Real device loss — the stream must be rebuilt.
    DeviceLost,
    /// A transient glitch (XRUN). NOT a device change — never triggers recovery.
    Underrun,
    /// Backend-specific or unknown — logged/counted, no recovery.
    Backend,
}

/// Classify a cpal stream error into a recovery-relevant event.
pub fn classify(err: &rodio::cpal::StreamError) -> HealthEvent {
    use rodio::cpal::StreamError;
    match err {
        StreamError::DeviceNotAvailable | StreamError::StreamInvalidated => HealthEvent::DeviceLost,
        StreamError::BufferUnderrun => HealthEvent::Underrun,
        // BackendSpecific and any future variants.
        _ => HealthEvent::Backend,
    }
}

/// Lock-free device-health signal. Written by the cpal error callback (on the
/// audio thread), read by the engine on the UI thread. Atomics only — no locks,
/// no allocation — so it is safe to touch from the realtime callback.
pub struct DeviceHealth {
    lost: AtomicBool,
    underruns: AtomicU32,
    backend_errors: AtomicU32,
}

impl DeviceHealth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            lost: AtomicBool::new(false),
            underruns: AtomicU32::new(0),
            backend_errors: AtomicU32::new(0),
        })
    }

    /// Record a stream error (called from the cpal error callback).
    pub fn record(&self, err: &rodio::cpal::StreamError) {
        match classify(err) {
            HealthEvent::DeviceLost => self.lost.store(true, Ordering::Release),
            HealthEvent::Underrun => {
                self.underruns.fetch_add(1, Ordering::Relaxed);
            }
            HealthEvent::Backend => {
                self.backend_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Read-and-clear the device-lost flag.
    pub fn take_lost(&self) -> bool {
        self.lost.swap(false, Ordering::AcqRel)
    }
}

/// What the engine should do this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchAction {
    /// Nothing to do.
    None,
    /// Rebuild the output device and resume playback in place.
    Rebuild,
    /// Recovery has failed repeatedly — stop trying, surface an error.
    GiveUp,
}

/// Pure recovery policy. No I/O: the engine feeds it events/ticks and acts on the
/// returned `WatchAction`, which makes the whole policy unit-testable without hardware.
pub struct DeviceWatch {
    tuning: crate::platform::PlatformTuning,
    stall_ticks: u32,
    last_pos: Duration,
    failed_rebuilds: u32,
    /// Ticks remaining in the post-rebuild debounce window.
    cooldown: u32,
}

impl DeviceWatch {
    pub fn new(tuning: crate::platform::PlatformTuning) -> Self {
        Self {
            tuning,
            stall_ticks: 0,
            last_pos: Duration::ZERO,
            failed_rebuilds: 0,
            cooldown: 0,
        }
    }

    /// Reset per-track stall tracking (call when a new source starts). Keeps the
    /// failed-rebuild counter so the cap still applies across a recovery replay.
    pub fn reset(&mut self) {
        self.stall_ticks = 0;
        self.last_pos = Duration::ZERO;
    }

    /// A Tier-1 device-loss event was observed this tick.
    pub fn on_lost(&mut self) -> WatchAction {
        if self.cooldown > 0 {
            // Debounce: a single physical event can surface multiple errors.
            return WatchAction::None;
        }
        self.issue_rebuild()
    }

    /// Per-tick update with current playback state. `pos` is the engine position.
    pub fn on_tick(&mut self, pos: Duration, playing: bool, analyzing: bool) -> WatchAction {
        if self.cooldown > 0 {
            self.cooldown -= 1;
        }
        if !playing {
            self.stall_ticks = 0;
            self.last_pos = pos;
            return WatchAction::None;
        }
        if pos > self.last_pos {
            // Real progress — healthy.
            self.stall_ticks = 0;
            self.failed_rebuilds = 0;
            self.last_pos = pos;
            return WatchAction::None;
        }
        // No progress. Tier-2 heuristic (Linux only), suppressed during analysis
        // and during the post-rebuild cooldown.
        if self.tuning.heuristic_enabled && !analyzing && self.cooldown == 0 {
            self.stall_ticks += 1;
            if self.stall_ticks >= self.tuning.stall_limit_ticks {
                return self.issue_rebuild();
            }
        }
        WatchAction::None
    }

    fn issue_rebuild(&mut self) -> WatchAction {
        if self.failed_rebuilds >= self.tuning.rebuild_cap {
            return WatchAction::GiveUp;
        }
        self.failed_rebuilds += 1;
        self.cooldown = self.tuning.rebuild_window_ticks;
        self.stall_ticks = 0;
        WatchAction::Rebuild
    }
}

pub const NUM_BANDS: usize = 10;

/// Centre frequencies for the graphic EQ (octave-spaced, Hz).
pub const BAND_FREQS: [f32; NUM_BANDS] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// Short labels for the EQ display.
pub const BAND_LABELS: [&str; NUM_BANDS] = [
    "31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k",
];

/// Maximum band/preamp gain magnitude in dB.
pub const MAX_GAIN_DB: f32 = 12.0;

/// Q used for the peaking filters (≈ one octave bandwidth).
const BAND_Q: f32 = 1.41;

// ---------------------------------------------------------------------------
// Shared EQ state
// ---------------------------------------------------------------------------

/// Atomically-shared EQ parameters + playback position counter.
pub struct EqShared {
    gains_millidb: [AtomicI32; NUM_BANDS],
    preamp_millidb: AtomicI32,
    enabled: AtomicBool,
    /// Bumped whenever any coefficient-affecting parameter changes.
    generation: AtomicU64,
    /// Sample rate of the currently playing source (0 if none).
    sample_rate: AtomicU32,
    /// Frames (per-channel samples) emitted for the current source.
    frames: AtomicU64,
    /// Per-band output envelope (0.0..~1.0) for the spectrum analyzer.
    /// Stored as f32 bits.
    levels: [AtomicU32; NUM_BANDS],
}

fn db_to_millidb(db: f32) -> i32 {
    (db * 1000.0).round() as i32
}

fn millidb_to_db(m: i32) -> f32 {
    m as f32 / 1000.0
}

impl EqShared {
    pub fn new(enabled: bool, preamp_db: f32, gains_db: &[f32]) -> Arc<Self> {
        let gains_millidb = std::array::from_fn(|i| {
            AtomicI32::new(db_to_millidb(gains_db.get(i).copied().unwrap_or(0.0)))
        });
        Arc::new(Self {
            gains_millidb,
            preamp_millidb: AtomicI32::new(db_to_millidb(preamp_db)),
            enabled: AtomicBool::new(enabled),
            generation: AtomicU64::new(1),
            sample_rate: AtomicU32::new(0),
            frames: AtomicU64::new(0),
            levels: std::array::from_fn(|_| AtomicU32::new(0)),
        })
    }

    /// Smoothed energy of band `i`, in roughly 0.0..1.0.
    pub fn level(&self, band: usize) -> f32 {
        f32::from_bits(self.levels[band].load(Ordering::Relaxed))
    }

    fn set_level(&self, band: usize, v: f32) {
        self.levels[band].store(v.to_bits(), Ordering::Relaxed);
    }

    /// Fade the spectrum toward zero (used while paused/stopped).
    pub fn decay_levels(&self, factor: f32) {
        for i in 0..NUM_BANDS {
            self.set_level(i, self.level(i) * factor);
        }
    }

    pub fn gain_db(&self, band: usize) -> f32 {
        millidb_to_db(self.gains_millidb[band].load(Ordering::Relaxed))
    }

    pub fn set_gain_db(&self, band: usize, db: f32) {
        let db = db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
        self.gains_millidb[band].store(db_to_millidb(db), Ordering::Relaxed);
        self.touch();
    }

    pub fn nudge_gain(&self, band: usize, delta_db: f32) {
        self.set_gain_db(band, self.gain_db(band) + delta_db);
    }

    pub fn preamp_db(&self) -> f32 {
        millidb_to_db(self.preamp_millidb.load(Ordering::Relaxed))
    }

    pub fn set_preamp_db(&self, db: f32) {
        let db = db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB);
        self.preamp_millidb.store(db_to_millidb(db), Ordering::Relaxed);
        self.touch();
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        self.touch();
    }

    pub fn toggle_enabled(&self) {
        self.set_enabled(!self.enabled());
    }

    pub fn all_gains_db(&self) -> [f32; NUM_BANDS] {
        std::array::from_fn(|i| self.gain_db(i))
    }

    /// Apply a preset's per-band gains.
    pub fn apply_preset(&self, preset: &Preset) {
        for (i, &g) in preset.gains.iter().enumerate() {
            self.gains_millidb[i].store(db_to_millidb(g), Ordering::Relaxed);
        }
        self.touch();
    }

    fn touch(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Current playback position of the active source.
    pub fn position(&self) -> Duration {
        let sr = self.sample_rate.load(Ordering::Relaxed);
        if sr == 0 {
            return Duration::ZERO;
        }
        let frames = self.frames.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / sr as f64)
    }

    fn reset_position(&self) {
        self.frames.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Biquad peaking filter
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    /// Identity (pass-through) filter.
    fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    /// Silent filter (outputs zero) — used for analysis bands above Nyquist.
    fn silent() -> Self {
        Self {
            b0: 0.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    /// RBJ cookbook band-pass (constant 0 dB peak gain) — used for metering.
    fn bandpass(freq: f32, sample_rate: f32, q: f32) -> Self {
        if freq >= sample_rate / 2.0 {
            return Self::silent();
        }
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q);

        let b0 = alpha;
        let b1 = 0.0;
        let b2 = -alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// RBJ cookbook peaking EQ.
    fn peaking(freq: f32, sample_rate: f32, gain_db: f32, q: f32) -> Self {
        if gain_db.abs() < 1e-4 || freq >= sample_rate / 2.0 {
            return Self::identity();
        }
        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }
}

/// Per-channel, per-band filter memory (Direct Form I).
#[derive(Clone, Copy, Default)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

#[inline]
fn process(coeff: &Biquad, st: &mut BiquadState, x: f32) -> f32 {
    let y = coeff.b0 * x + coeff.b1 * st.x1 + coeff.b2 * st.x2 - coeff.a1 * st.y1 - coeff.a2 * st.y2;
    st.x2 = st.x1;
    st.x1 = x;
    st.y2 = st.y1;
    st.y1 = y;
    y
}

// ---------------------------------------------------------------------------
// The Equalizer source wrapper
// ---------------------------------------------------------------------------

struct Equalizer<S: Source> {
    inner: S,
    shared: Arc<EqShared>,
    channels: usize,
    sample_rate: u32,
    coeffs: [Biquad; NUM_BANDS],
    /// One filter chain per channel.
    states: Vec<[BiquadState; NUM_BANDS]>,
    preamp_linear: f32,
    ch: usize,
    last_gen: u64,
    // --- spectrum analysis (mono, derived from the output) ---
    analysis: [Biquad; NUM_BANDS],
    ana_state: [BiquadState; NUM_BANDS],
    env: [f32; NUM_BANDS],
    frame_acc: f32,
}

/// Q for the analysis band-pass filters (sharper = more separated bars).
const ANALYSIS_Q: f32 = 2.5;
/// Per-frame release coefficient for the envelope follower.
const ENV_RELEASE: f32 = 0.9975;

impl<S: Source> Equalizer<S> {
    fn new(inner: S, shared: Arc<EqShared>) -> Self {
        let channels = inner.channels().get() as usize;
        let sample_rate = inner.sample_rate().get();
        shared.sample_rate.store(sample_rate, Ordering::Relaxed);
        shared.reset_position();
        let mut eq = Self {
            inner,
            shared,
            channels,
            sample_rate,
            coeffs: [Biquad::identity(); NUM_BANDS],
            states: vec![[BiquadState::default(); NUM_BANDS]; channels.max(1)],
            preamp_linear: 1.0,
            ch: 0,
            last_gen: 0,
            analysis: [Biquad::silent(); NUM_BANDS],
            ana_state: [BiquadState::default(); NUM_BANDS],
            env: [0.0; NUM_BANDS],
            frame_acc: 0.0,
        };
        eq.recompute();
        eq
    }

    fn recompute(&mut self) {
        let sr = self.sample_rate as f32;
        for (i, freq) in BAND_FREQS.iter().enumerate() {
            let gain = self.shared.gain_db(i);
            self.coeffs[i] = Biquad::peaking(*freq, sr, gain, BAND_Q);
            self.analysis[i] = Biquad::bandpass(*freq, sr, ANALYSIS_Q);
        }
        self.preamp_linear = 10f32.powf(self.shared.preamp_db() / 20.0);
        self.last_gen = self.shared.generation();
    }

    fn reset_states(&mut self) {
        for chain in &mut self.states {
            *chain = [BiquadState::default(); NUM_BANDS];
        }
        self.ana_state = [BiquadState::default(); NUM_BANDS];
        self.frame_acc = 0.0;
        self.ch = 0;
    }
}

impl<S: Source> Iterator for Equalizer<S> {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        let x = self.inner.next()?;

        // Adapt to format changes mid-stream (rare for MP3, but be safe).
        let ch_now = self.inner.channels().get() as usize;
        let sr_now = self.inner.sample_rate().get();
        if ch_now != self.channels {
            self.channels = ch_now.max(1);
            self.states = vec![[BiquadState::default(); NUM_BANDS]; self.channels];
            self.ch = 0;
        }
        if sr_now != self.sample_rate {
            self.sample_rate = sr_now;
            self.shared.sample_rate.store(sr_now, Ordering::Relaxed);
            self.recompute();
        } else if self.shared.generation() != self.last_gen {
            self.recompute();
        }

        let out = if self.shared.enabled() {
            let chain = &mut self.states[self.ch];
            let mut y = x;
            for i in 0..NUM_BANDS {
                y = process(&self.coeffs[i], &mut chain[i], y);
            }
            (y * self.preamp_linear).clamp(-1.0, 1.0)
        } else {
            x
        };

        self.frame_acc += out;
        self.ch += 1;
        if self.ch >= self.channels {
            self.ch = 0;
            // One full frame complete: analyze the mono mix into band envelopes.
            let mono = self.frame_acc / self.channels as f32;
            self.frame_acc = 0.0;
            for i in 0..NUM_BANDS {
                let band = process(&self.analysis[i], &mut self.ana_state[i], mono).abs();
                let e = &mut self.env[i];
                // Instant attack, smooth release.
                *e = if band > *e {
                    band
                } else {
                    *e * ENV_RELEASE + band * (1.0 - ENV_RELEASE)
                };
                self.shared.set_level(i, *e);
            }
            self.shared.frames.fetch_add(1, Ordering::Relaxed);
        }
        Some(out)
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

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        self.reset_states();
        let frames = (pos.as_secs_f64() * self.sample_rate as f64) as u64;
        self.shared.frames.store(frames, Ordering::Relaxed);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

pub struct Preset {
    pub name: &'static str,
    pub gains: [f32; NUM_BANDS],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Flat",
        gains: [0.0; NUM_BANDS],
    },
    Preset {
        name: "Bass Boost",
        gains: [7.0, 6.0, 4.5, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    },
    Preset {
        name: "Treble",
        gains: [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 3.0, 5.0, 6.5, 7.0],
    },
    Preset {
        name: "Vocal",
        gains: [-2.0, -1.5, 0.0, 2.0, 4.0, 4.0, 3.0, 1.5, 0.0, -1.0],
    },
    Preset {
        name: "Loudness",
        gains: [6.0, 4.0, 1.0, 0.0, -1.0, 0.0, 1.0, 3.0, 5.0, 6.0],
    },
];

// ---------------------------------------------------------------------------
// The playback engine
// ---------------------------------------------------------------------------

pub struct Engine {
    // Kept alive for the duration of playback; dropping stops audio.
    device: MixerDeviceSink,
    player: Player,
    eq: Arc<EqShared>,
    volume: f32,
    /// Duration of the source currently appended (from metadata or decoder).
    current_total: Option<Duration>,
    /// Lock-free signal fed by the cpal error callback.
    health: Arc<DeviceHealth>,
    /// Pure recovery policy.
    watch: DeviceWatch,
}

/// Open the default output device with our error callback wired to `health`.
fn open_device(health: Arc<DeviceHealth>) -> Result<MixerDeviceSink> {
    let h = health;
    let builder = DeviceSinkBuilder::from_default_device()
        .map_err(|e| anyhow!("no audio output device: {e}"))?
        .with_error_callback(move |err: rodio::cpal::StreamError| h.record(&err));
    let mut device = builder
        .open_sink_or_fallback()
        .map_err(|e| anyhow!("could not open audio output: {e}"))?;
    device.log_on_drop(false);
    Ok(device)
}

impl Engine {
    pub fn new(volume: f32, eq: Arc<EqShared>) -> Result<Self> {
        let health = DeviceHealth::new();
        let device = open_device(health.clone())?;
        let player = Player::connect_new(&device.mixer());
        player.set_volume(volume);
        Ok(Self {
            device,
            player,
            eq,
            volume,
            current_total: None,
            health,
            watch: DeviceWatch::new(crate::platform::tuning()),
        })
    }

    pub fn eq(&self) -> &Arc<EqShared> {
        &self.eq
    }

    /// A fresh player attached to the current device, replacing the old one.
    ///
    /// We never call `Player::clear()` / `stop()` to swap tracks: those block on
    /// the audio thread draining, which never happens if the output device has
    /// been pulled (e.g. unplugging headphones) — hanging the whole app. Dropping
    /// a `Player` and connecting a new one is non-blocking.
    fn replace_player(&mut self) {
        self.player = Player::connect_new(&self.device.mixer());
        self.player.set_volume(self.volume);
    }

    /// Reopen the default output device (e.g. after it changed) and a fresh
    /// player. Returns false if no device could be opened.
    pub fn rebuild_output(&mut self) -> bool {
        match open_device(self.health.clone()) {
            Ok(device) => {
                let player = Player::connect_new(&device.mixer());
                player.set_volume(self.volume);
                self.device = device; // drops the old (dead) stream
                self.player = player;
                self.health.take_lost(); // clear any stale flag from the dead device
                true
            }
            Err(_) => false,
        }
    }

    /// Drive the recovery state machine for one tick and return the action the
    /// app should take. `playing` = a source is actively producing audio;
    /// `analyzing` = the background recommender is decoding (suppresses the
    /// Linux stall heuristic so it can't false-positive on CPU starvation).
    pub fn poll_health(&mut self, playing: bool, analyzing: bool) -> WatchAction {
        // Tier 1: real device-loss event from cpal.
        if self.health.take_lost() {
            match self.watch.on_lost() {
                WatchAction::None => {}
                other => return other,
            }
        }
        // Tier 2: position-stall heuristic (Linux only, gated inside DeviceWatch).
        let pos = self.position();
        self.watch.on_tick(pos, playing, analyzing)
    }

    /// Decode `path`, wrap it in the equalizer, and start playing it now.
    pub fn play_path(&mut self, path: &Path) -> Result<()> {
        let file = File::open(path).map_err(|e| anyhow!("open {path:?}: {e}"))?;
        let decoder = Decoder::try_from(file).map_err(|e| anyhow!("decode {path:?}: {e}"))?;
        self.current_total = decoder.total_duration();
        let source = Equalizer::new(decoder, self.eq.clone());

        // Replace whatever was playing (non-blocking — see replace_player).
        self.replace_player();
        self.watch.reset();
        self.player.append(source);
        self.player.play();
        Ok(())
    }

    pub fn toggle_pause(&self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.player.is_paused()
    }

    /// True when the current source has finished and nothing is queued.
    pub fn is_finished(&self) -> bool {
        self.player.empty()
    }

    pub fn stop(&mut self) {
        self.replace_player();
        self.current_total = None;
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.25);
        self.player.set_volume(self.volume);
    }

    /// Set the live output volume without changing the stored volume (used for
    /// the sleep-timer fade-out).
    pub fn set_output_volume(&self, v: f32) {
        self.player.set_volume(v.clamp(0.0, 1.25));
    }

    /// Restore the live output volume to the stored value.
    pub fn restore_volume(&self) {
        self.player.set_volume(self.volume);
    }

    pub fn nudge_volume(&mut self, delta: f32) {
        let v = self.volume + delta;
        self.set_volume(v);
    }

    pub fn position(&self) -> Duration {
        self.eq.position()
    }

    pub fn total(&self) -> Option<Duration> {
        self.current_total
    }

    pub fn seek(&self, pos: Duration) {
        let _ = self.player.try_seek(pos);
    }

    /// Seek relative to the current position, clamped to the track bounds.
    pub fn seek_relative(&self, delta_secs: i64) {
        let cur = self.position().as_secs_f64();
        let mut target = cur + delta_secs as f64;
        if target < 0.0 {
            target = 0.0;
        }
        if let Some(total) = self.current_total {
            let max = total.as_secs_f64() - 0.5;
            if target > max {
                target = max.max(0.0);
            }
        }
        self.seek(Duration::from_secs_f64(target));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_band_is_transparent() {
        // A 0 dB peaking filter must pass samples through unchanged.
        let coeff = Biquad::peaking(1000.0, 44100.0, 0.0, BAND_Q);
        let mut st = BiquadState::default();
        for &x in &[0.0f32, 0.5, -0.3, 0.9, -1.0, 0.25] {
            let y = process(&coeff, &mut st, x);
            assert!((y - x).abs() < 1e-6, "expected {x}, got {y}");
        }
    }

    #[test]
    fn boost_increases_energy_at_center() {
        // A boosted peaking filter should amplify a tone near its centre.
        let sr = 44100.0;
        let freq = 1000.0;
        let flat = Biquad::peaking(freq, sr, 0.0, BAND_Q);
        let boost = Biquad::peaking(freq, sr, 9.0, BAND_Q);
        let mut sf = BiquadState::default();
        let mut sb = BiquadState::default();
        let mut energy_flat = 0.0f32;
        let mut energy_boost = 0.0f32;
        for n in 0..2000 {
            let t = n as f32 / sr;
            let x = (2.0 * std::f32::consts::PI * freq * t).sin();
            energy_flat += process(&flat, &mut sf, x).powi(2);
            energy_boost += process(&boost, &mut sb, x).powi(2);
        }
        assert!(
            energy_boost > energy_flat * 1.5,
            "boost {energy_boost} vs flat {energy_flat}"
        );
    }

    #[test]
    fn gains_clamp_to_range() {
        let eq = EqShared::new(true, 0.0, &[0.0; NUM_BANDS]);
        eq.set_gain_db(0, 999.0);
        assert!((eq.gain_db(0) - MAX_GAIN_DB).abs() < 1e-3);
        eq.set_gain_db(0, -999.0);
        assert!((eq.gain_db(0) + MAX_GAIN_DB).abs() < 1e-3);
    }

    #[test]
    fn classify_maps_stream_errors() {
        use rodio::cpal::StreamError;
        assert_eq!(classify(&StreamError::DeviceNotAvailable), HealthEvent::DeviceLost);
        assert_eq!(classify(&StreamError::StreamInvalidated), HealthEvent::DeviceLost);
        assert_eq!(classify(&StreamError::BufferUnderrun), HealthEvent::Underrun);
        let backend = StreamError::BackendSpecific {
            err: rodio::cpal::BackendSpecificError { description: "boom".into() },
        };
        assert_eq!(classify(&backend), HealthEvent::Backend);
    }

    #[test]
    fn device_health_records_and_takes_lost() {
        use rodio::cpal::StreamError;
        let h = DeviceHealth::new();
        assert!(!h.take_lost(), "fresh health is not lost");
        h.record(&StreamError::BufferUnderrun);
        assert!(!h.take_lost(), "underrun must not set lost");
        h.record(&StreamError::DeviceNotAvailable);
        assert!(h.take_lost(), "device-lost event sets the flag");
        assert!(!h.take_lost(), "take_lost clears the flag");
    }

    fn linux_tuning() -> crate::platform::PlatformTuning {
        crate::platform::PlatformTuning {
            heuristic_enabled: true,
            stall_limit_ticks: 3,
            rebuild_cap: 2,
            rebuild_window_ticks: 2,
        }
    }

    fn mac_tuning() -> crate::platform::PlatformTuning {
        crate::platform::PlatformTuning { heuristic_enabled: false, ..linux_tuning() }
    }

    const Z: Duration = Duration::ZERO;
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn event_triggers_rebuild() {
        let mut w = DeviceWatch::new(mac_tuning());
        assert_eq!(w.on_lost(), WatchAction::Rebuild);
    }

    #[test]
    fn repeated_failures_give_up_at_cap() {
        let mut w = DeviceWatch::new(mac_tuning()); // rebuild_cap = 2
        assert_eq!(w.on_lost(), WatchAction::Rebuild); // attempt 1
        // Drain the post-rebuild debounce window so the next event counts.
        w.on_tick(Z, false, false);
        w.on_tick(Z, false, false);
        assert_eq!(w.on_lost(), WatchAction::Rebuild); // attempt 2
        w.on_tick(Z, false, false);
        w.on_tick(Z, false, false);
        assert_eq!(w.on_lost(), WatchAction::GiveUp); // cap reached
    }

    #[test]
    fn progress_resets_failure_count() {
        let mut w = DeviceWatch::new(mac_tuning());
        assert_eq!(w.on_lost(), WatchAction::Rebuild);
        // Playback resumes and advances — healthy again.
        w.on_tick(ms(100), true, false);
        w.on_tick(ms(200), true, false);
        // A later event should rebuild again, not give up.
        for _ in 0..3 {
            w.on_tick(ms(200), false, false);
        }
        assert_eq!(w.on_lost(), WatchAction::Rebuild);
    }

    #[test]
    fn linux_stall_triggers_rebuild() {
        let mut w = DeviceWatch::new(linux_tuning()); // stall_limit_ticks = 3
        // First establish a baseline position, then freeze it.
        assert_eq!(w.on_tick(ms(500), true, false), WatchAction::None);
        assert_eq!(w.on_tick(ms(500), true, false), WatchAction::None); // stall 1
        assert_eq!(w.on_tick(ms(500), true, false), WatchAction::None); // stall 2
        assert_eq!(w.on_tick(ms(500), true, false), WatchAction::Rebuild); // stall 3 == limit
    }

    #[test]
    fn linux_stall_suppressed_while_analyzing() {
        let mut w = DeviceWatch::new(linux_tuning());
        w.on_tick(ms(500), true, false);
        for _ in 0..10 {
            assert_eq!(w.on_tick(ms(500), true, /*analyzing=*/ true), WatchAction::None);
        }
    }

    #[test]
    fn macos_stall_never_triggers_rebuild() {
        let mut w = DeviceWatch::new(mac_tuning()); // heuristic disabled
        w.on_tick(ms(500), true, false);
        for _ in 0..20 {
            assert_eq!(w.on_tick(ms(500), true, false), WatchAction::None);
        }
    }

    #[test]
    fn reset_clears_stall_but_not_failures() {
        let mut w = DeviceWatch::new(linux_tuning());
        assert_eq!(w.on_lost(), WatchAction::Rebuild); // failed_rebuilds = 1
        w.reset(); // new track starts at position 0
        // A fresh track at pos 0 must not be read as an instant stall.
        assert_eq!(w.on_tick(Z, true, false), WatchAction::None);
        assert_eq!(w.on_tick(ms(50), true, false), WatchAction::None);
    }
}
