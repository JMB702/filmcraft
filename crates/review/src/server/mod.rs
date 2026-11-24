//! The review server: what reviewers reach. One thread per connection, every size, count and
//! time bounded. It serves only the shares in its Review folder, by token; it never opens a
//! project and never talks to the app or its control channel. New comments are appended to the
//! share's log (synced to disk) before they are acknowledged, then `notify` is rewritten for the
//! app and live pages are told over server-sent events.

pub mod http;
pub mod page;

mod web {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::model::{Comment, CommentEvent, EventKind, ShareInfo, ShareRecord, fold};
use crate::store::{Bind, ReviewDir, ServerInfo, now_unix};
use crate::text::{self, MAX_COMMENT, MAX_NAME, is_hex};
use crate::timing;
use crate::token::{self, ID_HEX};
use http::{ReadError, Request};

/// Most connections at once.
const MAX_CONNS: usize = 96;
/// Most connections from one address (a tunnel's loopback address is not limited this way).
const MAX_CONNS_PER_IP: usize = 16;
/// Most live-comment streams at once, and from one address.
const MAX_SSE: usize = 64;
const MAX_SSE_PER_IP: usize = 6;
/// Largest request body (a comment post).
const MAX_BODY: usize = 8 * 1024;
/// Most comments a share takes, and most events in its log.
const MAX_COMMENTS: usize = 5000;
const MAX_EVENTS: usize = 20_000;
/// Comment posts: a burst of this many per address, then one per [`REFILL`].
const BURST: f64 = 6.0;
const REFILL: Duration = Duration::from_secs(5);
/// Time to send one request's head and body.
const REQUEST_TIME: Duration = Duration::from_secs(10);
/// Idle keep-alive connections are closed after this.
const IDLE: Duration = Duration::from_secs(15);
/// Requests on one connection.
const MAX_REQUESTS: usize = 500;
/// Live-comment keep-alive interval.
const SSE_PING: Duration = Duration::from_secs(20);

/// How a server run behaves.
#[derive(Clone, Debug)]
pub struct Options {
    /// Stop after this long with no live share (`None`: never).
    pub idle_exit: Option<Duration>,
}

impl Default for Options {
    fn default() -> Self {
        Self { idle_exit: Some(Duration::from_secs(15 * 60)) }
    }
}

struct Limiter {
    buckets: HashMap<IpAddr, (f64, Instant)>,
}

impl Limiter {
    fn take(&mut self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        if self.buckets.len() > 10_000 {
            self.buckets.retain(|_, (_, t)| now.duration_since(*t) < REFILL * 20);
        }
        let (tokens, last) = self.buckets.entry(ip).or_insert((BURST, now));
        let refill = now.duration_since(*last).as_secs_f64() / REFILL.as_secs_f64();
        *tokens = (*tokens + refill).min(BURST);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) * REFILL.as_secs_f64()).ceil() as u64)
        }
    }
}

/// Shared server state.
pub struct State {
    dir: ReviewDir,
    /// One writer of comment logs at a time (checks and appends are one step).
    write: Mutex<()>,
    /// Bumped after every event; live streams wait on it.
    generation: Mutex<u64>,
    changed: Condvar,
    limiter: Mutex<Limiter>,
    conns: AtomicUsize,
    sse: AtomicUsize,
    per_ip: Mutex<HashMap<IpAddr, (usize, usize)>>,
    /// token → share id, re-read every couple of seconds.
    index: Mutex<(Option<Instant>, Vec<(String, String)>)>,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    pub fn new(dir: ReviewDir) -> Arc<Self> {
        Arc::new(Self {
            dir,
            write: Mutex::new(()),
            generation: Mutex::new(0),
            changed: Condvar::new(),
            limiter: Mutex::new(Limiter { buckets: HashMap::new() }),
            conns: AtomicUsize::new(0),
            sse: AtomicUsize::new(0),
            per_ip: Mutex::new(HashMap::new()),
            index: Mutex::new((None, Vec::new())),
            stop: AtomicBool::new(false),
        })
    }

