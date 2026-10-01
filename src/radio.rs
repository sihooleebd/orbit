//! Offline radio: every track gets a timbre fingerprint (MFCC means and spreads plus spectral
//! centroid and zero-crossing rate, from up to 45 s of audio after a 10 s intro), computed in the
//! background and cached. Tracks with nearby fingerprints sound alike; the radio plays them.
//! Nothing leaves the machine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};

const FRAME: usize = 2048;
const HOP: usize = 1024;
const N_MELS: usize = 26;
const N_MFCC: usize = 13;
const MEL_FMAX: f32 = 8000.0;
/// Intro skipped before analysing.
const SKIP_SECS: f32 = 10.0;
/// At most this much of the body is analysed.
const WINDOW_SECS: f32 = 45.0;

/// [mfcc means (13), mfcc stds (13), centroid mean, zcr mean].
pub const FEATURE_DIM: usize = N_MFCC * 2 + 2;

/// Fingerprints by track path (an empty one: the file couldn't be decoded, don't retry it).
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Features(HashMap<String, Vec<f32>>);

impl Features {
    /// A missing or corrupt cache is empty (everything gets analysed again).
    pub fn load(path: &Path) -> Features {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        crate::state::write_atomic(path, &serde_json::to_vec(self)?)
    }

    pub fn has(&self, path: &Path) -> bool {
        self.0.contains_key(&*path.to_string_lossy())
    }

    pub fn get(&self, path: &Path) -> Option<&[f32]> {
        self.0.get(&*path.to_string_lossy()).map(Vec::as_slice).filter(|v| v.len() == FEATURE_DIM)
    }

    pub fn insert(&mut self, path: &Path, v: Option<Vec<f32>>) {
        self.0.insert(path.to_string_lossy().into_owned(), v.unwrap_or_default());
    }
}

/// Analyse `paths` on a background thread, one result per path (None: not decodable).
pub fn spawn_analyze(paths: Vec<PathBuf>) -> Receiver<(PathBuf, Option<Vec<f32>>)> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("radio".into()).spawn(move || {
        for path in paths {
            // a decoder panicking on a broken file only fails that file (it's logged, not retried)
            let v = std::panic::catch_unwind(|| analyze_file(&path)).ok().flatten();
            if tx.send((path, v)).is_err() {
                return;
            }
        }
    });
    rx
}

/// The `k` candidates closest to the seeds' average sound, nearest first, seeds left out. Every
/// dimension is z-scored over the candidates so none dominates by scale.
pub fn recommend<T: Copy + PartialEq>(pool: &[(T, &[f32])], seeds: &[T], k: usize) -> Vec<T> {
    if pool.len() < 2 {
        return Vec::new();
    }
    let n = pool.len() as f32;
    let mut mean = [0.0f32; FEATURE_DIM];
    for (_, v) in pool {
        for (m, x) in mean.iter_mut().zip(*v) {
            *m += x / n;
        }
    }
    let mut std = [0.0f32; FEATURE_DIM];
    for (_, v) in pool {
        for d in 0..FEATURE_DIM {
            std[d] += (v[d] - mean[d]).powi(2) / n;
        }
    }
    let z = |v: &[f32], d: usize| (v[d] - mean[d]) / std[d].sqrt().max(1e-6);

    let seed_vecs: Vec<&[f32]> = pool.iter().filter(|(id, _)| seeds.contains(id)).map(|(_, v)| *v).collect();
    if seed_vecs.is_empty() {
        return Vec::new();
    }
    let centroid: Vec<f32> = (0..FEATURE_DIM).map(|d| seed_vecs.iter().map(|v| z(v, d)).sum::<f32>() / seed_vecs.len() as f32).collect();
    let mut scored: Vec<(f32, T)> = pool
        .iter()
        .filter(|(id, _)| !seeds.contains(id))
        .map(|(id, v)| ((0..FEATURE_DIM).map(|d| (z(v, d) - centroid[d]).powi(2)).sum::<f32>(), *id))
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.into_iter().take(k).map(|(_, id)| id).collect()
}

/// Decode the leading SKIP + WINDOW seconds to mono.
fn decode_mono(path: &Path) -> Option<(Vec<f32>, u32)> {
    use rodio::Source;
    let decoder = rodio::Decoder::try_from(std::fs::File::open(path).ok()?).ok()?;
    let channels = decoder.channels().get() as usize;
    let rate = decoder.sample_rate().get();
    let cap = (((SKIP_SECS + WINDOW_SECS) * rate as f32) as usize + FRAME) * channels;
    let interleaved: Vec<f32> = decoder.take(cap).collect();
    let mono = interleaved.chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect();
    Some((mono, rate))
}

fn analyze_file(path: &Path) -> Option<Vec<f32>> {
    let (samples, rate) = decode_mono(path)?;
    let skip = ((SKIP_SECS * rate as f32) as usize).min(samples.len().checked_sub(FRAME)?);
    extract_features(&samples[skip..], rate)
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10f32.powf(mel / 2595.0) - 1.0)
}

