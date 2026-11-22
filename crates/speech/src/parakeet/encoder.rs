//! The FastConformer encoder of Parakeet (NeMo `ConformerEncoder`, as configured in the model's
//! `model_config.yaml`: `dw_striding` subsampling by 8 with 256 channels, 24 layers of width 1024,
//! 8 heads with relative positions (`rel_pos`, untied biases, no input scaling), convolution
//! kernel 9 with batch norm, no linear biases).
//!
//! Written from the published architecture (Gulati et al. 2020, "Conformer"; Rekesh et al. 2023,
//! "Fast Conformer with Linearly Scalable Attention"; Dai et al. 2019, "Transformer-XL" for the
//! relative-position attention) against candle's tensor API. Tensors stay batch 1 and time-major:
//! `(T, D)`.

use candle_core::{Module, Result, Tensor};
use candle_nn::{LayerNorm, Linear, VarBuilder};

/// Encoder shape (from the checkpoint and the model config).
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub n_mels: usize,
    pub d_model: usize,
    pub layers: usize,
    pub heads: usize,
    pub ff: usize,
    pub kernel: usize,
    pub sub_channels: usize,
}

impl Config {
    /// Parakeet TDT 0.6B (v2 and v3).
    pub const PARAKEET_0_6B: Config = Config { n_mels: 128, d_model: 1024, layers: 24, heads: 8, ff: 4096, kernel: 9, sub_channels: 256 };
}

fn linear_no_bias(i: usize, o: usize, vb: VarBuilder) -> Result<Linear> {
    Ok(Linear::new(vb.get((o, i), "weight")?, None))
}

fn layer_norm(d: usize, vb: VarBuilder) -> Result<LayerNorm> {
    Ok(LayerNorm::new(vb.get(d, "weight")?, vb.get(d, "bias")?, 1e-5))
}

/// A 3×3, stride-2, padding-1 depthwise convolution of `(1, C, H, W)` as nine shifted,
/// sub-sampled multiply-adds (candle runs grouped convolutions one group at a time).
fn depthwise_3x3_s2(x: &Tensor, w: &Tensor, b: &Tensor) -> Result<Tensor> {
    let (_, c, h, wd) = x.dims4()?;
    let (ho, wo) = (h.div_ceil(2), wd.div_ceil(2));
    // pad 1 before and enough after that every strided read stays in range
    let x = x.pad_with_zeros(2, 1, 2 * ho + 1 - h)?.pad_with_zeros(3, 1, 2 * wo + 1 - wd)?;
    let mut acc: Option<Tensor> = None;
    for i in 0..3 {
        let rows = x.narrow(2, i, 2 * ho)?.reshape((1, c, ho, 2, 2 * wo + 2))?.narrow(3, 0, 1)?.squeeze(3)?;
        for j in 0..3 {
            let tap = rows.narrow(3, j, 2 * wo)?.reshape((1, c, ho, wo, 2))?.narrow(4, 0, 1)?.squeeze(4)?;
            let k = w.narrow(2, i, 1)?.narrow(3, j, 1)?.reshape((1, c, 1, 1))?;
            let term = tap.broadcast_mul(&k)?;
            acc = Some(match acc {
                Some(a) => (a + term)?,
                None => term,
            });
        }
    }
    let Some(acc) = acc else { candle_core::bail!("empty kernel") };
    acc.broadcast_add(&b.reshape((1, c, 1, 1))?)
}

/// A 1×1 convolution of `(1, C, H, W)` as a matrix product.
fn pointwise(x: &Tensor, w: &Tensor, b: &Tensor) -> Result<Tensor> {
    let (_, c, h, wd) = x.dims4()?;
    let co = w.dim(0)?;
    let y = w.reshape((co, c))?.matmul(&x.reshape((c, h * wd))?)?;
    y.broadcast_add(&b.reshape((co, 1))?)?.reshape((1, co, h, wd))
}

/// `dw_striding` subsampling: conv 3×3/2 → ReLU → (depthwise 3×3/2 → pointwise → ReLU) × 2, then
/// a linear map of the flattened channels × frequencies to the model width.
struct Subsampling {
    conv0: (Tensor, Tensor),
    dw1: (Tensor, Tensor),
    pw1: (Tensor, Tensor),
    dw2: (Tensor, Tensor),
    pw2: (Tensor, Tensor),
    out: Linear,
}

