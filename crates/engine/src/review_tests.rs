//! Review links in the engine: comments from disk become markers at their exact frame, once,
//! undoably; they wait for their project; edits and deletes follow the marker.

use crate::Session;
use crate::review;
use filmcraft_project::ItemId;
use filmcraft_review::model::{CommentEvent, EventKind, Rendition, SHARE_FORMAT, ShareRecord};
use filmcraft_review::store::ReviewDir;
use filmcraft_time::{FrameRate, Tick};
use serde_json::json;

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: ReviewDir,
    path: String,
    s: Session,
    seq: ItemId,
}

/// A saved project with a 29.97 drop-frame sequence starting at 01:00:00;00, and a Review folder.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = ReviewDir::new(tmp.path().join("Review"));
    dir.ensure().unwrap();
    let path = tmp.path().join("vlog.fcproj").to_string_lossy().into_owned();
    let mut s = Session::default();
    s.review.dir = Some(dir.clone());
    s.execute("file.newSequence", json!({"name": "Vlog v3", "fps": 29.97})).unwrap();
    let seq = s.state.active_sequence.unwrap();
    s.edit_sequence("Timing", |q, _, _| {
        q.settings.frame_rate = FrameRate::FPS_29_97;
        q.settings.drop_frame = true;
        q.start_timecode = 107_892;
        Ok(())
    })
    .unwrap();
    s.execute("file.saveAs", json!({"path": path})).unwrap();
    assert!(!s.is_dirty());
    Fixture { _tmp: tmp, dir, path, s, seq }
}

fn share(f: &Fixture, id: &str, range_start: Tick) -> ShareRecord {
    let r = ShareRecord {
        format: SHARE_FORMAT,
        id: id.into(),
        token: "cd".repeat(32),
        title: "Vlog v3".into(),
        project: f.path.clone(),
        sequence: f.seq.0,
        rate_num: 30000,
        rate_den: 1001,
        drop_frame: true,
        start_timecode: 107_892,
        range_start: range_start.0,
        duration_frames: 9000,
        width: 1920,
        height: 1080,
        renditions: vec![Rendition { name: "small".into(), width: 960, height: 540, bitrate_kbps: 1400 }],
        created_unix: 1,
        expires_unix: None,
        revoked: false,
    };
    f.dir.save_share(&r).unwrap();
    r
}

fn ev(id: &str, kind: EventKind, comment: &str, text: &str, frame: i64, device: &str) -> CommentEvent {
    CommentEvent { id: id.into(), kind, comment: comment.into(), author: "Ana".into(), text: text.into(), frame, device: device.into(), at_unix: 5 }
}

fn markers(s: &Session, seq: ItemId) -> Vec<filmcraft_project::Marker> {
    s.project.sequence(seq).unwrap().markers.clone()
}

const SHARE: &str = "00000000000000aa";

#[test]
fn a_comment_becomes_one_undoable_marker_at_its_exact_frame() {
    let mut f = fixture();
    let rate = FrameRate::FPS_29_97;
    // the shared range starts 100 frames into the sequence
    share(&f, SHARE, rate.tick_of(100));
    f.dir.append_event(SHARE, &ev("e1", EventKind::Add, "c1", "The cut here is a beat late, trim two frames please", 1800, "d")).unwrap();
    let v = review::sync(&mut f.s).unwrap();
    assert_eq!(v["applied"], 1);
    let m = markers(&f.s, f.seq);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].start, rate.tick_of(1900), "exact sequence frame: range start + file frame");
    assert_eq!(m[0].name, "Ana: The cut here is a beat…");
    assert_eq!(m[0].comment, "The cut here is a beat late, trim two frames please");
    assert_eq!(m[0].author, "Ana");
    assert!(m[0].url.ends_with("#c=c1") && m[0].url.contains(&"cd".repeat(32)), "{}", m[0].url);
    assert!(f.s.is_dirty(), "a remote marker is an edit: auto-save covers it");
    assert_eq!(f.s.history.undo.last().unwrap().0, "Review Comment from Ana");
    // applied once: syncing again changes nothing
    assert_eq!(review::sync(&mut f.s).unwrap()["applied"], 0);
    assert_eq!(markers(&f.s, f.seq).len(), 1);
    // undo removes it and it stays removed
    f.s.undo();
    assert!(markers(&f.s, f.seq).is_empty());
    assert_eq!(review::sync(&mut f.s).unwrap()["applied"], 0);
    assert!(markers(&f.s, f.seq).is_empty());
    f.s.redo();
    assert_eq!(markers(&f.s, f.seq).len(), 1);
    // the ledger is on disk
    assert!(f.dir.applied(SHARE).contains("e1"));
}

