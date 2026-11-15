//! Headless UI tests of the open project's file changing on disk (`app/disk_watch.rs`,
//! `panels::disk_conflict`): another program writes the project file while it is open here.
//! Nothing unsaved: it reloads by itself. Unsaved changes, or a Save that would replace the newer
//! file: the dialog asks (Reload from Disk, Keep Mine, Save Mine as a Copy).

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_project::BinEntry;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    _dir: tempfile::TempDir,
    path: String,
}

impl Driver {
    /// A saved project open in the app.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Film.fcproj").to_string_lossy().into_owned();
        Session::default().execute("file.saveAs", json!({"path": path})).unwrap();
        let mut s = Session::default();
        s.execute("file.open", json!({"path": path})).unwrap();
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(s).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, _dir: dir, path };
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

    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let v = self.call(method, params.clone());
        assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
        v["result"].clone()
    }

    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }

    fn has(&mut self, id: &str) -> bool {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        v.as_array().unwrap().iter().any(|e| e["id"] == json!(id))
    }

    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(3);
    }

    fn app(&mut self) -> &mut FilmcraftApp {
        self.harness.state_mut()
    }

    fn bins(&mut self) -> Vec<String> {
        bins_of(&self.app().session.project)
    }

    /// Another program adds a bin and saves the file.
    fn write_from_outside(&self, bin: &str) {
        let mut other = Session::default();
        other.execute("file.open", json!({"path": self.path})).unwrap();
        other.execute("file.newBin", json!({"name": bin})).unwrap();
        other.execute("file.save", json!({})).unwrap();
    }

    /// The watcher's next look (the app takes one every couple of seconds).
    fn look(&mut self) {
        self.app().check_project_file();
        self.frames(3);
    }
}

fn bins_of(p: &filmcraft_project::Project) -> Vec<String> {
    p.root
        .children
        .iter()
        .filter_map(|e| match e {
            BinEntry::Bin(b) => Some(b.name.clone()),
            _ => None,
        })
        .collect()
}

fn bins_on_disk(path: &str) -> Vec<String> {
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    bins_of(&s.project)
}

#[test]
fn a_clean_project_reloads_when_another_program_changes_its_file() {
    let mut d = Driver::new();
    d.write_from_outside("From the CLI");
    d.look();
    assert!(d.bins().contains(&"From the CLI".to_string()), "{:?}", d.bins());
    assert!(!d.app().session.is_dirty());
    assert!(d.app().session.disk.conflict.is_none());
    assert!(!d.has("diskConflict.reload"), "no question for a clean project");
    assert!(d.app().ui.status.contains("Reloaded"), "{}", d.app().ui.status);
    // a half-written file is not loaded, and not retried until it changes again
    std::fs::write(&d.path, b"{ half a project").unwrap();
    d.look();
    assert!(d.bins().contains(&"From the CLI".to_string()));
    assert!(d.app().ui.status.contains("could not be reloaded"), "{}", d.app().ui.status);
    d.app().ui.status.clear();
    d.look();
    assert!(d.app().ui.status.is_empty(), "the same broken file is not reported again");
}

#[test]
fn unsaved_changes_are_asked_about_and_can_be_kept_as_a_copy() {
    let mut d = Driver::new();
    d.exec("file.newBin", json!({"name": "Mine"}));
    d.write_from_outside("Theirs");
    d.look();
    assert!(d.bins().contains(&"Mine".to_string()), "nothing is reloaded over unsaved work");
    assert!(d.app().session.disk.conflict.is_some());
    for id in ["diskConflict.reload", "diskConflict.keep", "diskConflict.copy"] {
        assert!(d.has(id), "{id}");
    }
    d.click("diskConflict.copy");
    assert!(!d.has("diskConflict.copy"), "answered");
    // the project is now the file's version; mine is in the copy next to it
    assert!(d.bins().contains(&"Theirs".to_string()));
    assert!(!d.bins().contains(&"Mine".to_string()));
    let status = d.app().ui.status.clone();
    let copy = std::fs::read_dir(std::path::Path::new(&d.path).parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains("(my changes "))
        .expect("the copy");
    assert!(status.contains("(my changes "), "{status}");
    assert!(bins_on_disk(copy.to_str().unwrap()).contains(&"Mine".to_string()));
}

#[test]
fn reload_from_disk_discards_only_when_chosen() {
    let mut d = Driver::new();
    d.exec("file.newBin", json!({"name": "Mine"}));
    d.write_from_outside("Theirs");
    d.look();
    d.click("diskConflict.reload");
    assert!(d.bins().contains(&"Theirs".to_string()));
    assert!(!d.bins().contains(&"Mine".to_string()));
    assert!(!d.app().session.is_dirty());
}

#[test]
fn save_over_a_newer_file_asks_and_keep_mine_saves() {
    let mut d = Driver::new();
    d.exec("file.newBin", json!({"name": "Mine"}));
    d.write_from_outside("Theirs");
    // Cmd+S: refused, and the question comes up at once
    let r = d.call("engine.execute", json!({"command": "file.save", "params": {}}));
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(bins_on_disk(&d.path), ["Theirs"], "the newer file is untouched");
    d.frames(3);
    assert!(d.has("diskConflict.keep"));
    d.click("diskConflict.keep");
    assert!(!d.app().session.is_dirty(), "Keep Mine after a refused save saves");
    let on_disk = bins_on_disk(&d.path);
    assert!(on_disk.contains(&"Mine".to_string()) && !on_disk.contains(&"Theirs".to_string()), "{on_disk:?}");
}

/// An outside tool that wrote the file and then asks the app (`filmcraft-cli --live inspect`) gets
/// the file's version: a request looks at the file before it is answered, without waiting for
/// the watcher's next look.
#[test]
fn a_request_right_after_an_outside_write_sees_the_new_file() {
    let mut d = Driver::new();
    d.write_from_outside("Written headless");
    let v = d.exec("project.inspect", json!({}));
    assert!(v.to_string().contains("Written headless"), "{v}");
    assert!(!d.app().session.is_dirty());
    // with unsaved work here the request is answered from this version, and the question waits
    d.exec("file.newBin", json!({"name": "Mine"}));
    d.write_from_outside("Second outside write");
    let v = d.exec("project.inspect", json!({}));
    assert!(v.to_string().contains("Mine") && !v.to_string().contains("Second outside write"), "{v}");
    assert!(d.app().session.disk.conflict.is_some());
    assert!(d.has("diskConflict.reload"));
}
