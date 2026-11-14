//! Automatic relinking end to end, on real (small, synthetic) files in temporary folders, with a
//! host whose search index, drives and cloud placeholders the test controls: nothing reaches
//! Spotlight, other drives or iCloud.
//!
//! The rules under test (`crate::relink_auto` module docs): a fingerprinted item links only to a
//! file with its fingerprint (found even when renamed); an item without one links only on name,
//! extension, duration and stream properties with no rival; anything else goes to Link Media
//! with candidates, never silently picked.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use filmcraft_media::DemoScene;
use filmcraft_project::{ItemId, MediaRef};
use serde_json::{Value, json};

use crate::media_test_util::{make_movie, session_with, tmp_dir};
use crate::relink_auto::{AutoState, Limits, Search, Via, Want};
use crate::{FsServices, Services, Session};

const W: u32 = 64;
const H: u32 = 36;

/// The native file host, with a fake search index, drive list and set of cloud placeholders.
#[derive(Default)]
struct Host {
    /// Every file the "index" knows; queries return the ones matching a name or size.
    index: Mutex<Vec<String>>,
    /// Asked of the index (names, sizes).
    queries: Mutex<Vec<(Vec<String>, Vec<u64>)>>,
    volumes: Vec<String>,
    home: Option<String>,
    /// Placeholders: listed and sized, never read, until materialized.
    cloud: Mutex<BTreeSet<String>>,
    /// Free bytes reported for every drive.
    free: Option<u64>,
}

impl Host {
    fn placeholder(&self, path: &str) -> bool {
        self.cloud.lock().unwrap().contains(path)
    }
}

impl Services for Host {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        if self.placeholder(path) { Err(filmcraft_media::reader::not_downloaded()) } else { FsServices.read_file(path) }
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        FsServices.write_file(path, data)
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        FsServices.file_size(path)
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        if self.placeholder(path) { Err(filmcraft_media::reader::not_downloaded()) } else { FsServices.read_range(path, offset, len) }
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        if self.placeholder(path) { Some(Err(filmcraft_media::reader::not_downloaded())) } else { FsServices.reader(path) }
    }
    fn volumes(&self) -> Vec<crate::media_browser::Volume> {
        self.volumes.iter().map(|p| crate::media_browser::Volume { name: p.clone(), path: p.clone(), kind: Default::default() }).collect()
    }
    fn home_dir(&self) -> Option<String> {
        self.home.clone()
    }
    fn find_indexed(&self, names: &[String], sizes: &[u64], _: std::time::Duration) -> Option<std::io::Result<Vec<String>>> {
        self.queries.lock().unwrap().push((names.to_vec(), sizes.to_vec()));
        let all = self.index.lock().unwrap().clone();
        let hits = all
            .into_iter()
            .filter(|p| {
                let name = crate::relink::file_name(p).to_lowercase();
                names.iter().any(|n| n.to_lowercase() == name) || std::fs::metadata(p).is_ok_and(|m| sizes.contains(&m.len()))
            })
            .collect();
        Some(Ok(hits))
    }
    fn is_cloud_placeholder(&self, path: &str) -> bool {
        self.placeholder(path)
    }
    fn materialize(&self, path: &str) -> std::io::Result<()> {
        self.cloud.lock().unwrap().remove(path);
        Ok(())
    }
    fn free_space(&self, _: &str) -> Option<u64> {
        self.free
    }
}

