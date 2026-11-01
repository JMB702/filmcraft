//! Premiere Pro projects: File ▸ Open converts one into a new, unsaved project; File ▸ Import
//! merges one into the open project. The documents are synthetic, in the structure Premiere saves.

use filmcraft_project::{BinEntry, ItemKind, MediaRef};
use serde_json::json;

use crate::Session;
use crate::media_test_util::{make_movie, tmp_dir};

const SEC: i64 = 254_016_000_000;

/// A project with one movie in a "Footage" bin and a sequence cutting one second of it. The movie's
/// absolute path is `missing`; its project-relative path is `relative`.
fn document(missing: &str, relative: &str) -> String {
    let o = |class: &str, id: u32, body: &str| format!("<{class} ObjectID=\"{id}\" Version=\"1\">{body}</{class}>");
    let uo = |class: &str, uid: &str, body: &str| format!("<{class} ObjectUID=\"{uid}\" Version=\"1\">{body}</{class}>");
    [
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<PremiereData Version=\"3\">".to_string(),
        o("VideoStream", 1, "<FrameRate>10584000000</FrameRate><Duration>254016000000</Duration><FrameRect>0,0,64,36</FrameRect>"),
        uo("Media", "m", &format!("<ActualMediaFilePath>{missing}</ActualMediaFilePath><RelativePath>{relative}</RelativePath><VideoStream ObjectRef=\"1\"/>")),
        o("VideoMediaSource", 2, "<MediaSource><Media ObjectURef=\"m\"/></MediaSource><OriginalDuration>254016000000</OriginalDuration>"),
        o("VideoClip", 3, "<Clip><Source ObjectRef=\"2\"/></Clip>"),
        uo("MasterClip", "mc", "<Name>shot.mov</Name><Clips><Clip Index=\"0\" ObjectRef=\"3\"/></Clips>"),
        uo("ClipProjectItem", "shot", "<ProjectItem><Name>shot.mov</Name></ProjectItem><MasterClip ObjectURef=\"mc\"/>"),
        o("VideoClip", 4, &format!("<Clip><Source ObjectRef=\"2\"/><InPoint>{}</InPoint></Clip>", SEC / 4)),
        o("SubClip", 5, "<Name>shot.mov</Name><Clip ObjectRef=\"4\"/><MasterClip ObjectURef=\"mc\"/>"),
        o("VideoClipTrackItem", 6, &format!("<ClipTrackItem><TrackItem><Start>0</Start><End>{}</End></TrackItem><SubClip ObjectRef=\"5\"/></ClipTrackItem>", SEC / 2)),
        o("VideoClipTrack", 7, "<ClipTrack><ClipItems><TrackItems><TrackItem Index=\"0\" ObjectRef=\"6\"/></TrackItems></ClipItems></ClipTrack>"),
        o("VideoTrackGroup", 8, "<TrackGroup><Tracks><Track Index=\"0\" ObjectRef=\"7\"/></Tracks><FrameRate>10584000000</FrameRate></TrackGroup><FrameRect>0,0,1280,720</FrameRect>"),
        uo("Sequence", "s", "<Name>Cut</Name><TrackGroups><TrackGroup Index=\"0\" ObjectRef=\"8\"/></TrackGroups>"),
        o("VideoSequenceSource", 9, "<SequenceSource><Sequence ObjectURef=\"s\"/></SequenceSource>"),
        o("VideoClip", 10, "<Clip><Source ObjectRef=\"9\"/></Clip>"),
        uo("MasterClip", "smc", "<Name>Cut</Name><Clips><Clip Index=\"0\" ObjectRef=\"10\"/></Clips>"),
        uo("ClipProjectItem", "cut", "<ProjectItem><Name>Cut</Name></ProjectItem><MasterClip ObjectURef=\"smc\"/>"),
        uo("BinProjectItem", "footage", "<ProjectItem><Name>Footage</Name></ProjectItem><ProjectItemContainer><Items><Item Index=\"0\" ObjectURef=\"shot\"/></Items></ProjectItemContainer>"),
        uo("RootProjectItem", "root", "<ProjectItem><Name>Root</Name></ProjectItem><ProjectItemContainer><Items><Item Index=\"0\" ObjectURef=\"footage\"/><Item Index=\"1\" ObjectURef=\"cut\"/></Items></ProjectItemContainer>"),
        "</PremiereData>\n".to_string(),
    ]
    .join("\n")
}

