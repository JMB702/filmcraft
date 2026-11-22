//! Transcription jobs: `transcript.generate` (Text panel ▸ Transcribe…, Sequence ▸ Transcript ▸
//! Transcribe…, the timeline's and Project panel's context menus).
//!
//! **Scope.** What is transcribed is a set of media items, each transcribed once however many
//! clips, nested sequences or multicam angles show it:
//!
//! - `items`: Project panel items, the whole media (a subclip: its range; a sequence: the media of
//!   its audio clips);
//! - `clips`: timeline clips (a video clip brings its linked audio);
//! - `track` (`"A2"`, an index from 0, or a track id) with `range: "all" | "inOut"`: the clips of
//!   one audio track, or only their part inside the sequence's In/Out;
//! - `sequence: true`: every audio clip of the active sequence (`range` applies too);
//! - nothing: the timeline selection, else the Project panel selection, else the whole sequence.
//!
//! Timeline scopes transcribe only the media the clips use (plus [`HANDLE_S`] either side) and
//! merge the words into the media's existing transcript; nested sequences are followed
//! ([`MAX_NEST`] deep). Each media item is heard through the channels and audio stream its clips
//! play (a recorder's mic channel, not the whole poly file). `replace: false` skips media that
//! already has a transcript.
//!
//! **Engines.** `parakeet` (the default) or `whisper`. Parakeet runs the language it was trained
//! on; a clip in another language (given, or detected by Whisper when the language is `auto`), or
//! a clip Parakeet fails on, is transcribed by Whisper instead when a Whisper model is installed.
//! Every transcript records the model that made it (`source`).
//!
//! **Speakers.** With `diarize` (default on), every word of every clip in the job is labelled by
//! clustering neural voice embeddings across all clips ([`filmcraft_speech::speakers`]), so one
//! voice gets one name in every clip. Clusters that match a voice of the project
//! (`Project::voices`, saved when a speaker is renamed) or a speaker of an earlier transcript take
//! that name; the rest become "Speaker N", numbered after the project's highest.
//!
//! **Job.** The work runs on a background thread ([`crate::Job`], `jobs.list`, `transcript.jobs`,
//! `transcript.cancel`); the editor stays responsive. The finished result is applied in one undo
//! step ("Transcribe") by [`poll`], and only then is the job marked finished, so an agent that
//! waits on the job reads the transcript right after. `wait: true` runs it to the end before
//! replying.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use filmcraft_media::SharedSource;
use filmcraft_project::{ItemId, ItemKind, Project, Speaker, TrackItem, TrackKind, Transcript, Word};
use filmcraft_speech::speakers::{self, Embedder};
use filmcraft_speech::{Options, SAMPLE_RATE, SpeechError, Transcriber, models};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{bad, bool_p, str_p, u64_p};
use crate::{EngineError, Result, Session};

pub(crate) const CMD: &str = "transcript.generate";
/// Longest piece of audio read and transcribed at once, seconds (memory: a poly recorder file is
/// read whole for each piece; the recogniser windows it further).
const PIECE_S: i64 = 10 * 60;
/// How far back from a piece's limit its cut may move to land in a pause, seconds.
const CUT_SEARCH_S: i64 = 30;
/// Most audio one job transcribes, seconds.
const MAX_JOB_S: i64 = 48 * 3600;
/// Media transcribed either side of what a clip uses, seconds (words cut by the clip's edge).
pub const HANDLE_S: f64 = 0.5;
/// Deepest nesting of sequences followed.
pub const MAX_NEST: usize = 8;
/// Most clip placements visited while resolving a scope (nested sequences can multiply them).
const MAX_VISITS: usize = 200_000;
/// Transcription reports kept for `jobs.list` after their jobs finish.
const MAX_REPORTS: usize = 32;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn secs(t: Tick) -> f64 {
    t.0 as f64 / TICKS_PER_SECOND as f64
}

// ---------------------------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------------------------

/// An audio stream of a media item and the channels heard from it (empty: the first two, as for
/// a clip with no channel mapping).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Feed {
    pub stream: usize,
    pub channels: Vec<u16>,
}

/// One media item of a job.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub feeds: BTreeSet<Feed>,
    /// Media-time ranges to transcribe, sorted and disjoint after [`Draft::finish`].
    pub ranges: Vec<TimeRange>,
    /// The whole media is transcribed: the result replaces the transcript.
    pub whole: bool,
}

impl Draft {
    fn finish(&mut self, duration: Tick) {
        let mut rs: Vec<TimeRange> = self
            .ranges
            .iter()
            .filter_map(|r| {
                let a = r.start.clamp(Tick::ZERO, duration);
                let b = r.end().clamp(a, duration);
                (b > a).then(|| TimeRange::from_bounds(a, b))
            })
            .collect();
        rs.sort_by_key(|r| r.start);
        let mut out: Vec<TimeRange> = Vec::new();
        for r in rs {
            match out.last_mut() {
                Some(l) if r.start <= l.end() => *l = TimeRange::from_bounds(l.start, l.end().max(r.end())),
                _ => out.push(r),
            }
        }
        if out.first().is_some_and(|r| r.start <= Tick::ZERO && r.end() >= duration) {
            self.whole = true;
        }
        if self.whole {
            out = vec![TimeRange::from_bounds(Tick::ZERO, duration)];
        }
        self.ranges = out;
    }
}

/// Collects the media a scope covers.
pub struct Scope<'a> {
    project: &'a Project,
    pub drafts: BTreeMap<ItemId, Draft>,
    visits: usize,
}

impl<'a> Scope<'a> {
    pub fn new(project: &'a Project) -> Self {
        Self { project, drafts: BTreeMap::new(), visits: 0 }
    }

    fn add_media(&mut self, root: ItemId, range: TimeRange, feed: Feed, whole: bool) {
        let d = self.drafts.entry(root).or_default();
        d.feeds.insert(feed);
        d.ranges.push(range);
        d.whole |= whole;
    }

