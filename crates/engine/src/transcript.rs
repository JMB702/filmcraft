//! Text-based editing commands (`transcript.*`): the Text panel ▸ Transcript tab.
//!
//! Transcripts belong to media items (`Project::transcripts`, media time); the sequence transcript
//! is derived from them ([`filmcraft_edit::transcript::sequence_words`]). Words of the sequence
//! transcript are addressed by index (`from`, `to`, inclusive), as `transcript.inspect` lists them.
//!
//! Speech recognition runs as a background job ([`crate::transcription`]): Parakeet by default,
//! Whisper for other languages, speakers labelled across every clip of the job. Hosts and tests
//! install their own models in [`Session::transcriber`] (and `transcriber_fallback`,
//! `speaker_embedder`). Without the engine features `parakeet` / `whisper`, `transcript.generate`
//! fails with a clear error, and agents can still bring their own transcript with
//! `transcript.set`.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};

use filmcraft_edit as edit;
use filmcraft_edit::transcript::{self as tx, CaptionRules, SeqWord};
use filmcraft_project::{CaptionFormat, CaptionTrack, ClipId, ItemId, Project, Speaker, TrackId, Transcript, Voice};
use filmcraft_speech::SpeechError;
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};

use crate::commands::{CommandSpec, always, bad, bool_p, f64_p, has_seq, str_p, u64_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal, named: crate::commands::NamedKeys::NONE }
}

/// Where downloaded speech models live (`<data dir>/models`).
pub fn models_dir() -> Option<std::path::PathBuf> {
    crate::autosave::default_data_dir().map(|d| d.join("models"))
}

/// Whether this build can transcribe with Whisper (feature `whisper`).
pub fn speech_available() -> bool {
    filmcraft_speech::available()
}

/// The words of the active sequence's transcript.
pub fn sequence_words(s: &Session) -> Vec<SeqWord> {
    match s.active_sequence() {
        Some(q) => tx::sequence_words(q, &s.project.transcripts),
        None => Vec::new(),
    }
}

fn has_transcript(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    if sequence_words(s).is_empty() { Err("the sequence has no transcript (Transcribe first)".into()) } else { Ok(()) }
}

fn has_transcripts(s: &Session) -> std::result::Result<(), String> {
    if s.project.transcripts.is_empty() { Err("there are no transcripts".into()) } else { Ok(()) }
}

/// Why speech-to-text can't run in this build (no installed transcriber, built without `whisper`).
pub(crate) const NO_SPEECH: &str =
    "speech-to-text is not available in this build (built without the `parakeet` and `whisper` features); import a transcript with transcript.set instead";

/// `transcript.generate` can run: a host installed a transcriber or the build has speech-to-text
/// (#97: it reported enabled and then always failed).
pub(crate) fn can_transcribe(s: &Session) -> std::result::Result<(), String> {
    if s.transcriber.is_some() || speech_available() { Ok(()) } else { Err(NO_SPEECH.into()) }
}

/// `transcript.downloadModel` can run: built with `speech-download` (#98).
fn can_download(_: &Session) -> std::result::Result<(), String> {
    if cfg!(feature = "speech-download") {
        Ok(())
    } else {
        Err("model downloads are not available in this build (built without the `speech-download` feature)".into())
    }
}

/// The media item behind a project item (subclips resolve to their parent).
fn media_item(s: &Session, item: ItemId) -> Option<ItemId> {
    s.project.resolve_media(item).map(|(root, _, _)| root)
}

fn ids_p(p: &Value, k: &str) -> Option<Vec<ItemId>> {
    p.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(ItemId).collect())
}

fn speech_err(e: SpeechError) -> EngineError {
    EngineError::Other(e.to_string())
}

/// Enabled audio placements, including media inside nested sequences, without recursive walks.
fn audio_placements(project: &Project) -> impl Iterator<Item = (ItemId, ClipId, ItemId)> + '_ {
    project.sequences().filter_map(|item| item.as_sequence().map(|q| (item.id, q))).flat_map(|(sequence, q)| {
        q.audio_tracks.iter().flat_map(|track| &track.items).filter(|clip| clip.enabled).map(move |clip| (sequence, clip.id, clip.item))
    })
}

