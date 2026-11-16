//! Headless UI tests of the Edit Marker dialog (#3023): double-clicking a marker on the Timeline
//! ruler opens it, OK applies the fields in one undo step, Cancel changes nothing.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
}

impl Driver {
    fn demo() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_step_dt(1.0 / 60.0).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx };
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

    fn has_element(&mut self, id: &str) -> bool {
        self.ok("ui.elements", json!({"prefix": id})).as_array().is_some_and(|v| v.iter().any(|e| e["id"] == id))
    }

    fn marker(&self, id: u64) -> filmcraft_project::Marker {
        self.harness.state().session.active_sequence().unwrap().markers.iter().find(|m| m.id.0 == id).unwrap().clone()
    }

    /// One new marker at 2 s (the demo has others), and the Timeline focused.
    fn setup(&mut self) -> u64 {
        self.ok("ui.set", json!({"focused": "Timeline"}));
        let id = self.exec("markers.add", json!({"time": filmcraft_time::Tick::from_seconds_f64(2.0).0, "name": "First"}))["marker"].as_u64().unwrap();
        self.frames(4);
        id
    }

    fn click(&mut self, id: &str, count: u64) {
        self.ok("ui.click", json!({"id": id, "count": count}));
        self.frames(2);
    }

    fn type_into(&mut self, id: &str, text: &str) {
        self.click(id, 3);
        self.ok("ui.type", json!({"text": text}));
        self.frames(2);
    }
}

#[test]
fn double_clicking_a_marker_opens_the_dialog_and_ok_applies_the_fields() {
    let mut d = Driver::demo();
    let id = d.setup();
    let undo_before = d.harness.state().session.history.undo.len();
    assert!(!d.has_element("markerProperties.apply"), "no dialog before the double-click");
    d.click(&format!("timeline.marker.{id}"), 1);
    assert!(!d.has_element("markerProperties.apply"), "one click only moves the playhead");
    d.click(&format!("timeline.marker.{id}"), 2);
    assert!(d.has_element("markerProperties.apply"), "a double-click opens Edit Marker");
    d.type_into("markerProperties.name", "Renamed");
    d.type_into("markerProperties.comment", "A note");
    d.click("markerProperties.apply", 1);
    assert!(!d.has_element("markerProperties.apply"), "OK closes the dialog");
    let m = d.marker(id);
    assert_eq!((m.name.as_str(), m.comment.as_str()), ("Renamed", "A note"));
    assert_eq!(d.harness.state().session.history.undo.len(), undo_before + 1, "one OK is one undo step");
}

#[test]
fn cancel_leaves_the_marker_alone() {
    let mut d = Driver::demo();
    let id = d.setup();
    let before = d.marker(id);
    d.click(&format!("timeline.marker.{id}"), 2);
    d.type_into("markerProperties.name", "Nope");
    d.click("markerProperties.cancel", 1);
    assert!(!d.has_element("markerProperties.cancel"));
    assert_eq!(d.marker(id), before);
}

/// Renaming a marker keeps a duration the dialog cannot show exactly: one that is not a whole number
/// of frames, and one beyond the duration control's range.
#[test]
fn ok_keeps_a_duration_that_was_not_edited() {
    for ticks in [|frame: i64| frame * 10 + 1, |frame: i64| frame * 20_000_000] {
        let mut d = Driver::demo();
        let id = d.setup();
        let frame = d.harness.state().session.sequence_rate().tick_of(1).0;
        let want = filmcraft_time::Tick(ticks(frame));
        d.harness
            .state_mut()
            .session
            .edit_sequence("Set duration", |q, _, _| {
                q.markers.iter_mut().filter(|m| m.id.0 == id).for_each(|m| m.duration = want);
                Ok(())
            })
            .expect("set the duration");
        d.click(&format!("timeline.marker.{id}"), 2);
        d.type_into("markerProperties.name", "Renamed");
        d.click("markerProperties.apply", 1);
        let m = d.marker(id);
        assert_eq!((m.name.as_str(), m.duration), ("Renamed", want), "only the name changed");
    }
}

impl Driver {
    fn element_label(&mut self, id: &str) -> Option<String> {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        v.as_array()?.iter().find(|e| e["id"] == id).and_then(|e| e["label"].as_str()).map(str::to_string)
    }