    /// A Project panel item: the whole media, a subclip's range, or a sequence's audio clips.
    pub fn add_item(&mut self, item: ItemId) {
        let p = self.project;
        let Some(it) = p.item(item) else { return };
        match &it.kind {
            ItemKind::Media(m) => self.add_media(item, TimeRange::from_bounds(Tick::ZERO, m.duration()), Feed { stream: 0, channels: Vec::new() }, true),
            ItemKind::Subclip { .. } => {
                if let Some((root, m, range)) = p.resolve_media(item) {
                    let r = range.unwrap_or_else(|| TimeRange::from_bounds(Tick::ZERO, m.duration()));
                    self.add_media(root, r, Feed { stream: 0, channels: Vec::new() }, false);
                }
            }
            ItemKind::Sequence(_) => {
                if let Some(q) = p.sequence(item) {
                    for t in &q.audio_tracks {
                        for c in t.items.iter().filter(|c| c.enabled) {
                            self.add_clip(c, None, 1);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// A clip on an audio track, or its part inside `window` (timeline time of its sequence).
    pub fn add_clip(&mut self, clip: &TrackItem, window: Option<TimeRange>, depth: usize) {
        self.visits += 1;
        if depth > MAX_NEST || self.visits > MAX_VISITS {
            return;
        }
        let r = match window {
            Some(w) => match clip.range().intersect(&w) {
                Some(r) if !r.is_empty() => r,
                _ => return,
            },
            None => clip.range(),
        };
        if r.duration <= Tick::ZERO {
            return;
        }
        let x = clip.source_time_at(r.start);
        let y = clip.source_time_at(r.end() - Tick(1));
        let (a, b) = (x.min(y), x.max(y) + Tick(1));
        let p = self.project;
        if let Some(inner) = p.sequence(clip.item) {
            // a nested or multicam sequence: what is heard inside the part shown
            let w = TimeRange::from_bounds(a, b);
            for t in inner.audio_tracks.iter().filter(|t| !t.muted) {
                for c in t.items.iter().filter(|c| c.enabled) {
                    self.add_clip(c, Some(w), depth + 1);
                }
            }
            return;
        }
        let Some((root, m, _)) = p.resolve_media(clip.item) else { return };
        let h = Tick::from_seconds_f64(HANDLE_S);
        let range = TimeRange::from_bounds((a - h).max(Tick::ZERO), (b + h).min(m.duration()));
        self.add_media(root, range, Feed { stream: clip.audio_stream, channels: clip.source_channels.clone() }, false);
    }
}

/// Timeline clips: audio clips as they are; a video clip brings the audio clips linked to it, or
/// its own media's audio when nothing is linked.
fn add_clips(s: &Session, scope: &mut Scope, ids: &[filmcraft_project::ClipId]) {
    let Some(q) = s.active_sequence() else { return };
    for id in ids {
        let Some((tid, it)) = q.find_item(*id) else { continue };
        let Some(track) = q.track(tid) else { continue };
        if track.kind == TrackKind::Audio {
            scope.add_clip(it, None, 0);
            continue;
        }
        let linked: Vec<&TrackItem> = match it.link {
            Some(l) => q.audio_tracks.iter().flat_map(|t| t.items.iter()).filter(|c| c.link == Some(l)).collect(),
            None => Vec::new(),
        };
        if linked.is_empty() {
            scope.add_clip(it, None, 0);
        }
        for c in linked {
            scope.add_clip(c, None, 0);
        }
    }
}

/// The sequence's In/Out as a timeline range (`mark_out` is the last frame shown).
fn in_out(s: &Session) -> Option<TimeRange> {
    let q = s.active_sequence()?;
    let fd = q.settings.frame_rate.frame_duration();
    let a = q.mark_in.unwrap_or(Tick::ZERO);
    let b = q.mark_out.map(|o| o + fd)?;
    (b > a).then(|| TimeRange::from_bounds(a, b))
}

fn window_p(s: &Session, p: &Value) -> Result<Option<TimeRange>> {
    match str_p(p, "range") {
        None | Some("all") => Ok(None),
        Some("inOut") => in_out(s).map(Some).ok_or_else(|| bad(CMD, "`range: \"inOut\"` needs an Out point on the sequence")),
        Some(o) => Err(bad(CMD, format!("unknown `range` {o:?} (all | inOut)"))),
    }
}

/// `track`: `"A2"`, an index from 0 into the audio tracks, or a track id.
fn audio_track_p(s: &Session, p: &Value) -> Result<filmcraft_project::TrackId> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    if let Some(i) = p.get("track").and_then(Value::as_u64)
        && let Some(t) = usize::try_from(i).ok().and_then(|i| q.audio_tracks.get(i))
    {
        return Ok(t.id);
    }
    let id = crate::commands::track_p(s, p, "track", CMD)?.ok_or_else(|| bad(CMD, "unknown `track`"))?;
    if q.track(id).is_none_or(|t| t.kind != TrackKind::Audio) {
        return Err(bad(CMD, "`track` must be an audio track"));
    }
    Ok(id)
}

fn ids_p(p: &Value, k: &str) -> Option<Vec<u64>> {
    p.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect())
}

/// What a job transcribes.
pub struct Plan {
    pub units: Vec<Unit>,
    /// `{item, name, reason}` of media left out.
    pub skipped: Vec<Value>,
    pub scope: &'static str,
}

/// One media item to transcribe.
pub struct Unit {
    pub item: ItemId,
    pub name: String,
    pub src: SharedSource,
    pub feeds: Vec<Feed>,
    pub ranges: Vec<TimeRange>,
    pub whole: bool,
}

impl Unit {
    fn seconds(&self) -> f64 {
        self.ranges.iter().map(|r| secs(r.duration)).sum()
    }
}

pub fn plan(s: &Session, p: &Value, replace: bool) -> Result<Plan> {
    let mut scope = Scope::new(&s.project);
    let name: &'static str;
    if let Some(items) = ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![i])) {
        name = "items";
        for i in items {
            scope.add_item(ItemId(i));
        }
    } else if let Some(clips) = ids_p(p, "clips").or_else(|| u64_p(p, "clip").map(|c| vec![c])) {
        name = "clips";
        add_clips(s, &mut scope, &clips.into_iter().map(filmcraft_project::ClipId).collect::<Vec<_>>());
    } else if p.get("track").is_some_and(|t| !t.is_null()) {
        name = "track";
        let tid = audio_track_p(s, p)?;
        let w = window_p(s, p)?;
        let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
        if let Some(t) = q.track(tid) {
            for c in t.items.iter().filter(|c| c.enabled) {
                scope.add_clip(c, w, 0);
            }
        }
    } else if bool_p(p, "sequence") == Some(true) || s.state.selection.is_empty() && s.state.project_selection.is_empty() {
        name = "sequence";
        let w = window_p(s, p)?;
        let q = s.active_sequence().ok_or_else(|| bad(CMD, "nothing to transcribe (pass `items`, `clips`, `track`, select clips, or open a sequence)"))?;
        for t in &q.audio_tracks {
            for c in t.items.iter().filter(|c| c.enabled) {
                scope.add_clip(c, w, 0);
            }
        }
    } else if !s.state.selection.is_empty() && s.active_sequence().is_some() {
        name = "selection";
        add_clips(s, &mut scope, &s.state.selection);
    } else {
        name = "items";
        for i in &s.state.project_selection {
            scope.add_item(*i);
        }
    }
    let mut units = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0f64;
    for (item, mut d) in std::mem::take(&mut scope.drafts) {
        let label = s.project.item(item).map(|i| i.name.clone()).unwrap_or_default();
        let Some(ItemKind::Media(m)) = s.project.item(item).map(|i| &i.kind) else { continue };
        d.finish(m.duration());
        let Some(src) = s.source(item) else {
            skipped.push(json!({"item": item.0, "name": label, "reason": "offline"}));
            continue;
        };
        if !src.info().has_audio() {
            skipped.push(json!({"item": item.0, "name": label, "reason": "no audio"}));
            continue;
        }
        if !replace && s.project.transcripts.contains_key(&item) {
            skipped.push(json!({"item": item.0, "name": label, "reason": "already transcribed"}));
            continue;
        }
        if d.ranges.is_empty() {
            continue;
        }
        let unit = Unit { item, name: label, src, feeds: d.feeds.into_iter().collect(), ranges: d.ranges, whole: d.whole };
        total += unit.seconds();
        units.push(unit);
    }
    if total > MAX_JOB_S as f64 {
        return Err(bad(
            CMD,
            format!("{:.1} hours of audio is more than one job transcribes ({} hours); transcribe fewer clips at a time", total / 3600.0, MAX_JOB_S / 3600),
        ));
    }
    Ok(Plan { units, skipped, scope: name })
}

// ---------------------------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------------------------

/// Channels heard per stream: the union over a media item's clips.
fn streams(feeds: &[Feed]) -> BTreeMap<usize, BTreeSet<u16>> {
    let mut out: BTreeMap<usize, BTreeSet<u16>> = BTreeMap::new();
    for f in feeds {
        let e = out.entry(f.stream).or_default();
        if f.channels.is_empty() {
            e.extend([0, 1]);
        } else {
            e.extend(f.channels.iter().copied());
        }
    }
    out
}

/// Mono 16 kHz audio of `range` (media time) of a unit: the average of the channels its clips
/// play, over every stream they use.
pub fn read_audio(src: &SharedSource, feeds: &[Feed], range: TimeRange) -> std::result::Result<Vec<f32>, String> {
    let sr = SAMPLE_RATE as i64;
    let start = range.start.to_units_floor(sr).max(0);
    let frames = usize::try_from(range.duration.to_units_floor(sr).max(0)).unwrap_or(0);
    let mut mix = vec![0f32; frames];
    let mut n = 0usize;
    for (stream, chans) in streams(feeds) {
        let buf = src.audio_stream(stream, start, frames, SAMPLE_RATE).map_err(|e| e.to_string())?;
        let mut picked: Vec<&Vec<f32>> = chans.iter().filter_map(|c| buf.channels.get(usize::from(*c))).collect();
        if picked.is_empty() {
            picked = buf.channels.iter().collect();
        }
        for c in picked {
            for (m, v) in mix.iter_mut().zip(c) {
                *m += v;
            }
            n += 1;
        }
    }
    if n > 1 {
        let k = 1.0 / n as f32;
        mix.iter_mut().for_each(|m| *m *= k);
    }
    Ok(mix)
}

/// Where to end a piece of `audio` read up to its limit: the middle of the quietest 200 ms in its
/// last [`CUT_SEARCH_S`] seconds.
pub fn quiet_cut(audio: &[f32]) -> usize {
    let sr = SAMPLE_RATE as usize;
    let hop = sr / 100;
    let win = 20;
    let n = audio.len() / hop;
    if n <= win {
        return audio.len();
    }
    let energy: Vec<f32> = (0..n).map(|i| audio[i * hop..(i + 1) * hop].iter().map(|v| v * v).sum()).collect();
    let lo = n.saturating_sub(CUT_SEARCH_S as usize * 100).min(n - win);
    let mut best = (f32::INFINITY, n);
    let mut acc: f32 = energy[lo..lo + win].iter().sum();
    for s in lo..=n - win {
        if s > lo {
            acc += energy[s + win - 1] - energy[s - 1];
        }
        if acc < best.0 {
            best = (acc, s + win / 2);
        }
    }
    (best.1 * hop).clamp(hop, audio.len())
}

// ---------------------------------------------------------------------------------------------
// Job
// ---------------------------------------------------------------------------------------------

/// A model given ready-made (a host's or test's) or to load from the models directory by id.
pub enum Model<T: ?Sized> {
    Ready(Arc<T>),
    Load(String),
}

impl<T: ?Sized> Clone for Model<T> {
    fn clone(&self) -> Self {
        match self {
            Model::Ready(t) => Model::Ready(t.clone()),
            Model::Load(id) => Model::Load(id.clone()),
        }
    }
}

/// The models a job uses.
#[derive(Clone)]
pub struct Engines {
    pub primary: Model<dyn Transcriber>,
    /// Whisper for other languages and when the primary fails.
    pub fallback: Option<Model<dyn Transcriber>>,
    /// Neural speaker labels; `None` with `diarize` asks the recogniser to label speakers itself.
    pub embedder: Option<Model<dyn Embedder>>,
    pub dir: Option<PathBuf>,
}

fn load_transcriber(m: &Model<dyn Transcriber>, dir: &Option<PathBuf>) -> std::result::Result<Arc<dyn Transcriber>, String> {
    match m {
        Model::Ready(t) => Ok(t.clone()),
        Model::Load(id) => {
            let dir = dir.as_ref().ok_or("no data directory for speech models")?;
            filmcraft_speech::load(dir, id).map_err(|e| e.to_string())
        }
    }
}

fn load_embedder(m: &Model<dyn Embedder>, dir: &Option<PathBuf>) -> std::result::Result<Arc<dyn Embedder>, String> {
    match m {
        Model::Ready(t) => Ok(t.clone()),
        Model::Load(id) => {
            let dir = dir.as_ref().ok_or("no data directory for speech models")?;
            filmcraft_speech::load_embedder(dir, id).map_err(|e| e.to_string())
        }
    }
}

/// Everything a job needs, owned (it runs on another thread).
pub struct Input {
    pub units: Vec<Unit>,
    pub engines: Engines,
    /// `None`: detect (Whisper's language identification when a Whisper model is installed).
    pub language: Option<String>,
    pub diarize: bool,
    /// Exact number of speakers; `None`: decide from the voices.
    pub speakers: Option<usize>,
    pub max_speakers: usize,
    /// The project when the job started (voices and earlier speakers to name clusters by).
    pub project: Arc<Project>,
}

/// One media item's new transcript.
#[derive(Clone, Debug)]
pub struct UnitResult {
    pub item: ItemId,
    pub name: String,
    pub ranges: Vec<TimeRange>,
    pub whole: bool,
    pub transcript: Transcript,
    /// Whisper stood in for Parakeet (language or failure).
    pub fallback: bool,
}

/// A finished job, before it is applied.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub units: Vec<UnitResult>,
    pub warnings: Vec<String>,
    /// `(name, seconds of speech)` per speaker found.
    pub speakers: Vec<(String, f64)>,
    pub audio_seconds: f64,
    pub compute_seconds: f64,
    pub engine: String,
}

/// The highest `N` of a "Speaker N" name in the project.
fn highest_speaker_number(p: &Project) -> u32 {
    let num = |n: &str| n.strip_prefix("Speaker ").and_then(|x| x.trim().parse::<u32>().ok());
    let a = p.transcripts.values().flat_map(|t| t.speakers.iter()).filter_map(|s| num(&s.name)).max().unwrap_or(0);
    let b = p.voices.iter().filter_map(|v| num(&v.name)).max().unwrap_or(0);
    a.max(b)
}

/// Known voices by name: the project's saved voices, then the speakers of earlier transcripts
/// made with the same speaker model.
fn known_voices(p: &Project, model: &str) -> Vec<(String, Vec<f32>)> {
    let mut known: Vec<(String, Vec<f32>)> = Vec::new();
    for v in p.voices.iter().filter(|v| v.model == model && !v.embedding.is_empty()) {
        if !known.iter().any(|k| k.0 == v.name) {
            known.push((v.name.clone(), v.embedding.clone()));
        }
    }
    for t in p.transcripts.values().filter(|t| t.voice_model == model) {
        for sp in t.speakers.iter().filter(|s| !s.embedding.is_empty()) {
            if !known.iter().any(|k| k.0 == sp.name) {
                known.push((sp.name.clone(), sp.embedding.clone()));
            }
        }
    }
    known
}

/// A name for each cluster: a known voice it matches, else the next "Speaker N".
pub fn name_clusters(p: &Project, model: &str, centroids: &[Vec<f32>]) -> Vec<String> {
    let known = known_voices(p, model);
    let refs: Vec<&[f32]> = known.iter().map(|k| k.1.as_slice()).collect();
    let matched = speakers::match_voices(centroids, &refs, speakers::VOICE_MATCH);
    let mut next = highest_speaker_number(p) + 1;
    matched
        .into_iter()
        .map(|m| match m.and_then(|j| known.get(j)) {
            Some(k) => k.0.clone(),
            None => {
                let n = format!("Speaker {next}");
                next += 1;
                n
            }
        })
        .collect()
}

/// The Whisper stand-in, loaded the first time it is needed.
struct Fallback {
    model: Option<Model<dyn Transcriber>>,
    loaded: Option<Arc<dyn Transcriber>>,
    error: Option<String>,
}

impl Fallback {
    fn get(&mut self, dir: &Option<PathBuf>) -> Option<Arc<dyn Transcriber>> {
        if self.loaded.is_none() && self.error.is_none() {
            match self.model.as_ref().map(|m| load_transcriber(m, dir)) {
                Some(Ok(t)) => self.loaded = Some(t),
                Some(Err(e)) => self.error = Some(e),
                None => self.error = Some("no Whisper model is installed".into()),
            }
        }
        self.loaded.clone()
    }
}

/// A piece of one unit: words in media time.
struct Piece {
    unit: usize,
    words: Vec<Word>,
}

/// Run a job to the end. `report(done, total, status)` returns false to stop.
pub fn run(input: &Input, report: &mut dyn FnMut(u64, u64, &str) -> bool) -> std::result::Result<Outcome, String> {
    let t0 = web_time::Instant::now();
    let hop = SAMPLE_RATE as f64 / 100.0;
    let total: u64 = input.units.iter().map(|u| (u.seconds() * 100.0) as u64).sum::<u64>().max(1);
    let stopped = || "stopped: nothing was changed".to_string();
    let primary_name = match &input.engines.primary {
        Model::Ready(t) => t.id(),
        Model::Load(id) => id.clone(),
    };
    if !report(0, total, &format!("Loading {primary_name}…")) {
        return Err(stopped());
    }
    let primary = load_transcriber(&input.engines.primary, &input.engines.dir)?;
    let mut fb = Fallback { model: input.engines.fallback.clone(), loaded: None, error: None };
    let embedder = match (&input.engines.embedder, input.diarize) {
        (Some(m), true) => Some(load_embedder(m, &input.engines.dir)?),
        _ => None,
    };
    let params = speakers::Params { speakers: input.speakers, max_speakers: input.max_speakers.max(1), ..Default::default() };
    let mut out = Outcome { engine: primary.id(), ..Default::default() };
    let mut pieces: Vec<Piece> = Vec::new();
    let mut segments: Vec<speakers::Segment> = Vec::new();
    let mut fallback_used = vec![false; input.units.len()];
    let mut languages: Vec<String> = vec![String::new(); input.units.len()];
    let mut sources: Vec<String> = vec![String::new(); input.units.len()];
    let mut done_hops = 0u64;
    let primary_langs = primary.languages();
    for (ui, unit) in input.units.iter().enumerate() {
        // which engine and language for this media item
        let mut language = input.language.clone();
        let mut use_fallback = false;
        if let (Some(l), Some(ls)) = (&language, primary_langs)
            && !ls.contains(&l.as_str())
        {
            use_fallback = true;
        }
        if language.is_none()
            && primary_langs.is_some()
            && let Some(whisper) = fb.get(&input.engines.dir)
            && let Some(first) = unit.ranges.first()
        {
            let probe = TimeRange::new(first.start, first.duration.min(Tick::from_seconds_f64(30.0)));
            if let Ok(a) = read_audio(&unit.src, &unit.feeds, probe)
                && let Ok(Some(l)) = whisper.detect_language(&a)
            {
                use_fallback = primary_langs.is_some_and(|ls| !ls.contains(&l.as_str()));
                language = Some(l);
            }
        }
        if use_fallback && fb.get(&input.engines.dir).is_none() {
            out.warnings.push(format!(
                "{}: {} does not transcribe {}, and Whisper is not available ({}); skipped",
                unit.name,
                primary.id(),
                language.as_deref().unwrap_or("this language"),
                fb.error.clone().unwrap_or_default()
            ));
            done_hops += (unit.seconds() * 100.0) as u64;
            continue;
        }
        let opts = Options { language: language.clone(), diarize: input.diarize && embedder.is_none(), max_speakers: input.max_speakers.max(1) };
        for range in &unit.ranges {
            let mut a = range.start;
            while a < range.end() {
                let lim = (a + Tick::from_seconds_f64(PIECE_S as f64)).min(range.end());
                let mut audio = read_audio(&unit.src, &unit.feeds, TimeRange::from_bounds(a, lim)).map_err(|e| format!("{}: {e}", unit.name))?;
                if lim < range.end() {
                    let cut = quiet_cut(&audio);
                    audio.truncate(cut);
                }
                if audio.is_empty() {
                    break;
                }
                let piece_hops = (audio.len() as f64 / hop) as u64;
                let label = unit.name.clone();
                let base = done_hops;
                let mut stop = false;
                let mut prog = |f: f32, _: &str| {
                    let ok = report(base + (f.clamp(0.0, 1.0) as f64 * piece_hops as f64) as u64, total, &format!("{label}: transcribing {:.0}%", f * 100.0));
                    stop |= !ok;
                    ok
                };
                let engine = if use_fallback { fb.get(&input.engines.dir).unwrap_or_else(|| primary.clone()) } else { primary.clone() };
                let result = match engine.transcribe(&audio, &opts, &mut prog) {
                    Err(SpeechError::Cancelled) => return Err(stopped()),
                    Err(e) if !use_fallback => match fb.get(&input.engines.dir) {
                        Some(whisper) => {
                            out.warnings.push(format!("{}: {} failed ({e}); transcribed with {}", unit.name, primary.id(), whisper.id()));
                            use_fallback = true;
                            whisper.transcribe(&audio, &opts, &mut prog)
                        }
                        None => Err(e),
                    },
                    r => r,
                };
                if stop {
                    return Err(stopped());
                }
                let tr = match result {
                    Ok(t) => t,
                    Err(SpeechError::Cancelled) => return Err(stopped()),
                    Err(e) => return Err(format!("{}: {e}", unit.name)),
                };
                fallback_used[ui] |= use_fallback;
                if languages[ui].is_empty() {
                    languages[ui] = tr.language.clone();
                }
                sources[ui] = tr.source.clone();
                if let Some(emb) = &embedder {
                    let clip = speakers::Clip { audio: &audio, words: &tr.words };
                    let mut sp = |_: f32| report(base + piece_hops, total, &format!("{label}: listening for voices"));
                    match speakers::embed_segments(emb.as_ref(), pieces.len(), &clip, &params, &mut sp) {
                        Ok(v) => segments.extend(v),
                        Err(SpeechError::Cancelled) => return Err(stopped()),
                        Err(e) => return Err(format!("{}: speaker detection: {e}", unit.name)),
                    }
                }
                let offset = a;
                let mut words = tr.words;
                // speaker labels of the recogniser's own (no neural model) map through its list
                let names: Vec<String> = tr.speakers.iter().map(|s| s.name.clone()).collect();
                for w in &mut words {
                    w.start += offset;
                    w.end += offset;
                    if embedder.is_some() {
                        w.speaker = None;
                    } else if let Some(i) = w.speaker {
                        // keep the recogniser's numbering per piece; renamed below
                        w.speaker = names.get(i as usize).map(|_| i);
                    }
                }
                pieces.push(Piece { unit: ui, words });
                done_hops += piece_hops;
                let used = Tick(audio.len() as i64 * filmcraft_speech::TICKS_PER_SAMPLE);
                a = (a + used).max(a + Tick(1));
            }
        }
        out.audio_seconds += unit.seconds();
    }
    // speakers across the whole job
    let mut unit_speakers: Vec<Vec<Speaker>> = vec![Vec::new(); input.units.len()];
    if let Some(emb) = &embedder {
        if !report(done_hops, total, "Identifying speakers…") {
            return Err(stopped());
        }
        let counts: Vec<usize> = pieces.iter().map(|p| p.words.len()).collect();
        let d = speakers::label(&segments, &counts, &params);
        let model = emb.id();
        let names = name_clusters(&input.project, &model, &d.centroids);
        for (pi, piece) in pieces.iter_mut().enumerate() {
            let list = &mut unit_speakers[piece.unit];
            for (w, l) in piece.words.iter_mut().zip(d.labels.get(pi).map(Vec::as_slice).unwrap_or(&[])) {
                w.speaker = l.and_then(|c| {
                    let c = c as usize;
                    let name = names.get(c)?;
                    let idx = match list.iter().position(|s| &s.name == name) {
                        Some(i) => i,
                        None => {
                            let emb = d.centroids.get(c).cloned().unwrap_or_default();
                            list.push(Speaker { name: name.clone(), embedding: emb });
                            list.len() - 1
                        }
                    };
                    u32::try_from(idx).ok()
                });
            }
        }
        out.speakers = names.iter().cloned().zip(d.seconds.iter().copied()).collect();
    } else if input.diarize {
        // the recogniser labelled each piece on its own: "Speaker N" per piece, numbered per item
        for piece in &mut pieces {
            let list = &mut unit_speakers[piece.unit];
            for w in &mut piece.words {
                w.speaker = w.speaker.and_then(|i| {
                    let name = format!("Speaker {}", i + 1);
                    let idx = match list.iter().position(|s| s.name == name) {
                        Some(i) => i,
                        None => {
                            list.push(Speaker::new(name));
                            list.len() - 1
                        }
                    };
                    u32::try_from(idx).ok()
                });
            }
        }
    }
    let voice_model = embedder.as_ref().map(|e| e.id()).unwrap_or_default();
    let mut by_unit: Vec<Vec<Word>> = vec![Vec::new(); input.units.len()];
    for piece in pieces {
        by_unit[piece.unit].extend(piece.words);
    }
    for (ui, (unit, words)) in input.units.iter().zip(by_unit).enumerate() {
        if sources[ui].is_empty() {
            continue;
        }
        let mut t = Transcript {
            language: if languages[ui].is_empty() { input.language.clone().unwrap_or_else(|| "en".into()) } else { languages[ui].clone() },
            source: sources[ui].clone(),
            speakers: std::mem::take(&mut unit_speakers[ui]),
            words,
            voice_model: voice_model.clone(),
        };
        t.normalize();
        out.units.push(UnitResult {
            item: unit.item,
            name: unit.name.clone(),
            ranges: unit.ranges.clone(),
            whole: unit.whole,
            transcript: t,
            fallback: fallback_used[ui],
        });
    }
    out.compute_seconds = t0.elapsed().as_secs_f64();
    report(total, total, "Applying…");
    Ok(out)
}

/// `new` (words of `ranges` only) into `old`: the old words inside the ranges go, the new ones
/// come in, speakers are matched by name.
pub fn merge(old: &Transcript, new: Transcript, ranges: &[TimeRange]) -> Transcript {
    let mut out = old.clone();
    let inside = |w: &Word| {
        let mid = Tick(w.start.0 + (w.end.0 - w.start.0) / 2);
        ranges.iter().any(|r| r.contains(mid))
    };
    // old speaker indices survive; new speakers are appended by name
    out.words.retain(|w| !inside(w));
    let mut map: Vec<Option<u32>> = Vec::with_capacity(new.speakers.len());
    for sp in &new.speakers {
        let i = match out.speakers.iter().position(|s| s.name == sp.name) {
            Some(i) => {
                if out.speakers[i].embedding.is_empty() {
                    out.speakers[i].embedding = sp.embedding.clone();
                }
                i
            }
            None => {
                out.speakers.push(sp.clone());
                out.speakers.len() - 1
            }
        };
        map.push(u32::try_from(i).ok());
    }
    for mut w in new.words {
        w.speaker = w.speaker.and_then(|i| map.get(i as usize).copied().flatten());
        out.words.push(w);
    }
    out.words.sort_by_key(|w| (w.start, w.end));
    out.source = new.source;
    if !new.language.is_empty() {
        out.language = new.language;
    }
    if !new.voice_model.is_empty() {
        out.voice_model = new.voice_model;
    }
    out.normalize();
    out
}

/// A transcription job whose result is applied by [`poll`] when it finishes.
pub struct Pending {
    pub job: u64,
    pub items: Vec<ItemId>,
    pub engine: String,
    pub outcome: Arc<Mutex<Option<std::result::Result<Outcome, String>>>>,
    pub done: Arc<AtomicBool>,
    /// `Session::document_epoch` when the job started.
    pub epoch: u64,
}

fn outcome_json(o: &Outcome, skipped: &[Value]) -> Value {
    json!({
        "items": o.units.iter().map(|u| json!({
            "item": u.item.0,
            "name": u.name,
            "words": u.transcript.words.len(),
            "speakers": u.transcript.speakers.iter().map(|s| &s.name).collect::<Vec<_>>(),
            "language": u.transcript.language,
            "source": u.transcript.source,
            "fallback": u.fallback,
            "whole": u.whole,
            "ranges": u.ranges.iter().map(|r| json!({"start": r.start.0, "end": r.end().0})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "speakers": o.speakers.iter().map(|(n, s)| json!({"name": n, "seconds": s})).collect::<Vec<_>>(),
        "skipped": skipped,
        "warnings": o.warnings,
        "engine": o.engine,
        "audioSeconds": o.audio_seconds,
        "computeSeconds": o.compute_seconds,
    })
}

/// Apply finished jobs (one undo step each) and drop stopped ones. Called once per UI frame from
/// [`Session::poll_persistence`], by `jobs.list` and after synchronous runs.
pub fn poll(s: &mut Session) {
    let mut i = 0;
    while i < s.transcribe_jobs.len() {
        let job_id = s.transcribe_jobs[i].job;
        let job = s.jobs.iter().find(|j| j.id == job_id).cloned();
        // the worker panicked: guard_job already finished the job with an error
        let failed = job.as_ref().is_none_or(|j| j.progress.finished.load(Ordering::Relaxed));
        if !s.transcribe_jobs[i].done.load(Ordering::Acquire) && !failed {
            i += 1;
            continue;
        }
        let pj = s.transcribe_jobs.remove(i);
        let Some(job) = job else { continue };
        let finish = |r: std::result::Result<filmcraft_export::Report, String>, status: String| {
            *lock(&job.progress.status) = status;
            *lock(&job.result) = Some(r);
            job.progress.finished.store(true, Ordering::Release);
        };
        if failed {
            continue;
        }
        let outcome = lock(&pj.outcome).take();
        let skipped: Vec<Value> = s.transcribe_skipped.remove(&pj.job).unwrap_or_default();
        match outcome {
            None => finish(Err("internal error: the job left no result".into()), "Failed".into()),
            Some(Err(e)) => {
                if !e.starts_with("stopped") {
                    s.error_toast(CMD, format!("Transcription: {e}"));
                }
                finish(Err(e.clone()), e);
            }
            Some(Ok(_)) if pj.epoch != s.document_epoch => {
                let m = "the project was replaced; nothing was changed".to_string();
                s.error_toast(CMD, format!("Transcription: {m}"));
                finish(Err(m.clone()), m);
            }
            Some(Ok(o)) => {
                let report = outcome_json(&o, &skipped);
                match apply(s, o) {
                    Ok(summary) => {
                        s.toast(summary.clone());
                        remember_report(s, pj.job, report);
                        finish(Ok(filmcraft_export::Report { seconds: 0.0, ..Default::default() }), summary);
                    }
                    Err(e) => {
                        s.error_toast(CMD, format!("Transcription: {e}"));
                        finish(Err(e.to_string()), e.to_string());
                    }
                }
            }
        }
    }
}

fn remember_report(s: &mut Session, job: u64, report: Value) {
    s.transcribe_reports.push((job, report));
    let n = s.transcribe_reports.len();
    if n > MAX_REPORTS {
        s.transcribe_reports.drain(..n - MAX_REPORTS);
    }
}

/// The report of a finished transcription job (`jobs.list` shows it as the job's `result`).
pub fn report_of(s: &Session, job: u64) -> Option<&Value> {
    s.transcribe_reports.iter().rev().find(|(j, _)| *j == job).map(|(_, r)| r)
}

fn apply(s: &mut Session, o: Outcome) -> Result<String> {
    let n_items = o.units.len();
    let n_words: usize = o.units.iter().map(|u| u.transcript.words.len()).sum();
    let n_speakers = o.speakers.len();
    if n_items == 0 {
        let why = o.warnings.first().cloned().unwrap_or_else(|| "nothing was transcribed".into());
        return Err(EngineError::Other(why));
    }
    s.edit("Transcribe", move |pr, _| {
        for u in o.units {
            let t = match pr.transcripts.get(&u.item) {
                Some(old) if !u.whole => merge(old, u.transcript, &u.ranges),
                _ => u.transcript,
            };
            pr.transcripts.insert(u.item, Arc::new(t));
        }
        Ok(())
    })?;
    let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    let mut m = format!("Transcribed {}: {}", plural(n_items, "clip"), plural(n_words, "word"));
    if n_speakers > 0 {
        m.push_str(&format!(", {}", plural(n_speakers, "speaker")));
    }
    Ok(m)
}

// ---------------------------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------------------------

/// The engine and models a command asks for, checked before anything starts.
fn engines(s: &Session, p: &Value, diarize: bool) -> Result<Engines> {
    let ma = &s.prefs.media_analysis;
    let dir = crate::transcript::models_dir();
    let installed = |id: &str| -> Result<()> {
        let m = models::find(id).ok_or_else(|| EngineError::Other(SpeechError::UnknownModel(id.into()).to_string()))?;
        if !filmcraft_speech::engine_available(m.engine) {
            return Err(EngineError::Other(format!("this build has no {} ({} feature)", m.name, m.engine.feature())));
        }
        let ok = dir.as_ref().is_some_and(|d| models::installed(d, m));
        if ok {
            Ok(())
        } else {
            Err(EngineError::Other(format!(
                "the model `{id}` is not downloaded ({:.0} MB): run transcript.downloadModel {{\"model\":\"{id}\"}} first",
                m.size() as f64 / 1e6
            )))
        }
    };
    let whisper_ready = |id: &str| installed(id).is_ok();
    let asked = match str_p(p, "engine") {
        Some(e) => Some(
            models::Engine::parse(e)
                .filter(|e| *e != models::Engine::Speaker)
                .ok_or_else(|| bad(CMD, format!("unknown `engine` {e:?} (parakeet | whisper)")))?,
        ),
        None => None,
    };
    // a host's or test's own models
    if let Some(t) = &s.transcriber {
        return Ok(Engines {
            primary: Model::Ready(t.clone()),
            fallback: s.transcriber_fallback.clone().map(Model::Ready),
            embedder: s.speaker_embedder.clone().map(Model::Ready).filter(|_| diarize),
            dir,
        });
    }
    if !filmcraft_speech::available() {
        return Err(EngineError::Other(crate::transcript::NO_SPEECH.into()));
    }
    let model = str_p(p, "model").map(str::to_string);
    let engine = match (asked, model.as_deref().and_then(models::find)) {
        (Some(e), _) => e,
        (None, Some(m)) => m.engine,
        (None, None) => models::Engine::parse(&ma.speech_engine).unwrap_or(models::Engine::Parakeet),
    };
    let engine = if filmcraft_speech::engine_available(engine) { engine } else { models::Engine::Whisper };
    let primary = match (&model, engine) {
        (Some(m), _) => m.clone(),
        (None, models::Engine::Parakeet) => ma.parakeet_model.clone(),
        (None, _) => ma.whisper_model.clone(),
    };
    if models::find(&primary).is_some_and(|m| m.engine != engine) {
        return Err(bad(CMD, format!("`{primary}` is not a {} model", engine.feature())));
    }
    installed(&primary)?;
    let whisper = ma.whisper_model.clone();
    let fallback = (engine == models::Engine::Parakeet && filmcraft_speech::engine_available(models::Engine::Whisper))
        .then(|| {
            if whisper_ready(&whisper) {
                Some(whisper)
            } else {
                // any installed Whisper model will do
                let d = dir.clone()?;
                models::catalogue().iter().find(|m| m.engine == models::Engine::Whisper && models::installed(&d, m)).map(|m| m.id.to_string())
            }
        })
        .flatten()
        .map(Model::Load);
    let embedder = if diarize && filmcraft_speech::engine_available(models::Engine::Speaker) {
        installed(models::SPEAKER_MODEL)?;
        Some(Model::Load(models::SPEAKER_MODEL.to_string()))
    } else {
        None
    };
    Ok(Engines { primary: Model::Load(primary), fallback, embedder, dir })
}

/// Worker threads a job's own parallel work may use: half the cores (at most 4), so the editor
/// and playback keep theirs.
fn thread_cap() -> usize {
    std::thread::available_parallelism().map(|n| (n.get() / 2).clamp(1, 4)).unwrap_or(2)
}

pub(crate) fn generate(s: &mut Session, p: &Value) -> Result<Value> {
    if let Some(j) = s.transcribe_jobs.first() {
        return Err(EngineError::Other(format!("a transcription is already running (job {}); wait for it or transcript.cancel", j.job)));
    }
    let ma = &s.prefs.media_analysis;
    let diarize = bool_p(p, "diarize").unwrap_or(ma.speaker_labeling != "off");
    let language = match str_p(p, "language") {
        Some(l) => Some(l.trim().to_ascii_lowercase()).filter(|l| !l.is_empty() && l != "auto"),
        None if ma.language_auto_detect => None,
        None => Some(ma.default_language.clone()).filter(|l| !l.is_empty()),
    };
    let speakers = match p.get("speakers") {
        None | Some(Value::Null) => None,
        Some(Value::String(a)) if a.eq_ignore_ascii_case("auto") => None,
        Some(v) => Some(v.as_u64().filter(|n| (1..=32).contains(n)).ok_or_else(|| bad(CMD, "`speakers` is \"auto\" or a number from 1 to 32"))? as usize),
    };
    let max_speakers = u64_p(p, "maxSpeakers").map(|n| n.clamp(1, 32) as usize).unwrap_or(12);
    let replace = bool_p(p, "replace").unwrap_or(true);
    let wait = bool_p(p, "wait").unwrap_or(false) || cfg!(target_arch = "wasm32");
    let plan = plan(s, p, replace)?;
    if plan.units.is_empty() {
        let why = if plan.skipped.is_empty() {
            "nothing to transcribe: no audio in the scope".to_string()
        } else {
            format!(
                "nothing to transcribe: {}",
                plan.skipped
                    .iter()
                    .map(|v| format!("{} ({})", v["name"].as_str().unwrap_or(""), v["reason"].as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return Err(EngineError::Other(why));
    }
    let engines = engines(s, p, diarize)?;
    let engine_name = match &engines.primary {
        Model::Ready(t) => t.id(),
        Model::Load(id) => id.clone(),
    };
    let items: Vec<ItemId> = plan.units.iter().map(|u| u.item).collect();
    let audio_seconds: f64 = plan.units.iter().map(Unit::seconds).sum();
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let label = format!("Transcribe ({} clip{})", items.len(), if items.len() == 1 { "" } else { "s" });
    let job = crate::Job { id, label, progress: Default::default(), result: Default::default() };
    let input = Input { units: plan.units, engines, language, diarize, speakers, max_speakers, project: s.project.clone() };
    let outcome: Arc<Mutex<Option<std::result::Result<Outcome, String>>>> = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let (prog, out, fin) = (job.progress.clone(), outcome.clone(), done.clone());
    let body = move || {
        let mut rep = |d: u64, t: u64, status: &str| {
            prog.total.store(t, Ordering::Relaxed);
            prog.done.store(d.min(t), Ordering::Relaxed);
            *lock(&prog.status) = status.to_string();
            !prog.cancel.load(Ordering::Relaxed)
        };
        let r = match rayon::ThreadPoolBuilder::new().num_threads(thread_cap()).thread_name(|i| format!("filmcraft-transcribe-{i}")).build() {
            Ok(pool) => pool.install(|| run(&input, &mut rep)),
            Err(_) => run(&input, &mut rep),
        };
        *lock(&out) = Some(r);
        fin.store(true, Ordering::Release);
    };
    let body = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), body);
    s.jobs.push(job);
    s.transcribe_skipped.insert(id, plan.skipped.clone());
    s.transcribe_jobs.push(Pending { job: id, items: items.clone(), engine: engine_name.clone(), outcome, done, epoch: s.document_epoch });
    let names: Vec<Value> = items.iter().map(|i| json!({"item": i.0, "name": s.project.item(*i).map(|x| x.name.clone()).unwrap_or_default()})).collect();
    let mut reply = json!({"job": id, "items": names, "scope": plan.scope, "engine": engine_name, "audioSeconds": audio_seconds, "skipped": plan.skipped});
    if wait {
        body();
        poll(s);
        let res = s.jobs.iter().find(|j| j.id == id).and_then(|j| lock(&j.result).clone());
        match res {
            Some(Err(e)) => return Err(EngineError::Other(e)),
            _ => {
                if let Some(r) = report_of(s, id).cloned() {
                    // the same shape as before jobs: per item words, speakers, language, source
                    reply["items"] = r["items"].clone();
                    reply["result"] = r;
                }
            }
        }
    } else {
        std::thread::Builder::new().name("filmcraft-transcribe".into()).spawn(body).map_err(|e| EngineError::Other(e.to_string()))?;
    }
    Ok(reply)
}

/// `transcript.jobs`: running and recent transcription jobs.
pub(crate) fn jobs(s: &mut Session, _: &Value) -> Result<Value> {
    poll(s);
    let running: Vec<u64> = s.transcribe_jobs.iter().map(|j| j.job).collect();
    let reported: Vec<u64> = s.transcribe_reports.iter().map(|r| r.0).collect();
    let list: Vec<Value> = s
        .jobs
        .iter()
        .filter(|j| running.contains(&j.id) || reported.contains(&j.id) || j.label.starts_with("Transcribe ("))
        .map(|j| {
            let mut v = crate::export_tools::job_json(s, j);
            if let Some(p) = s.transcribe_jobs.iter().find(|x| x.job == j.id) {
                v["items"] = json!(p.items.iter().map(|i| i.0).collect::<Vec<_>>());
                v["engine"] = json!(p.engine);
            }
            v
        })
        .collect();
    Ok(Value::Array(list))
}

/// `transcript.cancel {job?}`: stop a transcription (all running ones without `job`). Nothing in
/// the project changes.
pub(crate) fn cancel(s: &mut Session, p: &Value) -> Result<Value> {
    let want = u64_p(p, "job");
    let ids: Vec<u64> = s.transcribe_jobs.iter().map(|j| j.job).filter(|j| want.is_none_or(|w| w == *j)).collect();
    if ids.is_empty() {
        return Err(EngineError::Other(match want {
            Some(j) => format!("job {j} is not a running transcription"),
            None => "no transcription is running".into(),
        }));
    }
    for j in s.jobs.iter().filter(|j| ids.contains(&j.id)) {
        j.progress.cancel.store(true, Ordering::Relaxed);
    }
    Ok(json!({"cancelled": ids}))
}

/// Seconds of speech per speaker name over the project's transcripts.
pub(crate) fn speaker_seconds(p: &Project) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    for t in p.transcripts.values() {
        for w in &t.words {
            if let Some(sp) = w.speaker.and_then(|i| t.speakers.get(i as usize)) {
                *out.entry(sp.name.clone()).or_default() += secs(w.end - w.start);
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "transcription_tests.rs"]
mod tests;
