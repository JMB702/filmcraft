//! What the NVIDIA NeMo models (Parakeet speech recognition, TitaNet speaker embeddings) share:
//! the compute device, checkpoint loading and the log-mel front end.
//!
//! **Checkpoints.** NVIDIA publishes each model as a `.nemo` file, an uncompressed tar holding
//! `model_config.yaml`, the tokenizer files and `model_weights.ckpt` (a PyTorch state dict). The
//! catalogue ([`crate::models`]) downloads only the members FilmCraft reads, as pinned byte ranges
//! of NVIDIA's own file, each checked against its SHA-256; the checkpoint is then read in place
//! by candle's PyTorch reader.
//!
//! **Front end** (NeMo `AudioToMelSpectrogramPreprocessor`, as configured for these models in
//! their `model_config.yaml`): pre-emphasis 0.97, a 25 ms Hann window (symmetric, 400 samples,
//! centred in a 512-point FFT), 10 ms hop, frames centred with zero padding, power spectrum, the
//! model's own mel filter bank (`preprocessor.featurizer.fb` in the checkpoint), `ln(x + 2⁻²⁴)`,
//! then every mel band normalised to zero mean and unit standard deviation over the clip.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;

use crate::SpeechError;

/// Samples per hop (10 ms at 16 kHz).
pub const HOP: usize = 160;
const WIN: usize = 400;
const N_FFT: usize = 512;
const N_BINS: usize = N_FFT / 2 + 1;
const LOG_GUARD: f32 = 5.960_464_5e-8; // 2^-24
const NORM_EPS: f32 = 1e-5;

pub(crate) fn merr(e: candle_core::Error) -> SpeechError {
    SpeechError::Model(e.to_string())
}

/// The device models run on: the GPU through Metal on macOS when one is present, else the CPU.
/// `FILMCRAFT_SPEECH_CPU=1` forces the CPU (measurements, debugging).
pub fn device() -> Device {
    if std::env::var_os("FILMCRAFT_SPEECH_CPU").is_some() {
        return Device::Cpu;
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(d) = Device::new_metal(0) {
            return d;
        }
    }
    Device::Cpu
}

/// A short name of `d` for reports (`"metal"`, `"cpu"`).
pub fn device_name(d: &Device) -> &'static str {
    if d.is_metal() { "metal" } else { "cpu" }
}

/// The weights of a NeMo checkpoint (`model_weights.ckpt`) on `device`.
pub fn load_checkpoint(path: &Path, device: &Device) -> Result<VarBuilder<'static>, SpeechError> {
    VarBuilder::from_pth(path, DType::F32, device).map_err(|e| SpeechError::Model(format!("{}: {e}", path.display())))
}

/// The front end of one model: its mel filter bank (`n_mels × 257`) and window.
pub struct Features {
    pub n_mels: usize,
    filters: Vec<f32>,
    window: [f32; N_FFT],
}

impl Features {
    /// Read the filter bank from the checkpoint (`preprocessor.featurizer.fb`, shape
    /// `1 × n_mels × 257`).
    pub fn from_checkpoint(vb: &VarBuilder, n_mels: usize) -> Result<Self, SpeechError> {
        let fb = vb.get((1, n_mels, N_BINS), "preprocessor.featurizer.fb").map_err(merr)?;
        let filters = fb.to_device(&Device::Cpu).and_then(|t| t.flatten_all()).and_then(|t| t.to_vec1::<f32>()).map_err(merr)?;
        Ok(Self::with_filters(n_mels, filters))
    }

    /// A front end with the given filter bank (`n_mels × 257`, row-major).
    pub fn with_filters(n_mels: usize, filters: Vec<f32>) -> Self {
        let mut window = [0f32; N_FFT];
        let left = (N_FFT - WIN) / 2;
        for i in 0..WIN {
            // torch.hann_window(400, periodic=False)
            window[left + i] = (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (WIN - 1) as f64).cos()) as f32;
        }
        Self { n_mels, filters, window }
    }

    /// Normalised log-mel features of `audio` (16 kHz mono): `(frames, n_mels × frames)`, row-major
    /// by mel band. `frames = len / 160 + 1`.
    pub fn compute(&self, audio: &[f32]) -> (usize, Vec<f32>) {
        let frames = audio.len() / HOP + 1;
        let n_mels = self.n_mels;
        // pre-emphasis
        let mut x = Vec::with_capacity(audio.len());
        let mut prev = 0f32;
        for (i, &s) in audio.iter().enumerate() {
            x.push(if i == 0 { s } else { s - 0.97 * prev });
            prev = s;
        }
        let pad = N_FFT / 2;
        let mut out = vec![0f32; n_mels * frames];
        let mut col = vec![0f32; n_mels * frames];
        {
            use rayon::prelude::*;
            let fft = Fft::new(N_FFT);
            // frame-major first (each task writes one frame's mel bands), transposed below
            col.par_chunks_mut(n_mels).enumerate().for_each(|(f, mel)| {
                let mut re = [0f32; N_FFT];
                let mut im = [0f32; N_FFT];
                let base = (f * HOP) as isize - pad as isize;
                for (i, v) in re.iter_mut().enumerate() {
                    let j = base + i as isize;
                    let s = if j >= 0 { x.get(j as usize).copied().unwrap_or(0.0) } else { 0.0 };
                    *v = s * self.window[i];
                }
                fft.run(&mut re, &mut im);
                let mut power = [0f32; N_BINS];
                for (k, p) in power.iter_mut().enumerate() {
                    *p = re[k] * re[k] + im[k] * im[k];
                }
                for (m, v) in mel.iter_mut().enumerate() {
                    let w = self.filters.get(m * N_BINS..(m + 1) * N_BINS).unwrap_or(&[]);
                    let e: f32 = w.iter().zip(&power).map(|(a, b)| a * b).sum();
                    *v = (e + LOG_GUARD).ln();
                }
            });
        }
        for f in 0..frames {
            for m in 0..n_mels {
                out[m * frames + f] = col[f * n_mels + m];
            }
        }
        // per-band normalisation over the clip (sample standard deviation)
        if frames > 1 {
            for m in 0..n_mels {
                let row = &mut out[m * frames..(m + 1) * frames];
                let mean = row.iter().map(|&v| v as f64).sum::<f64>() / frames as f64;
                let var = row.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / (frames - 1) as f64;
                let std = var.sqrt() as f32 + NORM_EPS;
                for v in row.iter_mut() {
                    *v = (*v - mean as f32) / std;
                }
            }
        }
        (frames, out)
    }
}