    fn right_click_marker(&mut self, marker: u64) {
        let id = format!("timeline.marker.{marker}");
        let elements = self.ok("ui.elements", json!({"prefix": id}));
        let rect = elements.as_array().unwrap().iter().find(|e| e["id"] == id).unwrap()["rect"].as_array().unwrap().clone();
        let x = rect[0].as_f64().unwrap() + rect[2].as_f64().unwrap() / 2.0;
        let y = rect[1].as_f64().unwrap() + rect[3].as_f64().unwrap() / 2.0;
        self.ok("ui.click", json!({"x": x, "y": y, "button": "right"}));
        self.frames(3);
    }
}

/// The ruler's marker menu opens with the marker's title and comment above the actions.
#[test]
fn marker_menu_shows_the_title_and_comment() {
    let mut d = Driver::demo();
    d.ok("ui.set", json!({"focused": "Timeline"}));
    let id = d.exec("markers.add", json!({"time": filmcraft_time::Tick::from_seconds_f64(2.0).0, "name": "Beat", "comment": "cut on the downbeat"}))["marker"]
        .as_u64()
        .unwrap();
    d.frames(4);
    d.right_click_marker(id);
    assert_eq!(d.element_label("timeline.markerMenu.title").as_deref(), Some("Beat"));
    assert_eq!(d.element_label("timeline.markerMenu.comment").as_deref(), Some("cut on the downbeat"));
    assert!(d.has_element("timeline.markerMenu.markers.edit"), "the actions follow the header");
}

/// A marker without a name is titled "Marker", and one without a comment shows no comment line.
#[test]
fn marker_menu_title_falls_back_and_skips_an_empty_comment() {
    let mut d = Driver::demo();
    d.ok("ui.set", json!({"focused": "Timeline"}));
    let id = d.exec("markers.add", json!({"time": filmcraft_time::Tick::from_seconds_f64(2.0).0}))["marker"].as_u64().unwrap();
    d.frames(4);
    d.right_click_marker(id);
    assert_eq!(d.element_label("timeline.markerMenu.title").as_deref(), Some("Marker"));
    assert!(!d.has_element("timeline.markerMenu.comment"));
}

/// M on a frame that already has a marker opens that marker's dialog instead of stacking a second
/// marker on it; one frame later M adds again, and callers that name a time (or automation through
/// `engine.execute`) still add.
#[test]
fn m_on_a_marked_frame_opens_that_marker_instead_of_adding_another() {
    let mut d = Driver::demo();
    d.exec("markers.clearAll", json!({}));
    d.ok("ui.set", json!({"focused": "Timeline"}));
    d.exec("playhead.set", json!({"frame": 30}));
    d.frames(2);
    let count = |d: &Driver| d.harness.state().session.active_sequence().unwrap().markers.len();
    d.ok("ui.key", json!({"key": "M"}));
    d.frames(3);
    assert_eq!(count(&d), 1, "M on an empty frame adds a marker");
    assert!(d.harness.state().ui.marker_properties.is_none());
    let id = d.harness.state().session.active_sequence().unwrap().markers[0].id.0;
    let history = d.harness.state().session.history.undo.len();

    d.ok("ui.key", json!({"key": "M"}));
    d.frames(3);
    assert_eq!(count(&d), 1, "M on the marker's frame must not add another");
    assert_eq!(d.harness.state().session.history.undo.len(), history);
    assert_eq!(d.harness.state().ui.marker_properties.as_ref().map(|m| m.marker), Some(id));
    assert!(d.has_element("markerProperties.cancel"));
    d.click("markerProperties.cancel", 1);
    assert!(d.harness.state().ui.marker_properties.is_none());

    // the menu item is the same command
    assert_eq!(d.ok("ui.menu.invoke", json!({"id": "markers.add"}))["dialog"], "markerProperties");
    assert_eq!(count(&d), 1);
    d.click("markerProperties.cancel", 1);

    d.exec("playhead.set", json!({"frame": 31}));
    d.frames(2);
    d.ok("ui.key", json!({"key": "M"}));
    d.frames(3);
    assert_eq!(count(&d), 2, "M one frame later adds");
    assert!(d.harness.state().ui.marker_properties.is_none());

    d.exec("playhead.set", json!({"frame": 30}));
    d.exec("markers.add", json!({}));
    assert_eq!(count(&d), 3, "automation through engine.execute still adds");
    d.ok("ui.menu.invoke", json!({"id": "markers.add", "params": {"name": "Named"}}));
    d.frames(2);
    assert_eq!(count(&d), 4, "a caller that names a field still adds");
    assert!(d.harness.state().ui.marker_properties.is_none());
}