    /// The live share a token opens, or `None` (unknown, revoked, expired: all look the same).
    fn share(&self, tok: &str) -> Option<ShareRecord> {
        if !token::is_token(tok) {
            return None;
        }
        let id = {
            let mut idx = lock(&self.index);
            if idx.0.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                *idx = (Some(Instant::now()), self.dir.shares().into_iter().map(|s| (s.token, s.id)).collect());
            }
            // compare with every token, without stopping early
            let mut found = None;
            for (t, id) in &idx.1 {
                if token::same_secret(t, tok) {
                    found = Some(id.clone());
                }
            }
            found?
        };
        let s = self.dir.load_share(&id).ok()?;
        (token::same_secret(&s.token, tok) && s.is_live(now_unix())).then_some(s)
    }

    fn notify(&self) {
        *lock(&self.generation) += 1;
        self.changed.notify_all();
    }

    /// Ask the server to stop (accept loop and live streams end).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.changed.notify_all();
    }
}

/// The client's address for limits: through a tunnel on this Mac (loopback), the address the
/// tunnel forwards in `X-Forwarded-For` (its last entry, the one the tunnel added).
fn client_ip(peer: IpAddr, req: &Request) -> IpAddr {
    if peer.is_loopback()
        && let Some(f) = req.header("x-forwarded-for").or_else(|| req.header("cf-connecting-ip"))
        && let Some(ip) = f.rsplit(',').next().and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    peer
}

/// A reader that gives up when a deadline passes (slow senders cannot hold a thread forever).
struct Deadline {
    stream: TcpStream,
    until: Instant,
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request took too long"));
        }
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
}

/// Serve on `listener` until [`State::stop`] (or the idle rule in `opts`).
pub fn serve(listener: TcpListener, state: Arc<State>, opts: Options) -> io::Result<()> {
    let addr = listener.local_addr()?;
    if let Some(idle) = opts.idle_exit {
        let st = Arc::clone(&state);
        std::thread::Builder::new().name("review-idle".into()).spawn(move || idle_watch(st, addr, idle))?;
    }
    for conn in listener.incoming() {
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        let Ok(stream) = conn else { continue };
        let Ok(peer) = stream.peer_addr() else { continue };
        if state.conns.load(Ordering::Acquire) >= MAX_CONNS || !admit(&state, peer.ip()) {
            let mut s = stream;
            let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
            let _ = http::respond(&mut s, 503, "text/plain; charset=utf-8", &[("Retry-After", "5".into())], b"busy\n", false, true);
            continue;
        }
        state.conns.fetch_add(1, Ordering::AcqRel);
        let st = Arc::clone(&state);
        let spawned = std::thread::Builder::new().name("review-conn".into()).spawn(move || {
            let st2 = Arc::clone(&st);
            // a bug in one connection never takes the server down
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| connection(stream, peer, &st2)));
            release(&st, peer.ip());
            st.conns.fetch_sub(1, Ordering::AcqRel);
        });
        if spawned.is_err() {
            release(&state, peer.ip());
            state.conns.fetch_sub(1, Ordering::AcqRel);
        }
    }
    Ok(())
}

fn admit(state: &State, ip: IpAddr) -> bool {
    let mut m = lock(&state.per_ip);
    let e = m.entry(ip).or_insert((0, 0));
    if !ip.is_loopback() && e.0 >= MAX_CONNS_PER_IP {
        return false;
    }
    e.0 += 1;
    true
}

fn release(state: &State, ip: IpAddr) {
    let mut m = lock(&state.per_ip);
    if let Some(e) = m.get_mut(&ip) {
        e.0 = e.0.saturating_sub(1);
        if e.0 == 0 && e.1 == 0 {
            m.remove(&ip);
        }
    }
}

fn idle_watch(state: Arc<State>, addr: SocketAddr, idle: Duration) {
    let mut since = Instant::now();
    while !state.stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_secs(5).min(idle));
        let live = state.dir.shares().iter().any(|s| s.is_live(now_unix()));
        if live || state.conns.load(Ordering::Acquire) > 0 {
            since = Instant::now();
        } else if since.elapsed() >= idle {
            state.stop();
            // wake the accept loop
            let wake = if addr.ip().is_unspecified() { SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port()) } else { addr };
            let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        }
    }
}

