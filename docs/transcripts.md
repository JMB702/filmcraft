# Transcripts and text-based editing

FilmCraft's Text panel ▸ **Transcript** tab shows the dialogue of the open sequence as text. Select
words to mark In/Out, then extract or lift them; remove filler words and long pauses in one step;
turn the transcript into captions. Every action is an engine command (`transcript.*`), so the CLI,
the control channel and MCP agents can do the same.

## Model

- A **transcript** belongs to a media item (`Project::transcripts`, saved in the `.fcproj` since
  schema v9). It lists **words** with media-time bounds (`Tick`s), an optional speaker index and a
  confidence, plus the speaker names and the language. Because the times are media time, the
  transcript stays valid however the clip is trimmed, moved, sped up or reused.
- The **sequence transcript** is derived, never stored (`filmcraft_edit::transcript::sequence_words`):
  audio tracks are read top first; a word is heard through the first enabled clip whose range
  covers the word's midpoint (duplicates of the same dialogue on lower tracks read once); disabled,
  reversed and frame-hold clips contribute nothing.
- Speaker names come from the clip transcripts, so renaming "Speaker 1" in every transcript renames
  it across the sequence.
- **Voices** (`Project::voices`, saved in the `.fcproj`): naming a speaker saves its voice (the
  average speaker embedding of its words). Later transcriptions give a matching voice that name
  in every clip, so a host or guest is labelled once per project, not once per clip.

## Transcribing

**Transcribe…** opens one dialog from the Text panel (the Transcribe button, and the microphone
button in the Transcript toolbar), the timeline (a clip's right-click menu; an audio track
header's **Transcribe Track…**), the Project panel (an item's right-click menu) and
**Sequence ▸ Transcribe Sequence…** / **Sequence ▸ Transcript ▸ Transcribe…**. It offers:

- **What to transcribe:** the selected clips, the selected Project items, one audio track (all of
  it or only inside In/Out), or the whole sequence (all of it or inside In/Out). Whatever the
  scope, each media item is transcribed once, however many clips, nested sequences or multicam
  angles show it. A timeline scope transcribes only the parts of the media its clips use (plus
  half a second either side) and merges them into the media's transcript. A multi-channel file is
  heard through the channels and audio stream the clips play: a recorder's lav channel, not the
  whole poly file.
- **Engine and model:** Parakeet (the default) or Whisper; the model; the language, or
  **Detect automatically**.
- **Detect speakers** (on by default) and the **number of speakers** (Auto, or 1 to 10).
- **Replace existing transcripts** (on by default; off skips media that already has one).

A model that is not downloaded yet shows its size, and the button reads **Download … and
Transcribe**: the download runs in the background (Progress panel, with Cancel) and the
transcription starts when it is in. Transcription is a background job too: the editor stays
responsive, the status bar and Progress panel show it with Cancel, and the finished result is
applied as one undo step ("Transcribe"). One transcription runs at a time.

The Transcript tab shows each paragraph's speaker in its own colour (the same colour for the same
name everywhere). Right-click a speaker's name to rename it (the project remembers the voice),
merge it into another speaker, or give that paragraph to another speaker.

## Commands

Everything the dialog does is a command, with the same options, so the CLI (`--live`), the control
channel and MCP agents can do it.

