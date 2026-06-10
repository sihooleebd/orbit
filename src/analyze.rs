//! Local, content-based audio analysis and recommendation.
//!
//! For each track we decode a window of audio and compute a compact **MFCC**
//! feature vector (the standard timbre fingerprint) plus a couple of spectral
//! descriptors. Similar tracks have nearby vectors, which powers the "Radio"
//! bucket and the "play similar" action — all fully offline.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use rustfft::{num_complex::Complex, FftPlanner};
use serde::{Deserialize, Serialize};

use crate::config;

// -- analysis parameters -----------------------------------------------------

const FRAME: usize = 2048;
const HOP: usize = 1024;
const N_MELS: usize = 26;
const N_MFCC: usize = 13;
const MEL_FMIN: f32 = 0.0;
const MEL_FMAX: f32 = 8000.0;
/// Skip this many seconds of intro before analysing.
const SKIP_SECS: f32 = 10.0;
/// Analyse at most this many seconds of the body.
const WINDOW_SECS: f32 = 45.0;

/// Feature vector: [mfcc means (13), mfcc stds (13), centroid mean, zcr mean].
pub const FEATURE_DIM: usize = N_MFCC * 2 + 2;

// -- feature store -----------------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
pub struct Features {
    pub vecs: HashMap<PathBuf, Vec<f32>>,
}

impl Features {
    pub fn load() -> Self {
        fs::read_to_string(config::features_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = config::features_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        if let Ok(json) = serde_json::to_string(self) {
            fs::write(path, json).ok();
        }
    }

    pub fn has(&self, path: &Path) -> bool {
        self.vecs.contains_key(path)
    }

    pub fn insert(&mut self, path: PathBuf, vec: Vec<f32>) {
        self.vecs.insert(path, vec);
    }

    pub fn len(&self) -> usize {
        self.vecs.len()
    }
}

// -- decoding ----------------------------------------------------------------

/// Decode a leading window of a track to mono f32 samples + its sample rate.
fn decode_mono(path: &Path) -> Option<(Vec<f32>, u32)> {
    use rodio::Source;
    let file = std::fs::File::open(path).ok()?;
    let decoder = rodio::Decoder::try_from(file).ok()?;
    let channels = decoder.channels().get() as usize;
    let sample_rate = decoder.sample_rate().get();
    if channels == 0 {
        return None;
    }

    // Only decode enough for SKIP + WINDOW seconds.
    let cap = (((SKIP_SECS + WINDOW_SECS) * sample_rate as f32) as usize + FRAME) * channels;
    let mut interleaved: Vec<f32> = Vec::with_capacity(cap.min(1 << 22));
    for s in decoder {
        interleaved.push(s);
        if interleaved.len() >= cap {
            break;
        }
    }

    let mono: Vec<f32> = interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    Some((mono, sample_rate))
}

// -- DSP helpers -------------------------------------------------------------

fn hann_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = std::f32::consts::PI * 2.0 * i as f32 / (n as f32 - 1.0);
            0.5 - 0.5 * x.cos()
        })
        .collect()
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10f32.powf(mel / 2595.0) - 1.0)
}

/// Triangular mel filterbank over the positive FFT bins (FRAME/2 + 1 of them).
fn mel_filterbank(sample_rate: u32, n_mels: usize, fmax: f32) -> Vec<Vec<f32>> {
    let n_bins = FRAME / 2 + 1;
    let fmax = fmax.min(sample_rate as f32 / 2.0);
    let mel_min = hz_to_mel(MEL_FMIN);
    let mel_max = hz_to_mel(fmax);
    // n_mels + 2 equally-spaced points in mel space.
    let points: Vec<f32> = (0..n_mels + 2)
        .map(|i| {
            let mel = mel_min + (mel_max - mel_min) * i as f32 / (n_mels + 1) as f32;
            mel_to_hz(mel)
        })
        .collect();
    // Convert Hz points to fractional FFT bins.
    let bin = |hz: f32| hz * (FRAME as f32) / (sample_rate as f32);
    let bins: Vec<f32> = points.iter().map(|&hz| bin(hz)).collect();

    let mut filters = vec![vec![0.0f32; n_bins]; n_mels];
    for m in 0..n_mels {
        let (left, center, right) = (bins[m], bins[m + 1], bins[m + 2]);
        for (k, f) in filters[m].iter_mut().enumerate() {
            let k = k as f32;
            if k >= left && k <= center && center > left {
                *f = (k - left) / (center - left);
            } else if k > center && k <= right && right > center {
                *f = (right - k) / (right - center);
            }
        }
    }
    filters
}