fn connection(stream: TcpStream, peer: SocketAddr, state: &State) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::with_capacity(8192, Deadline { stream: read_half, until: Instant::now() + IDLE });
    let mut w = stream;
    for _ in 0..MAX_REQUESTS {
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        // the first byte may take up to IDLE; the rest of the request must follow quickly
        reader.get_mut().until = Instant::now() + IDLE;
        match io::BufRead::fill_buf(&mut reader) {
            Ok(b) if !b.is_empty() => {}
            _ => break,
        }
        reader.get_mut().until = Instant::now() + REQUEST_TIME;
        let req = match http::read_request(&mut reader, MAX_BODY) {
            Ok(r) => r,
            Err(ReadError::Closed) => break,
            Err(ReadError::Bad(m)) => {
                let _ = http::respond(&mut w, 400, "text/plain; charset=utf-8", &[], format!("{m}\n").as_bytes(), false, true);
                break;
            }
            Err(ReadError::TooLarge) => {
                let _ = http::respond(&mut w, 413, "text/plain; charset=utf-8", &[], b"too large\n", false, true);
                break;
            }
            Err(ReadError::Unsupported) => {
                let _ = http::respond(&mut w, 411, "text/plain; charset=utf-8", &[], b"send a Content-Length\n", false, true);
                break;
            }
        };
        let keep = match route(&req, &mut w, peer, state) {
            Ok(k) => k,
            Err(_) => break,
        };
        if !keep || req.close {
            break;
        }
    }
    let _ = w.shutdown(Shutdown::Both);
}

fn not_found(w: &mut TcpStream, req: &Request) -> io::Result<bool> {
    http::respond(w, 404, "text/plain; charset=utf-8", &[("Cache-Control", "no-store".into())], b"not found\n", req.method == "HEAD", false)?;
    Ok(true)
}

fn json_reply(w: &mut TcpStream, status: u16, v: &Value, head: bool) -> io::Result<bool> {
    let body = serde_json::to_vec(v).map_err(io::Error::other)?;
    http::respond(w, status, "application/json; charset=utf-8", &[("Cache-Control", "no-store".into())], &body, head, false)?;
    Ok(true)
}

fn error_reply(w: &mut TcpStream, status: u16, msg: &str, extra: &[(&str, String)]) -> io::Result<bool> {
    let body = serde_json::to_vec(&json!({ "error": msg })).map_err(io::Error::other)?;
    let mut h = vec![("Cache-Control", "no-store".to_string())];
    h.extend(extra.iter().cloned());
    http::respond(w, status, "application/json; charset=utf-8", &h, &body, false, false)?;
    Ok(true)
}

/// Handle one request. `Ok(false)`: close the connection.
fn route(req: &Request, w: &mut TcpStream, peer: SocketAddr, state: &State) -> io::Result<bool> {
    let get = req.method == "GET" || req.method == "HEAD";
    let head = req.method == "HEAD";
    let path = req.path.as_str();
    if path == "/healthz" && get {
        let body = format!("filmcraft-review\n{}\n", folder_tag(&state.dir));
        http::respond(w, 200, "text/plain; charset=utf-8", &[("Cache-Control", "no-store".into())], body.as_bytes(), head, false)?;
        return Ok(true);
    }
    if let Some(name) = path.strip_prefix("/assets/") {
        return if get { asset(w, req, name) } else { not_found(w, req) };
    }
    let Some(rest) = path.strip_prefix("/r/") else { return not_found(w, req) };
    let (tok, sub) = rest.split_once('/').map(|(t, s)| (t, Some(s))).unwrap_or((rest, None));
    let Some(share) = state.share(tok) else { return not_found(w, req) };
    if let Some(s) = sub {
        if req.method == "POST"
            && let Some(id) = s.strip_prefix("comments/")
        {
            return change_comment(w, req, peer, state, &share, id);
        }
        if get && let Some(file) = s.strip_prefix("v/") {
            return video(w, req, state, &share, file);
        }
    }
    match (req.method.as_str(), sub) {
        ("GET" | "HEAD", None) => {
            http::respond(w, 301, "text/plain; charset=utf-8", &[("Location", format!("/r/{tok}/")), ("Cache-Control", "no-store".into())], b"", head, false)?;
            Ok(true)
        }
        ("GET" | "HEAD", Some("")) => page(w, req, state, &share),
        ("GET" | "HEAD", Some("info")) => {
            let info = info(state, &share);
            json_reply(w, 200, &serde_json::to_value(info).map_err(io::Error::other)?, head)
        }
        ("GET" | "HEAD", Some("comments")) => {
            let comments = fold(&state.dir.read_events(&share.id).unwrap_or_default());
            json_reply(w, 200, &serde_json::to_value(comments).map_err(io::Error::other)?, head)
        }
        ("POST", Some("comments")) => post_comment(w, req, peer, state, &share),
        ("GET", Some("events")) => events(w, peer, state, &share),
        (_, Some("" | "info" | "comments" | "events")) => {
            http::respond(w, 405, "text/plain; charset=utf-8", &[], b"method not allowed\n", false, false)?;
            Ok(true)
        }
        _ => not_found(w, req),
    }
}

