//! Headless UI test of the Text panel's Transcript tab: the Transcribe button, the word view,
//! clicking words (In/Out from the selection) and extracting the selected text.
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also render the window offscreen with wgpu and write
//! `transcript-*.png` there; without it no GPU is needed.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

/// Reproducible redraw cost for a long sequence transcript, with and without many search hits.
/// Run alone with `--ignored --nocapture`; timings are observations, not CI thresholds.
#[test]
#[ignore = "manual transcript performance measurement"]
fn long_transcript_redraw_benchmark() {
    let mut d = Driver::demo();
    let item = d.harness.state().session.active_sequence().unwrap().audio_tracks[0].items[0].item;
    let words: Vec<Value> = (0..10_000)
        .map(|i| {
            json!({"text": "hello", "start": filmcraft_time::Tick::from_seconds_f64(i as f64 * 0.4).0,
            "end": filmcraft_time::Tick::from_seconds_f64(i as f64 * 0.4 + 0.3).0, "speaker": (i / 40) % 2})
        })
        .collect();
    d.ok(
        "engine.execute",
        json!({"command": "transcript.set", "params": {"item": item.0,
        "transcript": {"language": "en", "speakers": [{"name": "First"}, {"name": "Second"}], "words": words}}}),
    );
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.harness.state_mut().ui.text_tab = "Transcript".into();
    // Extend only the synthetic fixture clip so all 10,000 words are actually laid out.
    let session = &mut d.harness.state_mut().session;
    let id = session.state.active_sequence.unwrap();
    let seq = std::sync::Arc::make_mut(&mut session.project).sequence_mut(id).unwrap();
    seq.audio_tracks[0].items.truncate(1);
    seq.audio_tracks[0].items[0].source_in = filmcraft_time::Tick::ZERO;
    seq.audio_tracks[0].items[0].duration = filmcraft_time::Tick::from_seconds_f64(4000.0);
    assert_eq!(filmcraft_engine::transcript::sequence_words(session).len(), 10_000);
    for query in ["", "hello"] {
        d.harness.state_mut().ui.transcript_search = query.into();
        d.frames(5);
        let mut times = Vec::new();
        for _ in 0..40 {
            let start = std::time::Instant::now();
            d.frames(1);
            times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!("transcript words=10000 query={query:?} p50_ms={:.3} p95_ms={:.3}", times[20], times[38]);
    }
}

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<std::path::PathBuf>,
}

impl Driver {
    fn demo() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(std::path::PathBuf::from);
        let mut b = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000);
        if snapshots.is_some() {
            b = b.wgpu();
        }
        let harness = b.build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, snapshots };
        d.frames(4);
        d
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
                return v["result"].clone();
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ids(&mut self, prefix: &str) -> Vec<String> {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.as_array().unwrap().iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }

    fn snapshot(&mut self, name: &str) {
        let Some(dir) = self.snapshots.clone() else { return };
        self.frames(2);
        match self.harness.render() {
            Ok(img) => {
                std::fs::create_dir_all(&dir).unwrap();
                img.save(dir.join(format!("transcript-{name}.png"))).unwrap();
            }
            Err(e) => eprintln!("snapshot {name} skipped: {e}"),
        }
    }
}

#[test]
fn transcript_tab_selects_and_extracts_words() {
    let mut d = Driver::demo();
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "text.tab.Transcript"}));
    d.frames(3);
    assert_eq!(d.ids("text.transcript.generate"), vec!["text.transcript.generate".to_string()], "empty state offers Transcribe");

    // bring a transcript for the first A1 clip's media (as an agent without a speech model would)
    let mut probe = Session::default();
    probe.execute("file.openDemoProject", json!({})).unwrap();
    let a = probe.active_sequence().unwrap().audio_tracks[0].items[0].clone();
    let tk = |s: f64| a.source_in.0 + (s * filmcraft_time::TICKS_PER_SECOND as f64) as i64;
    let words: Vec<Value> = ["Hello", "um", "there", "friend."]
        .iter()
        .enumerate()
        .map(|(i, w)| json!({"text": w, "start": tk(0.5 + i as f64 * 0.5), "end": tk(0.9 + i as f64 * 0.5), "speaker": 0}))
        .collect();
    d.ok("engine.execute", json!({"command": "transcript.set", "params": {"item": a.item.0, "transcript": {"language": "en", "words": words}}}));
    d.frames(3);
    let ids = d.ids("text.transcript.word.");
    assert_eq!(ids.len(), 4, "{ids:?}");
    d.snapshot("words");

    d.ok("ui.click", json!({"id": "text.transcript.word.1"}));
    d.frames(3);
    d.ok("ui.click", json!({"id": "text.transcript.extract"}));
    d.frames(3);
    let r = d.ok("engine.execute", json!({"command": "transcript.inspect", "params": {}}));
    let left: Vec<&str> = r["words"].as_array().unwrap().iter().filter_map(|w| w["text"].as_str()).collect();
    assert_eq!(left, ["Hello", "there", "friend."]);
    assert_eq!(d.ids("text.transcript.word.").len(), 3);
}

