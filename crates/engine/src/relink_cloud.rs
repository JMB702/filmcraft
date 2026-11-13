//! iCloud media: a file whose contents are in the cloud (a placeholder left by "Optimise Mac
//! Storage") is downloaded in the background and comes online when it lands, without reopening
//! the project.
//!
//! A download is a worker thread that reads the file ([`Services::materialize`]): reading a
//! placeholder makes the system download all of it first. No helper tool or private API is
//! needed (macOS 26 has no `brctl download`), and nothing outside `crates/platform` needs
//! `unsafe`. The system reports no byte progress this way, so each download shows its size and
//! how long it has been running; a percentage needs Foundation's ubiquitous-item APIs, left for
//! `crates/platform`.
//!
//! - **Own file** (the item's stored path is the placeholder): once here, it is opened as an
//!   import opens it and the item takes the file's real properties and fingerprint, without an
//!   undo step (the file changed, not the edit), as a refreshed file does ([`crate::media_watch`]).
//! - **Candidate** (the automatic search found only a placeholder for a missing item): once here,
//!   it is checked like any candidate ([`crate::relink_auto::judge`]) and linked only if it passes;
//!   otherwise Link Media asks.
//! - **Disk space:** a download that would leave less than [`FREE_MARGIN`] free on its drive is
//!   not started by itself; Link Media offers it ([`DlState::NeedsSpace`]).

use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_project::{ItemId, MediaRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use web_time::Instant;

use crate::relink::Plan;
use crate::relink_auto::{ReportRow, Suggestion, Verdict, judge, signals};
use crate::{EngineError, Result, Session};

/// Space a download must leave free on its drive to start by itself.
pub const FREE_MARGIN: u64 = 10 << 30;

/// Where a download stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DlState {
    Downloading,
    /// Here and online.
    Done,
    Failed,
    /// Not started: it would leave less than [`FREE_MARGIN`] free.
    NeedsSpace,
    /// A candidate that, once downloaded, did not match the clip.
    Rejected,
}

/// One download, as the interface lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub item: u64,
    pub name: String,
    /// The file being downloaded.
    pub path: String,
    pub size: u64,
    /// A search candidate (checked once here), not the item's own file.
    pub candidate: bool,
    pub state: DlState,
    pub error: Option<String>,
    /// Seconds since it started.
    pub seconds: u64,
}

/// A download's result slot.
pub type DlSlot = Arc<Mutex<Option<std::result::Result<(), String>>>>;

/// Running downloads: item, file, start, slot.
#[derive(Default)]
pub struct Downloads {
    jobs: Vec<(ItemId, String, Instant, DlSlot)>,
}

impl Downloads {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

fn set_state(s: &mut Session, id: ItemId, path: &str, f: impl FnOnce(&mut Download)) {
    if let Some(d) = s.offline.downloads.iter_mut().find(|d| d.item == id.0 && d.path == path) {
        f(d);
    }
}

/// Download `path` for item `id` (`candidate`: a search candidate, checked once here). Refused
/// without `force` when it would leave less than [`FREE_MARGIN`] free on its drive.
pub fn start(s: &mut Session, id: ItemId, path: &str, candidate: bool, force: bool) -> Result<()> {
    if s.relink_auto.downloads.jobs.iter().any(|(i, p, ..)| *i == id && p == path) {
        return Ok(());
    }
    let name = s.project.item(id).map(|i| i.name.clone()).unwrap_or_default();
    let size = s.services.file_size(path).unwrap_or(0);
    s.offline.downloads.retain(|d| !(d.item == id.0 && d.path == path));
    let mut d = Download { item: id.0, name, path: path.to_string(), size, candidate, state: DlState::Downloading, error: None, seconds: 0 };
    if !force
        && let Some(free) = s.services.free_space(path)
        && free < size.saturating_add(FREE_MARGIN)
    {
        d.state = DlState::NeedsSpace;
        d.error = Some(format!("{:.1} GB to download, {:.1} GB free", size as f64 / 1e9, free as f64 / 1e9));
        s.offline.downloads.push(d);
        return Err(EngineError::Other(format!("not enough free space to download {path}")));
    }
    let slot: DlSlot = Arc::default();
    let out = slot.clone();
    let services = s.services.clone();
    let file = path.to_string();
    let spawned = std::thread::Builder::new().name("icloud-download".into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| services.materialize(&file)));
        let r = match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("the download failed".to_string()),
        };
        *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
    });
    if let Err(e) = spawned {
        d.state = DlState::Failed;
        d.error = Some(e.to_string());
        s.offline.downloads.push(d);
        return Err(EngineError::Other(e.to_string()));
    }
    s.offline.downloads.push(d);
    s.relink_auto.downloads.jobs.push((id, path.to_string(), Instant::now(), slot));
    Ok(())
}

