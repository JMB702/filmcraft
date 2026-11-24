//! Headless UI tests of clip markers: with a clip selected in the Timeline, M puts the marker on the
//! clip (its master clip), the clip draws it, and M again on that frame opens it; with nothing
//! selected M adds a sequence marker, as before.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::{ClipId, ItemId};
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use filmcraft_ui_egui::dock::PanelKind;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    item: ItemId,
    clip: ClipId,
}

impl Driver {
    /// One 48-frame A/V clip on V1/A1 at frame 24, from media frame 10; the playhead at frame 40.
    fn new() -> Self {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s.execute("file.newSequence", json!({"name": "Clip markers", "video": 2, "audio": 2})).unwrap();
        let rate = s.sequence_rate();
        let item = s.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).unwrap();
        let p = json!({"item": item.0, "track": "V1", "audioTrack": "A1", "frame": 24, "sourceIn": rate.tick_of(10).0, "duration": rate.tick_of(48).0});
        s.execute("timeline.place", p).unwrap();
        s.execute("edit.deselectAll", json!({})).unwrap();
        s.execute("playhead.set", json!({"frame": 40})).unwrap();
        let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(s).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, item, clip };
        d.frames(4);
        d.ok("ui.set", json!({"timeline": {"pps": 100.0, "scroll": 0.0}}));
        d.harness.state_mut().ui.focused = PanelKind::Timeline;
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

    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }

    fn press_m(&mut self) {
        for pressed in [true, false] {
            let e = egui::Event::Key { key: egui::Key::M, physical_key: Some(egui::Key::M), pressed, repeat: false, modifiers: egui::Modifiers::NONE };
            self.harness.input_mut().events.push(e);
            self.frames(1);
        }
        self.frames(2);
    }

    fn media_markers(&self) -> usize {
        self.harness.state().session.project.item(self.item).and_then(|i| i.as_media()).map_or(0, |m| m.markers.len())
    }

    fn sequence_markers(&self) -> usize {
        self.harness.state().session.active_sequence().unwrap().markers.len()
    }
}

#[test]
fn m_with_a_clip_selected_marks_the_clip_and_the_clip_draws_it() {
    let mut d = Driver::new();
    d.exec("timeline.select", json!({"clips": [d.clip.0]}));
    d.harness.state_mut().ui.focused = PanelKind::Timeline;
    d.press_m();
    assert_eq!((d.media_markers(), d.sequence_markers()), (1, 0), "M put the marker on the clip's master clip, not the sequence");
    let q = d.exec("sequence.inspect", json!({}));
    let clip = &q["video"][0]["items"][0];
    assert_eq!(clip["markers"][0]["startFrame"], json!(40), "{clip}");
    assert_eq!(q["audio"][0]["items"][0]["markers"][0]["startFrame"], json!(40), "the linked sound shows it too");
    // the clip draws it where the playhead is
    let marker = clip["markers"][0]["id"].as_u64().unwrap();
    let id = format!("timeline.clipMarker.{}.{marker}", d.clip.0);
    let el = d.ok("ui.elements", json!({"prefix": id}));
    let el = el.as_array().unwrap().iter().find(|e| e["id"] == json!(id)).cloned().expect("the clip marker is drawn");
    let x = el["rect"][0].as_f64().unwrap() + el["rect"][2].as_f64().unwrap() / 2.0;
    let head = d.ok("ui.elements", json!({"prefix": format!("timeline.clip.{}", d.clip.0)}));
    let body = head.as_array().unwrap().iter().find(|e| e["id"] == json!(format!("timeline.clip.{}", d.clip.0))).cloned().unwrap();
    let (bx, bw) = (body["rect"][0].as_f64().unwrap(), body["rect"][2].as_f64().unwrap());
    let expected = bx + bw * 16.0 / 48.0;
    assert!((x - expected).abs() < 2.0, "drawn 16 frames into the 48-frame clip: {x} vs {expected}");
    // M again on that frame opens the marker instead of stacking a second one
    d.harness.state_mut().ui.focused = PanelKind::Timeline;
    d.press_m();
    assert_eq!(d.media_markers(), 1);
    let draft = d.harness.state().ui.marker_properties.as_ref().expect("the Marker dialog opened");
    assert!(matches!(draft.owner, filmcraft_ui_egui::panels::marker_properties::Owner::Source(i) if i == d.item.0));
}

#[test]
fn m_with_nothing_selected_adds_a_sequence_marker() {
    let mut d = Driver::new();
    d.press_m();
    assert_eq!((d.media_markers(), d.sequence_markers()), (0, 1));
}