impl Subsampling {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let c = cfg.sub_channels;
        let conv = |i: usize, shape: (usize, usize, usize, usize)| -> Result<(Tensor, Tensor)> {
            Ok((vb.get(shape, &format!("conv.{i}.weight"))?, vb.get(shape.0, &format!("conv.{i}.bias"))?))
        };
        let freq = cfg.n_mels.div_ceil(2).div_ceil(2).div_ceil(2);
        Ok(Self {
            conv0: conv(0, (c, 1, 3, 3))?,
            dw1: conv(2, (c, 1, 3, 3))?,
            pw1: conv(3, (c, c, 1, 1))?,
            dw2: conv(5, (c, 1, 3, 3))?,
            pw2: conv(6, (c, c, 1, 1))?,
            out: candle_nn::linear(c * freq, cfg.d_model, vb.pp("out"))?,
        })
    }

    /// `(1, T, n_mels)` → `(T', d_model)` with `T' = ⌈T / 8⌉`.
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let x = x.unsqueeze(1)?; // (1, 1, T, F)
        let x = x.conv2d(&self.conv0.0, 1, 2, 1, 1)?.broadcast_add(&self.conv0.1.reshape((1, (), 1, 1))?)?.relu()?;
        let x = pointwise(&depthwise_3x3_s2(&x, &self.dw1.0, &self.dw1.1)?, &self.pw1.0, &self.pw1.1)?.relu()?;
        let x = pointwise(&depthwise_3x3_s2(&x, &self.dw2.0, &self.dw2.1)?, &self.pw2.0, &self.pw2.1)?.relu()?;
        let (_, c, t, f) = x.dims4()?;
        let x = x.squeeze(0)?.transpose(0, 1)?.contiguous()?.reshape((t, c * f))?;
        self.out.forward(&x)
    }
}

struct FeedForward {
    l1: Linear,
    l2: Linear,
}

impl FeedForward {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        Ok(Self { l1: linear_no_bias(cfg.d_model, cfg.ff, vb.pp("linear1"))?, l2: linear_no_bias(cfg.ff, cfg.d_model, vb.pp("linear2"))? })
    }
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        self.l2.forward(&candle_nn::ops::silu(&self.l1.forward(x)?)?)
    }
}

/// Multi-head self-attention with Transformer-XL relative positions.
struct Attention {
    q: Linear,
    k: Linear,
    v: Linear,
    out: Linear,
    pos: Linear,
    bias_u: Tensor,
    bias_v: Tensor,
    heads: usize,
}

impl Attention {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let d = cfg.d_model;
        let dk = d / cfg.heads;
        Ok(Self {
            q: linear_no_bias(d, d, vb.pp("linear_q"))?,
            k: linear_no_bias(d, d, vb.pp("linear_k"))?,
            v: linear_no_bias(d, d, vb.pp("linear_v"))?,
            out: linear_no_bias(d, d, vb.pp("linear_out"))?,
            pos: linear_no_bias(d, d, vb.pp("linear_pos"))?,
            bias_u: vb.get((cfg.heads, dk), "pos_bias_u")?,
            bias_v: vb.get((cfg.heads, dk), "pos_bias_v")?,
            heads: cfg.heads,
        })
    }

    /// `x`: `(T, D)`; `pos`: `(2T − 1, D)` sinusoids for relative distances `T − 1 … −(T − 1)`.
    fn forward(&self, x: &Tensor, pos: &Tensor) -> Result<Tensor> {
        let (t, d) = x.dims2()?;
        let h = self.heads;
        let dk = d / h;
        let q = self.q.forward(x)?.reshape((t, h, dk))?;
        let k = self.k.forward(x)?.reshape((t, h, dk))?.transpose(0, 1)?.contiguous()?; // (h, t, dk)
        let v = self.v.forward(x)?.reshape((t, h, dk))?.transpose(0, 1)?.contiguous()?;
        let p = self.pos.forward(pos)?.reshape((2 * t - 1, h, dk))?.transpose(0, 1)?.contiguous()?; // (h, 2t-1, dk)
        let qu = q.broadcast_add(&self.bias_u)?.transpose(0, 1)?.contiguous()?; // (h, t, dk)
        let qv = q.broadcast_add(&self.bias_v)?.transpose(0, 1)?.contiguous()?;
        let ac = qu.matmul(&k.t()?)?; // (h, t, t)
        let bd = qv.matmul(&p.t()?)?; // (h, t, 2t-1)
        let bd = rel_shift(&bd)?.narrow(2, 0, t)?;
        let scores = ((ac + bd)? / (dk as f64).sqrt())?;
        let attn = candle_nn::ops::softmax_last_dim(&scores)?;
        let o = attn.matmul(&v)?.transpose(0, 1)?.contiguous()?.reshape((t, d))?;
        self.out.forward(&o)
    }
}