| Command | What it does |
|---|---|
| `transcript.generate` | Start a transcription job and reply with its id at once. Scope: `items`, `clips`, `track` (`"A2"`, an index from 0 or a track id), `sequence: true`, `range` (`all` \| `inOut`, for `track` and `sequence`); with none, the timeline selection, else the Project selection, else the whole sequence. `engine` (`parakeet` \| `whisper`), `model`, `language` (`en`, … or `auto`), `diarize` (default true), `speakers` (`auto` or 1–32), `maxSpeakers`, `replace` (default true), `wait`. Reply: `{job, items: [{item, name}], scope, engine, audioSeconds, skipped}`. With `wait: true` it replies when the transcript is in, adding the per-item `result`. |
| `transcript.jobs` | The transcription jobs: progress, status, items, engine and, once finished, the report (per item: words, language, engine, speakers, seconds of audio and of compute). |
| `transcript.cancel` | Stop a running transcription (`job`, or all). Nothing in the project changes. |
| `transcript.set` | Store a transcript you bring (JSON: `language`, `speakers`, `words` with `text`/`start`/`end`/`speaker`); it is sorted and made well formed. |
| `transcript.delete` | Remove transcripts. |
| `transcript.inspect` | The sequence transcript: words (index, text, sequence times, speaker, clip), paragraphs, speakers, the word at the playhead. |
| `transcript.search` | Word-index ranges matching a phrase (case and punctuation ignored; the last word may be a prefix). |
| `transcript.select` | Mark In/Out around words `from..=to` (frame-snapped outward) and move the playhead there. |
| `transcript.extract` / `transcript.lift` | Extract (ripple) or lift the words' frames on the targeted tracks. |
| `transcript.speakers` | Every speaker name with its seconds of speech and whether the project knows its voice. |
| `transcript.renameSpeaker` | Rename a speaker by name (every transcript) or by index in one `item`; `remember` (default true) saves the voice under the new name. |
| `transcript.mergeSpeakers` | Merge `from` (a name or a list) into `into`. |
| `transcript.assignSpeaker` | Give sequence words `from..=to` to `speaker` (a known name or a new one). |
| `transcript.voices` / `transcript.forgetVoice` | The project's saved voices / forget one. |
| `transcript.removeFillers` | Ripple-delete filler words (`fillers`, default um/uh/erm/…; phrases such as "you know" allowed). |
| `transcript.removePauses` | Ripple-delete pauses longer than `minSeconds`, keeping `keepSeconds` of air on both sides. |
| `transcript.createCaptions` | Lay the words out as captions on a new caption track (`maxChars`, `lines`, `minSeconds`, `maxSeconds`, `gapFrames`). |
| `transcript.models` | The engines in this build, the defaults, and every model with its engine, languages, size, licence, source, whether it is installed and the bytes still to download. |
| `transcript.downloadModel` | Download `model` or `models` as a background job (`wait` to block); Cancel keeps the part file and the next download resumes it. |

**Waiting.** `transcript.generate` and `transcript.downloadModel` reply with a job id. Poll
`transcript.jobs` (or `jobs.list`) until the job is finished, pass `wait: true`, or use MCP: a
`wait: true` call from an MCP client runs as a job with progress notifications and honours the
client's cancellation. A job is marked finished only after its result is in the project, so the
transcript can be read as soon as the job says so.

```bash
filmcraft-cli --live exec transcript.generate '{"track":"A1","range":"inOut","speakers":2}'
filmcraft-cli --live exec transcript.jobs '{}'
filmcraft-cli --live exec transcript.renameSpeaker '{"speaker":"Speaker 1","name":"Ada"}'
```

## Speech recognition

