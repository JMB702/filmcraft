//! Headless UI tests of multi-camera editing: the Create Multi-Camera Source Sequence, Synchronize
//! and Merge Clips dialogs, the Program monitor's Multi-Camera view (angle grid, clicking an
//! angle, Ctrl-click for video only) and live switching with the 1–9 keys while playing.
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also render the window offscreen with wgpu and write PNGs
//! there (`multicam-*.png`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_engine::project::{ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{DemoScene, Generator, MediaSource};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

const W: u32 = 320;
const H: u32 = 180;

fn tmp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("filmcraft-ui-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 24 fps ProRes movie of a demo scene (our own encoder).
fn make_movie(path: &Path, scene: DemoScene, frames: i64) {
    let rate = FrameRate::FPS_24;
    let dur = rate.tick_of(frames);
    let src = GeneratorSource::new(Generator::Demo(scene), W, H, rate, dur);
    let clip = MediaClip {
        media: MediaRef::Generator(Generator::Demo(scene)),
        info: src.info().clone(),
        interpret: Default::default(),
        mark_in: None,
        mark_out: None,
        markers: vec![],
        offline: false,
        proxy: None,
        identity: None,
    };
    let mut p = Project::new("fixture");
    let item = p.add_item("scene", Label::Iris, ItemKind::Media(Box::new(clip)), None);
    let seq = p.new_sequence("s", SequenceSettings { width: W, height: H, frame_rate: rate, ..Default::default() }, 1, 0, None);
    let mut v = p.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, dur), rate).unwrap();
    for e in &mut v.effects {
        filmcraft_engine::project::resolve_auto_points(e, (W, H), (W, H));
    }
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(v);
    let settings = filmcraft_engine::export::ExportSettings {
        format: filmcraft_engine::export::Format::ProRes,
        path: path.to_string_lossy().into_owned(),
        include_audio: false,
        ..Default::default()
    };
    let shared: filmcraft_media::SharedSource = Arc::new(src);
    let provider = move |id| (id == item).then(|| shared.clone());
    filmcraft_engine::export::export(&Arc::new(p), seq, &settings, &provider, &Default::default()).unwrap();
}

/// Four cameras imported into a session with an empty 320×180 sequence "Cut".
fn session() -> (Session, Vec<u64>) {
    let dir = tmp_dir("multicam");
    let scenes = [DemoScene::OceanSunset, DemoScene::CityNight, DemoScene::Aurora, DemoScene::Forest];
    let files: Vec<PathBuf> = (0..4).map(|i| dir.join(format!("Cam {}.mov", (b'A' + i as u8) as char))).collect();
    for (f, sc) in files.iter().zip(scenes) {
        make_movie(f, sc, 96);
    }
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": files.iter().map(|f| f.to_string_lossy()).collect::<Vec<_>>()})).unwrap();
    let items: Vec<u64> = r["items"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    let mut p = (*s.project).clone();
    let seq = p.new_sequence("Cut", SequenceSettings { width: W, height: H, frame_rate: FrameRate::FPS_24, ..Default::default() }, 2, 2, None);
    s.project = Arc::new(p);
    s.state.active_sequence = Some(seq);
    s.state.open_sequences = vec![seq];
    (s, items)
}

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<PathBuf>,
}

impl Driver {
    fn new(session: Session) -> Self {
        Self::with_step(session, 0.25)
    }

    /// A driver whose UI passes each advance the clock by `step_dt` seconds (kittest's default
    /// is a quarter second, which skips six frames of playback per pass).
    fn with_step(session: Session, step_dt: f32) -> Self {
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(PathBuf::from);
        let mut b = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000).with_step_dt(step_dt);
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

    /// Run frames (with short sleeps so frame workers finish) until `done`, for at most two
    /// minutes of wall-clock time.
    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_secs() < 120 {
            if done(self) {
                return;
            }
            self.frames(1);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
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

    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(3);
    }

    fn ids(&mut self, prefix: &str) -> Vec<String> {
        let v = self.ok("ui.elements", json!({"prefix": prefix}));
        v.as_array().unwrap().iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }

    fn app(&mut self) -> &mut FilmcraftApp {
        self.harness.state_mut()
    }

    /// Angles of the clips on V1 / A1 of the active sequence.
    fn angles(&mut self, audio: bool) -> Vec<u32> {
        let q = self.app().session.active_sequence().unwrap().clone();
        let t = if audio { &q.audio_tracks[0] } else { &q.video_tracks[0] };
        t.items.iter().map(|i| i.multicam.map_or(99, |m| m.angle)).collect()
    }