/// A project folder moved since it was saved: the movie is found through its relative path.
fn moved_project(name: &str) -> (std::path::PathBuf, String) {
    let d = tmp_dir(name);
    std::fs::create_dir_all(d.join("Shoot")).unwrap();
    std::fs::create_dir_all(d.join("Project")).unwrap();
    make_movie(&d.join("Shoot/shot.mov"), filmcraft_media::DemoScene::OceanSunset, 64, 36, 24);
    let doc = document("/Volumes/Gone/Shoot/shot.mov", "../Shoot/shot.mov");
    let path = d.join("Project/show.prproj");
    std::fs::write(&path, doc).unwrap();
    (d, path.to_string_lossy().into_owned())
}

fn shot_path(s: &Session) -> Option<String> {
    s.project.items.values().find_map(|i| match &i.kind {
        ItemKind::Media(m) => match &m.media {
            MediaRef::File { path } if !m.offline => Some(path.clone()),
            _ => None,
        },
        _ => None,
    })
}

#[test]
fn opening_a_premiere_project_makes_a_new_unsaved_project() {
    let (d, path) = moved_project("prproj-open");
    let mut s = Session::default();
    let keep = s.execute("file.newBin", json!({"name": "Keep"})).unwrap()["bin"].as_u64().unwrap();
    // a damaged file leaves the open project as it was
    let bad = d.join("Project/bad.prproj");
    std::fs::write(&bad, "<?xml version=\"1.0\"?>\n<PremiereData Version=\"3\">\n<Sequence ObjectUID=\"s\">").unwrap();
    assert!(s.execute("file.open", json!({"path": bad.to_string_lossy()})).is_err());
    assert!(s.project.root.find_bin(filmcraft_project::BinId(keep)).is_some());

    let r = s.execute("file.open", json!({"path": path})).unwrap();
    assert_eq!(r["format"], "Premiere Pro project", "{r}");
    assert_eq!(s.project.name, "show");
    assert!(s.path.is_none(), "saving must not write over the .prproj");
    assert!(s.is_dirty());
    assert!(s.history.undo.is_empty(), "the conversion is not an undo step");
    assert!(s.project.root.find_bin(filmcraft_project::BinId(keep)).is_none());
    let bins: Vec<&str> = s.project.root.children.iter().filter_map(|c| if let BinEntry::Bin(b) = c { Some(b.name.as_str()) } else { None }).collect();
    assert_eq!(bins, ["Footage"]);
    // the movie was found through its relative path and linked
    let linked = shot_path(&s).expect("the movie is online");
    assert!(std::path::Path::new(&linked).exists(), "{linked}");
    assert_eq!(r["imported"]["linkedMedia"], 1, "{r}");
    let seq = s.state.active_sequence.expect("the sequence is open");
    let cut = s.project.sequence(seq).unwrap();
    assert_eq!((cut.settings.width, cut.settings.height), (1280, 720));
    let clip = &cut.video_tracks[0].items[0];
    assert_eq!((clip.duration.0, clip.source_in.0), (SEC / 2, SEC / 4));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn importing_a_premiere_project_merges_it_into_a_bin() {
    let (d, path) = moved_project("prproj-import");
    let mut s = Session::default();
    let bin = s.execute("file.newBin", json!({"name": "From Premiere"})).unwrap()["bin"].as_u64().unwrap();
    let before = s.project.items.len();
    let r = s.execute("file.import", json!({"paths": [path], "bin": bin})).unwrap();
    assert!(r["errors"].as_array().is_none_or(|e| e.is_empty()), "{r}");
    assert_eq!(s.project.items.len(), before + 2, "{r}");
    let mut inside = Vec::new();
    s.project.root.find_bin(filmcraft_project::BinId(bin)).unwrap().all_items(&mut inside);
    assert_eq!(inside.len(), 2);
    assert!(shot_path(&s).is_some_and(|p| std::path::Path::new(&p).exists()));
    // one undo step takes it out again
    while s.project.items.len() > before {
        s.execute("edit.undo", json!({})).unwrap();
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// The native file host, except that `shot.mov` is a cloud placeholder: it is there (its size is
/// known) but every read of its contents is refused, as `FileReader` refuses an evicted iCloud
/// file instead of waiting for its download.
struct CloudPlaceholder;

impl CloudPlaceholder {
    fn evicted(path: &str) -> bool {
        path.ends_with("shot.mov")
    }
}

impl crate::Services for CloudPlaceholder {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        if Self::evicted(path) { Err(filmcraft_media::reader::not_downloaded()) } else { crate::FsServices.read_file(path) }
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        crate::FsServices.write_file(path, data)
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        crate::FsServices.file_size(path)
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        if Self::evicted(path) { Err(filmcraft_media::reader::not_downloaded()) } else { crate::FsServices.read_range(path, offset, len) }
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        if Self::evicted(path) { Some(Err(filmcraft_media::reader::not_downloaded())) } else { crate::FsServices.reader(path) }
    }
}

#[test]
fn media_stored_in_icloud_opens_offline_without_being_read() {
    use filmcraft_render::offline::{OfflineReason, slate_rgba8};
    let (d, path) = moved_project("prproj-icloud");
    let mut s = Session::new(std::sync::Arc::new(CloudPlaceholder));
    let r = s.execute("file.open", json!({"path": path})).unwrap();
    assert_eq!((r["imported"]["linkedMedia"].as_u64(), r["notDownloaded"].as_u64()), (Some(0), Some(1)), "{r}");
    assert_eq!(r["imported"]["offlineMedia"].as_array().map(Vec::len), Some(1), "{r}");
    let report: Vec<&str> = r["imported"]["report"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert!(report.contains(&"shot.mov: stored in iCloud, not downloaded"), "{report:?}");
    // the file is there, so Link Media does not ask for it; the item says why it is offline
    assert!(!s.offline.prompt);
    let shot = s.project.items.values().find(|i| i.name == "shot.mov").map(|i| i.id).unwrap();
    assert_eq!(s.media.offline_status(shot).map(|o| o.reason), Some(OfflineReason::NotDownloaded));
    let st = s.execute("media.status", json!({"item": shot.0})).unwrap();
    assert_eq!(st[0]["status"], json!("notDownloaded"), "{st}");
    assert!(
        s.events.iter().any(|e| matches!(e, crate::Event::Toast { message, .. } if message.contains("stored in iCloud and not downloaded"))),
        "the open says so"
    );
    // its clips draw the not-downloaded slate
    let src = s.media.source_for(&s.project, shot, &*s.services).unwrap();
    let f = src.video_frame(filmcraft_media::FrameRequest { time: filmcraft_time::Tick(0), scale: 1.0 }).unwrap();
    assert_eq!(f.to_rgba8(), slate_rgba8(f.width as usize, f.height as usize, "shot.mov", OfflineReason::NotDownloaded));
    // importing it into an open project reports it the same way
    let mut s = Session::new(std::sync::Arc::new(CloudPlaceholder));
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    assert!(r.to_string().contains("shot.mov: stored in iCloud, not downloaded"), "{r}");
    let _ = std::fs::remove_dir_all(&d);
}

/// The native file host, except that reading `shot.mov` stalls for two seconds (a network share
/// that stopped answering).
struct StalledShare;

impl crate::Services for StalledShare {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        crate::FsServices.read_file(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        crate::FsServices.write_file(path, data)
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        crate::FsServices.file_size(path)
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        if path.ends_with("shot.mov") {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        crate::FsServices.reader(path)
    }
}

#[test]
fn a_stalled_media_file_does_not_hold_the_open() {
    let (d, path) = moved_project("prproj-stalled");
    let mut s = Session::new(std::sync::Arc::new(StalledShare));
    let bytes = std::fs::read(&path).unwrap();
    let format = filmcraft_interchange::Format::Prproj;
    let parsed = crate::interchange::parse(&mut s, &path, &bytes, format).unwrap();
    let started = std::time::Instant::now();
    let r = crate::interchange::merge_within(&mut s, &path, format, parsed, None, std::time::Duration::from_millis(200)).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_millis(1500), "waited {:?}", started.elapsed());
    assert_eq!(r["deferredMedia"].as_array().map(Vec::len), Some(1), "{r}");
    assert_eq!(r["linkedMedia"], 0, "{r}");
    assert!(r["report"].to_string().contains("took too long to open"), "{r}");
    // the item is not offline: it opens when it is played
    let shot = s.project.items.values().find(|i| i.name == "shot.mov").map(|i| i.id).unwrap();
    assert!(s.media.offline_status(shot).is_none());
    assert!(shot_path(&s).is_some());
    let _ = std::fs::remove_dir_all(&d);
}