/// Transformer-XL's relative shift of `(h, q, p)` scores: row `i` moves left by `q − 1 − i`.
fn rel_shift(x: &Tensor) -> Result<Tensor> {
    let (h, q, p) = x.dims3()?;
    x.pad_with_zeros(2, 1, 0)?.reshape((h, p + 1, q))?.narrow(1, 1, p)?.contiguous()?.reshape((h, q, p))
}

/// The sinusoids for relative distances `t − 1` down to `−(t − 1)`: `(2t − 1, d)`.
fn relative_positions(t: usize, d: usize, device: &candle_core::Device) -> Result<Tensor> {
    let n = 2 * t - 1;
    let mut pe = vec![0f32; n * d];
    for (row, chunk) in pe.chunks_mut(d).enumerate() {
        let pos = (t as f64 - 1.0) - row as f64;
        for i in 0..d / 2 {
            let div = (-(2.0 * i as f64) * (10_000f64.ln() / d as f64)).exp();
            chunk[2 * i] = (pos * div).sin() as f32;
            chunk[2 * i + 1] = (pos * div).cos() as f32;
        }
    }
    Tensor::from_vec(pe, (n, d), device)
}

/// Pointwise (GLU) → depthwise along time → batch norm → swish → pointwise.
struct ConvModule {
    pw1: Linear,
    dw: Tensor,
    bn_scale: Tensor,
    bn_shift: Tensor,
    pw2: Linear,
    kernel: usize,
}

impl ConvModule {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let d = cfg.d_model;
        let pw1 = Linear::new(vb.get((2 * d, d, 1), "pointwise_conv1.weight")?.squeeze(2)?, None);
        let pw2 = Linear::new(vb.get((d, d, 1), "pointwise_conv2.weight")?.squeeze(2)?, None);
        // (d, 1, k) → (k, d): one row per tap
        let dw = vb.get((d, 1, cfg.kernel), "depthwise_conv.weight")?.squeeze(1)?.t()?.contiguous()?;
        let bn = vb.pp("batch_norm");
        let (g, b, m, v) = (bn.get(d, "weight")?, bn.get(d, "bias")?, bn.get(d, "running_mean")?, bn.get(d, "running_var")?);
        let bn_scale = g.div(&(v + 1e-5)?.sqrt()?)?;
        let bn_shift = (b - m.mul(&bn_scale)?)?;
        Ok(Self { pw1, dw, bn_scale, bn_shift, pw2, kernel: cfg.kernel })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let (t, d) = x.dims2()?;
        let y = self.pw1.forward(x)?;
        let y = y.narrow(1, 0, d)?.mul(&candle_nn::ops::sigmoid(&y.narrow(1, d, d)?)?)?;
        let half = self.kernel / 2;
        let padded = y.pad_with_zeros(0, half, half)?;
        let mut acc: Option<Tensor> = None;
        for k in 0..self.kernel {
            let term = padded.narrow(0, k, t)?.broadcast_mul(&self.dw.narrow(0, k, 1)?)?;
            acc = Some(match acc {
                Some(a) => (a + term)?,
                None => term,
            });
        }
        let Some(y) = acc else { candle_core::bail!("empty kernel") };
        let y = y.broadcast_mul(&self.bn_scale)?.broadcast_add(&self.bn_shift)?;
        self.pw2.forward(&candle_nn::ops::silu(&y)?)
    }
}

struct Layer {
    norm_ff1: LayerNorm,
    ff1: FeedForward,
    norm_att: LayerNorm,
    att: Attention,
    norm_conv: LayerNorm,
    conv: ConvModule,
    norm_ff2: LayerNorm,
    ff2: FeedForward,
    norm_out: LayerNorm,
}

impl Layer {
    fn new(cfg: &Config, vb: VarBuilder) -> Result<Self> {
        let d = cfg.d_model;
        Ok(Self {
            norm_ff1: layer_norm(d, vb.pp("norm_feed_forward1"))?,
            ff1: FeedForward::new(cfg, vb.pp("feed_forward1"))?,
            norm_att: layer_norm(d, vb.pp("norm_self_att"))?,
            att: Attention::new(cfg, vb.pp("self_attn"))?,
            norm_conv: layer_norm(d, vb.pp("norm_conv"))?,
            conv: ConvModule::new(cfg, vb.pp("conv"))?,
            norm_ff2: layer_norm(d, vb.pp("norm_feed_forward2"))?,
            ff2: FeedForward::new(cfg, vb.pp("feed_forward2"))?,
            norm_out: layer_norm(d, vb.pp("norm_out"))?,
        })
    }

