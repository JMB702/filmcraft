//! NVIDIA Parakeet TDT 0.6B speech recognition on candle (Metal on macOS, else the CPU).
//!
//! 1. The whole clip's normalised log-mel features are computed once ([`crate::nemo::Features`],
//!    128 bands; normalising over the whole clip is what NeMo does for one file).
//! 2. The features are cut into windows of at most [`MAX_WINDOW_S`] seconds, each cut placed at
//!    the quietest 100 ms of the window's last [`CUT_SEARCH_S`] seconds, so a cut lands in a pause
//!    rather than inside a word. Each window runs through the FastConformer encoder
//!    ([`encoder`]) and greedy TDT decoding ([`tdt`]).
//! 3. Tokens are SentencePiece pieces; a piece starting with `▁` starts a word. A word starts at
//!    its first token's frame and ends at its last token's frame plus that token's duration
//!    (frames are 80 ms). Word confidence is the lowest token probability.
//! 4. Word bounds are tightened against the audio ([`crate::vad`]), as for Whisper.
//!
//! Speaker labels are added by the caller across all clips of a job ([`crate::speakers`]).

mod encoder;
mod tdt;

use std::path::Path;

use candle_core::Device;
use filmcraft_project::{Transcript, Word};

use crate::nemo::{self, Features, HOP, merr};
use crate::{Options, ProgressFn, SpeechError, Transcriber, sample_tick};

/// Longest window run through the encoder at once, seconds.
pub const MAX_WINDOW_S: usize = 90;
/// How far back from a window's end its cut may move to find a pause, seconds.
pub const CUT_SEARCH_S: usize = 20;
/// Feature frames per encoder frame.
const SUBSAMPLING: usize = 8;
const DURATIONS: [usize; 5] = [0, 1, 2, 3, 4];

/// The SentencePiece pieces of a model (`tokenizer.vocab`: one `piece<TAB>score` line per id).
pub struct Vocab {
    pieces: Vec<String>,
}

impl Vocab {
    pub fn parse(text: &str) -> Result<Self, SpeechError> {
        let pieces: Vec<String> = text.lines().map(|l| l.split('\t').next().unwrap_or_default().to_string()).collect();
        if pieces.is_empty() || pieces.len() > 1 << 20 {
            return Err(SpeechError::Model(format!("tokenizer.vocab: {} pieces", pieces.len())));
        }
        Ok(Self { pieces })
    }

    pub fn len(&self) -> usize {
        self.pieces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// Group token ids into words: `(text, first token index, last token index)`. Control
    /// pieces (`<…>`) are dropped; byte pieces (`<0xNN>`) are decoded as UTF-8.
    pub fn words(&self, ids: &[u32]) -> Vec<(String, usize, usize)> {
        let mut out: Vec<(String, usize, usize)> = Vec::new();
        let mut cur: Option<(Vec<u8>, usize, usize)> = None;
        let flush = |cur: &mut Option<(Vec<u8>, usize, usize)>, out: &mut Vec<(String, usize, usize)>| {
            if let Some((bytes, a, b)) = cur.take() {
                let text = String::from_utf8_lossy(&bytes).trim().to_string();
                if !text.is_empty() {
                    out.push((text, a, b));
                }
            }
        };
        for (i, &id) in ids.iter().enumerate() {
            let Some(p) = self.pieces.get(id as usize) else { continue };
            let byte = p.strip_prefix("<0x").and_then(|h| h.strip_suffix('>')).and_then(|h| u8::from_str_radix(h, 16).ok());
            if byte.is_none() && p.starts_with('<') && p.ends_with('>') {
                continue;
            }
            let starts = p.starts_with('▁');
            let text = p.trim_start_matches('▁');
            if starts || cur.is_none() {
                flush(&mut cur, &mut out);
                cur = Some((Vec::new(), i, i));
            }
            if let Some((bytes, _, b)) = cur.as_mut() {
                match byte {
                    Some(v) => bytes.push(v),
                    None => bytes.extend_from_slice(text.as_bytes()),
                }
                *b = i;
            }
        }
        flush(&mut cur, &mut out);
        out
    }
}

/// Where to cut `n` feature frames into windows of at most `max` frames: each cut is the middle of
/// the quietest 10-frame run in the last `search` frames before the limit (`energy` is per frame).
pub fn plan_windows(energy: &[f32], n: usize, max: usize, search: usize) -> Vec<(usize, usize)> {
    let max = max.max(16);
    let search = search.min(max / 2);
    let mut out = Vec::new();
    let mut a = 0;
    while a < n {
        if n - a <= max {
            out.push((a, n));
            break;
        }
        let limit = a + max;
        let lo = limit - search;
        let run = 10;
        let mut best = (f32::INFINITY, limit);
        for s in lo..limit.saturating_sub(run) {
            let e: f32 = (s..s + run).map(|f| energy.get(f).copied().unwrap_or(0.0)).sum();
            if e < best.0 {
                best = (e, s + run / 2);
            }
        }
        let cut = best.1.clamp(a + 1, limit);
        out.push((a, cut));
        a = cut;
    }
    out
}

/// Mean square of each 10 ms hop of `audio`.
fn hop_energy(audio: &[f32], frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|f| {
            let s = audio.get(f * HOP..((f + 1) * HOP).min(audio.len())).unwrap_or(&[]);
            if s.is_empty() { 0.0 } else { s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32 }
        })
        .collect()
}

pub struct Parakeet {
    id: String,
    languages: &'static [&'static str],
    device: Device,
    features: Features,
    encoder: encoder::Encoder,
    decoder: tdt::Decoder,
    vocab: Vocab,
}

