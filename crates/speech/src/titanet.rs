//! NVIDIA TitaNet-large speaker embeddings on candle (Metal on macOS, else the CPU).
//!
//! Written from the published architecture (Koluri et al. 2022, "TitaNet: Neural Model for Speaker
//! Representation with 1D Depth-wise Separable Convolutions and Global Context") and the shapes in
//! the released checkpoint: five blocks of depthwise-separable 1-D convolutions with batch norm
//! and squeeze-and-excitation over the whole segment (kernels 3, 7×3, 11×3, 15×3, 1; residual
//! 1×1 convolutions on the repeated blocks; ReLU), attentive statistics pooling, then batch norm
//! and a linear map to a 192-value embedding. Input: 80-band normalised log-mel
//! ([`crate::nemo::Features`]). Tensors are time-major `(T, C)`.

use std::path::Path;

use candle_core::{Device, Module, Result, Tensor};
use candle_nn::{Linear, VarBuilder};

use crate::SpeechError;
use crate::nemo::{self, Features, merr};

/// Length of an embedding.
pub const DIM: usize = 192;
const N_MELS: usize = 80;
/// Fewest feature frames an embedding is computed from (shorter input is repeated).
const MIN_FRAMES: usize = 50;
/// Batch-norm epsilon of the convolution blocks (NeMo's Jasper blocks use 1e-3).
const JASPER_EPS: f64 = 1e-3;

/// Batch norm folded into a scale and shift per channel.
struct Norm {
    scale: Tensor,
    shift: Tensor,
}

impl Norm {
    fn new(c: usize, eps: f64, vb: VarBuilder) -> Result<Self> {
        let (g, b, m, v) = (vb.get(c, "weight")?, vb.get(c, "bias")?, vb.get(c, "running_mean")?, vb.get(c, "running_var")?);
        let scale = g.div(&(v + eps)?.sqrt()?)?;
        let shift = (b - m.mul(&scale)?)?;
        Ok(Self { scale, shift })
    }
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        x.broadcast_mul(&self.scale)?.broadcast_add(&self.shift)
    }
}

/// Depthwise convolution along time with "same" padding: `(T, C)` → `(T, C)`.
fn depthwise(x: &Tensor, taps: &Tensor) -> Result<Tensor> {
    let (t, _) = x.dims2()?;
    let k = taps.dim(0)?;
    if k == 1 {
        return x.broadcast_mul(taps);
    }
    let padded = x.pad_with_zeros(0, k / 2, k / 2)?;
    let mut acc = padded.narrow(0, 0, t)?.broadcast_mul(&taps.narrow(0, 0, 1)?)?;
    for i in 1..k {
        acc = (acc + padded.narrow(0, i, t)?.broadcast_mul(&taps.narrow(0, i, 1)?)?)?;
    }
    Ok(acc)
}

struct SubBlock {
    taps: Tensor,
    pw: Linear,
    norm: Norm,
}

struct Block {
    subs: Vec<SubBlock>,
    se: (Linear, Linear),
    res: Option<(Linear, Norm)>,
}