Recognition goes through the `Transcriber` trait (`crates/speech`), on
[candle](https://github.com/huggingface/candle): Metal on macOS, the CPU elsewhere.

- **Parakeet** (default, `filmcraft_speech::parakeet`): NVIDIA's Parakeet TDT 0.6B, a
  FastConformer encoder with greedy token-and-duration decoding. Long audio is cut into windows of
  at most 90 s, each cut placed in the quietest 100 ms near the window's end, so a cut lands in a
  pause. Word times come from the decoder's frames (80 ms), then are tightened against the audio.
- **Whisper** (`filmcraft_speech::whisper`): OpenAI's Whisper, with timestamp decoding, language
  detection and word times from cross-attention alignment, tightened against the audio
  (`filmcraft_speech::vad`). It is the second engine and the fallback: a clip in a language the
  Parakeet model does not know (given, or detected by Whisper when the language is `auto`), and a
  clip Parakeet fails on, are transcribed by Whisper when a Whisper model is installed. Every
  transcript records the model that made it (`source`).
- **Speakers** (`filmcraft_speech::speakers`, `filmcraft_speech::titanet`): NVIDIA's TitaNet-large
  turns each stretch of speech (split at the recogniser's word pauses, so no separate voice-activity
  model is needed) into a voice embedding. The embeddings of every clip in the job are clustered
  together (average linkage on cosine similarity), so one voice gets one label in every clip;
  every word gets a speaker. With a number of speakers, the clustering stops at that many.
  Clusters that match a saved voice of the project, or a speaker of an earlier transcript, take its
  name; the rest are "Speaker N", numbered after the project's highest.

Features (never built for the web):

- `parakeet` and `whisper` on `filmcraft-speech` and `filmcraft-engine`; `speech-download` on the
  engine (HTTPS downloads with rustls + RustCrypto and the operating system's certificate
  verifier).
- The `filmcraft` and `filmcraft-cli` apps have `transcription` (Parakeet, Whisper and downloads)
  **on by default**, so release builds transcribe.

Without either engine, and with no recogniser installed, `transcript.generate` and Transcribe
Sequence are disabled, with "speech-to-text is not available in this build" as the reason
(`describe`, `command_list {"enabled_only": true}` and the menus show it); with Automatically
transcribe clips on with the all-imported scope, `file.import` reports the same reason as a
`transcription: …` entry in its `errors`.

The default sequence-only scope of automatic transcription waits until enabled audio is placed in
a sequence (including Source Insert/Overwrite, timeline placement, paste and sequence expansion).
It transcribes the underlying media once, preserving any existing transcript, and includes the new
transcript in the placement's undo step. Undo/Redo and opening or recovering a project do not run
recognition. If recognition fails, the clips stay placed and a warning appears in the Events panel
and a toast.

Without `speech-download`, `transcript.downloadModel` is disabled the same way. Transcripts can
still be imported with `transcript.set` and edited with every other command. Hosts and tests can
install any recogniser in `Session::transcriber` (and `speaker_embedder`), which enables
transcription in any build.

### Models

Weights are **never** bundled or committed. They are downloaded on request into
`<data dir>/models/<id>/` (see `filmcraft_engine::autosave::default_data_dir`), each file pinned to
a revision of its Hugging Face repository and checked against its SHA-256. NVIDIA's `.nemo` files
are tar archives: only the members needed (weights and tokenizer) are fetched, as HTTP byte
ranges. A verified file gets a small `.<name>.verified` stamp (size, modification time, hash), so
it is not hashed again until it changes.

| Id | Engine | Languages | Download | Licence |
|---|---|---|---|---|
| `parakeet-tdt-0.6b-v2` (default) | Parakeet | English | 2.47 GB | CC-BY-4.0 (NVIDIA) |
| `parakeet-tdt-0.6b-v3` | Parakeet | 25 European languages | 2.51 GB | CC-BY-4.0 (NVIDIA) |
| `titanet-large` (speakers) | Parakeet | any | 102 MB | CC-BY-4.0 (NVIDIA) |
| `whisper-tiny` | Whisper | multilingual | | MIT (OpenAI); HF conversion Apache-2.0 |
| `whisper-base` (default Whisper) | Whisper | multilingual | | MIT (OpenAI); HF conversion Apache-2.0 |
| `whisper-small` | Whisper | multilingual | | MIT (OpenAI); HF conversion Apache-2.0 |

Settings ▸ Media Analysis & Transcription picks the engine, the Parakeet model and the Whisper
model (`mediaAnalysis.speechEngine`, `.parakeetModel`, `.whisperModel`).

### Accuracy and speed

Measured on 2026-11-21 on an Apple M2 Max (Metal), release build, with
`cargo run --release -p filmcraft-speech --features parakeet,whisper --example speech_bench`
(`asr` and `diar` modes), on public speech only:

| Model | Device | Word error rate | Compute per minute of audio |
|---|---|---|---|
| `parakeet-tdt-0.6b-v2` (default) | Metal | 1.60 % | 1.15 s |
| `parakeet-tdt-0.6b-v3` | Metal | 1.71 % | 1.20 s |
| `whisper-base` | Metal | 4.70 % | 2.01 s |

The same 120 LibriSpeech test-clean utterances (16.9 minutes, 2683 words) for each.

- Word error rate: lower-cased, punctuation removed, against the LibriSpeech references
  (read speech, test-clean). Compute: wall time of recognition per minute of audio, after the
  model is loaded (loading Parakeet takes about 8 s).
- **Speakers:** 12 synthetic conversations of 2, 3 and 4 speakers, built by alternating
  LibriSpeech utterances of different readers (2763 words). Words given the wrong speaker: 1.2 %;
  speaker count wrong in 1 of 12; embedding and clustering about 0.4 s per minute of audio. The
  embeddings alone separate same-speaker from different-speaker pairs with an equal error rate of
  1.7 %.

### Testing

Unit and engine tests use fake recognisers and embedders (`FixedTranscriber`, the fakes in
`crates/engine/src/transcription_tests.rs`), so CI needs no model. With the models downloaded,
`speech_bench` reproduces the numbers above; `crates/speech/tests/whisper_model.rs` (feature
`whisper`) runs only when weights are in `target/models/<id>/` (or `$FILMCRAFT_MODELS_DIR`) and
samples are in `target/fixtures/speech/`, and otherwise prints SKIPPED.

## Limits

- Automatic transcription on placement still runs inside the placing command (it shares that
  command's undo step), so a long clip makes the placement wait.
- Track items that refer to a subclip are looked up by the subclip's id, so a transcript made for
  the parent media is not shown through subclip clips yet.
- Word bounds are only moved across pauses. A start placed late inside continuous speech (no pause
  of 0.2 s between the two words) stays where it is.
- Overlapping speech is given to one speaker.