impl Parakeet {
    /// Load a model directory (`model_weights.ckpt`, `tokenizer.vocab`).
    pub fn load(dir: &Path, id: &str, languages: &'static [&'static str]) -> Result<Self, SpeechError> {
        let device = nemo::device();
        let vocab_text = std::fs::read_to_string(dir.join("tokenizer.vocab")).map_err(|e| SpeechError::Model(format!("tokenizer.vocab: {e}")))?;
        let vocab = Vocab::parse(&vocab_text)?;
        let vb = nemo::load_checkpoint(&dir.join("model_weights.ckpt"), &device)?;
        let cfg = encoder::Config::PARAKEET_0_6B;
        let features = Features::from_checkpoint(&vb, cfg.n_mels)?;
        let encoder = encoder::Encoder::new(cfg, vb.pp("encoder")).map_err(merr)?;
        let decoder = tdt::Decoder::load(&vb, vocab.len(), cfg.d_model, &DURATIONS)?;
        Ok(Self { id: id.to_string(), languages, device, features, encoder, decoder, vocab })
    }

    /// `"metal"` or `"cpu"`.
    pub fn device_name(&self) -> &'static str {
        nemo::device_name(&self.device)
    }
}

impl Transcriber for Parakeet {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn languages(&self) -> Option<&'static [&'static str]> {
        Some(self.languages)
    }

    fn transcribe(&self, audio: &[f32], opts: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
        let language = opts.language.clone().filter(|l| !l.is_empty() && l != "auto");
        if let Some(l) = &language
            && !self.languages.contains(&l.as_str())
        {
            return Err(SpeechError::Language(l.clone()));
        }
        if !progress(0.0, "Analysing audio") {
            return Err(SpeechError::Cancelled);
        }
        let (frames, feats) = self.features.compute(audio);
        let energy = hop_energy(audio, frames);
        let per_s = 16_000 / HOP;
        let windows = plan_windows(&energy, frames, MAX_WINDOW_S * per_s, CUT_SEARCH_S * per_s);
        let cancelled = std::cell::Cell::new(false);
        let mut words: Vec<Word> = Vec::new();
        for (a, b) in windows {
            let frac = a as f32 / frames.max(1) as f32;
            if !progress(frac * 0.97, &format!("Transcribing {:.0}%", frac * 100.0)) {
                return Err(SpeechError::Cancelled);
            }
            let x = nemo::feature_window(&feats, self.features.n_mels, frames, a, b, &self.device)?;
            let mut check = || {
                let stop = !progress(frac * 0.97, &format!("Transcribing {:.0}%", frac * 100.0));
                if stop {
                    cancelled.set(true);
                }
                stop
            };
            let enc = match self.encoder.forward(&x, &mut check) {
                Ok(e) => e,
                Err(_) if cancelled.get() => return Err(SpeechError::Cancelled),
                Err(e) => return Err(merr(e)),
            };
            let proj = self.decoder.project(&enc)?;
            let tokens = self.decoder.decode(&proj, &|| cancelled.get())?;
            let ids: Vec<u32> = tokens.iter().map(|t| t.id).collect();
            let end_sample = (b * HOP).min(audio.len());
            for (text, i, j) in self.vocab.words(&ids) {
                let (Some(first), Some(last)) = (tokens.get(i), tokens.get(j)) else { continue };
                let s = a * HOP + first.frame * SUBSAMPLING * HOP;
                let e = a * HOP + (last.frame + last.duration.max(1)) * SUBSAMPLING * HOP;
                let (s, e) = (s.min(end_sample), e.min(end_sample));
                let mut w = Word::new(text, sample_tick(s as i64), sample_tick(e.max(s) as i64));
                w.confidence = tokens[i..=j].iter().map(|t| t.prob).fold(1.0, f32::min).clamp(0.0, 1.0);
                words.push(w);
            }
        }
        let mut t = Transcript {
            // without a language given, a multilingual model's transcript says English when it
            // knows English (the caller detects the language when it can)
            language: language.unwrap_or_else(|| self.languages.iter().find(|l| **l == "en").or(self.languages.first()).copied().unwrap_or("en").to_string()),
            source: self.id.clone(),
            speakers: Vec::new(),
            words,
            ..Default::default()
        };
        t.normalize();
        crate::vad::tighten_words(audio, &mut t.words);
        progress(1.0, "Done");
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_become_words() {
        let v = Vocab::parse("<unk>\t0\n▁hel\t-1\nlo\t-2\n▁world\t-3\n<|nospeech|>\t0\n<0xC3>\t0\n<0xA9>\t0\n▁caf\t0\n").unwrap();
        let w = v.words(&[1, 2, 3, 4, 7, 5, 6, 99]);
        let texts: Vec<&str> = w.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(texts, vec!["hello", "world", "café"]);
        assert_eq!((w[0].1, w[0].2), (0, 1));
        assert_eq!((w[2].1, w[2].2), (4, 6));
        assert!(v.words(&[]).is_empty());
        assert!(Vocab::parse("").is_err());
    }

    #[test]
    fn windows_cut_in_pauses_and_cover_everything() {
        // loud everywhere except a pause around frame 150
        let mut e = vec![1.0f32; 400];
        for v in &mut e[145..160] {
            *v = 0.0;
        }
        let w = plan_windows(&e, 400, 200, 80);
        assert_eq!(w.first().map(|x| x.0), Some(0));
        assert_eq!(w.last().map(|x| x.1), Some(400));
        assert!((145..160).contains(&w[0].1), "{w:?}");
        for pair in w.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
        assert!(w.iter().all(|(a, b)| b > a && b - a <= 200));
        assert!(plan_windows(&[], 0, 200, 80).is_empty());
        assert_eq!(plan_windows(&[], 5, 0, 0), vec![(0, 5)]);
    }
}
