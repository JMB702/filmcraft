//! Token-and-Duration Transducer decoding (Xu et al. 2023, "Efficient Sequence Transduction by
//! Jointly Predicting Tokens and Durations"), greedy, on the CPU.
//!
//! The prediction network is a token embedding (the blank token embeds to zeros) and a two-layer
//! LSTM; the joint network adds the projected encoder frame and the projected prediction, applies
//! ReLU and a linear layer whose outputs are the token logits (vocabulary + blank) followed by
//! the duration logits. Greedy decoding at frame `t`: take the best token and the best duration
//! `d`; a non-blank token is emitted at `t` and advances the prediction network; then `t += d`
//! (a blank never stays on its frame). At most `MAX_SYMBOLS` tokens are emitted per frame.
//!
//! The per-step work is small matrix-vector products, which the CPU does faster than a GPU round
//! trip; the encoder projection for all frames is one matrix product done before.

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;

use crate::SpeechError;
use crate::nemo::merr;

const MAX_SYMBOLS: usize = 10;

/// A dense layer kept as row-major f32 (`out × in`) for matrix-vector products.
struct Dense {
    w: Vec<f32>,
    b: Vec<f32>,
    inp: usize,
    out: usize,
}

impl Dense {
    fn load(vb: &VarBuilder, name: &str, out: usize, inp: usize, bias: bool) -> Result<Self, SpeechError> {
        let get = |n: &str, shape: &[usize]| -> Result<Vec<f32>, SpeechError> {
            vb.get(shape, n).and_then(|t| t.to_device(&Device::Cpu)).and_then(|t| t.flatten_all()).and_then(|t| t.to_vec1::<f32>()).map_err(merr)
        };
        let w = get(&format!("{name}.weight"), &[out, inp])?;
        let b = if bias { get(&format!("{name}.bias"), &[out])? } else { vec![0.0; out] };
        Ok(Self { w, b, inp, out })
    }