#[test]
fn comments_wait_for_their_project_and_apply_when_it_opens() {
    let mut f = fixture();
    share(&f, SHARE, Tick::ZERO);
    // another project is open
    let other = f.dir.root().parent().unwrap().join("other.fcproj").to_string_lossy().into_owned();
    f.s.execute("file.newProject", json!({})).unwrap();
    f.s.execute("file.newSequence", json!({"name": "Other"})).unwrap();
    f.s.execute("file.saveAs", json!({"path": other})).unwrap();
    f.dir.append_event(SHARE, &ev("e1", EventKind::Add, "c1", "queued while closed", 30, "d")).unwrap();
    f.dir.append_event(SHARE, &ev("e2", EventKind::Add, "c2", "second", 60, "d")).unwrap();
    assert_eq!(review::sync(&mut f.s).unwrap()["applied"], 0);
    assert!(f.dir.applied(SHARE).is_empty(), "nothing is marked applied for a closed project");
    // FilmCraft quits; later the project opens in a new session
    let mut s = Session::default();
    s.review.dir = Some(f.dir.clone());
    s.execute("file.open", json!({"path": f.path})).unwrap();
    assert_eq!(review::sync(&mut s).unwrap()["applied"], 2);
    let m = markers(&s, f.seq);
    assert_eq!(m.iter().map(|m| m.comment.as_str()).collect::<Vec<_>>(), ["queued while closed", "second"]);
    assert_eq!(m[0].start, FrameRate::FPS_29_97.tick_of(30));
    assert!(s.is_dirty());
}

#[test]
fn edits_and_deletes_follow_the_comment_marker() {
    let mut f = fixture();
    share(&f, SHARE, Tick::ZERO);
    f.dir.append_event(SHARE, &ev("e1", EventKind::Add, "c1", "first take", 10, "d")).unwrap();
    f.dir.append_event(SHARE, &ev("e2", EventKind::Add, "c2", "keep me", 20, "d")).unwrap();
    review::sync(&mut f.s).unwrap();
    f.dir.append_event(SHARE, &ev("e3", EventKind::Edit, "c1", "second thoughts", 0, "d")).unwrap();
    review::sync(&mut f.s).unwrap();
    let m = markers(&f.s, f.seq);
    assert_eq!((m[0].comment.as_str(), m[0].name.as_str()), ("second thoughts", "Ana: second thoughts"));
    f.dir.append_event(SHARE, &ev("e4", EventKind::Delete, "c1", "", 0, "d")).unwrap();
    review::sync(&mut f.s).unwrap();
    let m = markers(&f.s, f.seq);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].comment, "keep me");
    // an edit for a marker Jeff deleted is a no-op, not a resurrection
    f.s.edit_sequence("Delete", |q, _, _| {
        q.markers.clear();
        Ok(())
    })
    .unwrap();
    f.dir.append_event(SHARE, &ev("e5", EventKind::Edit, "c2", "changed", 0, "d")).unwrap();
    assert_eq!(review::sync(&mut f.s).unwrap()["applied"], 0);
    assert!(markers(&f.s, f.seq).is_empty());
}