/// DCT-II of `input`, keeping the first `N_MFCC` coefficients.
fn dct(input: &[f32]) -> [f32; N_MFCC] {
    let n = input.len() as f32;
    let mut out = [0.0f32; N_MFCC];
    for (k, o) in out.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (i, &x) in input.iter().enumerate() {
            sum += x
                * (std::f32::consts::PI / n * (i as f32 + 0.5) * k as f32).cos();
        }
        *o = sum;
    }
    out
}

// -- feature extraction ------------------------------------------------------

/// Compute the feature vector for a file, or `None` if it can't be analysed.
pub fn analyze_file(path: &Path) -> Option<Vec<f32>> {
    let (samples, sr) = decode_mono(path)?;
    if samples.len() < FRAME {
        return None;
    }

    let skip = ((SKIP_SECS * sr as f32) as usize).min(samples.len() - FRAME);
    let sig = &samples[skip..];

    extract_features(sig, sr)
}

/// The DSP core, split out so it can be unit-tested on synthetic signals.
fn extract_features(sig: &[f32], sr: u32) -> Option<Vec<f32>> {
    if sig.len() < FRAME {
        return None;
    }
    let window = hann_window(FRAME);
    let filterbank = mel_filterbank(sr, N_MELS, MEL_FMAX);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME);

    let mut mfcc_sum = [0.0f32; N_MFCC];
    let mut mfcc_sqsum = [0.0f32; N_MFCC];
    let mut centroid_sum = 0.0f32;
    let mut zcr_sum = 0.0f32;
    let mut frames = 0usize;

    let mut buf = vec![Complex::new(0.0f32, 0.0); FRAME];
    let n_bins = FRAME / 2 + 1;

    let mut start = 0;
    while start + FRAME <= sig.len() {
        let frame = &sig[start..start + FRAME];

        // Windowed FFT.
        for (i, b) in buf.iter_mut().enumerate() {
            *b = Complex::new(frame[i] * window[i], 0.0);
        }
        fft.process(&mut buf);

        // Power spectrum over positive bins.
        let power: Vec<f32> = buf[..n_bins].iter().map(|c| c.norm_sqr()).collect();
        let mag_total: f32 = power.iter().map(|p| p.sqrt()).sum::<f32>() + 1e-9;

        // Mel energies → log → DCT → MFCCs.
        let mut log_mel = [0.0f32; N_MELS];
        for (m, filt) in filterbank.iter().enumerate() {
            let e: f32 = filt.iter().zip(&power).map(|(w, p)| w * p).sum();
            log_mel[m] = (e + 1e-10).ln();
        }
        let mfcc = dct(&log_mel);
        for i in 0..N_MFCC {
            mfcc_sum[i] += mfcc[i];
            mfcc_sqsum[i] += mfcc[i] * mfcc[i];
        }

        // Spectral centroid (Hz).
        let centroid: f32 = power
            .iter()
            .enumerate()
            .map(|(k, p)| (k as f32 * sr as f32 / FRAME as f32) * p.sqrt())
            .sum::<f32>()
            / mag_total;
        centroid_sum += centroid;

        // Zero-crossing rate.
        let zc = frame
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count() as f32
            / FRAME as f32;
        zcr_sum += zc;

        frames += 1;
        start += HOP;
    }

    if frames == 0 {
        return None;
    }
    let n = frames as f32;
    let mut feat = Vec::with_capacity(FEATURE_DIM);
    for i in 0..N_MFCC {
        feat.push(mfcc_sum[i] / n);
    }
    for i in 0..N_MFCC {
        let mean = mfcc_sum[i] / n;
        let var = (mfcc_sqsum[i] / n - mean * mean).max(0.0);
        feat.push(var.sqrt());
    }
    feat.push(centroid_sum / n);
    feat.push(zcr_sum / n);
    Some(feat)
}

// -- background analysis -----------------------------------------------------