/// The glue's and the module's versioned URLs, when the wasm assets are built in.
fn glue_asset() -> Option<(String, String)> {
    let tag = |name: &str| web::WEB.iter().find(|(n, _)| *n == name).map(|(_, b)| page::version_tag(b));
    Some((format!("/assets/review_web.js?v={}", tag("review_web.js")?), format!("/assets/review_web_bg.wasm?v={}", tag("review_web_bg.wasm")?)))
}

fn asset(w: &mut TcpStream, req: &Request, name: &str) -> io::Result<bool> {
    let head = req.method == "HEAD";
    let (bytes, mime): (&[u8], &str) = if name == "review.css" {
        (page::CSS.as_bytes(), "text/css; charset=utf-8")
    } else if let Some((_, b)) = web::WEB.iter().find(|(n, _)| *n == name) {
        let mime = if name.ends_with(".wasm") { "application/wasm" } else { "text/javascript; charset=utf-8" };
        (b, mime)
    } else {
        return not_found(w, req);
    };
    let etag = format!("\"{}\"", page::version_tag(bytes));
    // versioned URLs never change; the glue and module are fetched by the loader without one
    let cache = if req.query.starts_with("v=") { "public, max-age=31536000, immutable" } else { "no-cache" };
    if req.header("if-none-match") == Some(etag.as_str()) {
        http::respond(w, 304, mime, &[("ETag", etag), ("Cache-Control", cache.into())], b"", true, false)?;
        return Ok(true);
    }
    http::respond(w, 200, mime, &[("ETag", etag), ("Cache-Control", cache.into())], bytes, head, false)?;
    Ok(true)
}

/// Renditions whose final file exists, smallest first.
fn ready(state: &State, share: &ShareRecord) -> Vec<crate::model::Rendition> {
    let mut v: Vec<_> = share.renditions.iter().filter(|r| state.dir.media_path(&share.id, &r.name).is_some_and(|p| p.is_file())).cloned().collect();
    v.sort_by_key(|r| u64::from(r.width) * u64::from(r.height));
    v
}

fn info(state: &State, share: &ShareRecord) -> ShareInfo {
    let rate = timing::rate(share.rate_num, share.rate_den);
    ShareInfo {
        title: share.title.clone(),
        rate_num: share.rate_num,
        rate_den: share.rate_den,
        drop_frame: share.drop_frame,
        tc_base: rate.map(|r| timing::tc_base(r, share.start_timecode, filmcraft_time::Tick(share.range_start))).unwrap_or(0),
        duration_frames: share.duration_frames,
        width: share.width,
        height: share.height,
        renditions: ready(state, share),
        comments: fold(&state.dir.read_events(&share.id).unwrap_or_default()),
    }
}