#[test]
fn hostile_and_orphaned_comments_never_break_the_project() {
    let mut f = fixture();
    share(&f, SHARE, Tick::ZERO);
    // outside the shared video, huge, negative
    for (i, frame) in [9000, i64::MAX, -1].into_iter().enumerate() {
        f.dir.append_event(SHARE, &ev(&format!("x{i}"), EventKind::Add, &format!("cx{i}"), "far away", frame, "d")).unwrap();
    }
    assert_eq!(review::sync(&mut f.s).unwrap()["applied"], 0);
    assert!(markers(&f.s, f.seq).is_empty());
    // a share whose sequence is gone keeps its comments waiting
    let mut gone = share(&f, "00000000000000bb", Tick::ZERO);
    gone.sequence = 999_999;
    f.dir.save_share(&gone).unwrap();
    f.dir.append_event("00000000000000bb", &ev("g1", EventKind::Add, "g1", "lost", 1, "d")).unwrap();
    let v = review::sync(&mut f.s).unwrap();
    assert_eq!((v["applied"].as_u64(), v["waiting"].as_u64()), (Some(0), Some(1)));
    assert!(f.dir.applied("00000000000000bb").is_empty());
    // a share with a broken frame rate is skipped, not divided by
    let mut zero = share(&f, "00000000000000cc", Tick::ZERO);
    zero.rate_den = 0;
    f.dir.save_share(&zero).unwrap();
    f.dir.append_event("00000000000000cc", &ev("z1", EventKind::Add, "z1", "x", 1, "d")).unwrap();
    assert_eq!(review::sync(&mut f.s).unwrap()["applied"], 0);
    let _ = ItemId(0);
}

#[test]
fn share_needs_a_saved_project_and_makes_a_private_link() {
    let mut s = Session::default();
    let tmp = tempfile::tempdir().unwrap();
    s.execute("file.newSequence", json!({"name": "Unsaved"})).unwrap();
    // without a Review folder (browser build) the command is unavailable
    assert!(s.execute("review.share", json!({})).is_err());
    s.review.dir = Some(ReviewDir::new(tmp.path().join("Review")));
    let e = s.execute("review.share", json!({})).unwrap_err().to_string();
    assert!(e.contains("not been saved") || e.contains("save the project"), "{e}");

    let mut f = fixture();
    f.s.execute("review.config", json!({"publicBaseUrl": "https://review.example.net/"})).unwrap();
    let v = f.s.execute("review.share", json!({"expiresDays": 7})).unwrap();
    let link = v["link"].as_str().unwrap();
    let id = v["share"].as_str().unwrap();
    let rec = f.dir.load_share(id).unwrap();
    assert_eq!(link, format!("https://review.example.net/r/{}/", rec.token));
    assert!(filmcraft_review::token::is_token(&rec.token));
    assert_eq!((rec.sequence, rec.rate_num, rec.rate_den, rec.drop_frame, rec.start_timecode), (f.seq.0, 30000, 1001, true, 107_892));
    assert!(rec.expires_unix.is_some());
    assert!(f.s.drain_events().iter().any(|e| matches!(e, crate::Event::CopyText(t) if t == link)), "the link goes on the clipboard");
    // the renders run as export jobs and land as the rendition files
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while f.s.review.rendering() && std::time::Instant::now() < deadline {
        f.s.poll_persistence();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!f.s.review.rendering(), "renders finished");
    for r in &rec.renditions {
        let p = f.dir.media_path(id, &r.name).unwrap();
        assert!(p.is_file(), "{} rendered", r.name);
        let head = std::fs::read(&p).unwrap();
        assert_eq!(&head[4..8], b"ftyp");
        // faststart: the movie box comes before the media data
        let moov = head.windows(4).position(|w| w == b"moov").unwrap();
        let mdat = head.windows(4).position(|w| w == b"mdat").unwrap();
        assert!(moov < mdat, "{} is faststart", r.name);
    }
    // listed and revocable
    let list = f.s.execute("review.list", json!({})).unwrap();
    assert_eq!(list["shares"][0]["share"], id);
    f.s.execute("review.revoke", json!({"share": id})).unwrap();
    assert!(f.dir.load_share(id).unwrap().revoked);
}