/// Triangular mel filters over the FRAME/2 + 1 positive FFT bins.
fn mel_filterbank(rate: u32) -> Vec<Vec<f32>> {
    let fmax = MEL_FMAX.min(rate as f32 / 2.0);
    let bins: Vec<f32> = (0..N_MELS + 2).map(|i| mel_to_hz(hz_to_mel(fmax) * i as f32 / (N_MELS + 1) as f32) * FRAME as f32 / rate as f32).collect();
    (0..N_MELS)
        .map(|m| {
            let (l, c, r) = (bins[m], bins[m + 1], bins[m + 2]);
            (0..=FRAME / 2)
                .map(|k| {
                    let k = k as f32;
                    if k >= l && k <= c && c > l {
                        (k - l) / (c - l)
                    } else if k > c && k <= r && r > c {
                        (r - k) / (r - c)
                    } else {
                        0.0
                    }
                })
                .collect()
        })
        .collect()
}

/// DCT-II, first N_MFCC coefficients.
fn dct(input: &[f32]) -> [f32; N_MFCC] {
    let n = input.len() as f32;
    std::array::from_fn(|k| input.iter().enumerate().map(|(i, x)| x * (std::f32::consts::PI / n * (i as f32 + 0.5) * k as f32).cos()).sum())
}

fn extract_features(sig: &[f32], rate: u32) -> Option<Vec<f32>> {
    if sig.len() < FRAME {
        return None;
    }
    let window: Vec<f32> = (0..FRAME).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (FRAME as f32 - 1.0)).cos()).collect();
    let filters = mel_filterbank(rate);
    let fft = FftPlanner::<f32>::new().plan_fft_forward(FRAME);
    let mut buf = vec![Complex::new(0.0f32, 0.0); FRAME];
    let (mut sum, mut sqsum) = ([0.0f32; N_MFCC], [0.0f32; N_MFCC]);
    let (mut centroid, mut zcr, mut frames) = (0.0f32, 0.0f32, 0usize);

    for frame in sig.windows(FRAME).step_by(HOP) {
        for ((b, x), w) in buf.iter_mut().zip(frame).zip(&window) {
            *b = Complex::new(x * w, 0.0);
        }
        fft.process(&mut buf);
        let power: Vec<f32> = buf[..=FRAME / 2].iter().map(|c| c.norm_sqr()).collect();
        let log_mel: Vec<f32> = filters.iter().map(|f| (f.iter().zip(&power).map(|(w, p)| w * p).sum::<f32>() + 1e-10).ln()).collect();
        for (i, c) in dct(&log_mel).into_iter().enumerate() {
            sum[i] += c;
            sqsum[i] += c * c;
        }
        let mag: f32 = power.iter().map(|p| p.sqrt()).sum::<f32>() + 1e-9;
        centroid += power.iter().enumerate().map(|(k, p)| k as f32 * rate as f32 / FRAME as f32 * p.sqrt()).sum::<f32>() / mag;
        zcr += frame.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count() as f32 / FRAME as f32;
        frames += 1;
    }
    let n = frames as f32;
    let means = sum.map(|s| s / n);
    let stds = std::array::from_fn::<f32, N_MFCC, _>(|i| (sqsum[i] / n - means[i] * means[i]).max(0.0).sqrt());
    Some(means.into_iter().chain(stds).chain([centroid / n, zcr / n]).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32) -> Vec<f32> {
        (0..88200).map(|i| (std::f32::consts::TAU * freq * i as f32 / 44100.0).sin()).collect()
    }

    #[test]
    fn features_have_the_expected_shape() {
        let f = extract_features(&tone(440.0), 44100).unwrap();
        assert_eq!(f.len(), FEATURE_DIM);
        assert!(f.iter().all(|x| x.is_finite()));
        assert!(extract_features(&[0.0; FRAME - 1], 44100).is_none());
    }

    #[test]
    fn brighter_tone_has_a_higher_centroid() {
        let ci = FEATURE_DIM - 2;
        assert!(extract_features(&tone(4000.0), 44100).unwrap()[ci] > extract_features(&tone(300.0), 44100).unwrap()[ci]);
    }

    #[test]
    fn recommends_the_closest_sound_first() {
        let [a, b, c] = [440.0, 460.0, 6000.0].map(|hz| extract_features(&tone(hz), 44100).unwrap());
        let pool = [(0, a.as_slice()), (1, b.as_slice()), (2, c.as_slice())];
        assert_eq!(recommend(&pool, &[0], 2), [1, 2], "460 Hz sounds most like 440 Hz; the seed is left out");
        assert!(recommend(&pool, &[9], 2).is_empty(), "unknown seed");
        assert!(recommend(&pool[..1], &[0], 2).is_empty());
    }

    #[test]
    fn store_round_trips_and_remembers_failures() {
        let dir = std::env::temp_dir().join(format!("orbit-radio-{}", std::process::id()));
        let file = dir.join("features.json");
        let mut f = Features::default();
        f.insert(Path::new("/m/a.mp3"), Some(vec![1.0; FEATURE_DIM]));
        f.insert(Path::new("/m/bad.opus"), None);
        f.save(&file).unwrap();
        let g = Features::load(&file);
        assert_eq!(g.get(Path::new("/m/a.mp3")).map(<[f32]>::len), Some(FEATURE_DIM));
        assert!(g.has(Path::new("/m/bad.opus")) && g.get(Path::new("/m/bad.opus")).is_none(), "not retried, never recommended");
        assert!(!g.has(Path::new("/m/new.mp3")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