/// #2402: without speech-to-text (a build without `whisper`) the Transcribe button is disabled,
/// with the reason as its tooltip, instead of looking ready and doing nothing visible.
#[test]
fn transcribe_is_disabled_without_speech_to_text() {
    if filmcraft_engine::transcript::speech_available() {
        return;
    }
    let mut d = Driver::demo();
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "text.tab.Transcript"}));
    d.frames(3);
    let els = d.ok("ui.elements", json!({"prefix": "text.transcript.generate"}));
    let button = els.as_array().unwrap().iter().find(|e| e["id"] == "text.transcript.generate").expect("the Transcribe button").clone();
    assert_eq!(button["enabled"], json!(false), "{button}");
    let undo = d.harness.state().session.history.undo.len();
    d.ok("ui.click", json!({"id": "text.transcript.generate"}));
    d.frames(3);
    assert_eq!(d.harness.state().session.history.undo.len(), undo, "nothing ran");
}

/// The Transcribe dialog opens from the Text panel, the timeline's clip and track menus and the
/// Project panel (each preset to its scope), runs the job in the background, and the Transcript
/// tab then shows each speaker in its colour with a menu to rename, merge and reassign.
/// `engine.execute transcript.generate` (automation) never opens the dialog.
#[test]
fn transcribe_dialog_runs_a_background_job_and_speakers_can_be_renamed() {
    use filmcraft_project::{Transcript, Word};
    use filmcraft_time::Tick;
    let mut d = Driver::demo();
    let mut t = Transcript { language: "en".into(), ..Default::default() };
    t.words.push(Word::new("Hello", Tick::from_seconds_f64(0.1), Tick::from_seconds_f64(0.4)));
    d.harness.state_mut().session.transcriber = Some(std::sync::Arc::new(filmcraft_speech::FixedTranscriber { transcript: t, id: "fixed".into() }));
    d.ok("ui.set", json!({"workspace": "Captions and Graphics"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "text.tab.Transcript"}));
    d.frames(3);

    // the empty state's button opens the dialog (the whole sequence: nothing is selected)
    d.ok("ui.click", json!({"id": "text.transcript.generate"}));
    d.frames(3);
    let dialog = |d: &mut Driver| d.harness.state().ui.extras.dialog.clone();
    let open = dialog(&mut d).expect("the Transcribe dialog");
    assert_eq!(open.command, "transcript.generate");
    assert_eq!(open.params["scope"], "sequence", "{}", open.params);
    assert_eq!(open.params["diarize"], json!(true), "speaker detection is on by default");
    assert_eq!(open.params["replace"], json!(true));
    for id in ["transcribe.ok", "transcribe.cancel", "transcribe.scope.sequence", "transcribe.scope.track", "transcribe.diarize", "transcribe.replace"] {
        assert_eq!(d.ids(id), vec![id.to_string()], "{id}");
    }
    d.ok("ui.click", json!({"id": "transcribe.cancel"}));
    d.frames(2);
    assert!(dialog(&mut d).is_none());

    // a track header's menu presets one track; the Project panel's item menu, the items
    d.ok("ui.menu.invoke", json!({"id": "transcript.generate", "params": {"dialog": {"scope": "track", "track": "A1"}}}));
    d.frames(2);
    let open = dialog(&mut d).unwrap();
    assert_eq!((open.params["scope"].as_str(), open.params["track"].as_str()), (Some("track"), Some("A1")));
    d.ok("ui.click", json!({"id": "transcribe.cancel"}));
    d.frames(2);

    // automation runs the job directly; the dialog stays closed
    let undo = d.harness.state().session.history.undo.len();
    let r = d.ok("engine.execute", json!({"command": "transcript.generate", "params": {"track": "A1", "diarize": false}}));
    assert!(r["job"].is_u64(), "{r}");
    assert!(dialog(&mut d).is_none());
    for _ in 0..600 {
        if d.harness.state().session.history.undo.len() > undo {
            break;
        }
        d.frames(1);
    }
    assert_eq!(d.harness.state().session.history.undo.len(), undo + 1, "one undo step");
    let words = d.ok("engine.execute", json!({"command": "transcript.inspect", "params": {}}));
    // every media item of A1 has a transcript by the fake recogniser
    assert!(!words["items"].as_array().unwrap().is_empty() && words["items"].as_array().unwrap().iter().all(|i| i["source"] == "fixed"), "{words}");

    // the dialog's own OK runs the job too (sequence scope, replacing what is there)
    d.ok("ui.menu.invoke", json!({"id": "sequence.transcribe", "params": {}}));
    d.frames(2);
    assert_eq!(dialog(&mut d).unwrap().params["scope"], "sequence");
    d.ok("ui.click", json!({"id": "transcribe.ok"}));
    d.frames(2);
    assert!(dialog(&mut d).is_none(), "OK closes the dialog: {:?}", dialog(&mut d).map(|x| x.error));
    for _ in 0..600 {
        if d.harness.state().session.history.undo.len() > undo + 1 {
            break;
        }
        d.frames(1);
    }
    assert_eq!(d.harness.state().session.history.undo.len(), undo + 2);

    // two named speakers: their paragraphs have a menu that renames one (remembering the voice)
    let a = d.harness.state().session.active_sequence().unwrap().audio_tracks[0].items[0].clone();
    let tk = |s: f64| a.source_in.0 + (s * filmcraft_time::TICKS_PER_SECOND as f64) as i64;
    let words: Vec<Value> = ["One", "two", "three", "four"]
        .iter()
        .enumerate()
        .map(|(i, w)| json!({"text": w, "start": tk(0.5 + i as f64 * 0.5), "end": tk(0.9 + i as f64 * 0.5), "speaker": i / 2}))
        .collect();
    d.ok(
        "engine.execute",
        json!({"command": "transcript.set", "params": {"item": a.item.0, "transcript": {"language": "en", "speakers": [{"name": "Speaker 1"}, {"name": "Speaker 2"}], "words": words}}}),
    );
    d.frames(3);
    let els = d.ok("ui.elements", json!({"prefix": "text.transcript.paragraph.0"}));
    let head = els.as_array().unwrap().iter().find(|e| e["id"] == "text.transcript.paragraph.0").expect("paragraph header").clone();
    assert_eq!(head["label"], "Speaker 1", "{head}");
    let rect = head["rect"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect::<Vec<_>>();
    d.ok("ui.click", json!({"x": rect[0] + 10.0, "y": rect[1] + rect[3] / 2.0, "button": "right"}));
    d.frames(3);
    for id in ["text.transcript.paragraph.0.rename", "text.transcript.paragraph.0.merge", "text.transcript.paragraph.0.assign"] {
        assert_eq!(d.ids(id).first().map(String::as_str), Some(id), "the speaker menu offers {id}");
    }
    d.ok("ui.click", json!({"id": "text.transcript.paragraph.0.rename"}));
    d.frames(1);
    // the field starts with the current name; the click put the cursor in it
    d.ok("ui.key", json!({"key": "End"}));
    for _ in 0.."Speaker 1".len() {
        d.ok("ui.key", json!({"key": "Backspace"}));
    }
    d.ok("ui.type", json!({"text": "Ada"}));
    d.frames(2);
    d.ok("ui.click", json!({"id": "text.transcript.paragraph.0.renameOk"}));
    d.frames(3);
    let sp = d.ok("engine.execute", json!({"command": "transcript.speakers", "params": {}}));
    assert!(sp.to_string().contains("Ada"), "{sp}");
}