    fn forward(&self, x: &Tensor, pos: &Tensor) -> Result<Tensor> {
        let r = (x + (self.ff1.forward(&self.norm_ff1.forward(x)?)? * 0.5)?)?;
        let r = (&r + self.att.forward(&self.norm_att.forward(&r)?, pos)?)?;
        let r = (&r + self.conv.forward(&self.norm_conv.forward(&r)?)?)?;
        let r = (&r + (self.ff2.forward(&self.norm_ff2.forward(&r)?)? * 0.5)?)?;
        self.norm_out.forward(&r)
    }
}

pub struct Encoder {
    sub: Subsampling,
    layers: Vec<Layer>,
    pub cfg: Config,
}

impl Encoder {
    pub fn new(cfg: Config, vb: VarBuilder) -> Result<Self> {
        let sub = Subsampling::new(&cfg, vb.pp("pre_encode"))?;
        let layers = (0..cfg.layers).map(|i| Layer::new(&cfg, vb.pp(format!("layers.{i}")))).collect::<Result<Vec<_>>>()?;
        Ok(Self { sub, layers, cfg })
    }

    /// Features `(1, T, n_mels)` → encoder frames `(⌈T / 8⌉, d_model)`, 80 ms apart.
    /// `cancel` is checked between layers.
    pub fn forward(&self, feats: &Tensor, cancel: &mut dyn FnMut() -> bool) -> Result<Tensor> {
        let mut x = self.sub.forward(feats)?;
        let t = x.dim(0)?;
        let pos = relative_positions(t, self.cfg.d_model, x.device())?;
        for l in &self.layers {
            if cancel() {
                candle_core::bail!("stopped");
            }
            x = l.forward(&x, &pos)?;
        }
        x.contiguous()
    }
}

#[cfg(test)]
/// Encoder frames for `frames` feature frames (each stride-2 stage: `⌊(n − 1) / 2⌋ + 1`).
pub fn encoded_len(frames: usize) -> usize {
    let mut n = frames;
    for _ in 0..3 {
        n = if n == 0 { 0 } else { (n - 1) / 2 + 1 };
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::Device;

    #[test]
    fn rel_shift_aligns_relative_positions() {
        // scores[i][j] = position index j of the (2t-1) table; after the shift, row i column j
        // must read the table at (t - 1) - i + j, the column of relative distance i - j
        let t = 4;
        let p = 2 * t - 1;
        let v: Vec<f32> = (0..t).flat_map(|_| (0..p).map(|j| j as f32)).collect();
        let x = Tensor::from_vec(v, (1, t, p), &Device::Cpu).unwrap();
        let y = rel_shift(&x).unwrap().narrow(2, 0, t).unwrap();
        let y = y.squeeze(0).unwrap().to_vec2::<f32>().unwrap();
        for (i, row) in y.iter().enumerate() {
            for (j, &val) in row.iter().enumerate() {
                assert_eq!(val as usize, t - 1 - i + j, "({i},{j})");
            }
        }
    }

    #[test]
    fn strided_depthwise_matches_direct_convolution() {
        let dev = Device::Cpu;
        let (c, h, w) = (2, 5, 4);
        let x = Tensor::arange(0f32, (c * h * w) as f32, &dev).unwrap().reshape((1, c, h, w)).unwrap();
        let k = Tensor::arange(0f32, (c * 9) as f32, &dev).unwrap().affine(0.1, -0.5).unwrap().reshape((c, 1, 3, 3)).unwrap();
        let b = Tensor::new(&[0.25f32, -1.0], &dev).unwrap();
        let got = depthwise_3x3_s2(&x, &k, &b).unwrap();
        let want = x.conv2d(&k, 1, 2, 1, c).unwrap().broadcast_add(&b.reshape((1, c, 1, 1)).unwrap()).unwrap();
        assert_eq!(got.dims(), want.dims());
        let diff = (got - want).unwrap().abs().unwrap().max_all().unwrap().to_scalar::<f32>().unwrap();
        assert!(diff < 1e-4, "{diff}");
    }

    #[test]
    fn encoded_lengths() {
        assert_eq!(encoded_len(0), 0);
        assert_eq!(encoded_len(1), 1);
        assert_eq!(encoded_len(8), 1);
        assert_eq!(encoded_len(9), 2);
        assert_eq!(encoded_len(6001), 751);
    }
}