/// Transcribe newly placed audio once per underlying media item. This runs after the outermost
/// editing command so every placement route shares the same scope, including paste and expansion.
pub(crate) fn auto_transcribe_sequence_clips(s: &mut Session, before: &Project) -> Result<()> {
    let old: HashSet<_> = audio_placements(before).collect();
    let items: BTreeSet<_> = audio_placements(&s.project)
        .filter(|placement| !old.contains(placement))
        .filter_map(|(_, _, item)| s.project.resolve_media(item))
        .filter(|(root, media, _)| media.info.has_audio() && !s.project.transcripts.contains_key(root))
        .map(|(root, _, _)| root.0)
        .collect();
    if items.is_empty() {
        return Ok(());
    }
    // Fold the transcript into the placement's undo step. Preserve the whole history, including
    // its bounded oldest entry and merge key; generate's own edit must not consume another slot.
    let history = s.history.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::transcription::generate(s, &json!({"items": items, "wait": true}))))
        .unwrap_or_else(|_| Err(EngineError::Other("internal error during automatic transcription (see the crash log); the clips were placed".into())));
    s.history = history;
    // a recogniser that panicked on its worker: say it happened while placing clips
    let result = result.map_err(|e| match e {
        EngineError::Other(m) if m.starts_with("internal error") => {
            EngineError::Other("internal error during automatic transcription (see the crash log); the clips were placed".into())
        }
        e => e,
    });
    result.map(|_| ())
}

fn set(s: &mut Session, p: &Value) -> Result<Value> {
    let item = u64_p(p, "item").map(ItemId).ok_or_else(|| bad("transcript.set", "`item` is required"))?;
    let item = media_item(s, item).ok_or_else(|| bad("transcript.set", "no such media item"))?;
    let v = p.get("transcript").cloned().ok_or_else(|| bad("transcript.set", "`transcript` is required"))?;
    let mut t: Transcript = serde_json::from_value(v).map_err(|e| bad("transcript.set", e.to_string()))?;
    if t.source.is_empty() {
        t.source = "imported".into();
    }
    t.normalize();
    t.check().map_err(|e| bad("transcript.set", e))?;
    let n = t.words.len();
    s.edit("Set Transcript", move |pr, _| {
        pr.transcripts.insert(item, Arc::new(t));
        Ok(())
    })?;
    Ok(json!({"item": item.0, "words": n}))
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let items: Vec<ItemId> = match ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![ItemId(i)])) {
        Some(v) => v.into_iter().filter_map(|i| media_item(s, i)).collect(),
        None => s.project.transcripts.keys().copied().collect(),
    };
    let n = items.iter().filter(|i| s.project.transcripts.contains_key(i)).count();
    if n == 0 {
        return Err(EngineError::Other("no transcript to delete".into()));
    }
    s.edit("Delete Transcript", move |pr, _| {
        for i in items {
            pr.transcripts.remove(&i);
        }
        Ok(())
    })?;
    Ok(json!({"deleted": n}))
}

fn word_json(i: usize, w: &SeqWord) -> Value {
    json!({"i": i, "text": w.text, "start": w.start.0, "end": w.end.0, "speaker": w.speaker, "clip": w.clip.0, "item": w.item.0, "confidence": w.confidence})
}

fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let gap = Tick::from_seconds_f64(f64_p(p, "paragraphGapSeconds").unwrap_or(1.5));
    let paras: Vec<Value> = tx::paragraphs(&words, gap)
        .into_iter()
        .map(|r| {
            let text = words[r.clone()].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            json!({"from": r.start, "to": r.end - 1, "speaker": words[r.start].speaker, "start": words[r.start].start.0, "end": words[r.end - 1].end.0, "text": text})
        })
        .collect();
    let speakers: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for w in &words {
            if let Some(n) = &w.speaker
                && !v.contains(n)
            {
                v.push(n.clone());
            }
        }
        v
    };
    let current = tx::word_at(&words, s.playhead());
    Ok(json!({
        "words": words.iter().enumerate().map(|(i, w)| word_json(i, w)).collect::<Vec<_>>(),
        "paragraphs": paras,
        "speakers": speakers,
        "current": current,
        "items": s.project.transcripts.iter().map(|(i, t)| json!({"item": i.0, "words": t.words.len(), "language": t.language, "source": t.source, "speakers": t.speakers.iter().map(|k| &k.name).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    }))
}

fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let q = str_p(p, "query").ok_or_else(|| bad("transcript.search", "`query` is required"))?;
    let words = sequence_words(s);
    let hits: Vec<Value> = tx::search(&words, q)
        .into_iter()
        .map(|r| json!({"from": r.start, "to": r.end - 1, "start": words[r.start].start.0, "end": words[r.end - 1].end.0}))
        .collect();
    Ok(json!({"matches": hits}))
}

/// Timeline range of the words `from..=to` (frame-snapped outward).
fn range_p(s: &Session, p: &Value, cmd: &str) -> Result<TimeRange> {
    let words = sequence_words(s);
    let from = u64_p(p, "from").ok_or_else(|| bad(cmd, "`from` (word index) is required"))? as usize;
    let to = u64_p(p, "to").map(|n| n as usize).unwrap_or(from);
    tx::word_range(&words, from, to, s.sequence_rate()).ok_or_else(|| bad(cmd, format!("word index out of range (the transcript has {} words)", words.len())))
}

fn range_json(r: TimeRange) -> Value {
    json!({"start": r.start.0, "end": r.end().0})
}

fn select(s: &mut Session, p: &Value) -> Result<Value> {
    let r = range_p(s, p, "transcript.select")?;
    let fd = s.sequence_rate().frame_duration();
    s.edit_sequence("Mark Transcript Selection", |q, _, _| {
        q.mark_in = Some(r.start);
        q.mark_out = Some(r.end() - fd);
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

fn extract_or_lift(s: &mut Session, p: &Value, extract: bool) -> Result<Value> {
    let cmd = if extract { "transcript.extract" } else { "transcript.lift" };
    let r = range_p(s, p, cmd)?;
    let tg = s.targeting().targeted;
    s.edit_sequence(if extract { "Extract Text" } else { "Lift Text" }, |q, ctx, _| {
        if extract {
            edit::extract(q, &tg, r, ctx);
        } else {
            edit::lift(q, &tg, r, ctx);
        }
        q.mark_in = None;
        q.mark_out = None;
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

/// Merge speakers of `t` that share a name (renaming one to another's name, merging two): the
/// first keeps its place, words of the others move to it.
fn dedupe_speakers(t: &mut Transcript) {
    let mut keep: Vec<Speaker> = Vec::new();
    let mut map: Vec<u32> = Vec::with_capacity(t.speakers.len());
    for sp in std::mem::take(&mut t.speakers) {
        match keep.iter().position(|k| k.name == sp.name) {
            Some(i) => {
                if keep[i].embedding.is_empty() {
                    keep[i].embedding = sp.embedding;
                }
                map.push(i as u32);
            }
            None => {
                keep.push(sp);
                map.push((keep.len() - 1) as u32);
            }
        }
    }
    for w in &mut t.words {
        w.speaker = w.speaker.and_then(|i| map.get(i as usize).copied());
    }
    t.speakers = keep;
}

/// "Speaker 3": a name the recogniser gave, not a person's.
fn is_generic(name: &str) -> bool {
    name.strip_prefix("Speaker ").is_some_and(|n| n.trim().parse::<u32>().is_ok())
}

fn name_p(p: &Value, k: &str, cmd: &str) -> Result<String> {
    let n = str_p(p, k).map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(cmd, format!("`{k}` (a speaker name) is required")))?;
    if n.len() > filmcraft_project::transcript::MAX_SPEAKER_NAME_BYTES {
        return Err(bad(cmd, "the name is too long"));
    }
    Ok(n.to_string())
}

/// The voice of the speaker called `name`: its embedding, model and seconds of speech, averaged
/// over every transcript that has it (same model only).
fn voice_of(pr: &Project, name: &str) -> Option<Voice> {
    let mut sum: Vec<f32> = Vec::new();
    let mut model = String::new();
    let mut seconds = 0f64;
    for t in pr.transcripts.values() {
        let Some(i) = t.speakers.iter().position(|sp| sp.name == name) else { continue };
        let sp = &t.speakers[i];
        if sp.embedding.is_empty() || t.voice_model.is_empty() || (!model.is_empty() && model != t.voice_model) {
            continue;
        }
        let secs: f64 = t.words.iter().filter(|w| w.speaker == Some(i as u32)).map(|w| (w.end - w.start).0 as f64 / TICKS_PER_SECOND as f64).sum();
        let wgt = secs.max(0.1) as f32;
        if sum.is_empty() {
            sum = vec![0.0; sp.embedding.len()];
            model = t.voice_model.clone();
        }
        if sum.len() == sp.embedding.len() {
            sum.iter_mut().zip(&sp.embedding).for_each(|(a, b)| *a += b * wgt);
            seconds += secs;
        }
    }
    if sum.is_empty() {
        return None;
    }
    filmcraft_speech::speakers::normalize(&mut sum);
    Some(Voice { name: name.to_string(), model, embedding: sum, seconds })
}

/// Save or refresh the project voice called `name` from its transcripts.
fn remember_voice(pr: &mut Project, name: &str) {
    if is_generic(name) {
        return;
    }
    if let Some(v) = voice_of(pr, name) {
        match pr.voices.iter_mut().find(|x| x.name == name) {
            Some(x) => *x = v,
            None => pr.voices.push(v),
        }
    }
}

fn rename_speaker(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "transcript.renameSpeaker";
    let name = name_p(p, "name", CMD)?;
    let item = u64_p(p, "item").map(ItemId).and_then(|i| media_item(s, i));
    // `speaker`: the current name (every transcript), or an index (needs `item`)
    let (old_name, index) = match p.get("speaker") {
        Some(Value::String(n)) => (Some(n.clone()), None),
        Some(v) if v.is_u64() => (None, v.as_u64().map(|n| n as usize)),
        _ => return Err(bad(CMD, "`speaker` (name, or index with `item`) is required")),
    };
    if index.is_some() && item.is_none() {
        return Err(bad(CMD, "a speaker index needs `item`"));
    }
    let remember = bool_p(p, "remember").unwrap_or(true);
    let mut n = 0;
    let mut renamed_from: Vec<String> = Vec::new();
    let mut next = s.project.transcripts.clone();
    for (i, t) in next.iter_mut() {
        if item.is_some_and(|x| x != *i) {
            continue;
        }
        let tt = Arc::make_mut(t);
        for (k, sp) in tt.speakers.iter_mut().enumerate() {
            if old_name.as_ref().is_some_and(|o| *o == sp.name) || index == Some(k) {
                if !renamed_from.contains(&sp.name) {
                    renamed_from.push(sp.name.clone());
                }
                sp.name = name.clone();
                n += 1;
            }
        }
        dedupe_speakers(tt);
    }
    if n == 0 {
        return Err(EngineError::Other("no such speaker".into()));
    }
    let whole = item.is_none();
    let new_name = name.clone();
    s.edit("Rename Speaker", move |pr, _| {
        pr.transcripts = next;
        // a saved voice follows its speaker's new name (renaming everywhere only)
        if whole {
            for old in &renamed_from {
                pr.voices.retain(|v| v.name != *old || *old == name);
            }
        }
        if remember {
            remember_voice(pr, &name);
        }
        Ok(())
    })?;
    let saved = s.project.voices.iter().any(|v| v.name == new_name);
    Ok(json!({"renamed": n, "voiceSaved": saved}))
}

fn merge_speakers(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "transcript.mergeSpeakers";
    let into = name_p(p, "into", CMD)?;
    let from: Vec<String> = match p.get("from") {
        Some(Value::String(n)) => vec![n.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => return Err(bad(CMD, "`from` (a speaker name or a list of names) is required")),
    };
    let from: Vec<String> = from.into_iter().filter(|f| *f != into).collect();
    if from.is_empty() {
        return Err(bad(CMD, "`from` names no other speaker than `into`"));
    }
    let mut next = s.project.transcripts.clone();
    let mut words = 0usize;
    let mut found = false;
    for t in next.values_mut() {
        if !t.speakers.iter().any(|sp| from.contains(&sp.name)) {
            continue;
        }
        found = true;
        let tt = Arc::make_mut(t);
        let moved: Vec<u32> = tt.speakers.iter().enumerate().filter(|(_, sp)| from.contains(&sp.name)).map(|(i, _)| i as u32).collect();
        words += tt.words.iter().filter(|w| w.speaker.is_some_and(|x| moved.contains(&x))).count();
        for sp in tt.speakers.iter_mut().filter(|sp| from.contains(&sp.name)) {
            sp.name = into.clone();
        }
        dedupe_speakers(tt);
    }
    if !found {
        return Err(EngineError::Other(format!("no speaker called {}", from.join(" or "))));
    }
    s.edit("Merge Speakers", move |pr, _| {
        pr.transcripts = next;
        pr.voices.retain(|v| !from.contains(&v.name));
        if pr.voices.iter().any(|v| v.name == into) || !is_generic(&into) {
            remember_voice(pr, &into);
        }
        Ok(())
    })?;
    Ok(json!({"merged": words}))
}

fn assign_speaker(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "transcript.assignSpeaker";
    let name = name_p(p, "speaker", CMD)?;
    let words = sequence_words(s);
    let from = u64_p(p, "from").ok_or_else(|| bad(CMD, "`from` (word index) is required"))? as usize;
    let to = u64_p(p, "to").map(|n| n as usize).unwrap_or(from);
    if from > to || to >= words.len() {
        return Err(bad(CMD, format!("word index out of range (the transcript has {} words)", words.len())));
    }
    // a voiceprint for the name, when the project knows its voice
    let known: Option<(String, Vec<f32>)> = s.project.voices.iter().find(|v| v.name == name).map(|v| (v.model.clone(), v.embedding.clone()));
    let mut next = s.project.transcripts.clone();
    let mut n = 0;
    for w in &words[from..=to] {
        let Some(t) = next.get_mut(&w.item) else { continue };
        let tt = Arc::make_mut(t);
        let idx = match tt.speakers.iter().position(|sp| sp.name == name) {
            Some(i) => i,
            None => {
                let mut sp = Speaker::new(name.clone());
                if let Some((model, e)) = &known
                    && (tt.voice_model.is_empty() || tt.voice_model == *model)
                {
                    sp.embedding = e.clone();
                }
                tt.speakers.push(sp);
                tt.speakers.len() - 1
            }
        };
        if let Some(word) = tt.words.get_mut(w.index) {
            word.speaker = Some(idx as u32);
            n += 1;
        }
    }
    s.edit("Assign Speaker", move |pr, _| {
        pr.transcripts = next;
        Ok(())
    })?;
    Ok(json!({"assigned": n}))
}

fn speakers_list(s: &mut Session, _: &Value) -> Result<Value> {
    let seconds = crate::transcription::speaker_seconds(&s.project);
    let mut list: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    // sequence order first (the Transcript tab's order), then the rest
    let order: Vec<String> = sequence_words(s)
        .into_iter()
        .filter_map(|w| w.speaker)
        .chain(s.project.transcripts.values().flat_map(|t| t.speakers.iter().map(|x| x.name.clone())))
        .collect();
    for name in order {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        let items: Vec<u64> = s.project.transcripts.iter().filter(|(_, t)| t.speakers.iter().any(|x| x.name == name)).map(|(i, _)| i.0).collect();
        let words: usize = s.project.transcripts.values().map(|t| t.words.iter().filter(|w| t.speaker_name(w).as_deref() == Some(name.as_str())).count()).sum();
        let voiceprint = s.project.transcripts.values().any(|t| t.speakers.iter().any(|x| x.name == name && !x.embedding.is_empty()));
        list.push(json!({
            "name": name,
            "words": words,
            "seconds": seconds.get(&name).copied().unwrap_or(0.0),
            "items": items,
            "voiceprint": voiceprint,
            "saved": s.project.voices.iter().any(|v| v.name == name),
        }));
    }
    Ok(Value::Array(list))
}

fn voices(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(Value::Array(s.project.voices.iter().map(|v| json!({"name": v.name, "model": v.model, "seconds": v.seconds})).collect()))
}

fn forget_voice(s: &mut Session, p: &Value) -> Result<Value> {
    let name = name_p(p, "name", "transcript.forgetVoice")?;
    if !s.project.voices.iter().any(|v| v.name == name) {
        return Err(EngineError::Other(format!("the project has no saved voice called {name}")));
    }
    s.edit("Forget Voice", move |pr, _| {
        pr.voices.retain(|v| v.name != name);
        Ok(())
    })?;
    Ok(Value::Null)
}

fn remove_ranges(s: &mut Session, label: &str, ranges: Vec<TimeRange>) -> Result<Value> {
    let n = ranges.len();
    if n == 0 {
        return Ok(json!({"removed": 0, "ticks": 0}));
    }
    let total = s.edit_sequence(label, |q, ctx, _| Ok(tx::ripple_delete_ranges(q, ranges, ctx)))?;
    Ok(json!({"removed": n, "ticks": total.0, "seconds": total.0 as f64 / TICKS_PER_SECOND as f64}))
}

fn remove_pauses(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let min = Tick::from_seconds_f64(f64_p(p, "minSeconds").unwrap_or(1.0));
    let keep = Tick::from_seconds_f64(f64_p(p, "keepSeconds").unwrap_or(0.15));
    let ranges = tx::find_pauses(&words, min, keep, s.sequence_rate());
    remove_ranges(s, "Remove Pauses", ranges)
}

fn remove_fillers(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let fillers: Vec<String> = match p.get("fillers").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => tx::DEFAULT_FILLERS.iter().map(|f| f.to_string()).collect(),
    };
    let hits = tx::find_fillers(&words, &fillers);
    let ranges = tx::filler_ranges(&words, &hits, s.sequence_rate());
    remove_ranges(s, "Remove Filler Words", ranges)
}

fn create_captions(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let d = CaptionRules::default();
    let rules = CaptionRules {
        max_chars: u64_p(p, "maxChars").map(|n| n as usize).unwrap_or(d.max_chars),
        lines: u64_p(p, "lines").map(|n| n as usize).unwrap_or(d.lines),
        min_duration: f64_p(p, "minSeconds").map(Tick::from_seconds_f64).unwrap_or(d.min_duration),
        max_duration: f64_p(p, "maxSeconds").map(Tick::from_seconds_f64).unwrap_or(d.max_duration),
        gap_frames: p.get("gapFrames").and_then(Value::as_i64).unwrap_or(d.gap_frames),
        break_pause: d.break_pause,
    };
    let blocks = tx::caption_blocks(&words, &rules, s.sequence_rate());
    let format = str_p(p, "format").and_then(CaptionFormat::from_name).unwrap_or_default();
    let name = str_p(p, "name").unwrap_or("Transcript").to_string();
    let n = blocks.len();
    let tid = s.edit_sequence("Create Captions", |q, ctx, st| {
        let tid = TrackId(ctx.alloc());
        let mut t = CaptionTrack::new(tid, name, format);
        t.captions = tx::blocks_to_captions(&blocks, ctx);
        q.caption_tracks.insert(0, t);
        st.caption_selection.clear();
        Ok(tid)
    })?;
    Ok(json!({"track": tid.0, "captions": n}))
}

fn models(s: &mut Session, _: &Value) -> Result<Value> {
    use filmcraft_speech::models::{self as m, Engine};
    let dir = models_dir();
    let ma = &s.prefs.media_analysis;
    Ok(json!({
        "available": filmcraft_speech::available(),
        "engines": {
            "parakeet": filmcraft_speech::engine_available(Engine::Parakeet),
            "whisper": filmcraft_speech::engine_available(Engine::Whisper),
            "speakers": filmcraft_speech::engine_available(Engine::Speaker),
        },
        "engine": ma.speech_engine,
        "default": m::DEFAULT_MODEL,
        "parakeetModel": ma.parakeet_model,
        "whisperModel": ma.whisper_model,
        "speakerModel": m::SPEAKER_MODEL,
        "dir": dir.as_ref().map(|d| d.to_string_lossy().to_string()),
        "models": m::catalogue().iter().map(|x| json!({
            "id": x.id, "name": x.name, "engine": x.engine.feature(), "languages": x.languages,
            "kind": if matches!(x.engine, Engine::Speaker) { "speakers" } else { "speech" },
            "multilingual": x.multilingual, "description": x.description,
            "license": x.license, "source": x.source, "size": x.size(),
            "available": filmcraft_speech::engine_available(x.engine),
            "installed": dir.as_ref().is_some_and(|d| m::installed(d, x)),
            "missingBytes": dir.as_ref().map(|d| m::missing_bytes(d, x)).unwrap_or(x.size()),
        })).collect::<Vec<_>>(),
    }))
}

/// Download catalogue models into `<data dir>/models` (feature `speech-download`) as a background
/// job (`jobs.list` shows the bytes, `transcript.cancel`/`jobs.cancel` stops it and keeps the part
/// file to resume). `model` or `models`; `wait: true` downloads before replying. Hosts show the
/// size, source and licence (`transcript.models`) and ask before running this.
fn download_model(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "transcript.downloadModel";
    let ids: Vec<String> = match (p.get("models").and_then(Value::as_array), str_p(p, "model")) {
        (Some(a), _) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        (None, Some(m)) => vec![m.to_string()],
        (None, None) => vec![s.prefs.media_analysis.parakeet_model.clone()],
    };
    let mut list = Vec::new();
    for id in &ids {
        let m = filmcraft_speech::models::find(id).ok_or_else(|| speech_err(SpeechError::UnknownModel(id.clone())))?;
        if !list.iter().any(|x: &&filmcraft_speech::models::ModelInfo| x.id == m.id) {
            list.push(m);
        }
    }
    if list.is_empty() {
        return Err(bad(CMD, "`model` or `models` is required"));
    }
    let dir = models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    #[cfg(feature = "speech-download")]
    {
        use std::sync::atomic::Ordering;
        let sizes: Vec<u64> = list.iter().map(|m| filmcraft_speech::models::missing_bytes(&dir, m)).collect();
        let total: u64 = sizes.iter().sum();
        let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        let names: Vec<&str> = list.iter().map(|m| m.id).collect();
        let job = crate::Job { id, label: format!("Download {}", names.join(", ")), progress: Default::default(), result: Default::default() };
        job.progress.total.store(total.max(1), Ordering::Relaxed);
        let (prog, res) = (job.progress.clone(), job.result.clone());
        let d = dir.clone();
        let t0 = web_time::Instant::now();
        let body = move || {
            let mut base = 0u64;
            let mut r = Ok(());
            for (m, size) in list.iter().zip(&sizes) {
                let before = base;
                let out = filmcraft_speech::models::download(&d, m, &mut |done, _, file| {
                    prog.done.store(before + done, Ordering::Relaxed);
                    *prog.status.lock().unwrap_or_else(|e| e.into_inner()) = format!("{}: {file} {:.0} MB", m.id, done as f64 / 1e6);
                    !prog.cancel.load(Ordering::Relaxed)
                });
                if let Err(e) = out {
                    r = Err(e.to_string());
                    break;
                }
                base += size;
            }
            let status = match &r {
                Ok(()) => "Downloaded".to_string(),
                Err(e) => e.clone(),
            };
            *prog.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
            *res.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(r.map(|()| filmcraft_export::Report { bytes: total, seconds: t0.elapsed().as_secs_f64(), ..Default::default() }));
            prog.finished.store(true, Ordering::Release);
        };
        let body = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), body);
        s.jobs.push(job);
        let reply = json!({"job": id, "models": names, "bytes": total, "dir": dir.to_string_lossy()});
        if bool_p(p, "wait").unwrap_or(false) || cfg!(target_arch = "wasm32") {
            body();
            if let Some(Err(e)) = s.jobs.iter().find(|j| j.id == id).and_then(|j| j.result.lock().unwrap_or_else(|e| e.into_inner()).clone()) {
                return Err(EngineError::Other(e));
            }
        } else {
            std::thread::Builder::new().name("filmcraft-model-download".into()).spawn(body).map_err(|e| EngineError::Other(e.to_string()))?;
        }
        Ok(reply)
    }
    #[cfg(not(feature = "speech-download"))]
    {
        let _ = (s, list, dir);
        Err(EngineError::Other("model downloads are not available in this build (built without the `speech-download` feature)".into()))
    }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            crate::transcription::CMD,
            "Transcribe…",
            &["Sequence", "Transcript"],
            r#"{"items":[id]?,"clips":[id]?,"track":"A1"|n?,"sequence":bool?,"range":"all|inOut"?,"engine":"parakeet|whisper"?,"model":str?,"language":"en|auto"?,"diarize":bool=true,"speakers":"auto"|n?,"maxSpeakers":n?,"replace":bool=true,"wait":bool=false}"#,
            can_transcribe,
            crate::transcription::generate,
            true,
        ),
        spec("transcript.jobs", "List Transcription Jobs", &[], "{}", always, crate::transcription::jobs, false),
        spec("transcript.cancel", "Cancel Transcription", &[], r#"{"job":id?}"#, always, crate::transcription::cancel, false),
        spec("transcript.speakers", "List Speakers", &[], "{}", always, speakers_list, false),
        spec("transcript.mergeSpeakers", "Merge Speakers…", &[], r#"{"from":"Speaker 2"|[name],"into":name}"#, has_transcripts, merge_speakers, true),
        spec("transcript.assignSpeaker", "Assign Speaker…", &[], r#"{"from":word,"to":word?,"speaker":name}"#, has_transcript, assign_speaker, true),
        spec("transcript.voices", "List Voices", &[], "{}", always, voices, false),
        spec("transcript.forgetVoice", "Forget Voice", &[], r#"{"name":str}"#, always, forget_voice, true),
        spec(
            "transcript.set",
            "Import Transcript",
            &[],
            r#"{"item":id,"transcript":{"language":str,"speakers":[{"name":str}],"words":[{"text":str,"start":tick,"end":tick,"speaker":n?}]}}"#,
            always,
            set,
            true,
        ),
        spec("transcript.delete", "Delete Transcript", &["Sequence", "Transcript"], r#"{"items":[id]?}"#, has_transcripts, delete, true),
        spec("transcript.inspect", "Inspect Transcript", &[], r#"{"paragraphGapSeconds":f?}"#, always, inspect, false),
        spec("transcript.search", "Search Transcript", &[], r#"{"query":str}"#, always, search, false),
        spec("transcript.models", "List Speech Models", &[], "{}", always, models, false),
        spec(
            "transcript.downloadModel",
            "Download Speech Model",
            &[],
            r#"{"model":"parakeet-tdt-0.6b-v2|titanet-large|whisper-base"?,"models":[str]?,"wait":bool=false}"#,
            can_download,
            download_model,
            true,
        ),
        spec("transcript.select", "Mark Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, select, true),
        spec("transcript.extract", "Extract Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, true), true),
        spec("transcript.lift", "Lift Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, false), true),
        spec(
            "transcript.renameSpeaker",
            "Rename Speaker…",
            &[],
            r#"{"speaker":"Speaker 1"|index,"name":str,"item":id?,"remember":bool=true}"#,
            has_transcripts,
            rename_speaker,
            true,
        ),
        spec(
            "transcript.removePauses",
            "Remove Pauses",
            &["Sequence", "Transcript"],
            r#"{"minSeconds":f?,"keepSeconds":f?}"#,
            has_transcript,
            remove_pauses,
            true,
        ),
        spec("transcript.removeFillers", "Remove Filler Words", &["Sequence", "Transcript"], r#"{"fillers":[str]?}"#, has_transcript, remove_fillers, true),
        spec(
            "transcript.createCaptions",
            "Create Captions from Transcript…",
            &["Sequence", "Transcript"],
            r#"{"maxChars":n?,"lines":1|2?,"minSeconds":f?,"maxSeconds":f?,"gapFrames":n?,"format":str?,"name":str?}"#,
            has_transcript,
            create_captions,
            true,
        ),
    ]
}