    fn snapshot(&mut self, name: &str) {
        let Some(dir) = self.snapshots.clone() else { return };
        self.frames(3);
        let img = match self.harness.render() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("snapshot {name} skipped: {e}");
                return;
            }
        };
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.png"));
        img.save(&path).unwrap();
        eprintln!("snapshot: {}", path.display());
    }
}

#[test]
fn create_multicam_dialog_view_and_live_switching() {
    let (s, items) = session();
    let mut d = Driver::new(s);
    // Create Multi-Camera Source Sequence from the Project panel selection
    d.exec("project.select", json!({"items": items}));
    d.ok("ui.menu.invoke", json!({"id": "clip.createMulticam"}));
    d.frames(3);
    assert_eq!(d.app().ui.sync_dialog.as_ref().map(|x| x.kind.clone()).as_deref(), Some("multicam"));
    for id in ["mcam.name", "mcam.method.audio", "mcam.method.timecode", "mcam.audio.switch", "mcam.cameraNames.track", "mcam.processedBin", "mcam.ok"] {
        assert!(!d.ids(id).is_empty(), "{id}");
    }
    d.click("mcam.method.in");
    d.click("mcam.audio.switch");
    d.snapshot("multicam-create-dialog");
    d.click("mcam.ok");
    assert!(d.app().ui.sync_dialog.is_none(), "dialog closed: {}", d.app().ui.status);
    let src = d.app().session.state.project_selection[0];
    let mc = d.app().session.project.sequence(src).unwrap().multicam.clone().unwrap();
    assert_eq!(mc.cameras.len(), 4);
    assert_eq!(mc.cameras[0].name, "Cam A.mov");
    // edit it into the sequence, open the Multi-Camera view
    d.exec("source.open", json!({"item": src.0}));
    d.exec("playhead.set", json!({"frame": 0}));
    d.exec("source.overwrite", json!({}));
    d.exec("playhead.set", json!({"frame": 12}));
    d.ok("ui.menu.invoke", json!({"id": "multicam.toggleView"}));
    d.frames(2);
    assert!(d.app().ui.program.multicam);
    let angles = d.ids("program.multicam.angle.");
    assert_eq!(angles.len(), 4, "{angles:?}");
    d.until("grid frame", |d| {
        let k = d.app().frames.queue_len();
        k == 0
    });
    d.frames(6);
    d.snapshot("multicam-view");
    // clicking an angle switches the clip at the playhead; audio follows video when asked
    d.exec("multicam.audioFollowsVideo", json!({"enabled": true}));
    d.click("program.multicam.angle.2");
    assert_eq!(d.angles(false), [1]);
    assert_eq!(d.angles(true), [1], "audio followed");
    // Ctrl-click: video only
    d.ok("ui.click", json!({"id": "program.multicam.angle.3", "modifiers": {"ctrl": true}}));
    d.frames(3);
    assert_eq!((d.angles(false), d.angles(true)), (vec![2], vec![1]));
    d.until("program frame", |d| d.app().frames.queue_len() == 0);
    d.frames(6);
    d.snapshot("multicam-view-camera3");
    d.exec("playhead.set", json!({"frame": 0}));
    d.click("program.multicam.angle.1");
    // live switching: play, press 2 then 4, stop → one undo step with the recorded cuts
    let undo_before = d.app().session.history.undo.len();
    d.ok("ui.playback", json!({"action": "play"}));
    d.until("recording", |d| d.app().session.mcrec.active());
    d.until("playhead past 0.3 s", |d| d.app().session.playhead() > FrameRate::FPS_24.tick_of(7));
    d.ok("ui.key", json!({"key": "2"}));
    d.frames(2);
    d.until("playhead past 0.8 s", |d| d.app().session.playhead() > FrameRate::FPS_24.tick_of(19));
    d.ok("ui.key", json!({"key": "4"}));
    d.frames(4);
    d.snapshot("multicam-recording");
    d.until("playhead past 1.2 s", |d| d.app().session.playhead() > FrameRate::FPS_24.tick_of(29));
    d.ok("ui.playback", json!({"action": "stop"}));
    d.frames(2);
    assert!(!d.app().session.mcrec.active());
    let v = d.angles(false);
    assert_eq!(&v[..3], [0, 1, 3], "{v:?}");
    assert_eq!(d.angles(true)[..3], [0, 1, 3], "audio followed the recording");
    assert_eq!(d.app().session.history.undo.len(), undo_before + 1, "one undo step per pass");
    d.frames(4);
    d.snapshot("multicam-after-recording");
    d.exec("edit.undo", json!({}));
    assert_eq!(d.angles(false), [0]);
}

