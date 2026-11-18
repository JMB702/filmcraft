//! Review links (File ▸ Share for Review…): render a sequence for the web, share it as a private
//! link served by the review server, and turn reviewers' comments into markers on that sequence.
//!
//! The server (`filmcraft-review`, its own process) only writes comments to disk. This module
//! reads them: [`sync`] applies every comment event not yet in the share's ledger to the open
//! project, one undo step each, so a remote marker can be undone and auto-save covers it. Comments
//! for a project that is not open wait on disk and are applied when it opens. The marker's web
//! address is the comment's link (`…#c=<comment id>`), which is how later edits and deletes find it.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use filmcraft_project::{ItemId, Label, Marker, MarkerId, MarkerKind};
use filmcraft_review::model::{CommentEvent, EventKind, Rendition, SHARE_FORMAT, ShareRecord};
use filmcraft_review::store::{Bind, ReviewConfig, ReviewDir, now_unix};
use filmcraft_review::{text, timing, token};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{bad, bool_p, str_p, u64_p};
use crate::panels::Level;
use crate::{EngineError, Event, Result, Session};

const CMD: &str = "review.share";

/// Starts the review server (or finds it running) for a Review folder. Installed by the desktop
/// app, which knows its own executable.
pub type Launcher = Arc<dyn Fn(&ReviewDir) -> std::result::Result<(), String> + Send + Sync>;

/// Review-link state of a session.
#[derive(Default)]
pub struct ReviewRuntime {
    /// `<data dir>/Review` (None: review links are off, e.g. in the browser build).
    pub dir: Option<ReviewDir>,
    pub launcher: Option<Launcher>,
    /// Renders still to make, one at a time.
    renders: Vec<PendingRender>,
    /// Events applied in this session whose ledger could not be written yet.
    applied_unsaved: HashSet<String>,
    /// Shares whose sequence is missing, reported once.
    reported: HashSet<String>,
}

struct PendingRender {
    share: String,
    /// Renditions still to encode after the current one, in order.
    queue: Vec<Rendition>,
    current: Option<(Rendition, u64)>,
    sequence: ItemId,
    range: TimeRange,
}

impl ReviewRuntime {
    /// Whether renders of any share are still running.
    pub fn rendering(&self) -> bool {
        !self.renders.is_empty()
    }
}

fn dir(s: &Session, cmd: &str) -> Result<ReviewDir> {
    s.review.dir.clone().ok_or_else(|| bad(cmd, "review links are not available in this build"))
}

/// The two renditions for a sequence of `w`×`h`: `small` within 960×540 for phones on cellular,
/// `hd` within 1920×1080. A sequence no larger than 960×540 gets only `small`.
pub fn renditions(w: u32, h: u32) -> Vec<Rendition> {
    fn fit(w: u32, h: u32, mw: u32, mh: u32) -> (u32, u32) {
        let (w, h) = (w.max(2), h.max(2));
        let s = (f64::from(mw) / f64::from(w)).min(f64::from(mh) / f64::from(h)).min(1.0);
        let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
        (even(f64::from(w) * s), even(f64::from(h) * s))
    }
    let (sw, sh) = fit(w, h, 960, 540);
    let small = Rendition { name: "small".into(), width: sw, height: sh, bitrate_kbps: 1400 };
    let (hw, hh) = fit(w, h, 1920, 1080);
    if (hw, hh) == (sw, sh) {
        return vec![small];
    }
    vec![small, Rendition { name: "hd".into(), width: hw, height: hh, bitrate_kbps: 6000 }]
}

/// The base of review links: the public URL (tunnel) when set, else this Mac's LAN address.
pub fn link_base(cfg: &ReviewConfig) -> String {
    let public = cfg.public_base_url.trim().trim_end_matches('/');
    if !public.is_empty() {
        return public.to_string();
    }
    let host = match cfg.bind {
        Bind::Local => "localhost".to_string(),
        Bind::Lan => filmcraft_review::server::lan_ipv4().map(|ip| ip.to_string()).unwrap_or_else(|| "localhost".into()),
    };
    format!("http://{host}:{}", cfg.port)
}

/// The link of a share.
pub fn link(cfg: &ReviewConfig, share: &ShareRecord) -> String {
    format!("{}/r/{}/", link_base(cfg), share.token)
}

