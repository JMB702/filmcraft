//! Transcription jobs with a fake recogniser and a fake voice model: scopes, background job and
//! cancel, speakers across clips and their names, fallback to the second engine, merging.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::*;
use crate::Session;
use filmcraft_project::{ClipId, ItemId, Transcript, Voice, Word};
use filmcraft_speech::speakers::Embedder;
use filmcraft_speech::{Options, ProgressFn, SpeechError, Transcriber};

/// A word every 0.4 s wherever the audio is not silent.
struct Words {
    id: &'static str,
    languages: Option<&'static [&'static str]>,
    fail: bool,
    calls: Arc<AtomicUsize>,
}

impl Transcriber for Words {
    fn id(&self) -> String {
        self.id.into()
    }
    fn languages(&self) -> Option<&'static [&'static str]> {
        self.languages
    }
    fn detect_language(&self, _: &[f32]) -> std::result::Result<Option<String>, SpeechError> {
        Ok(Some("de".into()))
    }
    fn transcribe(&self, audio: &[f32], opts: &Options, progress: ProgressFn) -> std::result::Result<Transcript, SpeechError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let (Some(l), Some(ls)) = (&opts.language, self.languages)
            && !ls.contains(&l.as_str())
        {
            return Err(SpeechError::Language(l.clone()));
        }
        if self.fail {
            return Err(SpeechError::Model("broken".into()));
        }
        let mut t = Transcript { language: opts.language.clone().unwrap_or_else(|| "en".into()), source: self.id.into(), ..Default::default() };
        let step = 6400; // 0.4 s
        let mut i = 0;
        while i + step <= audio.len() {
            if !progress(i as f32 / audio.len() as f32, "") {
                return Err(SpeechError::Cancelled);
            }
            if audio[i..i + step].iter().any(|v| v.abs() > 1e-4) {
                t.words.push(Word::new(
                    format!("w{}", t.words.len()),
                    filmcraft_speech::sample_tick(i as i64),
                    filmcraft_speech::sample_tick((i + step - 800) as i64),
                ));
            }
            i += step;
        }
        Ok(t)
    }
}

fn words(id: &'static str) -> (Arc<Words>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (Arc::new(Words { id, languages: None, fail: false, calls: calls.clone() }), calls)
}

/// A voice is the audio's level: one direction per level.
struct Levels;