impl Block {
    fn new(vb: VarBuilder, cin: usize, cout: usize, kernel: usize, repeat: usize, residual: bool) -> Result<Self> {
        let m = vb.pp("mconv");
        let mut subs = Vec::new();
        let mut c = cin;
        for r in 0..repeat {
            let base = 5 * r;
            let taps = m.get((c, 1, kernel), &format!("{base}.conv.weight"))?.squeeze(1)?.t()?.contiguous()?;
            let pw = Linear::new(m.get((cout, c, 1), &format!("{}.conv.weight", base + 1))?.squeeze(2)?, None);
            let norm = Norm::new(cout, JASPER_EPS, m.pp(format!("{}", base + 2)))?;
            subs.push(SubBlock { taps, pw, norm });
            c = cout;
        }
        let se_at = 5 * (repeat - 1) + 3;
        let red = cout / 8;
        let se =
            (Linear::new(m.get((red, cout), &format!("{se_at}.fc.0.weight"))?, None), Linear::new(m.get((cout, red), &format!("{se_at}.fc.2.weight"))?, None));
        let res = if residual {
            let r = vb.pp("res.0");
            Some((Linear::new(r.get((cout, cin, 1), "0.conv.weight")?.squeeze(2)?, None), Norm::new(cout, JASPER_EPS, r.pp("1"))?))
        } else {
            None
        };
        Ok(Self { subs, se, res })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let mut y = x.clone();
        let last = self.subs.len().saturating_sub(1);
        for (i, s) in self.subs.iter().enumerate() {
            y = s.norm.forward(&s.pw.forward(&depthwise(&y, &s.taps)?)?)?;
            if i < last {
                y = y.relu()?;
            }
        }
        // squeeze-and-excitation over the whole segment
        let g = y.mean_keepdim(0)?;
        let g = candle_nn::ops::sigmoid(&self.se.1.forward(&self.se.0.forward(&g)?.relu()?)?)?;
        let mut y = y.broadcast_mul(&g)?;
        if let Some((conv, norm)) = &self.res {
            y = (y + norm.forward(&conv.forward(x)?)?)?;
        }
        y.relu()
    }
}

/// Mean and standard deviation over time of `x` (T, C) weighted by `w` (T, C) (columns sum to 1).
fn weighted_stats(x: &Tensor, w: &Tensor) -> Result<(Tensor, Tensor)> {
    let mean = (x * w)?.sum_keepdim(0)?;
    let var = (x.broadcast_sub(&mean)?.sqr()? * w)?.sum_keepdim(0)?;
    Ok((mean, var.clamp(1e-10, f64::MAX)?.sqrt()?))
}

pub struct TitaNet {
    device: Device,
    features: Features,
    blocks: Vec<Block>,
    att_conv: Linear,
    att_norm: Norm,
    att_out: Linear,
    emb_norm: Norm,
    emb: Linear,
}

impl TitaNet {
    /// Load a model directory (`model_weights.ckpt`).
    pub fn load(dir: &Path) -> std::result::Result<Self, SpeechError> {
        let device = nemo::device();
        let vb = nemo::load_checkpoint(&dir.join("model_weights.ckpt"), &device)?;
        let features = Features::from_checkpoint(&vb, N_MELS)?;
        let e = vb.pp("encoder.encoder");
        let blocks = vec![
            Block::new(e.pp("0"), N_MELS, 1024, 3, 1, false).map_err(merr)?,
            Block::new(e.pp("1"), 1024, 1024, 7, 3, true).map_err(merr)?,
            Block::new(e.pp("2"), 1024, 1024, 11, 3, true).map_err(merr)?,
            Block::new(e.pp("3"), 1024, 1024, 15, 3, true).map_err(merr)?,
            Block::new(e.pp("4"), 1024, 3072, 1, 1, false).map_err(merr)?,
        ];
        let p = vb.pp("decoder._pooling.attention_layer");
        let lin = |vb: &VarBuilder, o: usize, i: usize, name: &str| -> Result<Linear> {
            Ok(Linear::new(vb.get((o, i, 1), &format!("{name}.weight"))?.squeeze(2)?, Some(vb.get(o, &format!("{name}.bias"))?)))
        };
        let att_conv = lin(&p, 128, 3 * 3072, "0.conv_layer").map_err(merr)?;
        let att_norm = Norm::new(128, 1e-5, p.pp("0.bn")).map_err(merr)?;
        let att_out = lin(&p, 3072, 128, "2").map_err(merr)?;
        let d = vb.pp("decoder.emb_layers.0");
        let emb_norm = Norm::new(2 * 3072, 1e-5, d.pp("0")).map_err(merr)?;
        let emb = lin(&d, DIM, 2 * 3072, "1").map_err(merr)?;
        Ok(Self { device, features, blocks, att_conv, att_norm, att_out, emb_norm, emb })
    }

