//! Headless UI tests of trimming several selected clips at once: with three clips selected, dragging
//! the Out edge of one of them trims the Out edge of all three (and their linked sounds) by the
//! same amount, in one undo step, and the drag stops for all of them at the first limit any of them
//! reaches. Dragging the edge of a clip that isn't selected trims only that clip, as before.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::ClipId;
use filmcraft_time::Tick;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    pointer: egui::Pos2,
    /// (picture, sound) of the clip on V1/A1, V2/A2 and V3/A3.
    clips: Vec<(ClipId, ClipId)>,
}

impl Driver {
    /// Three 2 s A/V clips on V1..V3 (sound on A1..A3), starting 0.5 s apart, with `tail[k]`
    /// frames of media after clip k's Out point.
    fn three(tail: [i64; 3]) -> Self {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s.execute("file.newSequence", json!({"name": "Multi-trim", "video": 3, "audio": 3})).unwrap();
        let rate = s.sequence_rate();
        let item = s.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).unwrap();
        let media = filmcraft_engine::media_duration(&s.project, &s.media, item).unwrap();
        let dur = rate.tick_of(48);
        for (k, t) in tail.into_iter().enumerate() {
            let source_in = media - dur - rate.tick_of(t);
            assert!(source_in >= Tick::ZERO, "the demo clip is long enough");
            let p = json!({"item": item.0, "track": format!("V{}", k + 1), "audioTrack": format!("A{}", k + 1), "seconds": 1.0 + 0.5 * k as f64, "sourceIn": source_in.0, "duration": dur.0});
            s.execute("timeline.place", p).unwrap();
        }
        s.execute("edit.deselectAll", json!({})).unwrap();
        let q = s.active_sequence().unwrap();
        let clips = (0..3).map(|k| (q.video_tracks[k].items[0].id, q.audio_tracks[k].items[0].id)).collect();
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(s).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, pointer: egui::Pos2::ZERO, clips };
        d.frames(4);
        d.ok("ui.set", json!({"timeline": {"pps": 100.0, "scroll": 0.0}}));
        d.frames(6);
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

    /// Run an engine command over the control channel, as an agent does.
    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }

    /// The on-screen rect `[x, y, w, h]` of a clip.
    fn clip_rect(&mut self, c: ClipId) -> [f32; 4] {
        let id = format!("timeline.clip.{}", c.0);
        let v = self.ok("ui.elements", json!({"prefix": id}));
        let e = v.as_array().unwrap().iter().find(|e| e["id"] == json!(id)).expect("the clip is drawn").clone();
        let r = e["rect"].as_array().unwrap();
        [0, 1, 2, 3].map(|k| r[k].as_f64().unwrap() as f32)
    }

    /// A point on `c`'s Out edge that grabs it for a regular trim.
    fn out_edge(&mut self, c: ClipId) -> egui::Pos2 {
        let at = self.ok("ui.timeline.locate", json!({"clip": c.0, "edge": "out"}));
        let (x, y) = (at["x"].as_f64().unwrap() as f32, at["y"].as_f64().unwrap() as f32);
        let p = [x, x + 1.0, x - 1.0, x + 2.0]
            .into_iter()
            .find(|&x| {
                let h = self.ok("ui.timeline.hit", json!({"x": x, "y": y}));
                h["clip"] == json!(c.0) && h["edge"] == json!("Out") && h["kind"] == json!("trim")
            })
            .expect("the Out edge grabs a trim");
        egui::pos2(p, y)
    }

    fn press(&mut self, at: egui::Pos2) {
        let events = &mut self.harness.input_mut().events;
        events.push(egui::Event::PointerMoved(at));
        events.push(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
        self.pointer = at;
        self.frames(1);
    }

    fn move_to(&mut self, to: egui::Pos2) {
        let from = self.pointer;
        let steps = ((to - from).length() / 20.0).ceil().max(1.0) as usize;
        for i in 1..=steps {
            self.harness.input_mut().events.push(egui::Event::PointerMoved(from + (to - from) * (i as f32 / steps as f32)));
            self.frames(1);
        }
        self.pointer = to;
    }

    fn release(&mut self) {
        let pos = self.pointer;
        self.harness.input_mut().events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        });
        self.frames(3);
    }

    fn end(&self, c: ClipId) -> Tick {
        self.harness.state().session.active_sequence().unwrap().find_item(c).map(|(_, i)| i.end()).unwrap()
    }

    fn all(&self) -> Vec<ClipId> {
        self.clips.iter().flat_map(|(v, a)| [*v, *a]).collect()
    }

    fn select_pictures(&mut self) {
        let ids: Vec<u64> = self.clips.iter().map(|c| c.0.0).collect();
        self.exec("timeline.select", json!({"clips": ids}));
    }
}

