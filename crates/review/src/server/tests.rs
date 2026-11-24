//! The server end to end, on a loopback port, with hostile clients.

use super::*;
use crate::model::{Rendition, SHARE_FORMAT};

struct Running {
    _tmp: tempfile::TempDir,
    dir: ReviewDir,
    addr: SocketAddr,
    state: Arc<State>,
    share: ShareRecord,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.state.stop();
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

const MEDIA: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn start() -> Running {
    let tmp = tempfile::tempdir().unwrap();
    let dir = ReviewDir::new(tmp.path().join("Review"));
    dir.ensure().unwrap();
    let share = ShareRecord {
        format: SHARE_FORMAT,
        id: token::new_id().unwrap(),
        token: token::new_token().unwrap(),
        title: "Vlog <v3>".into(),
        project: "/x.fcproj".into(),
        sequence: 7,
        rate_num: 25,
        rate_den: 1,
        drop_frame: false,
        start_timecode: 90_000,
        range_start: 0,
        duration_frames: 250,
        width: 1920,
        height: 1080,
        renditions: vec![
            Rendition { name: "small".into(), width: 960, height: 540, bitrate_kbps: 1400 },
            Rendition { name: "hd".into(), width: 1920, height: 1080, bitrate_kbps: 6000 },
        ],
        created_unix: now_unix(),
        expires_unix: Some(now_unix() + 3600),
        revoked: false,
    };
    dir.save_share(&share).unwrap();
    std::fs::write(dir.media_path(&share.id, "small").unwrap(), MEDIA).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state = State::new(dir.clone());
    let st = Arc::clone(&state);
    std::thread::spawn(move || serve(listener, st, Options { idle_exit: None }));
    Running { _tmp: tmp, dir, addr, state, share }
}

struct Resp {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

fn raw(addr: SocketAddr, bytes: &[u8]) -> Resp {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(bytes).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let status = head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0);
    Resp { status, head, body: buf[split..].to_vec() }
}

fn req(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut r = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (n, v) in headers {
        r.push_str(&format!("{n}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        r.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    r.push_str("\r\n");
    let mut b = r.into_bytes();
    b.extend_from_slice(body);
    raw(addr, &b)
}

fn post_json(addr: SocketAddr, path: &str, v: Value) -> Resp {
    req(addr, "POST", path, &[("Content-Type", "application/json")], &serde_json::to_vec(&v).unwrap())
}

const DEVICE: &str = "00112233445566778899aabbccddeeff";
const OTHER: &str = "ffeeddccbbaa99887766554433221100";

#[test]
fn only_a_live_token_opens_a_share() {
    let r = start();
    let base = format!("/r/{}/", r.share.token);
    let page = req(r.addr, "GET", &base, &[], b"");
    assert_eq!(page.status, 200);
    assert!(page.header("content-security-policy").unwrap().contains("script-src 'self'"));
    assert_eq!(page.header("referrer-policy").as_deref(), Some("no-referrer"));
    assert!(page.text().contains("Vlog &lt;v3&gt;"));
    // the path without its slash redirects to the page
    assert_eq!(req(r.addr, "GET", &format!("/r/{}", r.share.token), &[], b"").status, 301);
    // a wrong token of the right shape, a token prefix, garbage, another share's folder: 404
    let mut wrong = r.share.token.clone();
    wrong.replace_range(0..1, if wrong.starts_with('a') { "b" } else { "a" });
    for bad in [wrong.as_str(), &r.share.token[..63], "x", &r.share.id, "../Review"] {
        assert_eq!(req(r.addr, "GET", &format!("/r/{bad}/"), &[], b"").status, 404, "{bad}");
    }
    // revoked, then expired
    let mut s = r.share.clone();
    s.revoked = true;
    r.dir.save_share(&s).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(req(r.addr, "GET", &base, &[], b"").status, 404);
    s.revoked = false;
    s.expires_unix = Some(now_unix() - 1);
    r.dir.save_share(&s).unwrap();
    assert_eq!(req(r.addr, "GET", &base, &[], b"").status, 404);
    assert_eq!(req(r.addr, "GET", &format!("{base}v/small.mp4"), &[], b"").status, 404);
}

#[test]
fn video_is_served_in_ranges() {
    let r = start();
    let v = format!("/r/{}/v/small.mp4", r.share.token);
    let full = req(r.addr, "GET", &v, &[], b"");
    assert_eq!((full.status, full.body.as_slice()), (200, MEDIA));
    assert_eq!(full.header("accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(full.header("content-type").as_deref(), Some("video/mp4"));
    // a rendered file never changes: the browser keeps what it has and asks only for what it lacks
    assert!(full.header("cache-control").is_some_and(|c| c.contains("immutable")));
    let part = req(r.addr, "GET", &v, &[("Range", "bytes=0-1")], b"");
    assert_eq!((part.status, part.body.as_slice()), (206, &MEDIA[..2]));
    assert_eq!(part.header("content-range"), Some(format!("bytes 0-1/{}", MEDIA.len())));
    let tail = req(r.addr, "GET", &v, &[("Range", "bytes=-4")], b"");
    assert_eq!(tail.body.as_slice(), &MEDIA[MEDIA.len() - 4..]);
    assert_eq!(req(r.addr, "GET", &v, &[("Range", "bytes=999-")], b"").status, 416);
    let head = req(r.addr, "HEAD", &v, &[], b"");
    assert_eq!((head.status, head.body.len()), (200, 0));
    assert_eq!(head.header("content-length"), Some(MEDIA.len().to_string()));
    // not rendered yet, not a rendition, or a path out of the folder
    for p in ["v/hd.mp4", "v/poster.mp4", "v/../share.json", "v/small.mp4/../../share.json", "share.json", "events.jsonl"] {
        assert_eq!(req(r.addr, "GET", &format!("/r/{}/{p}", r.share.token), &[], b"").status, 404, "{p}");
    }
    // keep-alive: several range requests on one connection (iOS does this)
    let one = format!("GET {v} HTTP/1.1\r\nHost: t\r\nRange: bytes=0-0\r\n\r\n");
    let last = format!("GET {v} HTTP/1.1\r\nHost: t\r\nRange: bytes=1-1\r\nConnection: close\r\n\r\n");
    let both = raw(r.addr, format!("{one}{last}").as_bytes());
    assert_eq!(both.status, 206);
    assert!(String::from_utf8_lossy(&both.body).contains("HTTP/1.1 206"));
}

#[test]
fn a_comment_is_on_disk_before_it_is_acknowledged_and_reaches_live_pages() {
    let r = start();
    let base = format!("/r/{}/", r.share.token);
    // a live page is listening
    let mut sse = TcpStream::connect(r.addr).unwrap();
    sse.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sse.write_all(format!("GET {base}events HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes()).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&got).contains("event: snapshot") {
        let n = sse.read(&mut buf).unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    let resp = post_json(r.addr, &format!("{base}comments"), json!({"name": " Ana ", "text": "Cut <here>\u{0}", "frame": 30, "device": DEVICE}));
    assert_eq!(resp.status, 201, "{}", resp.text());
    let c = resp.json();
    assert_eq!((c["author"].as_str(), c["text"].as_str(), c["frame"].as_i64()), (Some("Ana"), Some("Cut <here>"), Some(30)));
    // on disk, with the device hashed, and the app is told
    let log = r.dir.read_events(&r.share.id).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].device, token::device_hash(DEVICE).unwrap());
    assert_eq!(std::fs::read_to_string(r.dir.notify_path()).unwrap(), "1");
    // the live page gets the event, without the device hash
    while !String::from_utf8_lossy(&got).contains("event: ev") {
        let n = sse.read(&mut buf).unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&got);
    assert!(text.contains("Cut <here>"));
    assert!(!text.contains(&log[0].device));
    // the page lists it, escaped
    let page = req(r.addr, "GET", &base, &[], b"").text();
    assert!(page.contains("Cut &lt;here&gt;") && page.contains("01:00:01:05"));
    // info has it too
    let info = req(r.addr, "GET", &format!("{base}info"), &[], b"").json();
    assert_eq!(info["comments"].as_array().unwrap().len(), 1);
    assert_eq!(info["tcBase"], 90_000);
    assert_eq!(info["renditions"].as_array().unwrap().len(), 1, "only the rendered file is offered");
}

#[test]
fn hostile_comments_are_refused() {
    let r = start();
    let c = format!("/r/{}/comments", r.share.token);
    for (body, why) in [
        (json!({"name": "", "text": "x", "frame": 1}), "no name"),
        (json!({"name": "A", "text": " \u{0} ", "frame": 1}), "empty"),
        (json!({"name": "A", "text": "x", "frame": -1}), "negative frame"),
        (json!({"name": "A", "text": "x", "frame": 250}), "past the end"),
        (json!({"name": "A", "text": "x", "frame": i64::MAX}), "huge frame"),
        (json!({"name": "A", "text": "x", "frame": 1, "device": "short"}), "bad device"),
        (json!({"name": 5, "text": "x"}), "wrong types"),
    ] {
        assert_eq!(post_json(r.addr, &c, body).status, 400, "{why}");
    }
    assert_eq!(req(r.addr, "POST", &c, &[("Content-Type", "text/plain")], b"hi").status, 400);
    // a body over the cap is refused before it is read
    let big = format!(r#"{{"name":"A","text":"{}","frame":1}}"#, "x".repeat(MAX_BODY));
    assert_eq!(req(r.addr, "POST", &c, &[("Content-Type", "application/json")], big.as_bytes()).status, 413);
    // a long comment is cut, not refused
    let long = post_json(r.addr, &c, json!({"name": "N".repeat(500), "text": "y".repeat(5000), "frame": 0}));
    assert_eq!(long.status, 201);
    assert_eq!(long.json()["text"].as_str().unwrap().chars().count(), MAX_COMMENT);
    assert_eq!(long.json()["author"].as_str().unwrap().chars().count(), MAX_NAME);
    assert_eq!(r.dir.read_events(&r.share.id).unwrap().len(), 1);
    // garbage request heads close the connection with 400, never crash the server
    assert_eq!(raw(r.addr, b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03\r\n\r\n").status, 400);
    assert_eq!(raw(r.addr, b"GET /r/../../etc/passwd HTTP/1.1\r\n\r\n").status, 404);
    assert_eq!(req(r.addr, "GET", "/healthz", &[], b"").status, 200);
}

#[test]
fn comment_posts_are_rate_limited_per_address() {
    let r = start();
    let c = format!("/r/{}/comments", r.share.token);
    let statuses: Vec<u16> = (0..8).map(|i| post_json(r.addr, &c, json!({"name": "A", "text": format!("n{i}"), "frame": i})).status).collect();
    assert_eq!(&statuses[..6], &[201; 6]);
    assert_eq!(statuses[7], 429);
    // through a tunnel (loopback), the forwarded address is limited on its own
    let fwd = req(r.addr, "POST", &c, &[("Content-Type", "application/json"), ("X-Forwarded-For", "203.0.113.9")], br#"{"name":"B","text":"t","frame":1}"#);
    assert_eq!(fwd.status, 201);
}

#[test]
fn only_the_writing_device_edits_or_deletes() {
    let r = start();
    let base = format!("/r/{}/comments", r.share.token);
    let id = post_json(r.addr, &base, json!({"name": "A", "text": "first", "frame": 3, "device": DEVICE})).json()["id"].as_str().unwrap().to_string();
    let one = format!("{base}/{id}");
    assert_eq!(post_json(r.addr, &one, json!({"action": "edit", "text": "hijack", "device": OTHER})).status, 403);
    assert_eq!(post_json(r.addr, &one, json!({"action": "edit", "text": "hijack"})).status, 403);
    assert_eq!(post_json(r.addr, &one, json!({"action": "edit", "text": "second", "device": DEVICE})).status, 200);
    assert_eq!(post_json(r.addr, &format!("{base}/0000000000000000"), json!({"action": "delete", "device": DEVICE})).status, 404);
    assert_eq!(post_json(r.addr, &format!("{base}/zz"), json!({"action": "delete", "device": DEVICE})).status, 404);
    let list = req(r.addr, "GET", &base, &[], b"").json();
    assert_eq!(list[0]["text"], "second");
    assert_eq!(post_json(r.addr, &one, json!({"action": "delete", "device": DEVICE})).status, 200);
    assert_eq!(req(r.addr, "GET", &base, &[], b"").json().as_array().unwrap().len(), 0);
    // a comment from the no-script form (no device) can't be changed by anyone
    let form = req(r.addr, "POST", &base, &[("Content-Type", "application/x-www-form-urlencoded")], b"name=Bo&tc=01%3A00%3A01%3A05&text=from+the+form");
    assert_eq!(form.status, 303);
    let list = req(r.addr, "GET", &base, &[], b"").json();
    assert_eq!((list[0]["frame"].as_i64(), list[0]["text"].as_str()), (Some(30), Some("from the form")));
    let fid = list[0]["id"].as_str().unwrap();
    assert_eq!(post_json(r.addr, &format!("{base}/{fid}"), json!({"action": "delete", "device": DEVICE})).status, 403);
    // a typed timecode outside the video is refused
    let bad = req(r.addr, "POST", &base, &[("Content-Type", "application/x-www-form-urlencoded")], b"name=Bo&tc=02%3A00%3A00%3A00&text=x");
    assert_eq!(bad.status, 400);
}

#[test]
fn a_comment_sent_twice_is_saved_once() {
    // a phone on a bad connection posts, loses the answer and posts the same comment again
    let r = start();
    let base = format!("/r/{}/comments", r.share.token);
    let body = json!({"name": "Jeff", "text": "Trim this", "frame": 12, "device": DEVICE});
    let first = post_json(r.addr, &base, body.clone());
    let again = post_json(r.addr, &base, body.clone());
    assert_eq!((first.status, again.status), (201, 201));
    assert_eq!(first.json()["id"], again.json()["id"]);
    assert_eq!(r.dir.read_events(&r.share.id).unwrap().len(), 1);
    // another device saying the same is a new comment (the posts here stay under the rate limit)
    assert_ne!(post_json(r.addr, &base, json!({"name": "Jeff", "text": "Trim this", "frame": 12, "device": OTHER})).json()["id"], first.json()["id"]);
    // once deleted, the same words post again
    let one = format!("{base}/{}", first.json()["id"].as_str().unwrap());
    assert_eq!(post_json(r.addr, &one, json!({"action": "delete", "device": DEVICE})).status, 200);
    assert_ne!(post_json(r.addr, &base, body).json()["id"], first.json()["id"]);
}

#[test]
fn the_server_owns_its_port_once() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = ReviewDir::new(tmp.path().join("Review"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(!probe(port, &dir), "nothing serves yet");
    let state = State::new(dir.clone());
    let st = Arc::clone(&state);
    let t = std::thread::spawn(move || serve(listener, st, Options { idle_exit: Some(Duration::from_millis(300)) }));
    assert!(probe(port, &dir));
    assert!(!probe(port, &ReviewDir::new(tmp.path().join("Other"))), "another data folder's server is not ours");
    // no live share: the idle rule stops it
    t.join().unwrap().unwrap();
    assert!(state.stop.load(Ordering::Acquire));
}