fn page(w: &mut TcpStream, req: &Request, state: &State, share: &ShareRecord) -> io::Result<bool> {
    let comments = fold(&state.dir.read_events(&share.id).unwrap_or_default());
    let ready: Vec<String> = ready(state, share).into_iter().map(|r| r.name).collect();
    let urls = glue_asset();
    let glue = urls.as_ref().map(|(g, m)| page::Boot { glue: g, module: m });
    let html = page::render(&page::PageInput { share, comments: &comments, ready: &ready, glue });
    let csp = match glue {
        Some(b) if !ready.is_empty() => http::csp_with_script(&page::boot_script(b)),
        _ => http::CSP.to_string(),
    };
    http::respond(
        w,
        200,
        "text/html; charset=utf-8",
        &[("Content-Security-Policy", csp), ("Cache-Control", "no-store".into())],
        html.as_bytes(),
        req.method == "HEAD",
        false,
    )?;
    Ok(true)
}

/// A reviewer's comment, as the page or the no-script form sends it.
struct Posted {
    name: String,
    text: String,
    frame: Option<i64>,
    tc: Option<String>,
    device: Option<String>,
    form: bool,
}

fn parse_post(req: &Request) -> Option<Posted> {
    let ct = req.header("content-type").unwrap_or("");
    if ct.starts_with("application/json") {
        let v: Value = serde_json::from_slice(&req.body).ok()?;
        Some(Posted {
            name: v.get("name")?.as_str()?.to_string(),
            text: v.get("text")?.as_str()?.to_string(),
            frame: v.get("frame").and_then(Value::as_i64),
            tc: None,
            device: v.get("device").and_then(Value::as_str).map(str::to_string),
            form: false,
        })
    } else if ct.starts_with("application/x-www-form-urlencoded") {
        let pairs = http::form_pairs(&req.body);
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        Some(Posted { name: get("name")?, text: get("text")?, frame: None, tc: get("tc"), device: None, form: true })
    } else {
        None
    }
}

fn rate_limited(w: &mut TcpStream, req: &Request, peer: SocketAddr, state: &State) -> io::Result<Option<bool>> {
    let ip = client_ip(peer.ip(), req);
    if let Err(secs) = lock(&state.limiter).take(ip, Instant::now()) {
        return error_reply(w, 429, "Too many comments at once. Try again in a few seconds.", &[("Retry-After", secs.to_string())]).map(Some);
    }
    Ok(None)
}

fn post_comment(w: &mut TcpStream, req: &Request, peer: SocketAddr, state: &State, share: &ShareRecord) -> io::Result<bool> {
    let Some(p) = parse_post(req) else { return error_reply(w, 400, "Send a name and a comment.", &[]) };
    let Some(name) = text::clean(&p.name, MAX_NAME, true) else { return error_reply(w, 400, "Add your name.", &[]) };
    let Some(body) = text::clean(&p.text, MAX_COMMENT, false) else { return error_reply(w, 400, "The comment is empty.", &[]) };
    let frame = match (p.frame, p.tc.as_deref().map(str::trim)) {
        (Some(f), _) => f,
        (None, Some(tc)) if !tc.is_empty() => {
            let parsed = timing::rate(share.rate_num, share.rate_den).and_then(|r| {
                let base = timing::tc_base(r, share.start_timecode, filmcraft_time::Tick(share.range_start));
                timing::parse_frame(r, share.drop_frame, base, share.duration_frames, tc)
            });
            match parsed {
                Some(f) => f,
                None => return error_reply(w, 400, "That timecode is not in this video.", &[]),
            }
        }
        _ => 0,
    };
    if !(0..share.duration_frames.max(1)).contains(&frame) {
        return error_reply(w, 400, "That time is not in this video.", &[]);
    }
    let device = match p.device.as_deref() {
        Some(d) => match token::device_hash(d) {
            Some(h) => h,
            None => return error_reply(w, 400, "Bad device id.", &[]),
        },
        None => String::new(),
    };
    let (Ok(event_id), Ok(comment_id)) = (token::new_id(), token::new_id()) else {
        return error_reply(w, 500, "The server could not make an id.", &[]);
    };
    let ev = CommentEvent { id: event_id, kind: EventKind::Add, comment: comment_id, author: name, text: body, frame, device, at_unix: now_unix() };
    // only writes count against the limit (a refused post costs nothing to disk)
    if let Some(k) = rate_limited(w, req, peer, state)? {
        return Ok(k);
    }
    {
        let _g = lock(&state.write);
        let log = state.dir.read_events(&share.id).unwrap_or_default();
        let standing = fold(&log);
        // the same comment again from the same device (its first answer was lost on a bad
        // connection and the page sent it again): answer with the one already saved
        if !ev.device.is_empty()
            && let Some(c) = standing
                .iter()
                .find(|c| c.frame == ev.frame && c.text == ev.text && c.author == ev.author && owner(&log, &c.id).as_deref() == Some(ev.device.as_str()))
        {
            if p.form {
                let to = format!("/r/{}/#c-{}", share.token, c.id);
                http::respond(w, 303, "text/plain; charset=utf-8", &[("Location", to), ("Cache-Control", "no-store".into())], b"", false, false)?;
                return Ok(true);
            }
            return json_reply(w, 201, &serde_json::to_value(c).map_err(io::Error::other)?, false);
        }
        if log.len() >= MAX_EVENTS || standing.len() >= MAX_COMMENTS {
            return error_reply(w, 507, "This review has reached its comment limit.", &[]);
        }
        if let Err(e) = state.dir.append_event(&share.id, &ev) {
            return error_reply(w, 500, &format!("The comment could not be saved: {e}"), &[]);
        }
        let _ = state.dir.bump_notify();
    }
    state.notify();
    if p.form {
        let to = format!("/r/{}/#c-{}", share.token, ev.comment);
        http::respond(w, 303, "text/plain; charset=utf-8", &[("Location", to), ("Cache-Control", "no-store".into())], b"", false, false)?;
        return Ok(true);
    }
    let c = Comment { id: ev.comment, author: ev.author, text: ev.text, frame: ev.frame, at_unix: ev.at_unix, edited: false };
    json_reply(w, 201, &serde_json::to_value(c).map_err(io::Error::other)?, false)
}