#[test]
fn dragging_one_edge_with_three_clips_selected_trims_all_three_in_one_undo_step() {
    let mut d = Driver::three([48; 3]);
    d.select_pictures();
    let ids = d.all();
    let ends: Vec<Tick> = ids.iter().map(|c| d.end(*c)).collect();
    let widths: Vec<f32> = d.clips.clone().iter().map(|c| d.clip_rect(c.0)[2]).collect();
    let undo = d.harness.state().session.history.undo.len();
    let grabbed = d.clips[1].0;
    let at = d.out_edge(grabbed);
    d.press(at);
    d.move_to(at + egui::vec2(40.0, 0.0));
    // mid-drag, every selected clip previews the longer Out edge
    let mid: Vec<f32> = d.clips.clone().iter().map(|c| d.clip_rect(c.0)[2]).collect();
    for (w0, w) in widths.iter().zip(&mid) {
        assert!(*w > *w0 + 20.0, "every selected clip grows while dragging: {widths:?} -> {mid:?}");
    }
    d.release();
    let moved: Vec<Tick> = ids.iter().zip(&ends).map(|(c, e)| d.end(*c) - *e).collect();
    assert!(moved[0] > Tick::ZERO, "the Out edges moved later: {moved:?}");
    assert!(moved.iter().all(|m| *m == moved[0]), "every picture and its sound moved by the same amount: {moved:?}");
    let s = &d.harness.state().session;
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    d.exec("edit.undo", json!({}));
    let back: Vec<Tick> = ids.iter().map(|c| d.end(*c)).collect();
    assert_eq!(back, ends, "one undo puts all six back");
}

#[test]
fn the_drag_stops_for_every_selected_clip_at_the_first_clips_media_end() {
    // the middle clip has 6 frames of media after its Out point
    let mut d = Driver::three([48, 6, 48]);
    d.select_pictures();
    let ids = d.all();
    let ends: Vec<Tick> = ids.iter().map(|c| d.end(*c)).collect();
    let six = d.harness.state().session.sequence_rate().tick_of(6);
    let at = d.out_edge(d.clips[0].0);
    d.press(at);
    d.move_to(at + egui::vec2(150.0, 0.0));
    d.release();
    let moved: Vec<Tick> = ids.iter().zip(&ends).map(|(c, e)| d.end(*c) - *e).collect();
    assert!(moved.iter().all(|m| *m == six), "all of them stopped after 6 frames, at V2's media end: {moved:?}");
}

#[test]
fn dragging_an_unselected_clips_edge_trims_only_that_clip() {
    let mut d = Driver::three([48; 3]);
    let (first, third) = (d.clips[0].0, d.clips[2].0);
    d.exec("timeline.select", json!({"clips": [first.0, third.0]}));
    let ids = d.all();
    let ends: Vec<Tick> = ids.iter().map(|c| d.end(*c)).collect();
    let at = d.out_edge(d.clips[1].0);
    d.press(at);
    d.move_to(at + egui::vec2(40.0, 0.0));
    d.release();
    let moved: Vec<bool> = ids.iter().zip(&ends).map(|(c, e)| d.end(*c) != *e).collect();
    assert_eq!(moved, vec![false, false, true, true, false, false], "only V2's clip and its sound trimmed");
}
