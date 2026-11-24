//! Headless UI tests of the Timeline scrolling its tracks up and down while a drag holds the
//! pointer at (or past) the edge of the track panes, as Premiere Pro does: a box dragged down past
//! the audio tracks scrolls them down and takes the tracks that come into view; one dragged up past
//! the video tracks scrolls them up. The start of the box stays on its track. A clip dragged past
//! the bottom of the audio tracks scrolls them too and lands on a track that was out of view.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::ClipId;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

/// Tracks of each kind: more than the panes show.
const N: usize = 12;

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    pointer: egui::Pos2,
    /// The clip on each video track (V1 first) and on each audio track (A1 first).
    video: Vec<ClipId>,
    audio: Vec<ClipId>,
}

impl Driver {
    /// A sequence with `N` video and `N` audio tracks, a clip on each from 1 s to 3 s (picture on
    /// V<n>, its sound on A<n>), Linked Selection off so a box takes only what it covers.
    fn tall() -> Self {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s.execute("file.newSequence", json!({"name": "Tall", "video": N, "audio": N})).unwrap();
        let rate = s.sequence_rate();
        let item = s.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).unwrap();
        for n in 1..=N {
            s.execute(
                "timeline.place",
                json!({"item": item.0, "track": format!("V{n}"), "audioTrack": format!("A{n}"), "seconds": 1.0, "sourceIn": 0, "duration": rate.tick_of(48).0}),
            )
            .unwrap();
        }
        s.execute("sequence.linkedSelection", json!({"on": false})).unwrap();
        s.execute("edit.deselectAll", json!({})).unwrap();
        let q = s.active_sequence().unwrap();
        let video: Vec<ClipId> = q.video_tracks.iter().map(|t| t.items[0].id).collect();
        let audio: Vec<ClipId> = q.audio_tracks.iter().map(|t| t.items[0].id).collect();
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(s).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, pointer: egui::Pos2::ZERO, video, audio };
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

    /// The on-screen rect of an element, if it is drawn.
    fn rect(&mut self, id: &str) -> Option<[f32; 4]> {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        let e = v.as_array().unwrap().iter().find(|e| e["id"] == json!(id))?.clone();
        let r = e["rect"].as_array().unwrap();
        Some([0, 1, 2, 3].map(|k| r[k].as_f64().unwrap() as f32))
    }

    fn clip_rect(&mut self, c: ClipId) -> Option<[f32; 4]> {
        self.rect(&format!("timeline.clip.{}", c.0))
    }

    fn press(&mut self, at: egui::Pos2) {
        let events = &mut self.harness.input_mut().events;
        events.push(egui::Event::PointerMoved(at));
        events.push(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
        self.pointer = at;
        self.frames(1);
    }

    /// Move a few points a frame, as a mouse does, with the button held.
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

    /// Hold the pointer still until `done` or 600 frames.
    fn hold_until(&mut self, mut done: impl FnMut(&FilmcraftApp) -> bool) {
        for _ in 0..600 {
            if done(self.harness.state()) {
                return;
            }
            self.frames(1);
        }
    }

    fn scrolls(&self) -> (f32, f32) {
        let v = &self.harness.state().ui.timeline;
        (v.v_scroll, v.a_scroll)
    }

    fn selected(&self) -> Vec<ClipId> {
        self.harness.state().session.state.selection.clone()
    }

    /// The tracks area of the panel, `[x, y, width, height]`.
    fn tracks_area(&mut self) -> [f32; 4] {
        self.rect("timeline.tracks").expect("the tracks area")
    }
}

#[test]
fn a_box_dragged_past_the_bottom_scrolls_the_audio_tracks_down_and_takes_them() {
    let mut d = Driver::tall();
    let (a1, last) = (d.audio[0], d.audio[N - 1]);
    let r1 = d.clip_rect(a1).expect("A1 is in view");
    assert!(d.clip_rect(last).is_none(), "A{N} starts out of view");
    let area = d.tracks_area();
    // press on A1 in the empty time before its clip, drag right into the clips and down past the
    // bottom of the panel's tracks (over the Mix row)
    d.press(egui::pos2(r1[0] - 40.0, r1[1] + r1[3] / 2.0));
    d.move_to(egui::pos2(r1[0] + 60.0, area[1] + area[3] - 4.0));
    let before = d.scrolls().1;
    d.hold_until(|app| app.session.state.selection.contains(&last));
    let (v, a) = d.scrolls();
    assert!(a > before, "the audio tracks scrolled down: {before} -> {a}");
    assert_eq!(v, 0.0, "the video tracks stay put");
    let sel = d.selected();
    assert!(sel.contains(&a1), "the box keeps A1, scrolled out of view: {sel:?}");
    assert!(sel.contains(&last), "and takes A{N}, scrolled in: {sel:?}");
    d.release();
    let sel = d.selected();
    for c in &d.audio {
        assert!(sel.contains(c), "every audio clip from A1 to A{N} is selected: {sel:?}");
    }
    assert!(d.video.iter().all(|c| !sel.contains(c)), "no video clip: {sel:?}");
    assert!(d.clip_rect(last).is_some(), "A{N} is in view now");

    // and back up: a box started low among the audio tracks, held at their top edge, scrolls them back
    let r = d.clip_rect(last).unwrap();
    d.press(egui::pos2(r[0] - 40.0, r[1] + r[3] / 2.0));
    let split = d.harness.state().tl.layout.as_ref().map(|l| l.split_y).unwrap();
    d.move_to(egui::pos2(r[0] + 60.0, split + 8.0));
    let held = d.scrolls().1;
    d.hold_until(|app| app.ui.timeline.a_scroll == 0.0);
    assert!(d.scrolls().1 < held, "the audio tracks scroll back toward A1");
    d.release();
}