/// The device hash that wrote a standing comment.
fn owner(log: &[CommentEvent], comment: &str) -> Option<String> {
    let mut who = None;
    for e in log.iter().filter(|e| e.comment == comment) {
        match e.kind {
            EventKind::Add if who.is_none() => who = Some(e.device.clone()),
            EventKind::Delete if who.as_deref() == Some(e.device.as_str()) => who = None,
            _ => {}
        }
    }
    who
}

fn change_comment(w: &mut TcpStream, req: &Request, peer: SocketAddr, state: &State, share: &ShareRecord, id: &str) -> io::Result<bool> {
    if !is_hex(id, ID_HEX) {
        return not_found(w, req);
    }
    let v: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return error_reply(w, 400, "Bad request.", &[]),
    };
    let Some(device) = v.get("device").and_then(Value::as_str).and_then(token::device_hash) else {
        return error_reply(w, 403, "Only the device that wrote a comment can change it.", &[]);
    };
    let kind = match v.get("action").and_then(Value::as_str) {
        Some("edit") => EventKind::Edit,
        Some("delete") => EventKind::Delete,
        _ => return error_reply(w, 400, "Unknown action.", &[]),
    };
    let new_text = if kind == EventKind::Edit {
        match v.get("text").and_then(Value::as_str).and_then(|t| text::clean(t, MAX_COMMENT, false)) {
            Some(t) => t,
            None => return error_reply(w, 400, "The comment is empty.", &[]),
        }
    } else {
        String::new()
    };
    let Ok(event_id) = token::new_id() else { return error_reply(w, 500, "The server could not make an id.", &[]) };
    {
        let _g = lock(&state.write);
        let log = state.dir.read_events(&share.id).unwrap_or_default();
        match owner(&log, id) {
            None => return not_found(w, req),
            Some(o) if o.is_empty() || !token::same_secret(&o, &device) => {
                return error_reply(w, 403, "Only the device that wrote a comment can change it.", &[]);
            }
            Some(_) => {}
        }
        if let Some(k) = rate_limited(w, req, peer, state)? {
            return Ok(k);
        }
        if log.len() >= MAX_EVENTS {
            return error_reply(w, 507, "This review has reached its comment limit.", &[]);
        }
        let author = log.iter().find(|e| e.comment == id).map(|e| e.author.clone()).unwrap_or_default();
        let ev = CommentEvent { id: event_id, kind, comment: id.to_string(), author, text: new_text, frame: 0, device, at_unix: now_unix() };
        if let Err(e) = state.dir.append_event(&share.id, &ev) {
            return error_reply(w, 500, &format!("The change could not be saved: {e}"), &[]);
        }
        let _ = state.dir.bump_notify();
    }
    state.notify();
    json_reply(w, 200, &json!({ "ok": true }), false)
}