    /// `"metal"` or `"cpu"`.
    pub fn device_name(&self) -> &'static str {
        nemo::device_name(&self.device)
    }

    /// The unit-length embedding of a speech segment (16 kHz mono). The segment is levelled to
    /// −20 dBFS RMS first.
    pub fn embed(&self, audio: &[f32]) -> std::result::Result<Vec<f32>, SpeechError> {
        let audio = level(audio);
        let (frames, feats) = self.features.compute(&audio);
        let x = nemo::feature_window(&feats, N_MELS, frames, 0, frames, &self.device)?.squeeze(0).map_err(merr)?;
        // repeat short segments up to the minimum length
        let x = if frames < MIN_FRAMES {
            let n = MIN_FRAMES.div_ceil(frames.max(1));
            Tensor::cat(&vec![x; n], 0).map_err(merr)?
        } else {
            x
        };
        let v = self.forward(&x).map_err(merr)?;
        let mut v = v.to_device(&Device::Cpu).and_then(|t| t.flatten_all()).and_then(|t| t.to_vec1::<f32>()).map_err(merr)?;
        crate::speakers::normalize(&mut v);
        Ok(v)
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let mut y = x.clone();
        for b in &self.blocks {
            y = b.forward(&y)?;
        }
        let (t, c) = y.dims2()?;
        let uniform = Tensor::full(1.0 / t as f32, (t, c), y.device())?;
        let (mean, std) = weighted_stats(&y, &uniform)?;
        let ctx = Tensor::cat(&[y.clone(), mean.broadcast_as((t, c))?.contiguous()?, std.broadcast_as((t, c))?.contiguous()?], 1)?;
        let a = self.att_norm.forward(&self.att_conv.forward(&ctx)?.relu()?)?.tanh()?;
        let a = self.att_out.forward(&a)?;
        let alpha = candle_nn::ops::softmax(&a, 0)?;
        let (mu, sg) = weighted_stats(&y, &alpha)?;
        let pooled = Tensor::cat(&[mu, sg], 1)?;
        self.emb.forward(&self.emb_norm.forward(&pooled)?)
    }
}

/// Scale `audio` to −20 dBFS RMS (silence stays silence; gain capped at +40 dB).
pub fn level(audio: &[f32]) -> Vec<f32> {
    let rms = (audio.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / audio.len().max(1) as f64).sqrt();
    let gain = if rms > 1e-6 { (0.1 / rms).min(100.0) } else { 1.0 };
    audio.iter().map(|v| (*v as f64 * gain) as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depthwise_is_a_same_padded_convolution() {
        let dev = Device::Cpu;
        // one channel, ramp input, taps [1, 2, 3]
        let x = Tensor::new(&[[1f32], [2.0], [3.0], [4.0]], &dev).unwrap();
        let taps = Tensor::new(&[[1f32], [2.0], [3.0]], &dev).unwrap();
        let y = depthwise(&x, &taps).unwrap().flatten_all().unwrap().to_vec1::<f32>().unwrap();
        // y[t] = x[t-1]·1 + x[t]·2 + x[t+1]·3
        assert_eq!(y, vec![8.0, 14.0, 20.0, 11.0]);
    }

    #[test]
    fn levelling_hits_minus_20_dbfs() {
        let a: Vec<f32> = (0..1600).map(|i| (i as f32 * 0.1).sin() * 0.01).collect();
        let l = level(&a);
        let rms = (l.iter().map(|v| v * v).sum::<f32>() / l.len() as f32).sqrt();
        assert!((rms - 0.1).abs() < 1e-3, "{rms}");
        assert_eq!(level(&[0.0; 10]), vec![0.0; 10]);
        assert!(level(&[]).is_empty());
    }

    #[test]
    fn weighted_stats_of_uniform_weights() {
        let dev = Device::Cpu;
        let x = Tensor::new(&[[1f32, 0.0], [3.0, 0.0]], &dev).unwrap();
        let w = Tensor::full(0.5f32, (2, 2), &dev).unwrap();
        let (m, s) = weighted_stats(&x, &w).unwrap();
        assert_eq!(m.flatten_all().unwrap().to_vec1::<f32>().unwrap(), vec![2.0, 0.0]);
        let s = s.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        assert!((s[0] - 1.0).abs() < 1e-5 && s[1] < 1e-4);
    }
}
