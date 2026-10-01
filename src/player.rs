//! The playback engine: owns the audio output (rodio 0.22 `DeviceSinkBuilder` + `Player`) and one
//! processing chain per track (decoder -> ReplayGain -> EQ -> fades/position/tap/volume, see
//! `dsp`), gapless preloading, seeking, A-B looping, volume, speed.
//!
//! Nothing here waits on the audio thread: controls are atomics the chains pick up within ~1.5 ms,
//! and `poll` (call it every tick) turns what the audio thread did into events. (rodio's own
//! `Player::clear` / `try_seek` / `stop` block until the audio thread responds, so they're not
//! used.) Formats symphonia can't decode (Opus, ALAC, WMA, ...) are decoded by an `ffmpeg` child
//! process when one is installed.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::cpal::{BufferSize, Device, StreamError, SupportedBufferSize};
use rodio::decoder::DecoderError;
use rodio::source::SeekError;
use rodio::{ChannelCount, Decoder, DeviceSinkBuilder, DeviceSinkError, MixerDeviceSink, Player, SampleRate, Source};

use crate::config::{EqSettings, PlaybackConfig, ReplayGainMode};
use crate::dsp::{AtomicF32, EqShared, Equalizer, Gain, OutputCtl, SampleTap, TrackCtl, TrackTap, replaygain_gain};
use crate::library::{Track, TrackId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlayState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    /// A track began playing (after `load`, or a gapless transition).
    Started(TrackId),
    /// The current track reached its end and nothing was preloaded; the engine is now Stopped.
    Finished(TrackId),
    /// Gapless: `from` ended and the preloaded `to` is now current (a `Started(to)` is NOT sent).
    Advanced { from: TrackId, to: TrackId },
    /// Decoding/output failed. With `track` = Some, that track was skipped/stopped.
    Error { track: Option<TrackId>, message: String },
}

/// 0..=150 % -> linear gain on a cubic (perceptual) curve: 50 % ≈ -18 dB, 100 % = unity,
/// 150 % ≈ +10.6 dB.
pub fn volume_gain(percent: u8) -> f32 {
    (percent.min(150) as f32 / 100.0).powi(3)
}

/// A track's processing chain as handed to the `Player`.
type ChainSource = TrackTap<Equalizer<Gain<Box<dyn Source + Send>>>>;

/// An ffmpeg decoding process (shared so a withdrawn chain's process can be stopped early).
type Process = Arc<Mutex<Child>>;

/// The engine's handle on a chain living in the `Player`.
struct Chain {
    track: Track,
    ctl: Arc<TrackCtl>,
    replaygain: Arc<AtomicF32>,
    duration: Option<Duration>,
    /// The decoding ffmpeg process, when the track needs one.
    ffmpeg: Option<Process>,
}

impl Chain {
    /// Withdraw the chain if it hasn't started. rodio only drops a queued source once playback
    /// reaches it, so an ffmpeg process behind it is stopped right away instead.
    fn cancel(&self) -> bool {
        let cancelled = self.ctl.cancel();
        if cancelled {
            self.stop_ffmpeg();
        }
        cancelled
    }

    /// `cancel`, or fade out and end a chain that is already playing.
    fn discard(&self) {
        if self.ctl.discard() {
            self.stop_ffmpeg();
        }
    }

    fn stop_ffmpeg(&self) {
        if let Some(Ok(mut child)) = self.ffmpeg.as_ref().map(|c| c.lock()) {
            let _ = child.kill();
        }
    }
}

/// How long the device stays open with nothing to play: an open stream keeps the Mac from idle
/// sleep and costs CPU, while reopening it takes a few ms.
const IDLE_CLOSE: Duration = Duration::from_secs(10);

/// The real audio device (absent for a detached engine).
struct Output {
    /// None while closed for being idle; `wake` reopens it.
    sink: Option<MixerDeviceSink>,
    /// Since when nothing plays (stopped, or paused and faded out).
    idle_since: Option<Instant>,
    errors: Receiver<StreamError>,
    report: Sender<StreamError>,
    /// Streams replaced by a rate change, kept until their tracks have faded out.
    retiring: Vec<(Player, MixerDeviceSink, Instant)>,
}

pub struct Engine {
    player: Player,
    output: Option<Output>,
    out: Arc<OutputCtl>,
    eq: Arc<EqShared>,
    state: PlayState,
    current: Option<Chain>,
    preload: Option<Chain>,
    events: Vec<EngineEvent>,
    replaygain: (ReplayGainMode, f32, bool),
    speed: f32,
}

impl Engine {
    /// Open the default audio device. Err if no output device is available.
    pub fn new(cfg: &PlaybackConfig) -> anyhow::Result<Engine> {
        let (report, errors) = mpsc::channel();
        let sink = open_output(report.clone(), None)?;
        let mut engine = Engine::with_player(Player::connect_new(sink.mixer()));
        engine.output = Some(Output { sink: Some(sink), idle_since: None, errors, report, retiring: Vec::new() });
        engine.set_fade(cfg.fade_ms);
        engine.set_volume(cfg.volume, false);
        engine.set_replaygain(cfg.replaygain, cfg.replaygain_preamp_db, cfg.replaygain_prevent_clip);
        Ok(engine)
    }

    /// An engine without a device: samples are pulled from the returned queue output (tests).
    #[cfg(test)]
    pub fn new_detached() -> (Engine, rodio::queue::SourcesQueueOutput) {
        let (player, output) = Player::new();
        (Engine::with_player(player), output)
    }

    fn with_player(player: Player) -> Engine {
        Engine {
            player,
            output: None,
            out: OutputCtl::new(SampleTap::new(16384)),
            eq: EqShared::new(),
            state: PlayState::Stopped,
            current: None,
            preload: None,
            events: Vec::new(),
            replaygain: (ReplayGainMode::Off, 0.0, true),
            speed: 1.0,
        }
    }

    /// Stop whatever plays, clear the preload, start `track` at `start` (paused if `paused`).
    /// Emits `Started`. Err (and state Stopped) if the file can't be opened/decoded.
    ///
    /// The previous track fades out (`fade_ms`) before the new one starts; a start mid-track fades
    /// in. Pending events about the replaced tracks are dropped, as is the A-B loop. The output
    /// follows the track's sample rate (see `match_rate`).
    pub fn load(&mut self, track: &Track, start: Duration, paused: bool) -> anyhow::Result<()> {
        let built = self.build(track, start);
        self.discard_all();
        let (source, chain) = built.inspect_err(|_| self.state = PlayState::Stopped)?;
        self.wake(Some(source.sample_rate()));
        self.match_rate(source.sample_rate());
        self.out.paused.store(paused, Relaxed);
        // Even when loading paused: the replaced chains must run out, then the new one holds
        // itself and `poll` pauses the player.
        self.player.play();
        self.player.append(source);
        self.current = Some(chain);
        self.state = if paused { PlayState::Paused } else { PlayState::Playing };
        self.events.push(EngineEvent::Started(track.id));
        Ok(())
    }

    /// Queue `track` to start the instant the current one ends (gapless). Replaces any earlier preload.
    pub fn preload(&mut self, track: &Track) -> anyhow::Result<()> {
        self.cancel_preload();
        if self.current.is_none() {
            bail!("nothing is playing");
        }
        let (source, chain) = self.build(track, Duration::ZERO)?;
        self.player.append(source);
        self.preload = Some(chain);
        Ok(())
    }

    pub fn cancel_preload(&mut self) {
        self.sync();
        if let Some(p) = self.preload.take()
            && !p.cancel()
        {
            // It started in the meantime: the current track just ended and this is the
            // transition, reported as `Advanced` by the next poll.
            self.preload = Some(p);
            self.sync();
        }
    }

    pub fn preloaded(&self) -> Option<TrackId> {
        self.preload.as_ref().map(|p| p.track.id)
    }

    /// Fades out, then clears.
    pub fn stop(&mut self) {
        self.discard_all();
        self.state = PlayState::Stopped;
        self.out.paused.store(false, Relaxed);
        // Let held chains run out their fade instead of lingering in a paused player.
        self.player.play();
    }