#[test]
fn a_box_dragged_past_the_top_scrolls_the_video_tracks_up_and_takes_them() {
    let mut d = Driver::tall();
    let (v1, top) = (d.video[0], d.video[N - 1]);
    let r1 = d.clip_rect(v1).expect("V1 is in view");
    assert!(d.clip_rect(top).is_none(), "V{N} starts out of view");
    let area = d.tracks_area();
    d.press(egui::pos2(r1[0] - 40.0, r1[1] + r1[3] / 2.0));
    d.move_to(egui::pos2(r1[0] + 60.0, area[1] - 6.0));
    let before = d.scrolls().0;
    d.hold_until(|app| app.session.state.selection.contains(&top));
    let (v, a) = d.scrolls();
    assert!(v > before, "the video tracks scrolled up: {before} -> {v}");
    assert_eq!(a, 0.0, "the audio tracks stay put");
    d.release();
    let sel = d.selected();
    for c in &d.video {
        assert!(sel.contains(c), "every video clip from V1 to V{N} is selected: {sel:?}");
    }
    assert!(d.audio.iter().all(|c| !sel.contains(c)), "no audio clip: {sel:?}");
}

/// The view also scrolls sideways at the right edge while it scrolls down.
#[test]
fn a_box_scrolls_down_and_sideways_at_once() {
    let mut d = Driver::tall();
    let r1 = d.clip_rect(d.audio[0]).unwrap();
    let area = d.tracks_area();
    let (x0, a0) = (d.harness.state().ui.timeline.scroll, d.scrolls().1);
    d.press(egui::pos2(r1[0] - 40.0, r1[1] + r1[3] / 2.0));
    // straight to the bottom-right corner, then hold
    d.harness.input_mut().events.push(egui::Event::PointerMoved(egui::pos2(area[0] + area[2] - 5.0, area[1] + area[3] - 4.0)));
    d.frames(20);
    let (x1, a1) = (d.harness.state().ui.timeline.scroll, d.scrolls().1);
    assert!(x1 > x0 && a1 > a0, "both at once: time {x0} -> {x1}, audio {a0} -> {a1}");
    d.release();
}

/// A clip dragged down past the bottom of the audio tracks scrolls them and lands on a track that
/// was out of view when the drag began.
#[test]
fn a_clip_dragged_past_the_bottom_lands_on_a_track_scrolled_into_view() {
    let mut d = Driver::tall();
    let a1 = d.audio[0];
    let r1 = d.clip_rect(a1).unwrap();
    let area = d.tracks_area();
    // a spot on the clip's body that moves it: not its Volume line or an edge
    let x = r1[0] + r1[2] / 2.0;
    let y = [0.85, 0.15, 0.3, 0.7]
        .into_iter()
        .map(|f| r1[1] + r1[3] * f)
        .find(|y| {
            let h = d.ok("ui.timeline.hit", json!({"x": x, "y": y}));
            h["clip"] == json!(a1.0) && h["kind"].is_null() && h["edge"].is_null()
        })
        .expect("a plain spot on the clip");
    d.press(egui::pos2(x, y));
    d.move_to(egui::pos2(x, area[1] + area[3] - 4.0));
    assert!(d.scrolls().1 > 0.0, "the audio tracks scroll while the clip is held past their bottom");
    d.hold_until(|app| {
        let v = &app.ui.timeline;
        v.a_scroll > 0.0 && app.tl.layout.as_ref().is_some_and(|l| v.a_scroll >= l.panes.max_a_scroll)
    });
    d.release();
    let q = d.harness.state().session.active_sequence().unwrap();
    let (track, _) = q.find_item(a1).unwrap();
    let index = q.audio_tracks.iter().position(|t| t.id == track).unwrap();
    let (v, a) = d.scrolls();
    assert_eq!(index, N - 1, "the clip went to A{N}, at the bottom (scrolls {v} {a}, layout {:?})", d.harness.state().tl.layout.as_ref().map(|l| l.panes));
}