impl Embedder for Levels {
    fn id(&self) -> String {
        "levels".into()
    }
    fn embed(&self, audio: &[f32]) -> std::result::Result<Vec<f32>, SpeechError> {
        let m = audio.iter().map(|v| v.abs()).fold(0f32, f32::max);
        let mut v = vec![0.02f32; 8];
        v[((m * 10.0).round() as usize).min(7)] += 1.0;
        filmcraft_speech::speakers::normalize(&mut v);
        Ok(v)
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("filmcraft-transcription-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A mono 16 kHz WAV of `parts`: `(seconds, level)` runs ("speech" is a constant level).
fn wav(dir: &Path, name: &str, parts: &[(f64, f32)]) -> std::path::PathBuf {
    let mut x = Vec::new();
    for &(sec, level) in parts {
        x.extend(std::iter::repeat_n(level, (sec * 16_000.0) as usize));
    }
    let p = dir.join(name);
    std::fs::write(&p, crate::voiceover::write_wav_f32(&x, 16_000)).unwrap();
    p
}

fn import(s: &mut Session, p: &Path) -> ItemId {
    let r = s.execute("file.import", json!({"paths": [p.to_string_lossy()]})).unwrap();
    ItemId(r["items"][0].as_u64().unwrap())
}

fn place(s: &mut Session, item: ItemId, track: &str, seconds: f64) -> ClipId {
    let r = s.execute("timeline.place", json!({"item": item.0, "track": track, "seconds": seconds})).unwrap();
    ClipId(r["clips"][0].as_u64().unwrap())
}

/// Poll until job `id` is finished (as an agent polling `jobs.list` would).
fn wait(s: &mut Session, id: u64) -> Value {
    for _ in 0..3000 {
        let jobs = s.execute("jobs.list", json!({})).unwrap();
        if let Some(j) = jobs.as_array().unwrap().iter().find(|j| j["id"] == id)
            && j["finished"] == true
        {
            return j.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("job {id} did not finish");
}

/// Three clips on A1: Ann (level 0.1), then Ann and Bo (0.3) taking turns, then Bo.
fn three_clips(dir: &Path) -> (Session, Vec<ItemId>, Vec<ClipId>) {
    let mut s = Session::default();
    let (t, _) = words("words");
    s.transcriber = Some(t);
    s.speaker_embedder = Some(Arc::new(Levels));
    let a = wav(dir, "a.wav", &[(4.0, 0.1)]);
    let b = wav(dir, "b.wav", &[(3.0, 0.1), (0.5, 0.0), (3.0, 0.3)]);
    let c = wav(dir, "c.wav", &[(4.0, 0.3)]);
    let items: Vec<ItemId> = [a, b, c].iter().map(|p| import(&mut s, p)).collect();
    s.execute("file.newSequence", json!({"video": 1, "audio": 2})).unwrap();
    let clips = vec![place(&mut s, items[0], "A1", 0.0), place(&mut s, items[1], "A1", 5.0), place(&mut s, items[2], "A1", 12.0)];
    (s, items, clips)
}

#[test]
fn three_selected_clips_in_the_background_with_speakers() {
    let dir = tmp("three");
    let (mut s, items, clips) = three_clips(&dir);
    s.state.selection = clips.clone();
    let undo = s.history.undo.len();
    let r = s.execute("transcript.generate", json!({})).unwrap();
    assert_eq!(r["scope"], "selection", "{r}");
    assert_eq!(r["items"].as_array().unwrap().len(), 3, "{r}");
    let id = r["job"].as_u64().unwrap();
    let j = wait(&mut s, id);
    assert!(j["result"]["items"].is_array(), "the job reports what it did: {j}");
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    // every word has a speaker; one voice has one name in every clip
    let name = |s: &Session, i: ItemId, w: usize| {
        let t = &s.project.transcripts[&i];
        t.speaker_name(&t.words[w]).unwrap()
    };
    for i in &items {
        let t = &s.project.transcripts[i];
        assert!(!t.words.is_empty());
        assert!(t.words.iter().all(|w| w.speaker.is_some()), "{t:?}");
        assert_eq!(t.voice_model, "levels");
    }
    let ann = name(&s, items[0], 0);
    let bo = name(&s, items[2], 0);
    assert_ne!(ann, bo);
    assert_eq!(name(&s, items[1], 0), ann, "the same voice in another clip");
    let last = s.project.transcripts[&items[1]].words.len() - 1;
    assert_eq!(name(&s, items[1], last), bo);
    assert_eq!((ann.as_str(), bo.as_str()), ("Speaker 1", "Speaker 2"));
    let sp = s.execute("transcript.speakers", json!({})).unwrap();
    assert_eq!(sp.as_array().unwrap().len(), 2, "{sp}");
    // undo removes all three transcripts at once
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.transcripts.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn named_voices_carry_over_and_speaker_commands() {
    let dir = tmp("voices");
    let (mut s, items, clips) = three_clips(&dir);
    s.execute("transcript.generate", json!({"clips": [clips[0].0], "wait": true})).unwrap();
    // naming a speaker saves the voice in the project
    let r = s.execute("transcript.renameSpeaker", json!({"speaker": "Speaker 1", "name": "Ann"})).unwrap();
    assert_eq!(r["voiceSaved"], true);
    assert_eq!(s.project.voices.len(), 1);
    assert_eq!(s.project.voices[0].model, "levels");
    // a later job knows Ann; the new voice is numbered after the project's speakers
    let r = s.execute("transcript.generate", json!({"clips": [clips[1].0, clips[2].0], "wait": true})).unwrap();
    assert_eq!(r["items"].as_array().unwrap().len(), 2, "{r}");
    let t = &s.project.transcripts[&items[1]];
    assert_eq!(t.speaker_name(&t.words[0]).as_deref(), Some("Ann"));
    let t = &s.project.transcripts[&items[2]];
    assert_eq!(t.speaker_name(&t.words[0]).as_deref(), Some("Speaker 1"), "Speaker 1 was renamed: its number is free again");
    // merge Speaker 1 into Ann everywhere
    s.execute("transcript.mergeSpeakers", json!({"from": "Speaker 1", "into": "Ann"})).unwrap();
    let t = &s.project.transcripts[&items[2]];
    assert!(t.words.iter().all(|w| t.speaker_name(w).as_deref() == Some("Ann")));
    assert!(s.project.transcripts.values().all(|t| t.speakers.iter().filter(|x| x.name == "Ann").count() <= 1), "one Ann per transcript");
    // reassign a word range (sequence word indices)
    let n = s.execute("transcript.inspect", json!({})).unwrap()["words"].as_array().unwrap().len();
    let r = s.execute("transcript.assignSpeaker", json!({"from": n - 2, "to": n - 1, "speaker": "Bo"})).unwrap();
    assert_eq!(r["assigned"], 2);
    let w = s.execute("transcript.inspect", json!({})).unwrap()["words"].clone();
    assert_eq!(w[n - 1]["speaker"], "Bo");
    assert_eq!(w[n - 3]["speaker"], "Ann");
    assert!(s.execute("transcript.assignSpeaker", json!({"from": n, "speaker": "Bo"})).is_err());
    assert!(s.execute("transcript.assignSpeaker", json!({"from": 0, "to": 1})).is_err(), "needs a name");
    // voices list and forget
    assert_eq!(s.execute("transcript.voices", json!({})).unwrap().as_array().unwrap().len(), 1);
    s.execute("transcript.forgetVoice", json!({"name": "Ann"})).unwrap();
    assert!(s.project.voices.is_empty());
    assert!(s.execute("transcript.forgetVoice", json!({"name": "Ann"})).is_err());
    // voices survive save and load
    s.execute("edit.undo", json!({})).unwrap();
    let back = filmcraft_format::decode(&filmcraft_format::encode(&s.project, true)).unwrap();
    assert_eq!(back.project.voices, s.project.voices);
    assert!(back.project.transcripts[&items[1]].speakers.iter().any(|x| !x.embedding.is_empty()));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn one_track_inside_in_out_merges_with_what_was_there() {
    let dir = tmp("track");
    let (mut s, items, _) = three_clips(&dir);
    let long = wav(&dir, "long.wav", &[(20.0, 0.1)]);
    let long = import(&mut s, &long);
    place(&mut s, long, "A2", 0.0);
    // a transcript of the whole file from before
    let mut old = Transcript { language: "en".into(), source: "manual".into(), ..Default::default() };
    for k in 0..20 {
        old.words.push(Word::new(format!("old{k}"), Tick::from_seconds_f64(k as f64), Tick::from_seconds_f64(k as f64 + 0.5)));
    }
    s.execute("transcript.set", json!({"item": long.0, "transcript": old})).unwrap();
    let fd = s.sequence_rate().frame_duration();
    let q = s.state.active_sequence.unwrap();
    s.edit_sequence("marks", |q, _, _| {
        q.mark_in = Some(Tick::from_seconds_f64(5.0));
        q.mark_out = Some(Tick::from_seconds_f64(10.0) - fd);
        Ok(())
    })
    .unwrap();
    assert!(s.project.sequence(q).is_some());
    let r = s.execute("transcript.generate", json!({"track": "A2", "range": "inOut", "wait": true})).unwrap();
    assert_eq!(r["scope"], "track");
    assert_eq!(r["items"].as_array().unwrap().len(), 1, "only A2's media: {r}");
    assert!(!s.project.transcripts.contains_key(&items[0]), "A1 untouched");
    let t = &s.project.transcripts[&long];
    let texts: Vec<&str> = t.words.iter().map(|w| w.text.as_str()).collect();
    assert!(texts.contains(&"old2") && texts.contains(&"old15"), "words outside In/Out stay: {texts:?}");
    assert!(!texts.contains(&"old7"), "words inside In/Out are replaced: {texts:?}");
    assert!(texts.iter().any(|w| w.starts_with('w')), "{texts:?}");
    let new_first = t.words.iter().find(|w| w.text.starts_with('w')).unwrap();
    assert!(new_first.start >= Tick::from_seconds_f64(4.4), "the range with its handles, in media time: {:?}", new_first.start);
    t.check().unwrap();
    // replace: false skips media that already has a transcript
    let e = s.execute("transcript.generate", json!({"track": "A2", "replace": false, "wait": true})).unwrap_err().to_string();
    assert!(e.contains("already transcribed"), "{e}");
    assert!(s.execute("transcript.generate", json!({"track": "A2", "range": "bogus"})).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn other_languages_and_failures_go_to_the_second_engine() {
    let dir = tmp("fallback");
    let (mut s, items, clips) = three_clips(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    s.transcriber = Some(Arc::new(Words { id: "parakeet-fake", languages: Some(&["en"]), fail: false, calls: calls.clone() }));
    let (whisper, whisper_calls) = words("whisper-fake");
    s.transcriber_fallback = Some(whisper);
    // German given: Whisper, without asking Parakeet
    let r = s.execute("transcript.generate", json!({"clips": [clips[0].0], "language": "de", "wait": true})).unwrap();
    assert_eq!(r["items"][0]["source"], "whisper-fake", "{r}");
    assert_eq!(r["items"][0]["fallback"], true);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(s.project.transcripts[&items[0]].source, "whisper-fake");
    // auto: Whisper detects German (the fake says so) and transcribes it
    let r = s.execute("transcript.generate", json!({"clips": [clips[1].0], "language": "auto", "wait": true})).unwrap();
    assert_eq!(r["items"][0]["source"], "whisper-fake");
    assert_eq!(r["items"][0]["language"], "de");
    // Parakeet fails: Whisper stands in, and the job says so
    s.transcriber = Some(Arc::new(Words { id: "parakeet-fake", languages: Some(&["en"]), fail: true, calls }));
    let r = s.execute("transcript.generate", json!({"clips": [clips[2].0], "language": "en", "wait": true})).unwrap();
    assert_eq!(r["items"][0]["source"], "whisper-fake");
    assert!(r["result"]["warnings"][0].as_str().unwrap().contains("failed"), "{r}");
    assert!(whisper_calls.load(Ordering::SeqCst) >= 3);
    // without a second engine, an unsupported language is skipped with a reason
    s.transcriber_fallback = None;
    let e = s.execute("transcript.generate", json!({"clips": [clips[0].0], "language": "de", "wait": true})).unwrap_err().to_string();
    assert!(e.contains("does not transcribe"), "{e}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Waits for the test to let it go, so the job can be cancelled while it runs.
struct Slow(Arc<std::sync::atomic::AtomicBool>);

impl Transcriber for Slow {
    fn id(&self) -> String {
        "slow".into()
    }
    fn transcribe(&self, _: &[f32], _: &Options, progress: ProgressFn) -> std::result::Result<Transcript, SpeechError> {
        for _ in 0..4000 {
            if !progress(0.5, "") {
                return Err(SpeechError::Cancelled);
            }
            if self.0.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(Transcript::default())
    }
}

#[test]
fn cancelling_changes_nothing() {
    let dir = tmp("cancel");
    let (mut s, _, clips) = three_clips(&dir);
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    s.transcriber = Some(Arc::new(Slow(release.clone())));
    let before = s.project.to_json();
    let r = s.execute("transcript.generate", json!({"clips": clips.iter().map(|c| c.0).collect::<Vec<_>>()})).unwrap();
    let id = r["job"].as_u64().unwrap();
    // one job at a time
    assert!(s.execute("transcript.generate", json!({"clips": [clips[0].0]})).unwrap_err().to_string().contains("already running"));
    let listed = s.execute("transcript.jobs", json!({})).unwrap();
    assert!(listed.as_array().unwrap().iter().any(|j| j["id"] == id), "{listed}");
    s.execute("transcript.cancel", json!({"job": id})).unwrap();
    let j = wait(&mut s, id);
    assert!(j["result"]["error"].as_str().unwrap().starts_with("stopped"), "{j}");
    assert_eq!(s.project.to_json(), before);
    assert!(s.execute("transcript.cancel", json!({})).is_err(), "nothing left to cancel");
    release.store(true, Ordering::SeqCst);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hostile_parameters_are_errors() {
    let dir = tmp("hostile");
    let (mut s, _, clips) = three_clips(&dir);
    for p in [
        json!({"speakers": 0}),
        json!({"speakers": 99}),
        json!({"speakers": "many"}),
        json!({"speakers": -1}),
        json!({"track": "A99"}),
        json!({"track": "V1"}),
        json!({"track": 1e300}),
        json!({"range": "inOut", "sequence": true}),
        json!({"items": [999_999]}),
        json!({"clips": [999_999]}),
        json!({"engine": "nope", "clips": [clips[0].0]}),
    ] {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("transcript.generate", p.clone())));
        assert!(matches!(r, Ok(Err(_))), "{p} should be an error: {r:?}");
    }
    for (cmd, p) in [
        ("transcript.renameSpeaker", json!({"speaker": "x", "name": "a".repeat(10_000)})),
        ("transcript.mergeSpeakers", json!({"from": [], "into": "A"})),
        ("transcript.mergeSpeakers", json!({"from": "A", "into": "A"})),
        ("transcript.cancel", json!({"job": u64::MAX})),
        ("transcript.forgetVoice", json!({})),
        ("transcript.downloadModel", json!({"model": "../../etc"})),
    ] {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(cmd, p.clone())));
        assert!(matches!(r, Ok(Err(_))), "{cmd} {p} should be an error: {r:?}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn nested_sequences_reach_their_media_once() {
    let dir = tmp("nested");
    let (mut s, items, clips) = three_clips(&dir);
    s.state.selection = vec![clips[0], clips[1]];
    let inner = ItemId(s.execute("clip.nest", json!({"name": "Inner"})).unwrap()["sequence"].as_u64().unwrap());
    // the Project panel's sequence item: the media of its audio clips
    let r = s.execute("transcript.generate", json!({"items": [inner.0], "wait": true})).unwrap();
    let got: Vec<u64> = r["items"].as_array().unwrap().iter().map(|x| x["item"].as_u64().unwrap()).collect();
    assert_eq!(got, vec![items[0].0, items[1].0], "{r}");
    // a sequence nested in itself is bounded, not endless
    let mut scope = Scope::new(&s.project);
    scope.add_item(inner);
    assert_eq!(scope.drafts.len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ranges_channels_and_cuts() {
    let mut d = Draft {
        ranges: vec![TimeRange::from_bounds(Tick(50), Tick(80)), TimeRange::from_bounds(Tick(-5), Tick(20)), TimeRange::from_bounds(Tick(10), Tick(30))],
        ..Default::default()
    };
    d.finish(Tick(70));
    assert_eq!(d.ranges, vec![TimeRange::from_bounds(Tick(0), Tick(30)), TimeRange::from_bounds(Tick(50), Tick(70))]);
    assert!(!d.whole);
    let mut d = Draft { ranges: vec![TimeRange::from_bounds(Tick(0), Tick(100))], ..Default::default() };
    d.finish(Tick(70));
    assert!(d.whole);
    let mut d = Draft::default();
    d.finish(Tick::ZERO);
    assert!(d.ranges.is_empty());
    // a clip with no mapping hears the first two channels; a recorder's mic channel only itself
    let f = streams(&[Feed { stream: 0, channels: vec![] }, Feed { stream: 0, channels: vec![5] }, Feed { stream: 2, channels: vec![1] }]);
    assert_eq!(f[&0].iter().copied().collect::<Vec<_>>(), vec![0, 1, 5]);
    assert_eq!(f[&2].iter().copied().collect::<Vec<_>>(), vec![1]);
    // pieces end in the quietest part of their last 30 s
    let mut a = vec![0.5f32; 16_000 * 60];
    for v in &mut a[16_000 * 50..16_000 * 50 + 4000] {
        *v = 0.0;
    }
    let cut = quiet_cut(&a);
    assert!((16_000 * 50..16_000 * 50 + 4000).contains(&cut), "{cut}");
    assert_eq!(quiet_cut(&[0.1; 100]), 100);
    assert_eq!(quiet_cut(&[]), 0);
}

#[test]
fn merging_keeps_speakers_by_name() {
    let mut old = Transcript { language: "en".into(), source: "a".into(), ..Default::default() };
    old.speakers = vec![filmcraft_project::Speaker::new("Ann"), filmcraft_project::Speaker::new("Bo")];
    for (k, sp) in [(0, 0), (5, 1), (9, 0)] {
        let mut w = Word::new(format!("o{k}"), Tick::from_seconds_f64(k as f64), Tick::from_seconds_f64(k as f64 + 0.5));
        w.speaker = Some(sp);
        old.words.push(w);
    }
    let mut new = Transcript { language: "en".into(), source: "b".into(), voice_model: "levels".into(), ..Default::default() };
    new.speakers = vec![filmcraft_project::Speaker { name: "Bo".into(), embedding: vec![1.0, 0.0] }, filmcraft_project::Speaker::new("Cy")];
    for (k, sp) in [(4.0, 1), (6.0, 0)] {
        let mut w = Word::new("n", Tick::from_seconds_f64(k), Tick::from_seconds_f64(k + 0.5));
        w.speaker = Some(sp);
        new.words.push(w);
    }
    let m = merge(&old, new, &[TimeRange::from_bounds(Tick::from_seconds_f64(3.0), Tick::from_seconds_f64(8.0))]);
    let names: Vec<String> = m.words.iter().map(|w| m.speaker_name(w).unwrap()).collect();
    assert_eq!(names, ["Ann", "Cy", "Bo", "Ann"]);
    assert_eq!(m.speakers.iter().find(|s| s.name == "Bo").unwrap().embedding, vec![1.0, 0.0]);
    assert_eq!(m.source, "b");
    m.check().unwrap();
}

#[test]
fn clusters_take_known_names_then_number_on() {
    let mut p = Project::new("t");
    p.voices.push(Voice { name: "Ann".into(), model: "m".into(), embedding: vec![1.0, 0.0, 0.0], seconds: 3.0 });
    p.voices.push(Voice { name: "Speaker 4".into(), model: "m".into(), embedding: vec![0.0, 0.0, 1.0], seconds: 3.0 });
    let names = name_clusters(&p, "m", &[vec![0.0, 1.0, 0.0], vec![0.99, 0.1, 0.0], vec![0.0, 0.2, 0.0]]);
    assert_eq!(names, ["Speaker 5", "Ann", "Speaker 6"]);
    // another model's voices are not compared
    assert_eq!(name_clusters(&p, "other", &[vec![1.0, 0.0, 0.0]]), ["Speaker 5"]);
}