    /// Fades out; `poll` pauses the player once the fade has finished.
    pub fn pause(&mut self) {
        if self.state == PlayState::Playing {
            self.out.paused.store(true, Relaxed);
            self.state = PlayState::Paused;
        }
    }

    /// Fades back in.
    pub fn resume(&mut self) {
        if self.state == PlayState::Paused {
            self.wake(self.current.as_ref().and_then(|c| SampleRate::new(c.ctl.sample_rate())));
            self.out.paused.store(false, Relaxed);
            self.player.play();
            self.state = PlayState::Playing;
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // public helper; used by the tests
    pub fn toggle(&mut self) {
        match self.state {
            PlayState::Playing => self.pause(),
            PlayState::Paused => self.resume(),
            PlayState::Stopped => {}
        }
    }

    /// Seek within the current track (clamped to its length). Never blocks: `position` reports
    /// the target right away; the jump happens on the audio thread with a short dip (at resume
    /// when paused). A failed seek arrives later as an `Error` event.
    pub fn seek(&mut self, pos: Duration) -> anyhow::Result<()> {
        self.sync();
        let chain = self.current.as_ref().context("nothing is playing")?;
        chain.ctl.seek(chain.duration.map_or(pos, |d| pos.min(d)));
        Ok(())
    }

    pub fn state(&self) -> PlayState {
        self.state
    }

    pub fn current(&self) -> Option<TrackId> {
        self.current.as_ref().map(|c| c.track.id)
    }

    /// Position in the current track's own timeline (independent of speed).
    pub fn position(&self) -> Duration {
        self.current.as_ref().map_or(Duration::ZERO, |c| {
            let pos = c.ctl.position();
            c.duration.map_or(pos, |d| pos.min(d))
        })
    }

    pub fn duration(&self) -> Option<Duration> {
        self.current.as_ref().and_then(|c| c.duration)
    }

    /// `percent` 0..=150 (above 100 amplifies), mapped through a perceptual curve.
    pub fn set_volume(&mut self, percent: u8, muted: bool) {
        self.out.volume.store(if muted { 0.0 } else { volume_gain(percent) });
    }

    /// 0.25..=4.0 (pitch changes with speed).
    pub fn set_speed(&mut self, speed: f32) {
        self.speed = if speed.is_finite() { speed.clamp(0.25, 4.0) } else { 1.0 };
        self.player.set_speed(self.speed);
        self.out.speed.store(self.speed);
    }

    /// Applies immediately (smoothly) to what is playing.
    pub fn set_eq(&mut self, eq: &EqSettings) {
        self.eq.set(eq);
    }

    /// Applies to tracks loaded from now on (and the current one if possible).
    pub fn set_replaygain(&mut self, mode: ReplayGainMode, preamp_db: f32, prevent_clip: bool) {
        self.replaygain = (mode, preamp_db, prevent_clip);
        for chain in self.current.iter().chain(&self.preload) {
            chain.replaygain.store(replaygain_gain(&chain.track, mode, preamp_db, prevent_clip));
        }
    }

    /// Loop between A and B (both Some and A < B); None clears. Enforced sample-accurately on the
    /// audio thread: reaching B, or being past it, jumps to A. Cleared when another track starts.
    pub fn set_ab_loop(&mut self, a: Option<Duration>, b: Option<Duration>) {
        if let Some(chain) = &self.current {
            chain.ctl.set_loop(a, b);
        }
    }

    /// Fade length used for pause/resume/stop/seek (0 = off).
    pub fn set_fade(&mut self, ms: u32) {
        self.out.fade_ms.store(ms, Relaxed);
    }

    /// The stereo samples being played, for the visualizer.
    pub fn tap(&self) -> Arc<SampleTap> {
        self.out.tap.clone()
    }

    /// Drive the engine: detect track ends and gapless transitions, finish pauses, report errors.
    pub fn poll(&mut self) -> Vec<EngineEvent> {
        self.sync();
        if let Some(chain) = &self.current {
            if self.state == PlayState::Paused && chain.ctl.held() && !self.player.is_paused() {
                self.player.pause();
            }
            if let Some(message) = chain.ctl.take_error() {
                self.events.push(EngineEvent::Error { track: None, message });
            }
        }
        self.check_output();
        self.close_when_idle();
        std::mem::take(&mut self.events)
    }

    /// Open and wire up a chain for `track`, positioned at `start` (fading in when mid-track).
    fn build(&self, track: &Track, start: Duration) -> anyhow::Result<(ChainSource, Chain)> {
        let (source, start, ffmpeg) = open(track, start)?;
        let duration = source.total_duration().or((!track.duration.is_zero()).then_some(track.duration));
        let (mode, preamp_db, prevent_clip) = self.replaygain;
        let replaygain = Arc::new(AtomicF32::new(replaygain_gain(track, mode, preamp_db, prevent_clip)));
        let ctl = TrackCtl::new();
        let gain = Gain::new(source, replaygain.clone());
        let chain = TrackTap::new(Equalizer::new(gain, self.eq.clone()), ctl.clone(), self.out.clone(), start, !start.is_zero());
        Ok((chain, Chain { track: track.clone(), ctl, replaygain, duration, ffmpeg }))
    }

    /// Fade out and drop the current and preloaded chains, with any events about them.
    fn discard_all(&mut self) {
        for chain in self.current.take().into_iter().chain(self.preload.take()) {
            chain.discard();
        }
        self.events.retain(|e| matches!(e, EngineEvent::Error { .. }));
        self.out.tap.clear();
    }

    /// Turn chain lifecycle changes into events: end of track, gapless transitions.
    fn sync(&mut self) {
        while let Some(from) = self.current.as_ref().filter(|c| c.ctl.ended()).map(|c| c.track.id) {
            match &self.preload {
                Some(next) if next.ctl.started() => {
                    let to = next.track.id;
                    self.current = self.preload.take();
                    self.events.push(EngineEvent::Advanced { from, to });
                }
                // The queue is switching over right now.
                Some(_) => break,
                None => {
                    self.current = None;
                    self.state = PlayState::Stopped;
                    self.out.paused.store(false, Relaxed);
                    self.out.tap.clear();
                    self.events.push(EngineEvent::Finished(from));
                }
            }
        }
    }

    /// Run the output at the track's own rate: rodio then passes samples through untouched and the
    /// OS (CoreAudio's converter on macOS) adapts them to the device, far better than rodio's linear
    /// resampler. The replaced stream keeps playing the old track's fade-out, then retires.
    fn match_rate(&mut self, rate: SampleRate) {
        let Some(output) = &mut self.output else { return };
        // closed while idle: `wake` reopens it at the track's rate
        let Some(current) = &mut output.sink else { return };
        if current.config().sample_rate() == rate {
            return;
        }
        let Ok(sink) = open_output(output.report.clone(), Some(rate)) else { return };
        let player = Player::connect_new(sink.mixer());
        player.set_speed(self.speed);
        let old_player = std::mem::replace(&mut self.player, player);
        let old_sink = std::mem::replace(current, sink);
        // The fade runs in track time, so it takes longer at slow speeds; then the device buffer.
        let until = Instant::now() + Duration::from_secs_f32(self.out.fade_ms.load(Relaxed) as f32 / 1000.0 / self.speed + 0.15);
        output.retiring.push((old_player, old_sink, until));
    }

    /// Report device errors; when the device is gone, reopen the default one and carry on.
    fn check_output(&mut self) {
        let Some(output) = &mut self.output else { return };
        let now = Instant::now();
        output.retiring.retain(|(_, _, until)| *until > now);
        let mut lost = false;
        while let Ok(err) = output.errors.try_recv() {
            lost |= matches!(err, StreamError::DeviceNotAvailable | StreamError::StreamInvalidated);
            self.events.push(EngineEvent::Error { track: None, message: format!("audio output: {err}") });
        }
        if !lost {
            return;
        }
        output.sink = None;
        self.wake(self.current.as_ref().and_then(|c| SampleRate::new(c.ctl.sample_rate())));
    }

    /// Close the device once nothing has played for `IDLE_CLOSE`.
    fn close_when_idle(&mut self) {
        let idle = self.state == PlayState::Stopped || (self.state == PlayState::Paused && self.player.is_paused());
        let Some(output) = self.output.as_mut().filter(|o| o.sink.is_some()) else { return };
        if !idle || !output.retiring.is_empty() {
            output.idle_since = None;
        } else if output.idle_since.get_or_insert_with(Instant::now).elapsed() >= IDLE_CLOSE {
            output.sink = None;
            output.idle_since = None;
        }
    }

    /// Reopen a closed device (at `rate` if it can) and move playback onto it.
    fn wake(&mut self, rate: Option<SampleRate>) {
        let Some(output) = self.output.as_mut().filter(|o| o.sink.is_none()) else { return };
        match open_output(output.report.clone(), rate).or_else(|_| open_output(output.report.clone(), None)) {
            Ok(sink) => {
                let player = Player::connect_new(sink.mixer());
                output.sink = Some(sink);
                self.replace_player(player);
            }
            Err(e) => self.events.push(EngineEvent::Error { track: None, message: format!("no audio output: {e}") }),
        }
    }

    /// Move playback onto a new `Player`, rebuilding the current (at its position) and preloaded
    /// chains, without events for the rebuild itself.
    fn replace_player(&mut self, player: Player) {
        player.set_speed(self.speed);
        drop(std::mem::replace(&mut self.player, player));
        let (current, preload) = (self.current.take(), self.preload.take());
        if let Some(old) = current {
            let pos = old.ctl.position();
            match self.build(&old.track, pos) {
                Ok((source, chain)) => {
                    let (a, b) = old.ctl.loop_points().unzip();
                    chain.ctl.set_loop(a, b);
                    self.player.append(source);
                    self.current = Some(chain);
                }
                Err(e) => {
                    self.state = PlayState::Stopped;
                    self.events.push(EngineEvent::Error { track: Some(old.track.id), message: e.to_string() });
                }
            }
        }
        if let Some(p) = preload.filter(|_| self.current.is_some())
            && let Err(e) = self.preload(&p.track)
        {
            self.events.push(EngineEvent::Error { track: Some(p.track.id), message: e.to_string() });
        }
    }
}

impl Drop for Engine {
    /// Quitting mid-song fades out instead of cutting off (the terminal is restored by then).
    fn drop(&mut self) {
        let fade = self.out.fade_ms.load(Relaxed);
        if self.output.is_some() && self.state == PlayState::Playing && fade > 0 {
            self.stop();
            // fade + the device buffer (<= 50 ms)
            std::thread::sleep(Duration::from_millis(fade as u64 + 60));
        }
    }
}

/// The default output device at `rate` (its own rate when None, then also trying the other
/// devices), with an error callback that reports instead of printing: this is a TUI, nothing may
/// write to the terminal.
fn open_output(report: Sender<StreamError>, rate: Option<SampleRate>) -> anyhow::Result<MixerDeviceSink> {
    let on_error = move |err: StreamError| {
        if !matches!(err, StreamError::BufferUnderrun) {
            let _ = report.send(err);
        }
    };
    let host = rodio::cpal::default_host();
    let default = host.default_output_device().ok_or(DeviceSinkError::NoDevice);
    let mut sink = match default.and_then(|d| open_device(d, rate, on_error.clone())) {
        Ok(sink) => sink,
        Err(e) if rate.is_none() => host.output_devices()?.find_map(|d| open_device(d, None, on_error.clone()).ok()).ok_or(e)?,
        Err(e) => return Err(e.into()),
    };
    sink.log_on_drop(false);
    Ok(sink)
}

fn open_device(
    device: Device,
    rate: Option<SampleRate>,
    on_error: impl FnMut(StreamError) + Clone + Send + 'static,
) -> Result<MixerDeviceSink, DeviceSinkError> {
    let config = device.default_output_config().map_err(DeviceSinkError::DefaultSinkConfigError)?;
    // ~50 ms of buffer, within the range the device takes (rodio's own choice can exceed it).
    let buffer = match *config.buffer_size() {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed((config.sample_rate() / 20).clamp(min, max)),
        SupportedBufferSize::Unknown => BufferSize::Default,
    };
    let builder =
        DeviceSinkBuilder::default().with_device(device).with_supported_config(&config).with_buffer_size(buffer).with_error_callback(on_error);
    match rate {
        Some(rate) => builder.with_sample_rate(rate).open_stream(),
        None => builder.open_sink_or_fallback(),
    }
}

/// Decoder for `track`, positioned at `start` (or at 0 if seeking there fails; the actual start
/// is returned), plus its ffmpeg process if it takes one.
fn open(track: &Track, start: Duration) -> anyhow::Result<(Box<dyn Source + Send>, Duration, Option<Process>)> {
    // Unreadable files fail here; any decoder error below (symphonia reports unknown formats in
    // several ways) is a reason to try ffmpeg.
    let file = File::open(&track.path)?;
    // symphonia ignores MP4 edit lists, so AAC would play its encoder priming and padding: ~40 ms
    // of silence at every gapless join
    let edit = if track.format == "AAC" { mp4_edit(&track.path) } else { None };
    let trim = |dec: Decoder<_>| -> Box<dyn Source + Send> {
        match edit {
            Some((skip, len)) => Box::new(Trim::new(dec, skip, len)),
            None => Box::new(dec),
        }
    };
    // symphonia's demuxers panic on some malformed files (isomp4 esds/stsc): probe on a helper
    // thread so a panic becomes a decode error (-> ffmpeg), not a crash of the UI thread.
    let probed = std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("orbit-probe".into())
            .spawn_scoped(s, || {
                Decoder::try_from(file).map(|dec| {
                    let mut dec = trim(dec);
                    let seeked = start.is_zero() || dec.try_seek(start).is_ok();
                    (dec, seeked)
                })
            })
            .map_or(Err(DecoderError::UnrecognizedFormat), |h| h.join().unwrap_or(Err(DecoderError::UnrecognizedFormat)))
    });
    match probed {
        Ok((dec, seeked)) => {
            if seeked {
                return Ok((dec, start, None));
            }
            // A failed seek can leave the decoder in a bad state: start over from the top.
            let dec = Decoder::try_from(File::open(&track.path)?).map_err(|e| anyhow!(describe(&e)))?;
            Ok((trim(dec), Duration::ZERO, None))
        }
        Err(err) => {
            let Some(ffmpeg) = ffmpeg() else { bail!("{} (installing ffmpeg adds more formats)", describe(&err)) };
            let source = FfmpegSource::open(ffmpeg, track, start).map_err(|e| anyhow!("{}; ffmpeg: {e}", describe(&err)))?;
            let process = source.pipe.child.clone();
            Ok((Box::new(source), start, Some(process)))
        }
    }
}