/// Analyse the given paths on a background thread, streaming results back.
pub fn spawn_analyze(paths: Vec<PathBuf>) -> Receiver<(PathBuf, Vec<f32>)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for path in paths {
            if let Some(vec) = analyze_file(&path) {
                if tx.send((path, vec)).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

// -- recommendation ----------------------------------------------------------

/// Rank `candidates` by audio similarity to the `seeds`, returning the closest
/// `k` paths (seeds excluded). Features are z-scored across the candidate set so
/// every dimension contributes comparably; loudness-like scale washes out.
pub fn recommend(
    features: &Features,
    candidates: &[PathBuf],
    seeds: &[PathBuf],
    k: usize,
) -> Vec<PathBuf> {
    // Gather candidate vectors that actually have features.
    let pool: Vec<(&PathBuf, &Vec<f32>)> = candidates
        .iter()
        .filter_map(|p| features.vecs.get(p).map(|v| (p, v)))
        .filter(|(_, v)| v.len() == FEATURE_DIM)
        .collect();
    if pool.len() < 2 {
        return Vec::new();
    }

    // Per-dimension mean / std across the pool.
    let mut mean = [0.0f32; FEATURE_DIM];
    for (_, v) in &pool {
        for (d, &x) in v.iter().enumerate() {
            mean[d] += x;
        }
    }
    for m in &mut mean {
        *m /= pool.len() as f32;
    }
    let mut std = [0.0f32; FEATURE_DIM];
    for (_, v) in &pool {
        for (d, &x) in v.iter().enumerate() {
            std[d] += (x - mean[d]).powi(2);
        }
    }
    for s in &mut std {
        *s = (*s / pool.len() as f32).sqrt().max(1e-6);
    }
    let z = |v: &[f32]| -> Vec<f32> {
        (0..FEATURE_DIM).map(|d| (v[d] - mean[d]) / std[d]).collect()
    };

    // Seed centroid in standardized space.
    let seed_set: std::collections::HashSet<&PathBuf> = seeds.iter().collect();
    let seed_vecs: Vec<Vec<f32>> = seeds
        .iter()
        .filter_map(|p| features.vecs.get(p))
        .filter(|v| v.len() == FEATURE_DIM)
        .map(|v| z(v))
        .collect();
    if seed_vecs.is_empty() {
        return Vec::new();
    }
    let mut centroid = vec![0.0f32; FEATURE_DIM];
    for v in &seed_vecs {
        for d in 0..FEATURE_DIM {
            centroid[d] += v[d];
        }
    }
    for c in &mut centroid {
        *c /= seed_vecs.len() as f32;
    }

    // Distance from each non-seed candidate to the seed centroid.
    let mut scored: Vec<(f32, PathBuf)> = pool
        .iter()
        .filter(|(p, _)| !seed_set.contains(*p))
        .map(|(p, v)| {
            let zv = z(v);
            let dist: f32 = zv
                .iter()
                .zip(&centroid)
                .map(|(a, b)| (a - b).powi(2))
                .sum::<f32>()
                .sqrt();
            (dist, (*p).clone())
        })
        .collect();
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, sr: u32, secs: f32) -> Vec<f32> {
        let n = (sr as f32 * secs) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin())
            .collect()
    }

    #[test]
    fn features_have_expected_shape() {
        let sig = tone(440.0, 44100, 2.0);
        let f = extract_features(&sig, 44100).expect("features");
        assert_eq!(f.len(), FEATURE_DIM);
        assert!(f.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn brighter_tone_has_higher_centroid() {
        // The spectral centroid is the second-to-last feature.
        let low = extract_features(&tone(300.0, 44100, 2.0), 44100).unwrap();
        let high = extract_features(&tone(4000.0, 44100, 2.0), 44100).unwrap();
        let ci = FEATURE_DIM - 2;
        assert!(high[ci] > low[ci], "high {} vs low {}", high[ci], low[ci]);
    }

    #[test]
    fn recommend_prefers_acoustically_closer_track() {
        let mut feats = Features::default();
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let c = PathBuf::from("c");
        feats.insert(a.clone(), extract_features(&tone(440.0, 44100, 2.0), 44100).unwrap());
        feats.insert(b.clone(), extract_features(&tone(460.0, 44100, 2.0), 44100).unwrap());
        feats.insert(c.clone(), extract_features(&tone(6000.0, 44100, 2.0), 44100).unwrap());
        let recs = recommend(&feats, &[a.clone(), b.clone(), c.clone()], &[a.clone()], 2);
        assert_eq!(recs.first(), Some(&b), "closest to 440Hz should be 460Hz");
    }
}