fn sse_write(w: &mut TcpStream, event: &str, data: &Value) -> io::Result<()> {
    let s = serde_json::to_string(data).map_err(io::Error::other)?;
    w.write_all(format!("event: {event}\ndata: {s}\n\n").as_bytes())?;
    w.flush()
}

/// Live comments: a snapshot, then each new event (without its device hash) as it is written.
fn events(w: &mut TcpStream, peer: SocketAddr, state: &State, share: &ShareRecord) -> io::Result<bool> {
    let ip = peer.ip();
    {
        let mut m = lock(&state.per_ip);
        let e = m.entry(ip).or_insert((0, 0));
        let over = state.sse.load(Ordering::Acquire) >= MAX_SSE || (!ip.is_loopback() && e.1 >= MAX_SSE_PER_IP);
        if over {
            drop(m);
            error_reply(w, 503, "Too many live connections.", &[("Retry-After", "10".into())])?;
            return Ok(false);
        }
        e.1 += 1;
    }
    state.sse.fetch_add(1, Ordering::AcqRel);
    let r = stream_events(w, state, share);
    state.sse.fetch_sub(1, Ordering::AcqRel);
    if let Some(e) = lock(&state.per_ip).get_mut(&ip) {
        e.1 = e.1.saturating_sub(1);
    }
    r.map(|()| false)
}

fn stream_events(w: &mut TcpStream, state: &State, share: &ShareRecord) -> io::Result<()> {
    http::write_head(
        w,
        200,
        &[("Content-Type", "text/event-stream; charset=utf-8".into()), ("Cache-Control", "no-store".into()), ("X-Accel-Buffering", "no".into())],
        true,
    )?;
    w.write_all(b"retry: 3000\n\n")?;
    let mut generation = *lock(&state.generation);
    let log = state.dir.read_events(&share.id).unwrap_or_default();
    let mut sent = log.len();
    sse_write(w, "snapshot", &serde_json::to_value(fold(&log)).map_err(io::Error::other)?)?;
    let mut last_ping = Instant::now();
    loop {
        {
            let g = lock(&state.generation);
            let (g, _) = state
                .changed
                .wait_timeout_while(g, Duration::from_secs(5), |g| *g == generation && !state.stop.load(Ordering::Acquire))
                .unwrap_or_else(PoisonError::into_inner);
            generation = *g;
        }
        if state.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        // a revoked or expired link stops streaming too
        if state.share(&share.token).is_none() {
            return Ok(());
        }
        let log = state.dir.read_events(&share.id).unwrap_or_default();
        if log.len() > sent {
            for e in &log[sent..] {
                let mut e = e.clone();
                e.device.clear();
                sse_write(w, "ev", &serde_json::to_value(e).map_err(io::Error::other)?)?;
            }
            sent = log.len();
            last_ping = Instant::now();
        } else if last_ping.elapsed() >= SSE_PING {
            w.write_all(b": ping\n\n")?;
            w.flush()?;
            last_ping = Instant::now();
        }
    }
}

fn video(w: &mut TcpStream, req: &Request, state: &State, share: &ShareRecord, file: &str) -> io::Result<bool> {
    let head = req.method == "HEAD";
    let Some(name) = file.strip_suffix(".mp4") else { return not_found(w, req) };
    if !share.renditions.iter().any(|r| r.name == name) {
        return not_found(w, req);
    }
    let Some(path) = state.dir.media_path(&share.id, name) else { return not_found(w, req) };
    // a rendered file never changes (a new version is a new link), so the browser may keep what
    // it has and ask only for the bytes it lacks
    send_file(w, req, &path, "video/mp4", "private, max-age=86400, immutable", head)
}