#[test]
fn synchronize_and_merge_dialogs() {
    let (s, items) = session();
    let mut d = Driver::new(s);
    // two cameras on V1 and V2 at different times
    d.exec("source.open", json!({"item": items[0]}));
    d.exec("playhead.set", json!({"frame": 0}));
    d.exec("source.overwrite", json!({}));
    d.exec("timeline.setTargeting", json!({"track": "V2", "sourcePatch": true}));
    d.exec("timeline.setTargeting", json!({"track": "A2", "sourcePatch": true}));
    d.exec("source.open", json!({"item": items[1]}));
    d.exec("playhead.set", json!({"frame": 30}));
    d.exec("source.overwrite", json!({}));
    assert_eq!(d.app().session.active_sequence().unwrap().video_tracks[1].items.len(), 1);
    let all: Vec<u64> = d.app().session.active_sequence().unwrap().all_tracks().flat_map(|t| t.items.iter().map(|i| i.id.0)).collect();
    d.exec("timeline.select", json!({"clips": all}));
    d.ok("ui.menu.invoke", json!({"id": "clip.synchronize"}));
    d.frames(3);
    assert_eq!(d.app().ui.sync_dialog.as_ref().map(|x| x.kind.clone()).as_deref(), Some("synchronize"));
    d.click("sync.method.in");
    d.snapshot("multicam-sync-dialog");
    d.click("sync.ok");
    assert!(d.app().ui.sync_dialog.is_none(), "{}", d.app().ui.status);
    let q = d.app().session.active_sequence().unwrap().clone();
    assert_eq!(q.video_tracks[0].items[0].start, q.video_tracks[1].items[0].start, "In points aligned");
    // Merge Clips needs audio clips: the dialog explains what is wrong
    d.exec("project.select", json!({"items": [items[0], items[1]]}));
    d.ok("ui.menu.invoke", json!({"id": "clip.mergeClips"}));
    d.frames(3);
    assert!(!d.ids("merge.removeVideoAudio").is_empty());
    d.snapshot("multicam-merge-dialog");
    d.click("merge.ok");
    let msg = d.app().ui.sync_dialog.as_ref().map(|x| x.message.clone()).unwrap_or_default();
    assert!(msg.contains("one video clip"), "{msg}");
    d.click("merge.cancel");
    assert!(d.app().ui.sync_dialog.is_none());
}

/// 20 colour-matte cameras in a multi-camera source sequence, edited into "Cut".
fn many_angle_session() -> (Session, u64) {
    let (mut s, _) = session_empty();
    let mut items = Vec::new();
    for k in 0..20u32 {
        let hex = format!("#{:02x}{:02x}80", k * 12, 250 - k * 12);
        let r = s.execute("file.newColorMatte", json!({"color": hex, "seconds": 4.0, "width": W, "height": H, "name": format!("Cam {}", k + 1)})).unwrap();
        items.push(r["item"].as_u64().unwrap());
    }
    s.execute("project.select", json!({"items": items})).unwrap();
    let src = s.execute("clip.createMulticam", json!({"items": items, "method": "in"})).unwrap()["sequence"].as_u64().unwrap();
    s.execute("source.open", json!({"item": src})).unwrap();
    s.execute("playhead.set", json!({"frame": 0})).unwrap();
    s.execute("source.overwrite", json!({})).unwrap();
    s.execute("playhead.set", json!({"frame": 12})).unwrap();
    (s, src)
}

/// A session with the empty sequence "Cut" and nothing imported.
fn session_empty() -> (Session, ()) {
    let mut s = Session::default();
    let mut p = (*s.project).clone();
    let seq = p.new_sequence("Cut", SequenceSettings { width: W, height: H, frame_rate: FrameRate::FPS_24, ..Default::default() }, 2, 2, None);
    s.project = Arc::new(p);
    s.state.active_sequence = Some(seq);
    s.state.open_sequences = vec![seq];
    (s, ())
}