#[test]
fn review_config_refuses_hostile_values() {
    let mut f = fixture();
    for bad in [
        json!({"publicBaseUrl": "javascript:alert(1)"}),
        json!({"publicBaseUrl": "https://a b"}),
        json!({"publicBaseUrl": "https://x/?a"}),
        json!({"publicBaseUrl": "https://u@x"}),
        json!({"port": 80}),
        json!({"port": 70000}),
        json!({"bind": "everywhere"}),
    ] {
        assert!(f.s.execute("review.config", bad.clone()).is_err(), "{bad}");
    }
    let v = f.s.execute("review.config", json!({"port": 8899, "bind": "local", "publicBaseUrl": ""})).unwrap();
    assert_eq!(v["linkBase"], "http://localhost:8899");
}

#[test]
fn renditions_keep_the_aspect_and_fit_phones() {
    let r = review::renditions(3840, 2160);
    assert_eq!(r.iter().map(|r| (r.name.as_str(), r.width, r.height)).collect::<Vec<_>>(), [("small", 960, 540), ("hd", 1920, 1080)]);
    let v = review::renditions(1080, 1920);
    assert_eq!((v[0].width, v[0].height, v[1].width, v[1].height), (304, 540, 608, 1080));
    assert_eq!(review::renditions(640, 360).len(), 1);
    assert_eq!(review::renditions(0, 0)[0].width % 2, 0);
}

#[test]
fn review_funnel_reads_tailscale_and_only_points_links_at_a_working_funnel() {
    use filmcraft_review::tunnel::ProbeOutput;
    use std::sync::{Arc, Mutex};
    let mut f = fixture();
    assert!(f.s.execute("review.funnel", json!({})).is_err(), "no probe in this build");
    let status = r#"{"BackendState":"Running","Self":{"DNSName":"cut.tail0.ts.net.","CapMap":{"https":null,"funnel":null}}}"#;
    let serve = Arc::new(Mutex::new(String::from("{}")));
    let sockets = Arc::new(Mutex::new(Vec::<String>::new()));
    let (sv, so) = (serve.clone(), sockets.clone());
    f.s.review.tunnel = Some(Arc::new(move |sock: &str| {
        so.lock().unwrap().push(sock.to_string());
        ProbeOutput { tool: Some("tailscale".into()), status: Ok(status.into()), serve: Ok(sv.lock().unwrap().clone()) }
    }));
    let v = f.s.execute("review.funnel", json!({})).unwrap();
    assert_eq!((v["state"].as_str(), v["ready"].as_bool(), v["serving"].is_null()), (Some("Running"), Some(false), true));
    assert_eq!(v["steps"][0], "Turn on Funnel for the review server: `tailscale funnel --bg 8787`.");
    assert!(f.s.execute("review.funnel", json!({"use": true})).is_err(), "no funnel yet: links stay as they are");
    assert_eq!(f.s.execute("review.config", json!({})).unwrap()["publicBaseUrl"], "");
    for bad in [json!({"socket": "relative.sock"}), json!({"socket": "/a\nb"}), json!({"socket": format!("/{}", "a".repeat(400))})] {
        assert!(f.s.execute("review.funnel", bad.clone()).is_err(), "{bad}");
    }
    *serve.lock().unwrap() =
        r#"{"TCP":{"443":{}},"Web":{"cut.tail0.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}}},"AllowFunnel":{"cut.tail0.ts.net:443":true}}"#
            .into();
    let v = f.s.execute("review.funnel", json!({"use": true, "socket": "/var/run/ts.sock"})).unwrap();
    assert_eq!((v["ready"].as_bool(), v["linkBase"].as_str()), (Some(true), Some("https://cut.tail0.ts.net")));
    assert_eq!(v["stop"], "tailscale --socket=/var/run/ts.sock funnel --https=443 off");
    assert_eq!(sockets.lock().unwrap().last().map(String::as_str), Some("/var/run/ts.sock"));
    assert_eq!(f.s.execute("review.config", json!({})).unwrap()["publicBaseUrl"], "https://cut.tail0.ts.net");
}