    /// `y = W x + b` (`y` is overwritten).
    fn apply(&self, x: &[f32], y: &mut [f32]) {
        for (o, yo) in y.iter_mut().enumerate().take(self.out) {
            let row = &self.w[o * self.inp..(o + 1) * self.inp];
            *yo = self.b[o] + dot(row, x);
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    // eight lanes so the compiler vectorises it
    let mut acc = [0f32; 8];
    let (ca, cb) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.0.iter().zip(cb.0) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ca.1.iter().zip(cb.1) {
        s += x * y;
    }
    s
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// One LSTM layer (PyTorch gate order: input, forget, cell, output).
struct Lstm {
    ih: Dense,
    hh: Dense,
    h: usize,
}

impl Lstm {
    fn step(&self, x: &[f32], h: &mut [f32], c: &mut [f32], gi: &mut [f32], gh: &mut [f32]) {
        self.ih.apply(x, gi);
        self.hh.apply(h, gh);
        let n = self.h;
        for j in 0..n {
            let i = sigmoid(gi[j] + gh[j]);
            let f = sigmoid(gi[n + j] + gh[n + j]);
            let g = (gi[2 * n + j] + gh[2 * n + j]).tanh();
            let o = sigmoid(gi[3 * n + j] + gh[3 * n + j]);
            c[j] = f * c[j] + i * g;
            h[j] = o * c[j].tanh();
        }
    }
}

/// An emitted token: id, encoder frame, duration in frames, probability.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Token {
    pub id: u32,
    pub frame: usize,
    pub duration: usize,
    pub prob: f32,
}

pub struct Decoder {
    embed: Vec<f32>,
    lstm: Vec<Lstm>,
    pred: Dense,
    head: Dense,
    pub enc_proj: (Tensor, Tensor),
    hidden: usize,
    /// Vocabulary size without the blank; the blank is token `vocab`.
    pub vocab: usize,
    durations: Vec<usize>,
}

impl Decoder {
    /// `vb` is the checkpoint root; `vocab` excludes the blank.
    pub fn load(vb: &VarBuilder, vocab: usize, enc_dim: usize, durations: &[usize]) -> Result<Self, SpeechError> {
        let pn = vb.pp("decoder.prediction");
        let hidden = pn.get_unchecked("embed.weight").map_err(merr)?.dim(1).map_err(merr)?;
        let embed = pn
            .get((vocab + 1, hidden), "embed.weight")
            .and_then(|t| t.to_device(&Device::Cpu))
            .and_then(|t| t.flatten_all())
            .and_then(|t| t.to_vec1::<f32>())
            .map_err(merr)?;
        let rnn = pn.pp("dec_rnn.lstm");
        let mut lstm = Vec::new();
        for l in 0..2 {
            let w = |kind: &str| -> Result<Dense, SpeechError> {
                let get = |n: String, shape: &[usize]| -> Result<Vec<f32>, SpeechError> {
                    rnn.get(shape, &n).and_then(|t| t.to_device(&Device::Cpu)).and_then(|t| t.flatten_all()).and_then(|t| t.to_vec1::<f32>()).map_err(merr)
                };
                Ok(Dense {
                    w: get(format!("weight_{kind}_l{l}"), &[4 * hidden, hidden])?,
                    b: get(format!("bias_{kind}_l{l}"), &[4 * hidden])?,
                    inp: hidden,
                    out: 4 * hidden,
                })
            };
            lstm.push(Lstm { ih: w("ih")?, hh: w("hh")?, h: hidden });
        }
        let joint = vb.pp("joint");
        let pred = Dense::load(&joint, "pred", hidden, hidden, true)?;
        let head = Dense::load(&joint, "joint_net.2", vocab + 1 + durations.len(), hidden, true)?;
        let enc_proj = (joint.get((hidden, enc_dim), "enc.weight").map_err(merr)?, joint.get(hidden, "enc.bias").map_err(merr)?);
        Ok(Self { embed, lstm, pred, head, enc_proj, hidden, vocab, durations: durations.to_vec() })
    }

    /// Project encoder frames `(T, D)` into the joint space: `(T, hidden)` on the CPU.
    pub fn project(&self, enc: &Tensor) -> Result<Vec<f32>, SpeechError> {
        let y = enc.matmul(&self.enc_proj.0.t().map_err(merr)?).and_then(|y| y.broadcast_add(&self.enc_proj.1)).map_err(merr)?;
        y.to_device(&Device::Cpu).and_then(|t| t.flatten_all()).and_then(|t| t.to_vec1::<f32>()).map_err(merr)
    }

    /// Greedy decoding of projected frames (`frames × hidden`).
    pub fn decode(&self, proj: &[f32], cancel: &dyn Fn() -> bool) -> Result<Vec<Token>, SpeechError> {
        let n = self.hidden;
        let frames = proj.len() / n.max(1);
        let blank = self.vocab;
        let mut h = vec![vec![0f32; n]; self.lstm.len()];
        let mut c = vec![vec![0f32; n]; self.lstm.len()];
        let (mut gi, mut gh) = (vec![0f32; 4 * n], vec![0f32; 4 * n]);
        let mut g = vec![0f32; n]; // projected prediction for the last emitted token
        let mut x = vec![0f32; n];
        let mut joint = vec![0f32; n];
        let mut logits = vec![0f32; self.head.out];
        // the prediction for "no token yet" (blank embeds to zeros) and the state it leaves
        let mut state_h = h.clone();
        let mut state_c = c.clone();
        self.predict(None, &mut state_h, &mut state_c, &mut gi, &mut gh, &mut x, &mut g);
        let mut out = Vec::new();
        let mut t = 0usize;
        while t < frames {
            if t.is_multiple_of(64) && cancel() {
                return Err(SpeechError::Cancelled);
            }
            let f = &proj[t * n..(t + 1) * n];
            let mut emitted = 0;
            loop {
                for j in 0..n {
                    joint[j] = (f[j] + g[j]).max(0.0);
                }
                self.head.apply(&joint, &mut logits);
                let k = argmax(&logits[..=blank]);
                let dk = argmax(&logits[blank + 1..]);
                let mut skip = self.durations.get(dk).copied().unwrap_or(1);
                if k == blank {
                    t += skip.max(1);
                    break;
                }
                out.push(Token { id: k as u32, frame: t, duration: skip, prob: softmax_at(&logits[..=blank], k) });
                emitted += 1;
                h.clone_from(&state_h);
                c.clone_from(&state_c);
                self.predict(Some(k), &mut h, &mut c, &mut gi, &mut gh, &mut x, &mut g);
                state_h.clone_from(&h);
                state_c.clone_from(&c);
                if emitted >= MAX_SYMBOLS && skip == 0 {
                    skip = 1;
                }
                if skip > 0 {
                    t += skip;
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Run the prediction network on `token` (None: start of sequence) from state `h`/`c`
    /// (updated), leaving the projected output in `g`.
    #[allow(clippy::too_many_arguments)]
    fn predict(&self, token: Option<usize>, h: &mut [Vec<f32>], c: &mut [Vec<f32>], gi: &mut [f32], gh: &mut [f32], x: &mut [f32], g: &mut [f32]) {
        let n = self.hidden;
        match token.and_then(|k| self.embed.get(k * n..(k + 1) * n)) {
            Some(e) => x.copy_from_slice(e),
            None => x.iter_mut().for_each(|v| *v = 0.0),
        }
        for (l, layer) in self.lstm.iter().enumerate() {
            layer.step(x, &mut h[l], &mut c[l], gi, gh);
            x.copy_from_slice(&h[l]);
        }
        self.pred.apply(x, g);
    }
}

fn softmax_at(x: &[f32], i: usize) -> f32 {
    let m = x.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let s: f32 = x.iter().map(|v| (v - m).exp()).sum();
    x.get(i).map(|v| (v - m).exp() / s.max(f32::MIN_POSITIVE)).unwrap_or(0.0)
}

fn argmax(x: &[f32]) -> usize {
    let mut best = 0;
    for (i, v) in x.iter().enumerate() {
        if *v > x[best] {
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..19).map(|i| i as f32 * 0.5).collect();
        let b: Vec<f32> = (0..19).map(|i| 1.0 - i as f32 * 0.1).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - naive).abs() < 1e-3);
    }

    /// A decoder whose joint emits token 0 with duration 2 whenever the frame's first value is
    /// positive, else blank with duration 0 (which must still advance).
    fn toy() -> Decoder {
        let n = 2;
        let vocab = 1; // token 0, blank 1
        let durations = vec![0, 1, 2];
        let zero = |o: usize, i: usize| Dense { w: vec![0.0; o * i], b: vec![0.0; o], inp: i, out: o };
        let lstm = (0..2).map(|_| Lstm { ih: zero(4 * n, n), hh: zero(4 * n, n), h: n }).collect();
        // logits: [token0, blank, d0, d1, d2]
        let head = Dense { w: vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0], b: vec![0.0, 0.5, 0.1, 0.0, 0.0], inp: n, out: 5 };
        Decoder {
            embed: vec![0.0; (vocab + 1) * n],
            lstm,
            pred: zero(n, n),
            head,
            enc_proj: (Tensor::zeros((n, n), candle_core::DType::F32, &Device::Cpu).unwrap(), Tensor::zeros(n, candle_core::DType::F32, &Device::Cpu).unwrap()),
            hidden: n,
            vocab,
            durations,
        }
    }

    #[test]
    fn greedy_decoding_emits_and_skips() {
        let d = toy();
        // frame 0 is speech (token 0; d2 gets the first value, so skip 2), the rest silence
        let proj = vec![2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let out = d.decode(&proj, &|| false).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].id, out[0].frame, out[0].duration), (0, 0, 2));
        assert!(out[0].prob > 0.5 && out[0].prob <= 1.0);
        // cancellation
        assert_eq!(d.decode(&proj, &|| true), Err(SpeechError::Cancelled));
        // empty / ragged input never panics
        assert!(d.decode(&[], &|| false).unwrap().is_empty());
        assert!(d.decode(&[1.0], &|| false).unwrap().is_empty());
    }

    #[test]
    fn zero_duration_tokens_are_capped_per_frame() {
        let mut d = toy();
        // duration logits always favour 0: token 0 repeats on one frame until the cap
        d.head.b = vec![0.0, 0.5, 5.0, 0.0, 0.0];
        d.head.w = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let out = d.decode(&[2.0, 0.0], &|| false).unwrap();
        assert_eq!(out.len(), MAX_SYMBOLS);
        assert!(out.iter().all(|t| t.frame == 0));
    }
}