/// Frames `a..b` of a `n_mels × frames` feature matrix as a `(1, b - a, n_mels)` tensor (time
/// first, the layout the NeMo encoders take).
pub fn feature_window(feats: &[f32], n_mels: usize, frames: usize, a: usize, b: usize, device: &Device) -> Result<Tensor, SpeechError> {
    let b = b.min(frames);
    let a = a.min(b);
    let n = b - a;
    let mut w = vec![0f32; n * n_mels];
    for m in 0..n_mels {
        let row = feats.get(m * frames + a..m * frames + b).unwrap_or(&[]);
        for (t, &v) in row.iter().enumerate() {
            w[t * n_mels + m] = v;
        }
    }
    Tensor::from_vec(w, (1, n, n_mels), device).map_err(merr)
}

/// An iterative radix-2 FFT of a fixed power-of-two size.
struct Fft {
    n: usize,
    cos: Vec<f32>,
    sin: Vec<f32>,
    rev: Vec<usize>,
}

impl Fft {
    fn new(n: usize) -> Self {
        let bits = n.trailing_zeros();
        let rev = (0..n).map(|i| i.reverse_bits() >> (usize::BITS - bits)).collect();
        let half = n / 2;
        let cos = (0..half).map(|k| (2.0 * std::f64::consts::PI * k as f64 / n as f64).cos() as f32).collect();
        let sin = (0..half).map(|k| -(2.0 * std::f64::consts::PI * k as f64 / n as f64).sin() as f32).collect();
        Self { n, cos, sin, rev }
    }

    /// In place; `re` and `im` are `n` long.
    fn run(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        if re.len() < n || im.len() < n {
            return;
        }
        for i in 0..n {
            let j = self.rev[i];
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = (self.cos[k * step], self.sin[k * step]);
                    let (a, b) = (start + k, start + k + len / 2);
                    let tr = re[b] * c - im[b] * s;
                    let ti = re[b] * s + im[b] * c;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_matches_a_direct_transform() {
        let n = 16;
        let fft = Fft::new(n);
        let x: Vec<f32> = (0..n).map(|i| ((i * 7 % 5) as f32 - 2.0) * 0.3).collect();
        let (mut re, mut im) = (x.clone(), vec![0f32; n]);
        fft.run(&mut re, &mut im);
        for k in 0..n {
            let (mut r, mut i) = (0f64, 0f64);
            for (t, &v) in x.iter().enumerate() {
                let a = -2.0 * std::f64::consts::PI * (k * t) as f64 / n as f64;
                r += v as f64 * a.cos();
                i += v as f64 * a.sin();
            }
            assert!((re[k] as f64 - r).abs() < 1e-4 && (im[k] as f64 - i).abs() < 1e-4, "bin {k}");
        }
    }

    #[test]
    fn features_are_normalised_per_band() {
        // a flat filter bank of 4 bands
        let f = Features::with_filters(4, vec![1.0 / N_BINS as f32; 4 * N_BINS]);
        let audio: Vec<f32> = (0..16_000).map(|i| ((i as f32) * 0.05).sin() * 0.1 + ((i * 31 % 17) as f32 - 8.0) * 0.001).collect();
        let (frames, m) = f.compute(&audio);
        assert_eq!(frames, 101);
        for b in 0..4 {
            let row = &m[b * frames..(b + 1) * frames];
            let mean: f32 = row.iter().sum::<f32>() / frames as f32;
            assert!(mean.abs() < 1e-3, "{mean}");
        }
        // empty and one-sample input never panic
        assert_eq!(f.compute(&[]).0, 1);
        assert_eq!(f.compute(&[0.5]).0, 1);
    }

    #[test]
    fn feature_window_is_time_first_and_clamped() {
        let feats: Vec<f32> = (0..2 * 5).map(|v| v as f32).collect(); // 2 bands × 5 frames
        let t = feature_window(&feats, 2, 5, 3, 99, &Device::Cpu).unwrap();
        assert_eq!(t.dims(), &[1, 2, 2]);
        assert_eq!(t.flatten_all().unwrap().to_vec1::<f32>().unwrap(), vec![3.0, 8.0, 4.0, 9.0]);
    }
}