/// Start downloading every media file that is a cloud placeholder. Returns how many started.
pub fn download_placeholders(s: &mut Session, force: bool) -> usize {
    let mut n = 0;
    for id in crate::relink::not_downloaded(s) {
        let Some(MediaRef::File { path }) = s.project.item(id).and_then(|i| i.as_media()).map(|m| m.media.clone()) else { continue };
        let Ok(file) = s.media.follow(&path) else { continue };
        if start(s, id, &file, false, force).is_ok() {
            n += 1;
        }
    }
    n
}

/// The item's own file has landed: open it as an import does and bring the item online.
fn bring_online(s: &mut Session, id: ItemId, file: &str) -> std::result::Result<(), String> {
    let Some(MediaRef::File { path }) = s.project.item(id).and_then(|i| i.as_media()).map(|m| m.media.clone()) else {
        return Err("the clip no longer uses this file".into());
    };
    if s.media.follow(&path).ok().as_deref() != Some(file) {
        return Err("the clip no longer uses this file".into());
    }
    let src = s.media.open_file(file, &*s.services).map_err(|e| e.to_string())?;
    let info = src.info().clone();
    let identity = crate::relink::identity_of(&*s.services, file).ok();
    let p = Arc::make_mut(&mut s.project);
    if let Some(m) = p.item_mut(id).and_then(|i| i.as_media_mut()) {
        m.info = info;
        if identity.is_some() {
            m.identity = identity;
        }
    }
    s.media.insert_file(id, &path, src);
    s.bump_view();
    Ok(())
}

/// A search candidate has landed: link it if it passes, otherwise leave it to Link Media.
fn check_landed(s: &mut Session, id: ItemId, file: &str) -> std::result::Result<bool, String> {
    let Some(it) = s.project.item(id) else { return Err("the clip is gone".into()) };
    let Some(m) = it.as_media().cloned() else { return Err("the clip is gone".into()) };
    let name = it.name.clone();
    let MediaRef::File { path: from } = m.media.clone() else { return Err("the clip is not file media".into()) };
    match judge(&m, &s.media, &*s.services, file) {
        Verdict::Pass(c) => {
            let row = ReportRow {
                item: id.0,
                name,
                from: from.clone(),
                to: c.path.clone(),
                via: crate::relink_auto::Via::Spotlight,
                signals: signals(&m, &c).into_iter().map(str::to_string).collect(),
            };
            crate::relink::apply(s, vec![Plan { item: id, cand: c }], false, "Link Downloaded Media").map_err(|e| e.to_string())?;
            if let Some(r) = s.offline.report.as_mut() {
                r.linked.push(row);
                r.downloading.retain(|i| *i != id.0);
            }
            Ok(true)
        }
        Verdict::Fail(c) => {
            let sugg = Suggestion { path: c.path, ok: false, identity_match: c.identity_match, problems: c.problems };
            s.offline.suggestions.entry(id.0).or_default().insert(0, sugg);
            Ok(false)
        }
        Verdict::Cloud => Err("the download did not complete".into()),
    }
}