/// Serve a file with byte ranges, so a player on a bad connection asks again for only the part it
/// is missing. `cache`: the `Cache-Control` value.
fn send_file(w: &mut TcpStream, req: &Request, path: &Path, mime: &str, cache: &str, head: bool) -> io::Result<bool> {
    let Ok(mut f) = std::fs::File::open(path) else { return not_found(w, req) };
    let meta = f.metadata()?;
    let len = meta.len();
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let etag = format!("\"{len:x}-{mtime:x}\"");
    let mut h: Vec<(&str, String)> = vec![("Accept-Ranges", "bytes".into()), ("ETag", etag.clone()), ("Cache-Control", cache.into())];
    if req.header("if-none-match") == Some(etag.as_str()) {
        http::write_head(w, 304, &h, false)?;
        w.flush()?;
        return Ok(true);
    }
    // a Range for another version of the file gets the whole new file
    let range_header = match req.header("if-range") {
        Some(v) if v != etag => None,
        _ => req.header("range"),
    };
    let (status, start, count) = match http::parse_range(range_header, len) {
        http::Range::Full => (200, 0, len),
        http::Range::Part(a, b) => {
            h.push(("Content-Range", format!("bytes {a}-{b}/{len}")));
            (206, a, b - a + 1)
        }
        http::Range::Unsatisfiable => {
            h.push(("Content-Range", format!("bytes */{len}")));
            h.push(("Content-Length", "0".into()));
            http::write_head(w, 416, &h, false)?;
            w.flush()?;
            return Ok(true);
        }
    };
    h.push(("Content-Type", mime.into()));
    h.push(("Content-Length", count.to_string()));
    http::write_head(w, status, &h, false)?;
    if !head {
        f.seek(SeekFrom::Start(start))?;
        let copied = io::copy(&mut f.take(count), w)?;
        if copied != count {
            // the file shrank under us: the client must not take this response as complete
            return Ok(false);
        }
    }
    w.flush()?;
    Ok(true)
}

/// A short tag of a Review folder's path, so a server for another data folder is told apart.
fn folder_tag(dir: &ReviewDir) -> String {
    page::version_tag(dir.root().as_os_str().as_encoded_bytes())
}

/// Whether the FilmCraft review server for `dir` answers on this Mac at `port`.
pub fn probe(port: u16, dir: &ReviewDir) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(800)));
    if s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = s.take(4096).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.starts_with("HTTP/1.1 200") && text.contains(&format!("filmcraft-review\n{}\n", folder_tag(dir)))
}

/// The address a config binds.
pub fn bind_addr(bind: Bind, port: u16) -> SocketAddr {
    match bind {
        Bind::Lan => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        Bind::Local => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
    }
}

/// `filmcraft --review-server`: serve the Review folder until no share is live for a while.
/// Exit code 0 when it served or another server already does, 1 on an error (logged to stderr).
pub fn run_main(dir: ReviewDir) -> i32 {
    if let Err(e) = dir.ensure() {
        eprintln!("review server: {}: {e}", dir.root().display());
        return 1;
    }
    let cfg = dir.config();
    let listener = match TcpListener::bind(bind_addr(cfg.bind, cfg.port)) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse && probe(cfg.port, &dir) => return 0,
        Err(e) => {
            eprintln!("review server: port {}: {e}", cfg.port);
            return 1;
        }
    };
    let pid = std::process::id();
    if let Err(e) = dir.save_server_info(&ServerInfo { pid, port: cfg.port, bind: cfg.bind, started_unix: now_unix() }) {
        eprintln!("review server: {e}");
    }
    eprintln!("review server: serving {} on {}", dir.root().display(), bind_addr(cfg.bind, cfg.port));
    let state = State::new(dir.clone());
    let r = serve(listener, state, Options::default());
    dir.clear_server_info(pid);
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("review server: {e}");
            1
        }
    }
}

/// This Mac's address on the local network (the one a default route leaves from). Found with a
/// UDP `connect`, which sends nothing.
pub fn lan_ipv4() -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