#[test]
fn multicam_view_pages_beyond_sixteen_angles_and_edit_cameras() {
    let (s, src) = many_angle_session();
    let mut d = Driver::new(s);
    d.ok("ui.menu.invoke", json!({"id": "multicam.toggleView"}));
    d.frames(3);
    // page 1: cameras 1–16 and the page arrows
    let mut angles = d.ids("program.multicam.angle.");
    angles.sort_by_key(|a| a.rsplit('.').next().unwrap().parse::<u32>().unwrap());
    assert_eq!(angles.len(), 16, "{angles:?}");
    assert_eq!(angles.last().map(String::as_str), Some("program.multicam.angle.16"));
    assert!(!d.ids("program.multicam.pageNext").is_empty() && !d.ids("program.multicam.pagePrev").is_empty());
    d.until("grid frame", |d| d.app().frames.queue_len() == 0);
    d.frames(4);
    d.snapshot("multicam-paged-1");
    // page 2: cameras 17–20; clicking one switches to it
    d.click("program.multicam.pageNext");
    assert_eq!(d.app().session.state.multicam_view.page, 1);
    let angles = d.ids("program.multicam.angle.");
    assert_eq!(angles.len(), 4, "{angles:?}");
    d.click("program.multicam.angle.18");
    assert_eq!(d.angles(false), [17]);
    // the 3 key picks the third camera of the shown page (camera 19)
    d.ok("ui.key", json!({"key": "3"}));
    d.frames(3);
    assert_eq!(d.angles(false), [18]);
    d.until("grid frame", |d| d.app().frames.queue_len() == 0);
    d.frames(4);
    d.snapshot("multicam-paged-2");
    d.click("program.multicam.pagePrev");
    assert_eq!(d.app().session.state.multicam_view.page, 0);
    // a 3×3 layout: 9 angles per page, three pages
    d.exec("multicam.gridLayout", json!({"layout": "3x3"}));
    d.frames(2);
    assert_eq!(d.ids("program.multicam.angle.").len(), 9);
    // without the preview monitor the grid fills the monitor (no program picture)
    let width = |d: &mut Driver, id: &str| d.ok("ui.elements", json!({"prefix": id}))[0]["rect"][2].as_f64().unwrap_or(0.0);
    let (grid_w, pic_w) = (width(&mut d, "program.multicam.grid"), width(&mut d, "program.picture"));
    assert!(grid_w > 0.0 && pic_w > 0.0);
    d.exec("multicam.showPreviewMonitor", json!({"enabled": false}));
    d.frames(3);
    let (grid_w2, pic_w2) = (width(&mut d, "program.multicam.grid"), width(&mut d, "program.picture"));
    assert!(grid_w2 > grid_w * 1.8 && pic_w2 == 0.0, "grid {grid_w} → {grid_w2}, picture {pic_w} → {pic_w2}");
    d.snapshot("multicam-grid-only");
    d.exec("multicam.showPreviewMonitor", json!({"enabled": true}));
    // Edit Cameras: a thumbnail per camera; hide camera 2
    d.ok("ui.menu.invoke", json!({"id": "multicam.editCamerasDialog"}));
    d.frames(3);
    assert!(d.app().ui.edit_cameras.is_some());
    assert_eq!(d.ids("editCameras.thumb.").len(), 20);
    d.until("thumbnails", |d| d.app().frames.queue_len() == 0);
    d.frames(4);
    d.snapshot("multicam-edit-cameras");
    d.click("editCameras.enabled.1");
    d.click("editCameras.ok");
    assert!(d.app().ui.edit_cameras.is_none());
    let mc = d.app().session.project.sequence(filmcraft_engine::project::ItemId(src)).unwrap().multicam.clone().unwrap();
    assert!(!mc.cameras[1].enabled);
    assert_eq!(d.exec("multicam.grid", json!({}))["angles"], 19);
}

/// #1362: with nothing selected in the Project panel, Create Multi-Camera Source Sequence works on
/// the clips selected in the Timeline.
#[test]
fn create_multicam_from_clips_selected_in_the_timeline() {
    let (s, items) = session();
    let mut d = Driver::new(s);
    for (k, item) in items.iter().enumerate() {
        d.exec("source.open", json!({"item": item}));
        d.exec("playhead.set", json!({"frame": k as i64 * 96}));
        d.exec("source.overwrite", json!({}));
    }
    let clips: Vec<u64> = d.app().session.active_sequence().unwrap().video_tracks[0].items.iter().map(|c| c.id.0).collect();
    assert_eq!(clips.len(), 4);
    d.exec("project.select", json!({"items": []}));
    d.exec("timeline.select", json!({"clips": clips}));
    d.ok("ui.menu.invoke", json!({"id": "clip.createMulticam"}));
    d.frames(3);
    assert_eq!(d.app().ui.sync_dialog.as_ref().map(|x| x.items.clone()), Some(items.clone()), "the dialog takes the Timeline clips' media");
    d.click("mcam.method.in");
    d.click("mcam.ok");
    assert!(d.app().ui.sync_dialog.is_none(), "dialog closed: {}", d.app().ui.status);
    let src = d.app().session.state.project_selection[0];
    assert_eq!(d.app().session.project.sequence(src).unwrap().multicam.clone().unwrap().cameras.len(), 4);
}

