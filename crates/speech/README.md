# filmcraft-speech

Speech-to-text for FilmCraft's Text panel ▸ Transcript (L2). User-facing behaviour, commands and
model handling: [docs/transcripts.md](../../docs/transcripts.md).

- `Transcriber` trait (mono 16 kHz in, word-timed `filmcraft_project::Transcript` out) and
  `FixedTranscriber` for tests and agents.
- `models`: the model catalogue (Parakeet, TitaNet, Whisper: pinned revisions, SHA-256, sizes,
  licences) and, with feature `download`, the verified downloader (ureq + rustls + RustCrypto),
  which fetches only the needed members of NVIDIA's `.nemo` archives as byte ranges. Weights are
  never bundled.
- `parakeet` (feature `parakeet`): Parakeet TDT 0.6B (FastConformer encoder, greedy TDT decoding)
  on candle, Metal on macOS; `nemo` holds the shared NeMo pieces (checkpoint reader, log-mel
  features). `titanet` and `speakers`: TitaNet-large voice embeddings and speaker labels
  clustered across every clip of a job, matched to the project's saved voices.
- `whisper` (feature `whisper`): Whisper inference on candle (pure Rust; Metal on macOS, else the CPU).
- `mel`, `vad`, `diarize`: log-mel front end, word-bound tightening, MFCC-clustering speaker labels
  (used when no speaker model is installed).
- `examples/speech_bench.rs`: word error rate, speaker error and speed on public speech.

Both features are off by default in this crate and are never enabled for wasm (`cargo xtask wasm`
checks the crate without them). The `filmcraft` and `filmcraft-cli` apps turn them on.

## References

Implemented from the published description of the model: A. Radford et al., "Robust Speech
Recognition via Large-Scale Weak Supervision" (OpenAI, 2022), the model cards and configuration
files published with the weights (`config.json`, `generation_config.json`, `tokenizer.json`), and
the candle tensor library's public API. Weights: OpenAI Whisper, MIT licence.

Parakeet and TitaNet: implemented from NVIDIA's published papers (D. Rekesh et al., "Fast
Conformer with Linearly Scalable Attention for Efficient Speech Recognition", 2023; H. Xu et al.,
"Efficient Sequence Transduction by Jointly Predicting Tokens and Durations", 2023; N. R. Koluguri
et al., "TitaNet: Neural Model for Speaker Representation with 1D Depth-wise Separable
Convolutions and Global Context", 2022), the model cards and the `model_config.yaml` files
published with the weights. Weights: NVIDIA, CC-BY-4.0.