fn ps(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A project saved at `<root>/Project/p.fcproj` using `<root>/Media/<names>` (each a different
/// demo scene). Returns the project path and the items.
fn saved(root: &Path, names: &[&str]) -> (String, Vec<ItemId>) {
    let media = root.join("Media");
    std::fs::create_dir_all(&media).unwrap();
    let scenes = [DemoScene::Aurora, DemoScene::CityNight, DemoScene::OceanSunset];
    let files: Vec<PathBuf> = names.iter().map(|n| media.join(n)).collect();
    for (k, f) in files.iter().enumerate() {
        make_movie(f, scenes[k % scenes.len()], W, H, 12);
    }
    let refs: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
    let (mut s, items, _) = session_with(&refs);
    let proj = root.join("Project");
    std::fs::create_dir_all(&proj).unwrap();
    let path = ps(&proj.join("p.fcproj"));
    s.execute("file.save", json!({"path": path})).unwrap();
    (path, items)
}

/// The project at `path` with every item's recorded fingerprint removed (what a `.prproj`
/// import of missing media has), saved again.
fn forget_fingerprints(path: &str) {
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    let ids: Vec<ItemId> = s.project.items.values().filter(|i| i.as_media().is_some()).map(|i| i.id).collect();
    s.edit("forget", |p, _| {
        for id in &ids {
            let m = p.item_mut(*id).unwrap().as_media_mut().unwrap();
            m.identity = None;
            m.info.file_size = None;
        }
        Ok(())
    })
    .unwrap();
    s.execute("file.save", json!({"path": path})).unwrap();
}

fn open_with(path: &str, host: Arc<Host>) -> Session {
    let mut s = Session::new(host);
    s.execute("file.open", json!({"path": path})).unwrap();
    s
}

fn path_of(s: &Session, id: ItemId) -> String {
    match &s.project.item(id).unwrap().as_media().unwrap().media {
        MediaRef::File { path } => path.clone(),
        MediaRef::Generator(_) => panic!("file media"),
    }
}

fn find(s: &mut Session) -> Value {
    s.execute("media.findAndLink", json!({"index": true, "parentLevels": 1})).unwrap()
}

fn ids(v: &Value) -> Vec<u64> {
    v.as_array().unwrap().iter().filter_map(Value::as_u64).collect()
}

#[test]
fn a_moved_media_folder_is_found_near_the_project_and_linked_as_one_undo_step() {
    let root = tmp_dir("auto-moved");
    let (path, items) = saved(&root, &["a.mov", "b.mov"]);
    std::fs::create_dir_all(root.join("Shoot")).unwrap();
    std::fs::rename(root.join("Media"), root.join("Shoot/Media")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    assert_eq!(s.offline.auto, AutoState::Requested, "opening asks the frontend to search");
    assert!(s.offline.prompt);
    let r = find(&mut s);
    assert_eq!(r["relinked"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(r["summary"], json!("Relinked 2 automatically"));
    assert_eq!(r["relinked"][0]["via"], json!("projectFolder"), "{r}");
    assert_eq!(r["relinked"][0]["signals"][0], json!("same content fingerprint and size"));
    assert!(!s.offline.prompt, "nothing left to ask");
    assert_eq!(Path::new(&path_of(&s, items[1])), root.join("Shoot/Media/b.mov"));
    // one undo step puts both back; Undo Automatic Relink does it as a new step
    s.execute("edit.undo", json!({})).unwrap();
    assert!(path_of(&s, items[0]).ends_with("Media/a.mov") && !path_of(&s, items[0]).contains("Shoot"));
    s.execute("edit.redo", json!({})).unwrap();
    let u = s.execute("media.undoAutoRelink", json!({})).unwrap();
    assert_eq!(ids(&u["restored"]).len(), 2, "{u}");
    assert_eq!(s.execute("media.findMissing", json!({})).unwrap()["missing"].as_array().unwrap().len(), 2);
    assert!(s.execute("media.undoAutoRelink", json!({})).is_err(), "only once");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_renamed_file_is_found_by_its_fingerprint() {
    let root = tmp_dir("auto-renamed");
    let (path, items) = saved(&root, &["a.mov", "b.mov"]);
    std::fs::rename(root.join("Media/a.mov"), root.join("Media/renamed take.mov")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    let r = find(&mut s);
    assert_eq!(r["relinked"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(Path::new(&path_of(&s, items[0])), root.join("Media/renamed take.mov"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_same_named_file_with_another_fingerprint_is_not_linked() {
    let root = tmp_dir("auto-mismatch");
    let (path, items) = saved(&root, &["a.mov"]);
    std::fs::remove_file(root.join("Media/a.mov")).unwrap();
    // another take, same name, same length and format, elsewhere near the project
    std::fs::create_dir_all(root.join("Other")).unwrap();
    make_movie(&root.join("Other/a.mov"), DemoScene::OceanSunset, W, H, 12);
    let mut s = open_with(&path, Arc::new(Host::default()));
    let r = find(&mut s);
    assert!(r["relinked"].as_array().unwrap().is_empty(), "{r}");
    assert_eq!(ids(&r["needYou"]), vec![items[0].0]);
    let sugg = &s.offline.suggestions[&items[0].0];
    assert_eq!(sugg.len(), 1);
    assert!(!sugg[0].ok && sugg[0].identity_match == Some(false), "{sugg:?}");
    assert!(s.offline.prompt, "Link Media asks");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn without_a_fingerprint_a_unique_match_on_every_property_links_and_records_one() {
    let root = tmp_dir("auto-unverified");
    let (path, items) = saved(&root, &["a.mov"]);
    forget_fingerprints(&path);
    std::fs::rename(root.join("Media"), root.join("Moved")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    assert!(s.project.item(items[0]).unwrap().as_media().unwrap().identity.is_none());
    let r = find(&mut s);
    assert_eq!(r["relinked"].as_array().unwrap().len(), 1, "{r}");
    let signals: Vec<&str> = r["relinked"][0]["signals"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(signals.contains(&"duration") && signals.contains(&"frame size and rate"), "{signals:?}");
    let m = s.project.item(items[0]).unwrap().as_media().unwrap();
    assert_eq!(m.identity, crate::relink::identity_of(&FsServices, &ps(&root.join("Moved/a.mov"))).ok(), "the next relink is exact");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn without_a_fingerprint_two_same_named_files_in_different_folders_are_not_auto_picked() {
    let root = tmp_dir("auto-two");
    let (path, items) = saved(&root, &["a.mov"]);
    forget_fingerprints(&path);
    for d in ["Day1", "Day2"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::rename(root.join("Media/a.mov"), root.join("Day1/a.mov")).unwrap();
    // the same name, length and format: another clip
    make_movie(&root.join("Day2/a.mov"), DemoScene::CityNight, W, H, 12);
    let mut s = open_with(&path, Arc::new(Host::default()));
    let r = find(&mut s);
    assert!(r["relinked"].as_array().unwrap().is_empty(), "{r}");
    let sugg = &s.offline.suggestions[&items[0].0];
    assert_eq!(sugg.iter().filter(|c| c.ok).count(), 2, "both listed as matching: {sugg:?}");
    assert!(s.offline.prompt);
    // a copy of the same file is not a rival: delete the other clip and copy the original
    std::fs::copy(root.join("Day1/a.mov"), root.join("Day2/a.mov")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    assert_eq!(find(&mut s)["relinked"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn without_a_fingerprint_another_duration_is_not_linked() {
    let root = tmp_dir("auto-duration");
    let (path, items) = saved(&root, &["a.mov"]);
    forget_fingerprints(&path);
    std::fs::remove_file(root.join("Media/a.mov")).unwrap();
    std::fs::create_dir_all(root.join("Other")).unwrap();
    make_movie(&root.join("Other/a.mov"), DemoScene::Aurora, W, H, 20);
    let mut s = open_with(&path, Arc::new(Host::default()));
    let r = find(&mut s);
    assert!(r["relinked"].as_array().unwrap().is_empty(), "{r}");
    let sugg = &s.offline.suggestions[&items[0].0];
    assert!(sugg[0].problems.iter().any(|p| p.contains("duration")), "{sugg:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_moved_project_folder_finds_its_media_again() {
    let root = tmp_dir("auto-project-moved");
    let (path, items) = saved(&root, &["a.mov", "b.mov"]);
    let _ = path;
    // the whole job folder moves: project and media together
    let moved = tmp_dir("auto-project-moved-dest").join("Job");
    std::fs::rename(&root, &moved).unwrap();
    let path = ps(&moved.join("Project/p.fcproj"));
    let mut s = open_with(&path, Arc::new(Host::default()));
    let r = find(&mut s);
    assert_eq!(r["relinked"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(Path::new(&path_of(&s, items[0])), moved.join("Media/a.mov"));
    let _ = std::fs::remove_dir_all(moved.parent().unwrap());
}

/// The search itself, given a document's alternate path for one file: the other file, in the same
/// moved folder but nowhere near the project, is found by the remap the first one implies.
#[test]
fn a_remap_learned_from_one_file_finds_the_others() {
    let root = tmp_dir("auto-remap");
    let (path, items) = saved(&root, &["a.mov", "b.mov"]);
    forget_fingerprints(&path);
    let far = tmp_dir("auto-remap-far").join("Archive/Shoot");
    std::fs::create_dir_all(far.parent().unwrap()).unwrap();
    std::fs::rename(root.join("Media"), &far).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    let a_old = path_of(&s, items[0]);
    s.offline.alternates.insert(a_old, vec![ps(&far.join("a.mov"))]);
    let mut search = crate::relink_auto::search_for(&s, None);
    search.limits.parent_levels = 0;
    search.index = false;
    let o = crate::relink_auto::run(&search, &s.media, &*s.services);
    let vias: Vec<(u64, Via)> = o.linked.iter().map(|l| (l.item.0, l.via)).collect();
    assert_eq!(vias, vec![(items[0].0, Via::ProjectRelative), (items[1].0, Via::Remap)]);
    crate::relink_auto::apply_outcome(&mut s, o, false).unwrap();
    assert!(s.offline.remaps.iter().any(|(_, to)| Path::new(to) == far), "{:?}", s.offline.remaps);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(far.parent().unwrap().parent().unwrap());
}

#[test]
fn the_same_trailing_path_on_another_drive_is_found() {
    let root = tmp_dir("auto-drive");
    let (path, items) = saved(&root, &["a.mov"]);
    let drive = tmp_dir("auto-drive-volume");
    // stored as if on a drive that is gone: /Volumes/Gone/Shoot/Day 1/a.mov
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    s.edit("move", |p, _| {
        p.item_mut(items[0]).unwrap().as_media_mut().unwrap().media = MediaRef::File { path: "/Volumes/Gone/Shoot/Day 1/a.mov".into() };
        Ok(())
    })
    .unwrap();
    s.execute("file.save", json!({"path": path})).unwrap();
    std::fs::create_dir_all(drive.join("Shoot/Day 1")).unwrap();
    std::fs::rename(root.join("Media/a.mov"), drive.join("Shoot/Day 1/a.mov")).unwrap();
    let host = Host { volumes: vec![ps(&drive)], ..Default::default() };
    let mut s = open_with(&path, Arc::new(host));
    let r = find(&mut s);
    assert_eq!(r["relinked"][0]["via"], json!("otherDrive"), "{r}");
    assert_eq!(Path::new(&path_of(&s, items[0])), drive.join("Shoot/Day 1/a.mov"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&drive);
}

#[test]
fn the_search_index_finds_a_far_renamed_file_by_size() {
    let root = tmp_dir("auto-index");
    let (path, items) = saved(&root, &["a.mov"]);
    let far = tmp_dir("auto-index-far");
    std::fs::rename(root.join("Media/a.mov"), far.join("final.mov")).unwrap();
    let host = Arc::new(Host::default());
    host.index.lock().unwrap().push(ps(&far.join("final.mov")));
    let mut s = open_with(&path, host.clone());
    let r = find(&mut s);
    assert_eq!(r["relinked"][0]["via"], json!("spotlight"), "{r}");
    let q = host.queries.lock().unwrap();
    assert_eq!(q[0].0, vec!["a.mov".to_string()]);
    assert_eq!(q[0].1.len(), 1, "the fingerprinted size is asked for too");
    assert_eq!(Path::new(&path_of(&s, items[0])), far.join("final.mov"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&far);
}

#[test]
fn spotlight_queries_escape_hostile_names() {
    let q = crate::relink_auto::spotlight_query(&["a\"b*c\\d.mov".into(), String::new()], &[42]).unwrap();
    assert_eq!(q, r#"kMDItemFSName == "a\"b\*c\\d.mov"c || kMDItemFSSize == 42"#);
    assert_eq!(crate::relink_auto::spotlight_query(&[], &[]), None);
    let long = "x".repeat(300);
    assert_eq!(crate::relink_auto::spotlight_query(&[long], &[]), None, "names longer than a file name can be are dropped");
}

#[test]
fn hostile_and_degenerate_searches_do_not_panic() {
    let root = tmp_dir("auto-hostile");
    let (path, items) = saved(&root, &["a.mov"]);
    let s = open_with(&path, Arc::new(Host::default()));
    let media = s.project.item(items[0]).unwrap().as_media().unwrap().clone();
    let weird = ["", "/", "\\\\server\\share\\a.mov", "C:", "///", "a/../../b.mov", "/Volumes", "/Volumes/X", "\u{0}.mov"];
    let wants =
        weird.iter().map(|p| Want { item: items[0], path: p.to_string(), alternates: vec![p.to_string(), String::new()], media: media.clone() }).collect();
    let search = Search {
        wants,
        document: Some("/".into()),
        media_dirs: vec![String::new(), "/nonexistent".into()],
        remaps: vec![(String::new(), String::new()), ("/".into(), "".into())],
        roots: vec![String::new(), ps(&root)],
        index: false,
        limits: Limits { files: 50, parent_levels: 8, ..Default::default() },
    };
    let o = crate::relink_auto::run(&search, &s.media, &*s.services);
    assert_eq!(o.linked.len() + o.ambiguous.len() + o.cloud.len() + o.not_found.len(), weird.len());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_background_search_waits_for_link_media_and_ignores_another_project() {
    let root = tmp_dir("auto-background");
    let (path, items) = saved(&root, &["a.mov"]);
    std::fs::rename(root.join("Media"), root.join("Moved")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    s.start_auto_relink();
    assert_eq!(s.offline.auto, AutoState::Running);
    let r = wait(&mut s);
    assert_eq!(r["relinked"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(s.offline.auto, AutoState::Idle);
    assert!(!s.offline.prompt);
    assert!(s.events.iter().any(|e| matches!(e, crate::Event::Toast { message, .. } if message == "Relinked 1 automatically")));
    assert_eq!(Path::new(&path_of(&s, items[0])), root.join("Moved/a.mov"));

    // a search whose project was replaced meanwhile is dropped
    std::fs::rename(root.join("Moved"), root.join("Moved again")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    s.start_auto_relink();
    s.execute("file.newProject", json!({"name": "other"})).unwrap();
    for _ in 0..400 {
        if s.relink_auto.search.as_ref().is_some_and(|(_, slot)| slot.lock().unwrap().is_some()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(s.poll_auto_relink().is_none(), "nothing applied to the new project");
    // turned off in Settings, the search doesn't start and Link Media asks at once
    let mut s = open_with(&path, Arc::new(Host::default()));
    s.prefs.media.auto_relink = false;
    s.start_auto_relink();
    assert_eq!(s.offline.auto, AutoState::Idle);
    assert!(s.offline.prompt);
    let _ = std::fs::remove_dir_all(&root);
}

fn wait(s: &mut Session) -> Value {
    for _ in 0..1000 {
        if let Some(r) = s.poll_auto_relink() {
            return r.unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the search did not finish");
}

#[test]
fn a_file_moved_while_the_project_is_open_is_relinked_quietly() {
    let root = tmp_dir("auto-vanished");
    let (path, items) = saved(&root, &["a.mov"]);
    let mut s = open_with(&path, Arc::new(Host::default()));
    s.execute("media.refreshChanged", json!({})).unwrap();
    std::fs::rename(root.join("Media/a.mov"), root.join("Media/a renamed.mov")).unwrap();
    s.execute("media.refreshChanged", json!({})).unwrap();
    assert_eq!(s.offline.auto, AutoState::Requested, "the vanished file is looked for");
    s.start_auto_relink();
    wait(&mut s);
    assert_eq!(Path::new(&path_of(&s, items[0])), root.join("Media/a renamed.mov"));
    assert!(!s.offline.prompt, "no dialog for a file moved in the background");
    // the old path is not searched for again on every look
    s.execute("media.refreshChanged", json!({})).unwrap();
    assert_eq!(s.offline.auto, AutoState::Idle);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn icloud_media_downloads_and_comes_online_without_reopening() {
    let root = tmp_dir("auto-icloud");
    let (path, items) = saved(&root, &["a.mov", "b.mov"]);
    let host = Arc::new(Host::default());
    host.cloud.lock().unwrap().insert(ps(&root.join("Media/b.mov")));
    let mut s = open_with(&path, host.clone());
    assert_eq!(crate::relink::not_downloaded(&s), vec![items[1]]);
    assert!(s.events.iter().any(|e| matches!(e, crate::Event::Toast { message, .. } if message.contains("stored in iCloud; downloading"))));
    assert_eq!(s.offline.auto, AutoState::Requested);
    s.start_auto_relink();
    assert_eq!(s.offline.downloads.len(), 1);
    let mut lines = Vec::new();
    for _ in 0..500 {
        lines = crate::relink_cloud::poll(&mut s);
        if !lines.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(lines, vec!["b.mov downloaded from iCloud and is online".to_string()]);
    assert_eq!(s.offline.downloads[0].state, crate::relink_cloud::DlState::Done);
    assert!(crate::relink::not_downloaded(&s).is_empty());
    let st = s.execute("media.status", json!({"item": items[1].0})).unwrap();
    assert_eq!(st[0]["status"], json!("online"), "{st}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_download_that_would_fill_the_drive_waits_for_the_user() {
    let root = tmp_dir("auto-icloud-space");
    let (path, items) = saved(&root, &["a.mov"]);
    let host = Arc::new(Host { free: Some(1 << 20), ..Default::default() });
    host.cloud.lock().unwrap().insert(ps(&root.join("Media/a.mov")));
    let mut s = open_with(&path, host);
    s.start_auto_relink();
    assert_eq!(s.offline.downloads[0].state, crate::relink_cloud::DlState::NeedsSpace);
    let r = s.execute("media.downloadFromCloud", json!({"items": [items[0].0], "force": true})).unwrap();
    assert_eq!(r["started"], json!(1), "{r}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_placeholder_candidate_is_downloaded_then_checked() {
    let root = tmp_dir("auto-icloud-candidate");
    let (path, items) = saved(&root, &["a.mov", "b.mov"]);
    forget_fingerprints(&path);
    std::fs::create_dir_all(root.join("iCloud")).unwrap();
    std::fs::rename(root.join("Media/a.mov"), root.join("iCloud/a.mov")).unwrap();
    // b's only candidate is another clip that happens to share its name
    std::fs::remove_file(root.join("Media/b.mov")).unwrap();
    make_movie(&root.join("iCloud/b.mov"), DemoScene::Aurora, W, H, 30);
    let host = Arc::new(Host::default());
    for f in ["a.mov", "b.mov"] {
        host.cloud.lock().unwrap().insert(ps(&root.join("iCloud").join(f)));
    }
    let mut s = open_with(&path, host);
    let r = find(&mut s);
    assert_eq!(ids(&r["downloading"]), vec![items[0].0, items[1].0], "{r}");
    let mut lines = Vec::new();
    for _ in 0..500 {
        lines.extend(crate::relink_cloud::poll(&mut s));
        if lines.len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    lines.sort();
    assert!(lines[0].starts_with("a.mov downloaded from iCloud and is online"), "{lines:?}");
    assert!(lines[1].contains("is not the clip's media"), "{lines:?}");
    assert_eq!(Path::new(&path_of(&s, items[0])), root.join("iCloud/a.mov"));
    assert!(path_of(&s, items[1]).ends_with("Media/b.mov"), "b stays missing");
    assert!(s.offline.prompt && s.offline.suggestions.contains_key(&items[1].0));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_cloud_placeholder_of_another_size_is_a_rival() {
    let root = tmp_dir("auto-icloud-rival");
    let (path, items) = saved(&root, &["a.mov"]);
    forget_fingerprints(&path);
    for d in ["Here", "Cloud"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::rename(root.join("Media/a.mov"), root.join("Here/a.mov")).unwrap();
    make_movie(&root.join("Cloud/a.mov"), DemoScene::CityNight, W, H, 40);
    let host = Arc::new(Host::default());
    host.cloud.lock().unwrap().insert(ps(&root.join("Cloud/a.mov")));
    let mut s = open_with(&path, host);
    let r = find(&mut s);
    assert!(r["relinked"].as_array().unwrap().is_empty(), "{r}");
    assert_eq!(ids(&r["needYou"]), vec![items[0].0]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_new_drive_starts_a_search_when_media_is_missing() {
    let root = tmp_dir("auto-mount");
    let (path, _) = saved(&root, &["a.mov"]);
    std::fs::remove_file(root.join("Media/a.mov")).unwrap();
    struct Mounting(Mutex<usize>);
    impl Services for Mounting {
        fn read_file(&self, p: &str) -> std::io::Result<Vec<u8>> {
            FsServices.read_file(p)
        }
        fn write_file(&self, p: &str, d: &[u8]) -> std::io::Result<()> {
            FsServices.write_file(p, d)
        }
        fn file_size(&self, p: &str) -> std::io::Result<u64> {
            FsServices.file_size(p)
        }
        fn volumes(&self) -> Vec<crate::media_browser::Volume> {
            let mut n = self.0.lock().unwrap();
            *n += 1;
            let names: &[&str] = if *n == 1 { &["/Volumes/A"] } else { &["/Volumes/A", "/Volumes/B"] };
            names.iter().map(|p| crate::media_browser::Volume { name: p.to_string(), path: p.to_string(), kind: Default::default() }).collect()
        }
    }
    let mut s = Session::new(Arc::new(Mounting(Mutex::new(0))));
    s.execute("file.open", json!({"path": path})).unwrap();
    s.offline.auto = AutoState::Idle;
    assert!(!s.check_new_volumes(), "the first look only remembers the drives");
    assert!(s.check_new_volumes(), "a drive was mounted");
    assert_eq!(s.offline.auto, AutoState::Requested);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_report_lists_what_was_linked_from_where() {
    let root = tmp_dir("auto-report");
    let (path, _) = saved(&root, &["a.mov", "b.mov", "c.mov"]);
    std::fs::rename(root.join("Media"), root.join("Moved")).unwrap();
    std::fs::remove_file(root.join("Moved/c.mov")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    find(&mut s);
    let r = s.execute("media.relinkReport", json!({})).unwrap();
    assert_eq!(r["summary"], json!("Relinked 2 automatically, 1 need you"), "{r}");
    let row = &r["report"]["linked"][0];
    assert!(row["from"].as_str().unwrap().ends_with("Media/a.mov") && row["to"].as_str().unwrap().ends_with("Moved/a.mov"), "{row}");
    assert_eq!(row["name"], json!("a.mov"));
    let _ = std::fs::remove_dir_all(&root);
}

/// The native host, except that no file but `keep` exists: a project opened through it has all
/// of its media missing, as on a computer without the media drive.
struct OnlyDocument {
    keep: String,
}

impl Services for OnlyDocument {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        if path == self.keep { FsServices.read_file(path) } else { Err(std::io::ErrorKind::NotFound.into()) }
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        if path == self.keep { FsServices.file_size(path) } else { Err(std::io::ErrorKind::NotFound.into()) }
    }
}

/// Accuracy on a real Premiere project (`FILMCRAFT_RELINK_SAMPLE=<path to .prproj>`; skipped
/// without it). Read-only: nothing is written. Prints counts only.
///
/// 1. The project is opened as if none of its media were on this computer, so every item has only
///    what the `.prproj` records (no fingerprint), then each item's real file is judged under the
///    automatic rules: how many would link by themselves once found.
/// 2. Each item's stored path is replaced by one on a drive that is gone, and the search runs on
///    the real disk (the project's folders, drives, Spotlight): every link it makes must be the
///    item's real file. A wrong link fails the test.
#[test]
fn real_project_from_env() {
    let Ok(doc) = std::env::var("FILMCRAFT_RELINK_SAMPLE") else { return };
    let mut s = Session::new(Arc::new(OnlyDocument { keep: doc.clone() }));
    s.execute("file.open", json!({"path": doc})).unwrap();
    let truth: Vec<(ItemId, String)> = s
        .project
        .items
        .values()
        .filter_map(|i| match &i.as_media()?.media {
            MediaRef::File { path } if Path::new(path).is_file() => Some((i.id, path.clone())),
            _ => None,
        })
        .collect();
    let media_items = s.project.items.values().filter(|i| i.as_media().is_some_and(|m| matches!(m.media, MediaRef::File { .. }))).count();
    eprintln!("media items: {media_items}, with their file on disk: {}", truth.len());
    // 1. judged directly
    let pool = crate::MediaPool::default();
    let mut pass = 0;
    let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
    for (id, path) in &truth {
        let m = s.project.item(*id).unwrap().as_media().unwrap();
        match crate::relink_auto::judge(m, &pool, &FsServices, path) {
            crate::relink_auto::Verdict::Pass(_) => pass += 1,
            crate::relink_auto::Verdict::Fail(c) => {
                for p in c.problems {
                    // the kind of problem, without names or numbers
                    let kind: String = p.split(['(', ':']).next().unwrap_or_default().trim().to_string();
                    *reasons.entry(kind).or_default() += 1;
                }
            }
            crate::relink_auto::Verdict::Cloud => *reasons.entry("in iCloud".into()).or_default() += 1,
        }
    }
    eprintln!("judged against their own file: {pass} pass, {} fail; reasons: {reasons:?}", truth.len() - pass);
    // 2. searched for from a drive that is gone
    let wants: Vec<Want> = truth
        .iter()
        .map(|(id, path)| {
            let mut media = s.project.item(*id).unwrap().as_media().unwrap().clone();
            let gone = format!("/Volumes/FilmCraftGoneDrive/{}", crate::relink::file_name(path));
            media.media = MediaRef::File { path: gone.clone() };
            Want { item: *id, path: gone, alternates: vec![], media }
        })
        .collect();
    let mut search = Search { wants, document: Some(doc.clone()), index: true, ..Default::default() };
    search.roots = FsServices.volumes().into_iter().map(|v| v.path).collect();
    let t = std::time::Instant::now();
    let o = crate::relink_auto::run(&search, &pool, &FsServices);
    let real: std::collections::HashMap<ItemId, String> = truth.iter().cloned().collect();
    let canon = |p: &str| std::fs::canonicalize(p).ok();
    let (mut right, mut wrong) = (0, 0);
    for l in &o.linked {
        if canon(&l.cand.path).is_some() && canon(&l.cand.path) == real.get(&l.item).and_then(|r| canon(r)) {
            right += 1;
        } else {
            wrong += 1;
        }
    }
    let mut vias: std::collections::BTreeMap<&str, usize> = Default::default();
    for l in &o.linked {
        *vias.entry(l.via.label()).or_default() += 1;
    }
    eprintln!(
        "search ({:.1}s, {} files listed, truncated: {}): linked {right} right, {wrong} wrong; ask {}; download {}; not found {}; found via {vias:?}",
        t.elapsed().as_secs_f64(),
        o.files_seen,
        o.truncated,
        o.ambiguous.len(),
        o.cloud.len(),
        o.not_found.len()
    );
    assert_eq!(wrong, 0, "a wrong link");
}

#[test]
fn rates_a_few_millionths_apart_are_the_same_rate() {
    use crate::relink::same_rate;
    use filmcraft_time::FrameRate;
    assert!(same_rate(FrameRate::new(30000, 1001), FrameRate::new(127_008_000_000, 4_237_825_133)));
    assert!(!same_rate(FrameRate::new(30000, 1001), FrameRate::new(30, 1)));
    assert!(!same_rate(FrameRate::new(25, 1), FrameRate::new(0, 0)), "a degenerate rate matches nothing");
}

#[test]
fn without_a_fingerprint_a_recorded_duration_a_few_frames_longer_still_matches() {
    let root = tmp_dir("auto-slack");
    let (path, items) = saved(&root, &["a.mov", "b.mov"]);
    forget_fingerprints(&path);
    // the project says 3 frames more than the file has (Premiere counts trailing audio); b says 6
    let mut s = Session::default();
    s.execute("file.open", json!({"path": path})).unwrap();
    let fd = s.project.item(items[0]).unwrap().as_media().unwrap().info.frame_rate().frame_duration();
    s.edit("longer", |p, _| {
        for (id, n) in [(items[0], 3), (items[1], 6)] {
            let m = p.item_mut(id).unwrap().as_media_mut().unwrap();
            m.info.duration += filmcraft_time::Tick(fd.0 * n);
        }
        Ok(())
    })
    .unwrap();
    s.execute("file.save", json!({"path": path})).unwrap();
    std::fs::rename(root.join("Media"), root.join("Moved")).unwrap();
    let mut s = open_with(&path, Arc::new(Host::default()));
    let r = find(&mut s);
    let linked: Vec<u64> = r["relinked"].as_array().unwrap().iter().filter_map(|l| l["item"].as_u64()).collect();
    assert_eq!(linked, vec![items[0].0], "{r}");
    assert_eq!(ids(&r["needYou"]), vec![items[1].0]);
    let _ = std::fs::remove_dir_all(&root);
}