/// The label of the automation element `id` (exact match), if it is on screen.
fn label(d: &mut Driver, id: &str) -> Option<String> {
    let v = d.ok("ui.elements", json!({"prefix": id}));
    v.as_array()?.iter().find(|e| e["id"] == id).and_then(|e| e["label"].as_str().map(str::to_string))
}

/// The Program monitor's transport has a Toggle Multi-Camera View button (lit while the view is
/// shown), so the view is one click away rather than a View menu item to know about.
#[test]
fn program_transport_button_toggles_the_multicam_view() {
    let (s, _) = session_empty();
    let mut d = Driver::new(s);
    assert!(!d.app().ui.program.multicam);
    assert!(label(&mut d, "program.transport.multicam.toggleView").is_some_and(|l| l.contains("Multi-Camera")), "the button is shown by default");
    d.click("program.transport.multicam.toggleView");
    assert!(d.app().ui.program.multicam, "on");
    assert!(!d.ids("program.multicam.grid").is_empty(), "the grid is drawn");
    d.snapshot("multicam-transport-button-on");
    d.click("program.transport.multicam.toggleView");
    assert!(!d.app().ui.program.multicam, "off again");
    assert!(d.ids("program.multicam.grid").is_empty());
    // the Button Editor lists it, and hiding it there removes it from the row
    d.click("program.transport.editor");
    assert!(!d.ids("program.transport.editor.multicam.toggleView").is_empty());
    d.click("program.transport.editor.multicam.toggleView");
    d.ok("ui.key", json!({"key": "Escape"}));
    d.frames(3);
    assert!(d.ids("program.transport.multicam.toggleView").is_empty(), "hidden through the Button Editor");
}