/// Finish the downloads that are done. Returns status lines for the interface.
pub fn poll(s: &mut Session) -> Vec<String> {
    let mut lines = Vec::new();
    let mut jobs = std::mem::take(&mut s.relink_auto.downloads.jobs);
    let now = Instant::now();
    for (id, path, started, _) in &jobs {
        let secs = now.saturating_duration_since(*started).as_secs();
        set_state(s, *id, path, |d| d.seconds = secs);
    }
    let mut done = Vec::new();
    jobs.retain(|(id, path, _, slot)| match slot.lock().unwrap_or_else(PoisonError::into_inner).take() {
        Some(r) => {
            done.push((*id, path.clone(), r));
            false
        }
        None => true,
    });
    s.relink_auto.downloads.jobs = jobs;
    for (id, path, r) in done {
        let candidate = s.offline.downloads.iter().any(|d| d.item == id.0 && d.path == path && d.candidate);
        let landed = r.and_then(|()| if s.services.is_cloud_placeholder(&path) { Err("the download did not complete".into()) } else { Ok(()) });
        let result = landed.and_then(|()| if candidate { check_landed(s, id, &path) } else { bring_online(s, id, &path).map(|()| true) });
        let name = crate::relink::file_name(&path).to_string();
        match result {
            Ok(true) => {
                set_state(s, id, &path, |d| d.state = DlState::Done);
                lines.push(format!("{name} downloaded from iCloud and is online"));
            }
            Ok(false) => {
                set_state(s, id, &path, |d| d.state = DlState::Rejected);
                lines.push(format!("{name} downloaded from iCloud but is not the clip's media; Link Media lists it"));
                s.offline.prompt = true;
            }
            Err(e) => {
                set_state(s, id, &path, |d| {
                    d.state = DlState::Failed;
                    d.error = Some(e.clone());
                });
                lines.push(format!("{name} could not be downloaded from iCloud: {e}"));
            }
        }
    }
    if !lines.is_empty() {
        crate::relink::refresh(s);
    }
    lines
}

/// `media.downloadFromCloud {items?, force?}`: download the items' cloud placeholders (every
/// placeholder without `items`). `force` starts them even when the drive is nearly full.
pub fn download_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let force = p.get("force").and_then(Value::as_bool).unwrap_or(false);
    let items: Option<Vec<u64>> = p.get("items").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect());
    let Some(items) = items else {
        let n = download_placeholders(s, force);
        return Ok(json!({"started": n, "downloads": s.offline.downloads}));
    };
    let mut n = 0;
    for i in items {
        // an item's own placeholder, or the candidate a search wanted to download
        let target = s.offline.downloads.iter().find(|d| d.item == i).map(|d| (d.path.clone(), d.candidate));
        let target = target.or_else(|| match s.project.item(ItemId(i)).and_then(|it| it.as_media()).map(|m| m.media.clone()) {
            Some(MediaRef::File { path }) => s.media.follow(&path).ok().map(|f| (f, false)),
            _ => None,
        });
        let Some((path, candidate)) = target else { continue };
        if start(s, ItemId(i), &path, candidate, force).is_ok() {
            n += 1;
        }
    }
    Ok(json!({"started": n, "downloads": s.offline.downloads}))
}

/// Free bytes on the drive holding `path` (its nearest existing folder), from `df`. None when it
/// can't be told.
#[cfg(unix)]
pub fn free_space(path: &str) -> Option<u64> {
    let mut p = std::path::Path::new(path);
    while !p.exists() {
        p = p.parent()?;
    }
    let out = std::process::Command::new("/bin/df").arg("-Pk").arg(p).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    // Filesystem 1024-blocks Used Available Capacity Mounted-on. The mount point may hold
    // spaces, so Available is found as the field before Capacity (the one ending in `%`).
    let fields: Vec<&str> = line.split_whitespace().collect();
    let cap = fields.iter().position(|f| f.ends_with('%'))?;
    let avail = fields.get(cap.checked_sub(1)?)?;
    avail.parse::<u64>().ok()?.checked_mul(1024)
}

#[cfg(not(unix))]
pub fn free_space(_: &str) -> Option<u64> {
    None
}