fn same_file(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `review.share`: render the sequence for the web and make a link.
pub fn share(s: &mut Session, p: &Value) -> Result<Value> {
    let d = dir(s, CMD)?;
    let project_path = s.path.clone().ok_or_else(|| bad(CMD, "save the project first: review comments come back to its file"))?;
    let seq = match u64_p(p, "sequence") {
        Some(id) => ItemId(id),
        None => s.state.active_sequence.ok_or(EngineError::NoSequence)?,
    };
    let project = s.project.clone();
    let q = project.sequence(seq).ok_or_else(|| bad(CMD, "no such sequence"))?;
    let title = project.item(seq).map(|i| i.name.clone()).unwrap_or_else(|| "Sequence".into());
    let rate = q.settings.frame_rate;
    let fd = rate.frame_duration();
    let range = match str_p(p, "range") {
        None | Some("entire") => TimeRange::from_bounds(Tick::ZERO, q.duration().max(fd)),
        Some("inOut") => filmcraft_export::export_range(&project, seq, &filmcraft_export::ExportSettings::default()).map_err(|e| bad(CMD, e.to_string()))?,
        Some(other) => return Err(bad(CMD, format!("range `{other}`: entire | inOut"))),
    };
    let end = range.start.0.checked_add(range.duration.0).ok_or_else(|| bad(CMD, "the sequence is too long"))?;
    let frames = rate.frame_at(Tick(end.saturating_sub(1))).saturating_sub(rate.frame_at(range.start)).saturating_add(1).max(1);
    let cfg = d.config();
    let days = u64_p(p, "expiresDays").map(|v| v.min(3650) as u32).unwrap_or(cfg.expire_days);
    let now = now_unix();
    let token = token::new_token().map_err(|e| bad(CMD, format!("no random source: {e}")))?;
    let id = token::new_id().map_err(|e| bad(CMD, format!("no random source: {e}")))?;
    let rends = renditions(q.settings.width, q.settings.height);
    let record = ShareRecord {
        format: SHARE_FORMAT,
        id: id.clone(),
        token,
        title,
        project: project_path,
        sequence: seq.0,
        rate_num: rate.num,
        rate_den: rate.den,
        drop_frame: q.settings.drop_frame,
        start_timecode: q.start_timecode,
        range_start: range.start.0,
        duration_frames: frames,
        width: q.settings.width,
        height: q.settings.height,
        renditions: rends.clone(),
        created_unix: now,
        expires_unix: (days > 0).then(|| now.saturating_add(i64::from(days) * 86_400)),
        revoked: false,
    };
    d.ensure().map_err(|e| bad(CMD, format!("{}: {e}", d.root().display())))?;
    d.save_share(&record).map_err(|e| bad(CMD, format!("could not save the share: {e}")))?;
    let mut pending = PendingRender { share: id.clone(), queue: rends.clone(), current: None, sequence: seq, range };
    let job = start_next(s, &d, &mut pending)?;
    s.review.renders.push(pending);
    let server = match s.review.launcher.clone() {
        Some(l) => match l(&d) {
            Ok(()) => "running".to_string(),
            Err(e) => {
                s.log.push(Level::Error, "review", format!("The review server did not start: {e}"));
                format!("not running: {e}")
            }
        },
        None => "not started (no server in this host)".into(),
    };
    let url = link(&cfg, &record);
    s.events.push(Event::CopyText(url.clone()));
    s.events.push(Event::Toast { message: format!("Review link copied. Rendering {} for the web…", record.title), error: false });
    s.log.push(Level::Info, "review", format!("Shared {} for review: {url}", record.title));
    Ok(json!({
        "link": url,
        "share": id,
        "sequence": seq.0,
        "frames": frames,
        "renditions": rends,
        "job": job,
        "server": server,
        "expiresUnix": record.expires_unix,
        "copied": true,
    }))
}

/// Start the next rendition's export. Returns its job id (None when none are left).
fn start_next(s: &mut Session, d: &ReviewDir, r: &mut PendingRender) -> Result<Option<u64>> {
    if r.queue.is_empty() {
        r.current = None;
        return Ok(None);
    }
    let rend = r.queue.remove(0);
    let partial = d.partial_media_path(&r.share, &rend.name).ok_or_else(|| bad(CMD, "bad share id"))?;
    let _ = std::fs::remove_file(&partial);
    let end = r.range.start.0.saturating_add(r.range.duration.0);
    let v = s.execute(
        "file.exportMedia",
        json!({
            "path": path_str(&partial),
            "format": "h264",
            "sequence": r.sequence.0,
            "width": rend.width,
            "height": rend.height,
            "bitrateKbps": rend.bitrate_kbps,
            "hardwareEncoding": "auto",
            "audio": true,
            "range": {"mode": "custom", "startTime": r.range.start.0, "endTime": end},
        }),
    )?;
    let job = v.get("job").and_then(Value::as_u64).ok_or_else(|| bad(CMD, "the export did not start"))?;
    r.current = Some((rend, job));
    Ok(Some(job))
}

/// Follow review renders: a finished rendition is moved into place and the next one starts.
/// Called with the other background polls (cheap when nothing renders).
pub fn poll(s: &mut Session) {
    if s.review.renders.is_empty() {
        return;
    }
    let Some(d) = s.review.dir.clone() else { return };
    let mut renders = std::mem::take(&mut s.review.renders);
    renders.retain_mut(|r| {
        let Some((rend, job)) = r.current.clone() else { return false };
        let Some(j) = s.jobs.iter().find(|j| j.id == job) else { return false };
        let result = j.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        match result {
            None => true,
            Some(Ok(_)) => {
                let (Some(from), Some(to)) = (d.partial_media_path(&r.share, &rend.name), d.media_path(&r.share, &rend.name)) else { return false };
                if let Err(e) = std::fs::rename(&from, &to) {
                    s.log.push(Level::Error, "review", format!("Review render {}: {e}", rend.name));
                    return false;
                }
                s.log.push(Level::Info, "review", format!("Review video ready ({} {}×{})", rend.name, rend.width, rend.height));
                match start_next(s, &d, r) {
                    Ok(Some(_)) => true,
                    Ok(None) => {
                        s.events.push(Event::Toast { message: "Review video ready".into(), error: false });
                        false
                    }
                    Err(e) => {
                        s.log.push(Level::Error, "review", format!("Review render: {e}"));
                        false
                    }
                }
            }
            Some(Err(e)) => {
                s.log.push(Level::Error, "review", format!("Review render {} failed: {e}", rend.name));
                s.events.push(Event::Toast { message: format!("Review render failed: {e}"), error: true });
                false
            }
        }
    });
    renders.append(&mut s.review.renders);
    s.review.renders = renders;
}

/// The marker a comment makes.
fn marker_for(id: MarkerId, at: Tick, ev: &CommentEvent, url: String) -> Marker {
    Marker {
        url,
        author: ev.author.clone(),
        custom_color: None,
        id,
        start: at,
        duration: Tick::ZERO,
        name: text::marker_name(&ev.author, &ev.text),
        comment: ev.text.clone(),
        kind: MarkerKind::Comment,
        color: Label::Cerulean,
    }
}

fn comment_tag(comment: &str) -> String {
    format!("#c={comment}")
}

/// Apply one comment event to the sequence. `Ok(false)`: nothing to do (already there, or its
/// marker is gone).
fn apply(s: &mut Session, share: &ShareRecord, seq: ItemId, ev: &CommentEvent, url: &str) -> Result<bool> {
    let tag = comment_tag(&ev.comment);
    let q = s.project.sequence(seq).ok_or(EngineError::NoSequence)?;
    let existing = q.markers.iter().position(|m| m.url.ends_with(&tag));
    let who = if ev.author.is_empty() { "a reviewer".to_string() } else { ev.author.clone() };
    match ev.kind {
        EventKind::Add => {
            if existing.is_some() {
                return Ok(false);
            }
            let rate = timing::rate(share.rate_num, share.rate_den).ok_or_else(|| bad("review.sync", "the share has no frame rate"))?;
            let Some(at) = timing::frame_to_tick(rate, Tick(share.range_start), share.duration_frames, ev.frame) else {
                s.log.push(Level::Warning, "review", format!("A comment from {who} is outside the shared video; skipped"));
                return Ok(false);
            };
            let url = format!("{url}{tag}");
            s.edit(&format!("Review Comment from {who}"), |p, _| {
                let id = MarkerId(p.next_id);
                p.next_id = p.next_id.checked_add(1).ok_or_else(|| EngineError::Other("out of ids".into()))?;
                let q = p.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                q.markers.push(marker_for(id, at, ev, url));
                q.markers.sort_by_key(|m| m.start);
                Ok(())
            })?;
            let tc = timing::timecode(rate, share.drop_frame, timing::tc_base(rate, share.start_timecode, Tick(share.range_start)), ev.frame);
            s.events.push(Event::Toast { message: format!("{who} commented at {tc}: {}", text::marker_name("", &ev.text)), error: false });
            Ok(true)
        }
        EventKind::Edit => {
            let Some(i) = existing else { return Ok(false) };
            s.edit(&format!("Review Comment Edited by {who}"), |p, _| {
                let q = p.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                if let Some(m) = q.markers.get_mut(i) {
                    m.comment = ev.text.clone();
                    m.name = text::marker_name(&m.author, &ev.text);
                }
                Ok(())
            })?;
            Ok(true)
        }
        EventKind::Delete => {
            let Some(i) = existing else { return Ok(false) };
            s.edit(&format!("Review Comment Deleted by {who}"), |p, _| {
                let q = p.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
                if i < q.markers.len() {
                    q.markers.remove(i);
                }
                Ok(())
            })?;
            Ok(true)
        }
    }
}

/// Apply the comments waiting for the open project (`review.sync`). Cheap when there are none:
/// it lists the shares and reads the logs of this project's shares.
pub fn sync(s: &mut Session) -> Result<Value> {
    let Some(d) = s.review.dir.clone() else { return Ok(json!({"applied": 0})) };
    let Some(path) = s.path.clone() else { return Ok(json!({"applied": 0})) };
    let cfg = d.config();
    let mut applied_n = 0u64;
    let mut waiting = 0u64;
    for share in d.shares().into_iter().filter(|sh| same_file(&sh.project, &path)) {
        let events = match d.read_events(&share.id) {
            Ok(e) => e,
            Err(e) => {
                s.log.push(Level::Error, "review", format!("Review comments of {}: {e}", share.title));
                continue;
            }
        };
        let mut ledger: BTreeSet<String> = d.applied(&share.id);
        // applied earlier in this session, but the ledger could not be written then
        let unsaved: Vec<String> =
            events.iter().filter(|e| s.review.applied_unsaved.contains(&e.id) && !ledger.contains(&e.id)).map(|e| e.id.clone()).collect();
        let todo: Vec<CommentEvent> = events.into_iter().filter(|e| !ledger.contains(&e.id) && !s.review.applied_unsaved.contains(&e.id)).collect();
        if todo.is_empty() && unsaved.is_empty() {
            continue;
        }
        let seq = ItemId(share.sequence);
        if !todo.is_empty() && s.project.sequence(seq).is_none() {
            waiting += todo.len() as u64;
            if s.review.reported.insert(share.id.clone()) {
                s.log.push(
                    Level::Warning,
                    "review",
                    format!("{} review comments wait for sequence “{}”, which is not in this project", todo.len(), share.title),
                );
            }
            continue;
        }
        let url = link(&cfg, &share);
        for ev in &todo {
            match apply(s, &share, seq, ev, &url) {
                Ok(done) => applied_n += u64::from(done),
                Err(e) => s.log.push(Level::Error, "review", format!("Review comment: {e}")),
            }
            ledger.insert(ev.id.clone());
        }
        ledger.extend(unsaved.iter().cloned());
        match d.save_applied(&share.id, &ledger) {
            Ok(()) => s.review.applied_unsaved.retain(|e| !ledger.contains(e)),
            Err(e) => {
                s.log.push(Level::Error, "review", format!("Could not record applied review comments: {e}"));
                s.review.applied_unsaved.extend(todo.iter().map(|e| e.id.clone()));
            }
        }
    }
    Ok(json!({"applied": applied_n, "waiting": waiting}))
}

fn share_json(cfg: &ReviewConfig, d: &ReviewDir, sh: &ShareRecord, now: i64) -> Value {
    let events = d.read_events(&sh.id).unwrap_or_default();
    let comments = filmcraft_review::model::fold(&events).len();
    let applied = d.applied(&sh.id);
    let ready: Vec<&str> = sh.renditions.iter().filter(|r| d.media_path(&sh.id, &r.name).is_some_and(|p| p.is_file())).map(|r| r.name.as_str()).collect();
    json!({
        "share": sh.id,
        "title": sh.title,
        "link": link(cfg, sh),
        "project": sh.project,
        "sequence": sh.sequence,
        "createdUnix": sh.created_unix,
        "expiresUnix": sh.expires_unix,
        "revoked": sh.revoked,
        "live": sh.is_live(now),
        "comments": comments,
        "pending": events.iter().filter(|e| !applied.contains(&e.id)).count(),
        "ready": ready,
    })
}

/// `review.list`: the shares of the open project (`all`: every share).
pub fn list(s: &mut Session, p: &Value) -> Result<Value> {
    let d = dir(s, "review.list")?;
    let cfg = d.config();
    let all = bool_p(p, "all").unwrap_or(false);
    let now = now_unix();
    let path = s.path.clone();
    let v: Vec<Value> =
        d.shares().iter().filter(|sh| all || path.as_deref().is_some_and(|p| same_file(&sh.project, p))).map(|sh| share_json(&cfg, &d, sh, now)).collect();
    Ok(json!({ "shares": v, "folder": path_str(d.root()) }))
}

/// `review.revoke`: the link stops working at once.
pub fn revoke(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "review.revoke";
    let d = dir(s, cmd)?;
    let id = str_p(p, "share").ok_or_else(|| bad(cmd, "need `share`"))?;
    let mut sh = d.load_share(id).map_err(|e| bad(cmd, format!("share {id}: {e}")))?;
    sh.revoked = true;
    d.save_share(&sh).map_err(|e| bad(cmd, e.to_string()))?;
    s.log.push(Level::Info, "review", format!("Revoked the review link of {}", sh.title));
    Ok(json!({"share": id, "revoked": true}))
}

/// Check a public base URL: `http(s)://host[:port][/path]`, no credentials, query or fragment.
fn public_base(v: &str) -> std::result::Result<String, String> {
    let v = v.trim().trim_end_matches('/');
    if v.is_empty() {
        return Ok(String::new());
    }
    if v.len() > 200 || v.chars().any(|c| c.is_whitespace() || c.is_control()) || v.contains(['?', '#', '@', '\\', '"', '<', '>']) {
        return Err("publicBaseUrl: a plain http(s) address, like https://review.example.net".into());
    }
    let rest = v.strip_prefix("https://").or_else(|| v.strip_prefix("http://")).ok_or("publicBaseUrl must start with https:// or http://")?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err("publicBaseUrl needs a host".into());
    }
    Ok(v.to_string())
}