/// (priming to skip, length to play) from the edit list of an MP4 file's audio track (no length:
/// to the end); None for anything else (not MP4, no or several edits).
fn mp4_edit(path: &Path) -> Option<(Duration, Option<Duration>)> {
    use std::io::{Seek, SeekFrom};
    type Mp4Box = ([u8; 4], u64, u64); // type, body start, end
    fn children(f: &mut File, mut pos: u64, end: u64) -> Vec<Mp4Box> {
        let mut out = Vec::new();
        let mut h = [0u8; 16];
        while pos + 8 <= end && f.seek(SeekFrom::Start(pos)).is_ok() && f.read_exact(&mut h[..8]).is_ok() {
            let (mut size, mut body) = (u64::from(u32::from_be_bytes([h[0], h[1], h[2], h[3]])), pos + 8);
            if size == 1 {
                if f.read_exact(&mut h[8..]).is_err() {
                    break;
                }
                (size, body) = (u64::from_be_bytes(h[8..].try_into().unwrap()), pos + 16);
            } else if size == 0 {
                size = end - pos;
            }
            if size < body - pos || size > end - pos {
                break;
            }
            out.push(([h[4], h[5], h[6], h[7]], body, pos + size));
            pos += size;
        }
        out
    }
    fn find(boxes: &[Mp4Box], kind: &[u8; 4]) -> Option<Mp4Box> {
        boxes.iter().find(|b| &b.0 == kind).copied()
    }
    fn body(f: &mut File, (_, start, end): Mp4Box) -> Option<Vec<u8>> {
        let mut buf = vec![0; (end - start).min(64) as usize];
        f.seek(SeekFrom::Start(start)).ok()?;
        f.read_exact(&mut buf).ok()?;
        Some(buf)
    }
    let be = |b: &[u8], at: usize, n: usize| b.get(at..at + n).map(|s| s.iter().fold(0u64, |v, &x| v << 8 | u64::from(x)));
    // mvhd / mdhd: version, flags, creation and modification time (4 or 8 bytes each), timescale
    let timescale = |b: &[u8]| be(b, if b.first() == Some(&1) { 20 } else { 12 }, 4).filter(|&t| t > 0);

    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let moov = find(&children(&mut f, 0, len), b"moov")?;
    let moov = children(&mut f, moov.1, moov.2);
    let movie_scale = timescale(&body(&mut f, find(&moov, b"mvhd")?)?)?;
    for trak in moov.iter().filter(|b| &b.0 == b"trak") {
        let trak = children(&mut f, trak.1, trak.2);
        let Some(mdia) = find(&trak, b"mdia") else { continue };
        let mdia = children(&mut f, mdia.1, mdia.2);
        if body(&mut f, find(&mdia, b"hdlr")?)?.get(8..12) != Some(b"soun") {
            continue;
        }
        let media_scale = timescale(&body(&mut f, find(&mdia, b"mdhd")?)?)?;
        let edts = find(&trak, b"edts")?;
        let elst = find(&children(&mut f, edts.1, edts.2), b"elst")?;
        let elst = body(&mut f, elst)?;
        // version, flags, entry count, then (segment duration, media time, rate) with 4- or 8-byte times
        let w = if elst.first() == Some(&1) { 8 } else { 4 };
        let (segment, media_time) = (be(&elst, 8, w)?, be(&elst, 8 + w, w)?);
        if be(&elst, 4, 4)? != 1 || media_time >= 1 << (8 * w - 1) {
            return None; // several edits, or an empty one (media time -1)
        }
        let secs = |v: u64, scale: u64| Duration::from_nanos((u128::from(v) * 1_000_000_000 / u128::from(scale)) as u64);
        // a zero segment duration (fragmented files) means "all of it"
        return Some((secs(media_time, media_scale), (segment > 0).then(|| secs(segment, movie_scale))));
    }
    None
}