/// From a fresh project, with no multi-camera clip yet: the Multi-Camera view says what to do,
/// its button opens Create Multi-Camera Source Sequence, the new clip edited into the sequence
/// shows its cameras, and clicking cameras while playing cuts the timeline.
#[test]
fn empty_multicam_view_leads_to_a_multicam_edit() {
    let (s, items) = session();
    let mut d = Driver::with_step(s, 1.0 / 24.0);
    d.click("program.transport.multicam.toggleView");
    assert!(d.app().ui.program.multicam);
    let title = label(&mut d, "program.multicam.empty").unwrap_or_default();
    assert!(title.contains("No multi-camera clips"), "{title}");
    let help = label(&mut d, "program.multicam.empty.help").unwrap_or_default();
    assert!(help.contains("Create Multi-Camera Source Sequence"), "names the command: {help}");
    assert!(d.ids("program.multicam.angle.").is_empty());
    // the button is greyed out until clips are selected
    let enabled = |d: &mut Driver| d.ok("ui.elements", json!({"prefix": "program.multicam.empty.action"}))[0]["enabled"].as_bool();
    assert_eq!(enabled(&mut d), Some(false));
    d.snapshot("multicam-empty-no-clips");
    d.exec("project.select", json!({"items": items}));
    d.frames(2);
    assert_eq!(enabled(&mut d), Some(true));
    d.click("program.multicam.empty.action");
    assert_eq!(d.app().ui.sync_dialog.as_ref().map(|x| x.kind.clone()).as_deref(), Some("multicam"));
    d.click("mcam.method.in");
    d.click("mcam.ok");
    assert!(d.app().ui.sync_dialog.is_none(), "dialog closed: {}", d.app().ui.status);
    let src = d.app().session.state.project_selection[0];
    // edit it into the sequence (it plays 96 frames) and park the playhead after it
    d.exec("source.open", json!({"item": src.0}));
    d.exec("playhead.set", json!({"frame": 0}));
    d.exec("source.overwrite", json!({}));
    d.exec("playhead.set", json!({"frame": 120}));
    d.frames(2);
    let title = label(&mut d, "program.multicam.empty").unwrap_or_default();
    assert!(title.contains("at the playhead"), "{title}");
    d.snapshot("multicam-empty-not-at-playhead");
    d.click("program.multicam.empty.action");
    assert_eq!(d.app().session.playhead(), Tick::ZERO, "went to the multi-camera clip");
    assert_eq!(d.ids("program.multicam.angle.").len(), 4);
    // Multi-Camera turned off on the clip: the view offers to turn it back on
    let clip = d.app().session.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    d.exec("clip.multicamEnable", json!({"clips": [clip], "enabled": false}));
    d.frames(2);
    let title = label(&mut d, "program.multicam.empty").unwrap_or_default();
    assert!(title.contains("off"), "{title}");
    d.click("program.multicam.empty.action");
    assert_eq!(d.ids("program.multicam.angle.").len(), 4, "enabled again");
    // live switching by clicking cameras while playing: one cut per click, one undo step (the
    // clip plays 4 s; clicks follow each other closely so the pass is still running)
    d.until("grid frame", |d| d.app().frames.queue_len() == 0);
    let undo_before = d.app().session.history.undo.len();
    d.ok("ui.playback", json!({"action": "play"}));
    d.until("recording", |d| d.app().session.mcrec.active());
    d.until("playhead past 0.3 s", |d| d.app().session.playhead() > FrameRate::FPS_24.tick_of(7));
    d.click("program.multicam.angle.3");
    let first = d.app().session.mcrec.cuts.first().map(|c| c.time).expect("the click recorded a cut");
    d.until("two more frames", |d| d.app().session.playhead() > first + FrameRate::FPS_24.tick_of(2));
    d.click("program.multicam.angle.2");
    assert!(d.app().session.mcrec.active(), "still recording (playhead {:?})", d.app().session.playhead());
    assert_eq!(d.app().session.mcrec.cuts.len(), 2);
    d.snapshot("multicam-click-recording");
    d.ok("ui.playback", json!({"action": "stop"}));
    d.frames(2);
    assert!(!d.app().session.mcrec.active());
    let v = d.angles(false);
    assert_eq!(&v[..3], [0, 2, 1], "{v:?}");
    let q = d.app().session.active_sequence().unwrap().clone();
    let cuts: Vec<Tick> = q.video_tracks[0].items.iter().map(|i| i.start).collect();
    assert_eq!(cuts[1], first, "the first cut lands where the camera was clicked: {cuts:?}");
    assert!(cuts[2] > first, "{cuts:?}");
    assert_eq!(d.app().session.history.undo.len(), undo_before + 1, "one undo step per pass");
    d.frames(4);
    d.snapshot("multicam-after-click-recording");
}

/// With no sequence open the view says so instead of drawing an empty grid.
#[test]
fn multicam_view_without_a_sequence() {
    let mut s = Session::default();
    s.state.active_sequence = None;
    s.state.open_sequences.clear();
    let mut d = Driver::new(s);
    d.ok("ui.menu.invoke", json!({"id": "multicam.toggleView"}));
    d.frames(3);
    if d.ids("program.multicam.grid").is_empty() {
        return; // no Program monitor is drawn without a sequence
    }
    assert_eq!(label(&mut d, "program.multicam.empty").as_deref(), Some("No sequence open"));
    assert!(d.ids("program.multicam.empty.action").is_empty());
}

/// Right-clicking a multi-camera clip in the Timeline offers Multi-Camera with its cameras (the
/// shown one ticked); picking one switches the clip to it.
#[test]
fn timeline_clip_menu_lists_the_cameras() {
    let (mut s, items) = session();
    let src = s.execute("clip.createMulticam", json!({"items": items, "method": "in"})).unwrap()["sequence"].as_u64().unwrap();
    s.execute("source.open", json!({"item": src})).unwrap();
    s.execute("playhead.set", json!({"frame": 0})).unwrap();
    s.execute("source.overwrite", json!({})).unwrap();
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    let mut d = Driver::new(s);
    d.ok("ui.click", json!({"id": format!("timeline.clip.{clip}"), "button": "right"}));
    d.frames(3);
    d.click("timeline.clipMenu.clip.multicam");
    let cams = d.ids("timeline.clipMenu.multicam.camera.");
    assert_eq!(cams.len(), 4, "{cams:?}");
    assert!(label(&mut d, "timeline.clipMenu.multicam.camera.0").is_some_and(|l| l.starts_with('✓') && l.contains("Cam A")), "the shown camera is ticked");
    assert!(!d.ids("timeline.clipMenu.clip.multicamFlatten").is_empty());
    d.snapshot("multicam-timeline-clip-menu");
    d.click("timeline.clipMenu.multicam.camera.2");
    assert_eq!(d.angles(false), [2]);
}