/// `review.config`: read, or change, where the server listens and what links look like.
pub fn config(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "review.config";
    let d = dir(s, cmd)?;
    let mut cfg = d.config();
    let mut changed = false;
    if let Some(port) = u64_p(p, "port") {
        cfg.port = u16::try_from(port).ok().filter(|p| *p >= 1024).ok_or_else(|| bad(cmd, "port: 1024–65535"))?;
        changed = true;
    }
    if let Some(b) = str_p(p, "bind") {
        cfg.bind = match b {
            "lan" => Bind::Lan,
            "local" => Bind::Local,
            _ => return Err(bad(cmd, "bind: lan | local")),
        };
        changed = true;
    }
    if let Some(u) = str_p(p, "publicBaseUrl") {
        cfg.public_base_url = public_base(u).map_err(|e| bad(cmd, e))?;
        changed = true;
    }
    if let Some(days) = u64_p(p, "expireDays") {
        cfg.expire_days = days.min(3650) as u32;
        changed = true;
    }
    if changed {
        d.save_config(&cfg).map_err(|e| bad(cmd, e.to_string()))?;
    }
    Ok(json!({
        "port": cfg.port,
        "bind": cfg.bind,
        "publicBaseUrl": cfg.public_base_url,
        "expireDays": cfg.expire_days,
        "linkBase": link_base(&cfg),
        "folder": path_str(d.root()),
        "changed": changed,
        "note": if changed { "a running server keeps its port and bind until it restarts" } else { "" },
    }))
}

/// Start the review server if a live share needs it (the desktop app calls this at start, so
/// links work again after the Mac restarts and FilmCraft opens).
pub fn ensure_server_for_live_shares(s: &mut Session) {
    let (Some(d), Some(l)) = (s.review.dir.clone(), s.review.launcher.clone()) else { return };
    let now = now_unix();
    if d.shares().iter().any(|sh| sh.is_live(now))
        && let Err(e) = l(&d)
    {
        s.log.push(Level::Error, "review", format!("The review server did not start: {e}"));
    }
}

/// The folder a session's review data lives in, for a data folder.
pub fn dir_for(data_dir: &Path) -> PathBuf {
    data_dir.join("Review")
}