/// `inner` without its first `skip` and cut off after `len` more (an MP4 edit list).
struct Trim<S> {
    inner: S,
    skip: u64,
    /// Frames to play; u64::MAX: all.
    len: u64,
    /// Samples left before the cut.
    left: u64,
}

impl<S: Source> Trim<S> {
    fn new(inner: S, skip: Duration, len: Option<Duration>) -> Trim<S> {
        let mut t = Trim { skip: 0, len: u64::MAX, left: 0, inner };
        t.skip = t.frames(skip);
        if let Some(len) = len {
            t.len = t.frames(len);
        }
        let ch = u64::from(t.inner.channels().get());
        for _ in 0..t.skip * ch {
            t.inner.next();
        }
        t.left = t.len.saturating_mul(ch);
        t
    }

    fn frames(&self, d: Duration) -> u64 {
        (d.as_nanos() * u128::from(self.inner.sample_rate().get()) / 1_000_000_000) as u64
    }

    fn time(&self, frames: u64) -> Duration {
        Duration::from_nanos((u128::from(frames) * 1_000_000_000 / u128::from(self.inner.sample_rate().get())) as u64)
    }
}

impl<S: Source> Iterator for Trim<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.left = self.left.checked_sub(1)?;
        self.inner.next()
    }
}

impl<S: Source> Source for Trim<S> {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.inner.current_span_len().map_or(self.left, |n| (n as u64).min(self.left)) as usize)
    }
    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }
    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        match self.len {
            u64::MAX => self.inner.total_duration().map(|d| d.saturating_sub(self.time(self.skip))),
            len => Some(self.time(len)),
        }
    }
    /// Seeks arrive at frame boundaries (see `TrackTap`).
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let frame = self.frames(pos).min(self.len);
        self.inner.try_seek(self.time(self.skip + frame))?;
        self.left = (self.len - frame).saturating_mul(u64::from(self.inner.channels().get()));
        Ok(())
    }
}

fn describe(err: &DecoderError) -> String {
    match err {
        DecoderError::UnrecognizedFormat => "unsupported format".into(),
        other => other.to_string(),
    }
}

/// `ffmpeg` on PATH or in the usual install locations.
fn ffmpeg() -> Option<&'static Path> {
    static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
    FFMPEG
        .get_or_init(|| {
            let path = std::env::var_os("PATH").unwrap_or_default();
            std::env::split_paths(&path)
                .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from))
                .map(|dir| dir.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }))
                .find(|p| p.is_file())
        })
        .as_deref()
}

/// Frames per chunk read from ffmpeg; also the span length reported to rodio.
const FFMPEG_CHUNK_FRAMES: usize = 1024;
/// Chunks decoded ahead of playback (~0.7 s at 48 kHz).
const FFMPEG_AHEAD: usize = 32;

/// Decodes through an `ffmpeg` child process writing raw f32 PCM; a reader thread feeds a bounded
/// channel. Seeking restarts ffmpeg at the target.
struct FfmpegSource {
    ffmpeg: &'static Path,
    path: PathBuf,
    channels: ChannelCount,
    rate: SampleRate,
    duration: Option<Duration>,
    pipe: Pipe,
    chunk: Vec<f32>,
    pos: usize,
    ended: bool,
}

struct Pipe {
    child: Process,
    chunks: Receiver<Vec<f32>>,
}

impl Pipe {
    fn spawn(ffmpeg: &Path, path: &Path, start: Duration, channels: ChannelCount, rate: SampleRate) -> io::Result<Pipe> {
        let mut child = Command::new(ffmpeg)
            .args(["-nostdin", "-hide_banner", "-loglevel", "quiet", "-ss"])
            .arg(format!("{:.6}", start.as_secs_f64()))
            .arg("-i")
            // `file:` so a name is never taken for another ffmpeg protocol
            .arg({
                let mut input = std::ffi::OsString::from("file:");
                input.push(path);
                input
            })
            .args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le", "-ac"])
            .arg(channels.to_string())
            .arg("-ar")
            .arg(rate.to_string())
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
        let (tx, chunks) = mpsc::sync_channel(FFMPEG_AHEAD);
        let chunk_bytes = FFMPEG_CHUNK_FRAMES * channels.get() as usize * 4;
        let reader = std::thread::Builder::new().name("orbit-ffmpeg".into()).spawn(move || {
            let mut bytes = vec![0u8; chunk_bytes];
            loop {
                let mut n = 0;
                while n < bytes.len() {
                    match stdout.read(&mut bytes[n..]) {
                        Ok(0) => break,
                        Ok(k) => n += k,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let samples: Vec<f32> = bytes[..n].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
                if samples.is_empty() || tx.send(samples).is_err() || n < bytes.len() {
                    break;
                }
            }
        });
        if let Err(e) = reader {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(Pipe { child: Arc::new(Mutex::new(child)), chunks })
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl FfmpegSource {
    fn open(ffmpeg: &'static Path, track: &Track, start: Duration) -> anyhow::Result<FfmpegSource> {
        let channels = ChannelCount::new(if track.channels == Some(1) { 1 } else { 2 }).context("channels")?;
        let rate = SampleRate::new(track.sample_rate.filter(|r| (8000..=384_000).contains(r)).unwrap_or(48_000)).context("rate")?;
        let pipe = Pipe::spawn(ffmpeg, &track.path, start, channels, rate)?;
        // Wait for the first audio so a file ffmpeg can't decode fails here, not mid-playback.
        let first = pipe.chunks.recv_timeout(Duration::from_secs(3)).map_err(|_| anyhow!("can't decode it"))?;
        Ok(FfmpegSource {
            ffmpeg,
            path: track.path.clone(),
            channels,
            rate,
            duration: (!track.duration.is_zero()).then_some(track.duration),
            pipe,
            chunk: first,
            pos: 0,
            ended: false,
        })
    }
}

impl Iterator for FfmpegSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.pos == self.chunk.len() {
            if self.ended {
                return None;
            }
            self.chunk = match self.pipe.chunks.recv_timeout(Duration::from_millis(250)) {
                Ok(chunk) => chunk,
                // Stalled (it never is for local files): keep the device fed with silence.
                Err(RecvTimeoutError::Timeout) => vec![0.0; FFMPEG_CHUNK_FRAMES * self.channels.get() as usize],
                Err(RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                    return None;
                }
            };
            self.pos = 0;
        }
        self.pos += 1;
        Some(self.chunk[self.pos - 1])
    }
}

impl Source for FfmpegSource {
    fn current_span_len(&self) -> Option<usize> {
        // Finite spans let rodio's resampler re-read the format after this track ends.
        Some(if self.ended { 0 } else { FFMPEG_CHUNK_FRAMES * self.channels.get() as usize })
    }
    fn channels(&self) -> ChannelCount {
        self.channels
    }
    fn sample_rate(&self) -> SampleRate {
        self.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        self.duration
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let pos = self.duration.map_or(pos, |d| pos.min(d));
        self.pipe = Pipe::spawn(self.ffmpeg, &self.path, pos, self.channels, self.rate).map_err(|e| SeekError::Other(Arc::new(e)))?;
        (self.chunk, self.pos, self.ended) = (Vec::new(), 0, false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EngineEvent::{Advanced, Finished, Started};
    use rodio::queue::SourcesQueueOutput;
    use std::f64::consts::PI;

    /// 8 kHz keeps the numbers readable: one frame = 125 µs.
    const RATE: u32 = 8000;

    fn frames(n: u64) -> Duration {
        Duration::from_nanos(n * 1_000_000_000 / RATE as u64)
    }

    /// A temp folder for generated tracks, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("orbit-engine-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }

        /// A 32-bit float WAV (so samples round-trip exactly) wrapped in a library track.
        fn wav(&self, id: TrackId, rate: u32, channels: u16, samples: &[f32]) -> Track {
            let path = self.0.join(format!("{id}.wav"));
            let data = samples.len() as u32 * 4;
            let mut b = Vec::new();
            b.extend(b"RIFF");
            b.extend((36 + data).to_le_bytes());
            b.extend(b"WAVEfmt ");
            b.extend(16u32.to_le_bytes());
            b.extend(3u16.to_le_bytes()); // IEEE float
            b.extend(channels.to_le_bytes());
            b.extend(rate.to_le_bytes());
            b.extend((rate * channels as u32 * 4).to_le_bytes());
            b.extend((channels * 4).to_le_bytes());
            b.extend(32u16.to_le_bytes());
            b.extend(b"data");
            b.extend(data.to_le_bytes());
            samples.iter().for_each(|s| b.extend(s.to_le_bytes()));
            std::fs::write(&path, b).unwrap();
            let duration = Duration::from_secs_f64(samples.len() as f64 / channels as f64 / rate as f64);
            Track { id, path, duration, ..Track::default() }
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pull(out: &mut SourcesQueueOutput, n: usize) -> Vec<f32> {
        out.by_ref().take(n).collect()
    }

    /// Mono samples whose value identifies the frame.
    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32 / n as f32 * 0.9).collect()
    }

    fn non_increasing(x: &[f32]) -> bool {
        x.windows(2).all(|w| w[1] <= w[0])
    }

    #[test]
    fn plays_through_then_finishes() {
        let dir = Dir::new("finish");
        let a = dir.wav(0, RATE, 2, &[0.5; 2 * 800]);
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        assert_eq!(e.poll(), vec![Started(0)]);
        assert_eq!((e.state(), e.current(), e.duration()), (PlayState::Playing, Some(0), Some(Duration::from_millis(100))));
        assert!(pull(&mut out, 600).iter().all(|&x| x == 0.5));
        assert_eq!(e.position(), frames(300));
        assert!(e.poll().is_empty());
        assert_eq!(pull(&mut out, 1200).iter().filter(|&&x| x == 0.5).count(), 1000);
        assert_eq!(e.poll(), vec![Finished(0)]);
        assert_eq!((e.state(), e.current(), e.position()), (PlayState::Stopped, None, Duration::ZERO));
    }

    #[test]
    fn mp4_edit_list_trims_priming_and_padding() {
        // moov { mvhd, trak (video), trak (audio: edts/elst + mdia/mdhd/hdlr) } as ffmpeg writes it
        let atom = |kind: &[u8; 4], body: &[u8]| [&(8 + body.len() as u32).to_be_bytes()[..], kind, body].concat();
        let full = |v: u32, fields: &[u32]| [&v.to_be_bytes()[..], &fields.iter().flat_map(|f| f.to_be_bytes()).collect::<Vec<u8>>()].concat();
        let trak = |handler: &[u8; 4], scale: u32, edit: &[u32]| {
            let hdlr = [&[0u8; 8][..], handler, &[0u8; 12]].concat();
            let mdia = [atom(b"mdhd", &full(0, &[0, 0, scale, 0])), atom(b"hdlr", &hdlr)].concat();
            atom(b"trak", &[atom(b"edts", &atom(b"elst", &full(0, edit))), atom(b"mdia", &mdia)].concat())
        };
        let moov = [atom(b"mvhd", &full(0, &[0, 0, 1000, 3000])), trak(b"vide", 12288, &[1, 3000, 0, 1 << 16]), trak(b"soun", 44100, &[1, 3000, 1024, 1 << 16])].concat();
        let dir = Dir::new("mp4");
        let path = dir.0.join("a.m4a");
        std::fs::write(&path, [atom(b"ftyp", b"M4A \0\0\0\0"), atom(b"moov", &moov), atom(b"mdat", &[0; 16])].concat()).unwrap();
        assert_eq!(mp4_edit(&path), Some((Duration::from_nanos(1024 * 1_000_000_000 / 44100), Some(Duration::from_secs(3)))));
        std::fs::write(&path, b"ID3 not an mp4 at all").unwrap();
        assert_eq!(mp4_edit(&path), None);

        // 1 kHz, so a frame is a millisecond: frames 10..60 of 0..100 play, seeks count from 10
        let buffer = |n: usize| rodio::buffer::SamplesBuffer::new(ChannelCount::new(1).unwrap(), SampleRate::new(1000).unwrap(), (0..n).map(|i| i as f32).collect::<Vec<f32>>());
        let mut t = Trim::new(buffer(100), Duration::from_millis(10), Some(Duration::from_millis(50)));
        assert_eq!(t.total_duration(), Some(Duration::from_millis(50)));
        assert_eq!(t.by_ref().take(3).collect::<Vec<f32>>(), [10.0, 11.0, 12.0]);
        t.try_seek(Duration::from_millis(45)).unwrap();
        assert_eq!(t.collect::<Vec<f32>>(), [55.0, 56.0, 57.0, 58.0, 59.0]);
        // no length (a zero segment duration): priming only
        assert_eq!(Trim::new(buffer(20), Duration::from_millis(15), None).collect::<Vec<f32>>(), [15.0, 16.0, 17.0, 18.0, 19.0]);
    }

    #[test]
    fn gapless_transition_is_sample_exact() {
        let dir = Dir::new("gapless");
        let (a, b) = (dir.wav(1, RATE, 1, &[0.5; 1000]), dir.wav(2, RATE, 1, &[-0.25; 1000]));
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        e.preload(&b).unwrap();
        assert_eq!((e.poll(), e.preloaded()), (vec![Started(1)], Some(2)));
        let played = pull(&mut out, 1500);
        assert!(played[..1000].iter().all(|&x| x == 0.5) && played[1000..].iter().all(|&x| x == -0.25));
        assert_eq!(e.poll(), vec![Advanced { from: 1, to: 2 }]);
        assert_eq!((e.current(), e.preloaded(), e.state(), e.position()), (Some(2), None, PlayState::Playing, frames(500)));
        pull(&mut out, 600);
        assert_eq!(e.poll(), vec![Finished(2)]);
    }

    #[test]
    fn cancelled_or_replaced_preloads_never_play() {
        let dir = Dir::new("preloads");
        let (a, b, c) = (dir.wav(1, RATE, 1, &[0.5; 500]), dir.wav(2, RATE, 1, &[0.25; 500]), dir.wav(3, RATE, 1, &[0.125; 500]));
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        e.preload(&b).unwrap();
        e.cancel_preload();
        assert_eq!(e.preloaded(), None);
        assert!(pull(&mut out, 1000)[500..].iter().all(|&x| x == 0.0));
        assert_eq!(e.poll(), vec![Started(1), Finished(1)]);

        e.load(&a, Duration::ZERO, false).unwrap();
        e.preload(&b).unwrap();
        e.preload(&c).unwrap();
        // (rodio's idle queue first finishes the silence chunk it was playing)
        let played: Vec<f32> = pull(&mut out, 900).into_iter().skip_while(|&x| x == 0.0).collect();
        assert!(played[..500].iter().all(|&x| x == 0.5) && played[500..].iter().all(|&x| x == 0.125));
        assert_eq!(e.poll(), vec![Started(1), Advanced { from: 1, to: 3 }]);
        assert!(e.preload(&b).is_ok() && e.preloaded() == Some(2));
        e.stop();
        assert!(e.preload(&b).is_err(), "nothing to follow");
    }

    #[test]
    fn seek_reports_at_once_and_lands_exactly() {
        let dir = Dir::new("seek");
        let samples = ramp(16000);
        let a = dir.wav(0, RATE, 1, &samples);
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        assert_eq!(pull(&mut out, 800), samples[..800]);
        e.seek(Duration::from_secs(1)).unwrap();
        assert_eq!(e.position(), Duration::from_secs(1));
        let after = pull(&mut out, 400);
        // Picked up at the next control read (<= 64 frames), then exact from frame 8000 on.
        let jump = after.iter().position(|&x| x >= samples[8000]).unwrap();
        assert!(jump <= 64 && after[..jump] == samples[800..800 + jump]);
        assert_eq!(after[jump..], samples[8000..8400 - jump]);
        assert_eq!(e.position(), frames(8400 - jump as u64));
        // Clamped to the end, which finishes the track.
        e.seek(Duration::from_secs(60)).unwrap();
        assert_eq!(e.position(), Duration::from_secs(2));
        pull(&mut out, 200);
        assert_eq!(e.poll(), vec![Started(0), Finished(0)]);
        assert!(e.seek(Duration::ZERO).is_err(), "nothing playing");
    }

    #[test]
    fn seek_dips_briefly_with_fades_on() {
        let dir = Dir::new("dip");
        let a = dir.wav(0, RATE, 1, &[0.5; 16000]);
        let (mut e, mut out) = Engine::new_detached();
        e.set_fade(120);
        e.load(&a, Duration::ZERO, false).unwrap();
        pull(&mut out, 400);
        e.seek(Duration::from_millis(1500)).unwrap();
        let around = pull(&mut out, 800);
        // 20 ms down (160 frames) and 20 ms up, not the full 120 ms fade.
        let (quiet, low) = around.iter().enumerate().fold((0, 1.0), |m, (i, &x)| if x < m.1 { (i, x) } else { m });
        assert!(quiet <= 64 + 161 && low < 1e-3, "lowest {low} at {quiet}");
        assert!(around[quiet + 170..].iter().all(|&x| x == 0.5));
        assert!(e.position() >= Duration::from_millis(1500) && e.position() < Duration::from_millis(1600));
    }

    #[test]
    fn pause_fades_holds_still_and_resume_continues() {
        let dir = Dir::new("pause");
        let a = dir.wav(0, RATE, 1, &[0.5; 8000]);
        let (mut e, mut out) = Engine::new_detached();
        e.set_fade(10); // 80 frames
        e.load(&a, Duration::ZERO, false).unwrap();
        assert!(pull(&mut out, 400).iter().all(|&x| x == 0.5), "no fade-in from the top");
        e.pause();
        assert_eq!(e.state(), PlayState::Paused);
        let fading = pull(&mut out, 400);
        assert!(non_increasing(&fading) && fading[64 + 80..].iter().all(|&x| x == 0.0));
        let frozen = e.position();
        e.poll();
        assert!(e.player.is_paused());
        assert!(pull(&mut out, 2000).iter().all(|&x| x == 0.0));
        assert_eq!(e.position(), frozen);
        e.toggle();
        assert_eq!(e.state(), PlayState::Playing);
        let back = pull(&mut out, 400);
        assert!(back.windows(2).all(|w| w[1] >= w[0]) && back[399] == 0.5);
        assert!(e.position() > frozen);
    }

    #[test]
    fn seeking_while_paused_waits_for_resume() {
        let dir = Dir::new("pausedseek");
        let samples = ramp(16000);
        let a = dir.wav(0, RATE, 1, &samples);
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        pull(&mut out, 100);
        e.pause();
        pull(&mut out, 200);
        e.poll();
        e.seek(Duration::from_millis(1250)).unwrap();
        assert!(pull(&mut out, 400).iter().all(|&x| x == 0.0));
        assert_eq!(e.position(), Duration::from_millis(1250));
        e.resume();
        let resumed = pull(&mut out, 400);
        let first = resumed.iter().position(|&x| x != 0.0).unwrap();
        assert_eq!(resumed[first..], samples[10000..10400 - first]);
    }

    #[test]
    fn stop_fades_out_and_forgets_the_track() {
        let dir = Dir::new("stop");
        let a = dir.wav(0, RATE, 1, &[0.5; 8000]);
        let (mut e, mut out) = Engine::new_detached();
        e.set_fade(10);
        e.load(&a, Duration::ZERO, false).unwrap();
        pull(&mut out, 400);
        e.stop();
        assert_eq!((e.state(), e.current(), e.position(), e.duration()), (PlayState::Stopped, None, Duration::ZERO, None));
        let tail = pull(&mut out, 400);
        assert!(non_increasing(&tail) && tail[64 + 80..].iter().all(|&x| x == 0.0) && tail[0] > 0.0);
        assert!(e.poll().is_empty(), "stale Started dropped, no Finished");
    }

    #[test]
    fn loading_over_a_playing_track_fades_it_out_first() {
        let dir = Dir::new("switch");
        let (a, b) = (dir.wav(1, RATE, 1, &[0.5; 8000]), dir.wav(2, RATE, 1, &[-0.25; 8000]));
        let (mut e, mut out) = Engine::new_detached();
        e.set_fade(10);
        e.load(&a, Duration::ZERO, false).unwrap();
        assert_eq!(e.poll(), vec![Started(1)]);
        pull(&mut out, 400);
        e.load(&b, Duration::ZERO, false).unwrap();
        assert_eq!((e.current(), e.position()), (Some(2), Duration::ZERO));
        let played = pull(&mut out, 800);
        let switch = played.iter().position(|&x| x < 0.0).unwrap();
        assert!(switch <= 64 + 80 + 1 && non_increasing(&played[..switch]));
        assert!(played[switch..].iter().all(|&x| x == -0.25), "starts from the top at full level");
        assert_eq!(e.poll(), vec![Started(2)]);
    }

    #[test]
    fn loading_paused_holds_at_the_start_then_fades_in() {
        let dir = Dir::new("resume");
        let samples = ramp(16000);
        let a = dir.wav(0, RATE, 1, &samples);
        let (mut e, mut out) = Engine::new_detached();
        e.set_fade(10);
        e.load(&a, Duration::from_secs(1), true).unwrap();
        assert_eq!((e.state(), e.position()), (PlayState::Paused, Duration::from_secs(1)));
        assert!(pull(&mut out, 400).iter().all(|&x| x == 0.0));
        e.poll();
        assert!(e.player.is_paused() && e.position() == Duration::from_secs(1));
        e.resume();
        let played = pull(&mut out, 600);
        let first = played.iter().position(|&x| x != 0.0).unwrap();
        // Mid-track start: fades in over 80 frames from exactly frame 8000.
        assert!(played[first] < samples[8000 + 1] * 0.1);
        assert_eq!(played[first + 81..], samples[8000 + 81..8000 + 600 - first]);
    }

    #[test]
    fn ab_loop_repeats_exactly_until_cleared() {
        let dir = Dir::new("ab");
        let samples = ramp(16000);
        let a = dir.wav(0, RATE, 1, &samples);
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        e.set_ab_loop(Some(frames(2000)), Some(frames(4000)));
        let played = pull(&mut out, 8000);
        assert_eq!(played[..4000], samples[..4000]);
        assert_eq!(played[4000..6000], samples[2000..4000]);
        assert_eq!(played[6000..], samples[2000..4000]);
        e.set_ab_loop(None, None);
        pull(&mut out, 3000);
        assert!(e.position() > frames(4000));
        // Setting B behind the playhead (the usual "press B now") jumps back to A at once.
        let at = e.position();
        e.set_ab_loop(Some(frames(1000)), Some(at));
        let jumped = pull(&mut out, 100);
        let back = jumped.iter().position(|&x| x < samples[1001]).unwrap();
        assert!(back <= 64 && jumped[back] == samples[1000], "{back}");
    }

    #[test]
    fn volume_is_perceptual_smooth_and_after_the_tap() {
        assert!((0..150).all(|p| volume_gain(p + 1) > volume_gain(p)));
        assert_eq!((volume_gain(0), volume_gain(100), volume_gain(255)), (0.0, 1.0, volume_gain(150)));
        let dir = Dir::new("volume");
        let a = dir.wav(0, RATE, 1, &[0.8; 8000]);
        let (mut e, mut out) = Engine::new_detached();
        e.set_volume(50, false);
        e.load(&a, Duration::ZERO, false).unwrap();
        assert!(pull(&mut out, 400).iter().all(|&x| x == 0.8 * 0.125));
        let mut tapped = Vec::new();
        e.tap().latest(2, &mut tapped);
        assert_eq!(tapped, vec![[0.8; 2]; 2], "the visualizer sees the music, not the volume");
        e.set_volume(50, true);
        let fade = pull(&mut out, 4000);
        assert!(non_increasing(&fade) && fade.windows(2).all(|w| w[0] - w[1] < 0.001) && fade[3999] == 0.0);
    }

    #[test]
    fn replaygain_applies_and_follows_setting_changes() {
        let dir = Dir::new("rg");
        let mut a = dir.wav(0, RATE, 1, &[0.8; 8000]);
        a.rg_track_gain = Some(-6.0);
        let (mut e, mut out) = Engine::new_detached();
        e.set_replaygain(ReplayGainMode::Track, 0.0, true);
        e.load(&a, Duration::ZERO, false).unwrap();
        assert!(pull(&mut out, 200).iter().all(|&x| (x - 0.8 * crate::dsp::db_to_gain(-6.0)).abs() < 1e-6));
        e.set_replaygain(ReplayGainMode::Off, 0.0, true);
        assert_eq!(pull(&mut out, 4000)[3999], 0.8);
    }

    #[test]
    fn eq_changes_reach_the_playing_track() {
        let dir = Dir::new("eq");
        let tone: Vec<f32> = (0..48000).map(|i| 0.25 * (2.0 * PI * 1000.0 * i as f64 / 48000.0).sin() as f32).collect();
        let a = dir.wav(0, 48000, 1, &tone);
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        let rms = |x: &[f32]| (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        let flat = rms(&pull(&mut out, 4800));
        let mut bands = [0.0; 10];
        bands[5] = 6.0;
        e.set_eq(&EqSettings { enabled: true, preset: "custom".into(), preamp_db: 0.0, bands });
        pull(&mut out, 14400);
        let boosted = rms(&pull(&mut out, 4800));
        assert!((20.0 * (boosted / flat).log10() - 6.0).abs() < 0.1, "{flat} -> {boosted}");
    }

    #[test]
    fn a_preloaded_track_starts_with_the_settings_of_its_start() {
        let dir = Dir::new("preset");
        let tone: Vec<f32> = (0..4800).map(|i| 0.25 * (2.0 * PI * 1000.0 * i as f64 / 48000.0).sin() as f32).collect();
        let (a, mut b) = (dir.wav(1, 48000, 1, &[0.1; 480]), dir.wav(2, 48000, 1, &tone));
        b.rg_track_gain = Some(-12.0);
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        e.preload(&b).unwrap();
        // Changed while B waits: B must start with them, not glide over from the old ones.
        let mut bands = [0.0; 10];
        bands[5] = 6.0;
        e.set_eq(&EqSettings { enabled: true, preset: "custom".into(), preamp_db: 0.0, bands });
        e.set_replaygain(ReplayGainMode::Track, 0.0, true);
        let played = pull(&mut out, 480 + 1200);
        let rms = |x: &[f32]| (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        let db = 20.0 * (rms(&played[480 + 240..]) / rms(&tone[240..1200])).log10();
        assert!((db + 6.0).abs() < 0.2, "+6 dB EQ and -12 dB ReplayGain from the first 5 ms: {db:.2} dB");
    }

    #[test]
    fn speed_leaves_the_track_timeline_alone() {
        let dir = Dir::new("speed");
        let a = dir.wav(0, RATE, 1, &[0.5; 8000]);
        let (mut e, mut out) = Engine::new_detached();
        e.set_speed(2.0);
        e.load(&a, Duration::ZERO, false).unwrap();
        pull(&mut out, 4000);
        assert_eq!(e.position(), frames(4000));
        assert_eq!(e.tap().latest(1, &mut Vec::new()), 2 * RATE, "the tap reports what is heard");
        e.set_speed(100.0);
        assert_eq!(e.player.speed(), 4.0);
    }

    #[test]
    fn unreadable_files_fail_cleanly() {
        let dir = Dir::new("bad");
        let good = dir.wav(0, RATE, 1, &[0.5; 800]);
        let junk = Track { id: 1, path: dir.0.join("junk.mp3"), ..Track::default() };
        std::fs::write(&junk.path, b"definitely not audio ".repeat(200)).unwrap();
        let missing = Track { id: 2, path: dir.0.join("missing.flac"), ..Track::default() };
        let (mut e, mut out) = Engine::new_detached();
        e.load(&good, Duration::ZERO, false).unwrap();
        assert!(e.load(&junk, Duration::ZERO, false).is_err());
        assert_eq!((e.state(), e.current()), (PlayState::Stopped, None));
        let err = e.load(&missing, Duration::ZERO, false).unwrap_err().to_string();
        assert!(err.contains("No such file"), "{err}");
        e.load(&good, Duration::ZERO, false).unwrap();
        assert!(e.preload(&junk).is_err() && e.preloaded().is_none());
        assert_eq!(pull(&mut out, 1000).iter().filter(|&&x| x == 0.5).count(), 800);
        assert_eq!(e.poll(), vec![Started(0), Finished(0)]);
    }

    #[test]
    fn a_new_player_resumes_where_the_old_one_was() {
        let dir = Dir::new("replace");
        let samples = ramp(16000);
        let (a, b) = (dir.wav(0, RATE, 1, &samples), dir.wav(1, RATE, 1, &[0.25; 100]));
        let (mut e, mut out) = Engine::new_detached();
        e.load(&a, Duration::ZERO, false).unwrap();
        e.preload(&b).unwrap();
        e.set_ab_loop(Some(frames(1000)), Some(frames(9000)));
        pull(&mut out, 1000);
        e.poll();
        let (player, mut out) = Player::new();
        e.replace_player(player);
        assert!(e.poll().is_empty());
        assert_eq!((e.position(), e.preloaded()), (frames(1000), Some(1)));
        assert_eq!(pull(&mut out, 500), samples[1000..1500]);
        assert!(e.current.as_ref().unwrap().ctl.loop_points().is_some());
    }

    #[test]
    fn formats_symphonia_lacks_go_through_ffmpeg() {
        let Some(ffmpeg) = ffmpeg() else { return };
        let dir = Dir::new("ffmpeg");
        let path = dir.0.join("tone.opus");
        let made = Command::new(ffmpeg)
            .args(["-nostdin", "-loglevel", "quiet", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "libopus"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !made.is_ok_and(|s| s.success()) {
            return; // an ffmpeg build without libopus
        }
        let track = Track { id: 7, path, duration: Duration::from_secs(2), ..Track::default() };
        let (mut e, mut out) = Engine::new_detached();
        e.load(&track, Duration::ZERO, false).unwrap();
        let loud = |x: &[f32]| x.iter().map(|v| v.abs()).fold(0.0, f32::max) > 0.05;
        assert!(loud(&pull(&mut out, 48000)));
        assert_eq!(e.position(), Duration::from_millis(500));
        e.seek(Duration::from_millis(1500)).unwrap();
        assert!(loud(&pull(&mut out, 24000)));
        let pos = e.position();
        assert!(pos > Duration::from_millis(1600) && pos <= Duration::from_millis(1750), "{pos:?}");
        pull(&mut out, 96000);
        assert_eq!(e.poll(), vec![Started(7), Finished(7)]);
        // A withdrawn preload's ffmpeg is stopped right away, not when rodio reaches it.
        e.load(&track, Duration::ZERO, false).unwrap();
        e.preload(&track).unwrap();
        let process = e.preload.as_ref().and_then(|p| p.ffmpeg.clone()).unwrap();
        e.cancel_preload();
        let exited = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            process.lock().unwrap().try_wait().unwrap().is_some()
        });
        assert!(exited);
    }

    /// Opens, decodes and seeks a sample of the user's library (read-only, silent) and prints
    /// timings: `cargo test smoke_real_library -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads ~/Music/LocalFiles"]
    fn smoke_real_library() {
        let root = PathBuf::from(std::env::var_os("HOME").unwrap()).join("Music/LocalFiles");
        let files = crate::library::collect_audio_files(&root, &crate::config::LibraryConfig::default());
        let pick = |ext: &str, n: usize| {
            let all: Vec<_> = files.iter().filter(|p| p.extension().is_some_and(|e| e == ext)).cloned().collect();
            all.iter().step_by((all.len() / n).max(1)).take(n).cloned().collect::<Vec<_>>()
        };
        let sample = [pick("mp3", 12), pick("mp4", 4), pick("wav", 4)].concat();
        let (mut audible, mut audio, mut busy) = (0, Duration::ZERO, Duration::ZERO);
        for (id, path) in sample.iter().enumerate() {
            let name: String = path.file_name().unwrap().to_string_lossy().chars().take(40).collect();
            // What the library scanner fills in (it reads the same properties with lofty).
            let props = lofty::read_from_path(path).map(|f| lofty::file::AudioFile::properties(&f).clone());
            let track = match &props {
                Ok(p) => {
                    Track { id, path: path.clone(), duration: p.duration(), sample_rate: p.sample_rate(), channels: p.channels(), ..Track::default() }
                }
                Err(_) => Track { id, path: path.clone(), ..Track::default() },
            };
            let (mut e, mut out) = Engine::new_detached();
            let t = std::time::Instant::now();
            if let Err(err) = e.load(&track, Duration::ZERO, false) {
                eprintln!("FAIL {name:>40}: {err}");
                continue;
            }
            let open = t.elapsed();
            let dur = e.duration().unwrap_or_default();
            let t = std::time::Instant::now();
            let head = pull(&mut out, 192_000);
            let head_secs = e.position();
            e.seek(dur / 2).unwrap();
            let mid = pull(&mut out, 192_000);
            busy += t.elapsed();
            let decoded = head_secs + e.position().saturating_sub(dur / 2);
            audio += decoded;
            let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let ok = peak(&head).max(peak(&mid)) > 0.01 && e.position() > dur / 2;
            audible += ok as usize;
            eprintln!(
                "{} {name:>40}  open {:>5.1} ms  {:.2} s decoded in {:>5.1} ms  duration {:>6.1} s  peaks {:.2} / {:.2}",
                if ok { "ok  " } else { "BAD " },
                open.as_secs_f64() * 1e3,
                decoded.as_secs_f64(),
                t.elapsed().as_secs_f64() * 1e3,
                dur.as_secs_f64(),
                peak(&head),
                peak(&mid),
            );
        }
        eprintln!("{audible}/{} ok; {:.1} ms per second of audio (decode + chain)", sample.len(), busy.as_secs_f64() * 1e3 / audio.as_secs_f64());
        assert_eq!(audible, sample.len());
    }

    /// CPU cost of decode + chain per second of audio:
    /// `cargo test chain_cost -- --ignored --nocapture` (add `--release` to compare).
    #[test]
    #[ignore = "benchmark"]
    fn chain_cost_per_second_of_audio() {
        let dir = Dir::new("bench");
        let secs = 30;
        let tone: Vec<f32> = (0..44100 * secs)
            .flat_map(|i| {
                let t = i as f64 / 44100.0;
                let v = 0.3 * (2.0 * PI * 220.0 * t).sin() + 0.2 * (2.0 * PI * 3300.0 * t).sin();
                [v as f32, (v * 0.8) as f32]
            })
            .collect();
        let mut wav = dir.wav(0, 44100, 2, &tone);
        wav.rg_track_gain = Some(-4.0);
        let mp3 = crate::library::collect_audio_files(
            &PathBuf::from(std::env::var_os("HOME").unwrap()).join("Music/LocalFiles/Jpop"),
            &crate::config::LibraryConfig::default(),
        )
        .into_iter()
        .next()
        .map(|path| Track { id: 1, path, ..Track::default() });
        let per_sec = |track: &Track, eq: bool| {
            let (mut e, mut out) = Engine::new_detached();
            e.set_fade(120);
            e.set_volume(70, false);
            if eq {
                e.set_replaygain(ReplayGainMode::Track, 2.0, true);
                e.set_eq(&EqSettings { enabled: true, preset: "rock".into(), preamp_db: -3.0, bands: crate::dsp::eq_preset("rock").unwrap() });
            }
            e.load(track, Duration::ZERO, false).unwrap();
            let t = std::time::Instant::now();
            pull(&mut out, 44100 * 2 * secs);
            let audio = e.position().as_secs_f64();
            t.elapsed().as_secs_f64() * 1e3 / audio
        };
        let raw = |track: &Track| {
            let t = std::time::Instant::now();
            let n = Decoder::try_from(File::open(&track.path).unwrap()).unwrap().take(44100 * 2 * secs).count();
            t.elapsed().as_secs_f64() * 1e3 / (n as f64 / 88200.0)
        };
        let profile = if cfg!(debug_assertions) { "dev" } else { "release" };
        for (name, track) in [("wav", Some(wav)), ("mp3", mp3)] {
            let Some(track) = track else { continue };
            let (d, plain, full) = (raw(&track), per_sec(&track, false), per_sec(&track, true));
            eprintln!(
                "[{profile}] {name}: decoder alone {d:.2} ms/s; decoder + chain {plain:.2} ms/s (EQ off), {full:.2} ms/s (EQ rock + ReplayGain); real time = 1000 ms/s"
            );
            assert!(full < 100.0, "must stay far below real time");
        }
    }
}
