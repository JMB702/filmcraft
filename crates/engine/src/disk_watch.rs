//! The open project's file on disk, compared with what this session read or wrote there.
//!
//! Another program may write the project file while it is open here: `filmcraft-cli --headless`, a
//! sync tool, another FilmCraft. The session remembers the file as it was when it was opened or
//! last saved (its size, modification time and a hash of its bytes, a [`DiskStamp`]); the
//! frontend asks [`check`] every few seconds and when its window comes to the front. A changed
//! file is reloaded when nothing here is unsaved, and the user is asked otherwise (reload, keep
//! mine, or save mine as a copy).
//!
//! Saving never silently replaces a newer file: `file.save` over the open project's own file is
//! refused while the file has changed since this session read or wrote it ([`guard_save`]), unless
//! the user chose to keep their version (`file.keepMine`) or the caller says `force: true`. The
//! auto-save worker's "also save the project" checks the same before it writes.
//!
//! An outside tool that wants its edits to show up in the open app, undoable there, uses the live
//! channel instead (`filmcraft-cli --project P` finds the app that has P open); `history.groupBegin`
//! and `history.groupEnd` make such a run one undo step.

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always};
use crate::{EngineError, Result, Session};

/// The project file as this session last knew it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskStamp {
    pub len: u64,
    /// Modification time in nanoseconds since the Unix epoch, when the host reports one.
    pub modified: Option<u128>,
    /// A hash of the bytes (not a security measure: it tells our own write from another one).
    pub hash: u64,
}

impl DiskStamp {
    /// A base that matches no file: the next check reports any existing file as changed.
    const UNKNOWN: DiskStamp = DiskStamp { len: u64::MAX, modified: None, hash: 0 };

    fn of(stat: (u64, Option<u128>), bytes: &[u8]) -> Self {
        Self { len: stat.0, modified: stat.1, hash: hash_bytes(bytes) }
    }

    fn same_stat(&self, stat: (u64, Option<u128>)) -> bool {
        self.modified.is_some() && (self.len, self.modified) == stat
    }

    fn same_bytes(&self, other: &DiskStamp) -> bool {
        self.len == other.len && self.hash == other.hash
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    // `DefaultHasher::new` has fixed keys: the same bytes give the same hash every time
    let mut h = std::hash::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// What the session knows about its project file (in [`Session::disk`]).
#[derive(Debug, Default)]
pub struct DiskTracker {
    /// The file as it was opened or last saved here. `None`: nothing to compare with (a new
    /// project, one converted from Premiere, a host without files).
    pub base: Option<DiskStamp>,
    /// A newer version on disk the user chose to keep their own over (`file.keepMine`): no longer
    /// reported, and the next save may replace it.
    pub accepted: Option<DiskStamp>,
    /// A newer version on disk not answered yet (the frontend shows the question).
    pub conflict: Option<DiskStamp>,
    /// A newer version that could not be loaded (a half-written file): not retried until the file
    /// changes again.
    pub failed: Option<DiskStamp>,
    /// A save was just refused: the frontend asks at once instead of at its next check.
    pub ask_now: bool,
    /// What the auto-save worker last wrote to the project file ("also save the project"), so its
    /// write isn't taken for another program's before the session hears about it.
    pub own_write: Arc<Mutex<Option<(String, DiskStamp)>>>,
}

/// The project file compared with the session's [`DiskTracker::base`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskState {
    /// The project has no file, or nothing to compare it with.
    NoFile,
    /// As this session read or wrote it (or a version the user chose to keep theirs over).
    Same,
    /// Another program changed it.
    Changed(DiskStamp),
    /// It is gone (moved or deleted); the next save writes it again.
    Missing,
}

impl DiskState {
    pub fn name(&self) -> &'static str {
        match self {
            DiskState::NoFile => "none",
            DiskState::Same => "same",
            DiskState::Changed(_) => "changed",
            DiskState::Missing => "missing",
        }
    }
}

fn stat(s: &Session, path: &str) -> std::io::Result<(u64, Option<u128>)> {
    s.services.file_stamp(path)
}

/// Before reading `path` to open it: its size and time, taken first so a write that lands while
/// it is read shows as a change at the next check rather than being taken for what was read.
pub(crate) fn before_read(s: &Session, path: &str) -> Option<(u64, Option<u128>)> {
    stat(s, path).ok()
}

/// `path` was opened from `bytes` (`stat` from [`before_read`]).
pub(crate) fn loaded(s: &mut Session, stat: Option<(u64, Option<u128>)>, bytes: &[u8]) {
    s.disk.base = stat.map(|st| DiskStamp::of(st, bytes));
    s.disk.accepted = None;
    s.disk.conflict = None;
    s.disk.failed = None;
}

/// `bytes` were just written to the project's own file `path`.
pub(crate) fn saved(s: &mut Session, path: &str, bytes: &[u8]) {
    let st = stat(s, path).unwrap_or((bytes.len() as u64, None));
    s.disk.base = Some(DiskStamp::of(st, bytes));
    s.disk.accepted = None;
    s.disk.conflict = None;
    s.disk.failed = None;
}

/// A project with no file, or one whose file was never read here (converted from Premiere).
pub(crate) fn forget(s: &mut Session) {
    s.disk.base = None;
    s.disk.accepted = None;
    s.disk.conflict = None;
    s.disk.failed = None;
}

/// Unsaved changes recovered after a crash, made over `path` up to `saved_unix`: a file written
/// after that is newer than the recovered work, so it is reported as changed (and a save asks);
/// one written before is the version the work was made over.
pub(crate) fn recovered(s: &mut Session, path: Option<&str>, saved_unix: i64) {
    forget(s);
    let Some(path) = path else { return };
    let Ok(st) = stat(s, path) else { return };
    let newer = st.1.is_none_or(|ns| ns / 1_000_000_000 > u128::try_from(saved_unix.max(0)).unwrap_or(0));
    if newer {
        s.disk.base = Some(DiskStamp::UNKNOWN);
    } else if let Ok(bytes) = s.services.read_file(path) {
        s.disk.base = Some(DiskStamp::of(st, &bytes));
    }
}

/// Compare the project's file with what the session knows. Cheap while the file's size and time
/// are unchanged (one `stat`); reads and hashes it only when they differ.
pub fn check(s: &mut Session) -> DiskState {
    let (Some(path), Some(base)) = (s.path.clone(), s.disk.base) else { return DiskState::NoFile };
    let st = match stat(s, &path) {
        Ok(st) => st,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DiskState::Missing,
        // unreadable for now (a network volume went away): nothing to say yet
        Err(_) => return DiskState::Same,
    };
    if base.same_stat(st) || s.disk.accepted.is_some_and(|a| a.same_stat(st)) || s.disk.failed.is_some_and(|f| f.same_stat(st)) {
        return DiskState::Same;
    }
    let Ok(bytes) = s.services.read_file(&path) else { return DiskState::Same };
    let now = DiskStamp::of(st, &bytes);
    // touched but not changed (a sync tool, `touch`): remember the new time
    if base.same_bytes(&now) {
        s.disk.base = Some(now);
        return DiskState::Same;
    }
    // the auto-save worker's own write, before the session heard of it
    let own = s.disk.own_write.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if own.is_some_and(|(p, o)| p == path && o.same_bytes(&now)) {
        s.disk.base = Some(now);
        return DiskState::Same;
    }
    if s.disk.accepted.is_some_and(|a| a.same_bytes(&now)) {
        s.disk.accepted = Some(now);
        return DiskState::Same;
    }
    if s.disk.failed.is_some_and(|f| f.same_bytes(&now)) {
        s.disk.failed = Some(now);
        return DiskState::Same;
    }
    DiskState::Changed(now)
}

/// May `file.save` write `path` now? Only the project's own file is guarded: Save As and Save a
/// Copy to another file replace whatever is there, as the user chose. A refused save leaves the
/// question for the frontend ([`DiskTracker::conflict`], [`DiskTracker::ask_now`]).
pub(crate) fn guard_save(s: &mut Session, path: &str, force: bool) -> Result<()> {
    if force || s.path.as_deref() != Some(path) {
        return Ok(());
    }
    match check(s) {
        DiskState::Changed(now) => {
            s.disk.conflict = Some(now);
            s.disk.ask_now = true;
            Err(EngineError::Other(format!(
                "{path} changed on disk since it was opened or saved here (another program wrote it), so it was not saved over. \
                 Reload it, keep your version (save again with force), or save yours as a copy."
            )))
        }
        _ => Ok(()),
    }
}

/// When the auto-save worker may write over the project file: while it is still the base, or the
/// version the user chose to keep theirs over; never while a question about a newer file is open.
pub(crate) fn autosave_expectation(s: &Session) -> AutosaveGuard {
    if s.disk.conflict.is_some() {
        return AutosaveGuard::Blocked;
    }
    match s.disk.base {
        None => AutosaveGuard::Unguarded,
        Some(base) => AutosaveGuard::Expect { base, accepted: s.disk.accepted, own_write: Arc::clone(&s.disk.own_write) },
    }
}

/// The condition under which the auto-save worker writes the project file.
#[derive(Clone, Debug)]
pub enum AutosaveGuard {
    /// No file to compare with (as before this guard existed).
    Unguarded,
    /// The user hasn't answered about a newer file: don't write.
    Blocked,
    /// Write only over a file that is still `base` (or `accepted`); record the write in `own_write`.
    Expect { base: DiskStamp, accepted: Option<DiskStamp>, own_write: Arc<Mutex<Option<(String, DiskStamp)>>> },
}

impl AutosaveGuard {
    /// Called by the auto-save worker before it writes `bytes` to `path`: may it?
    pub fn allows(&self, path: &str) -> std::result::Result<(), String> {
        let Self::Expect { base, accepted, own_write } = self else {
            return match self {
                Self::Blocked => Err(format!("Auto Save did not save {path}: it changed on disk and is waiting for your answer")),
                _ => Ok(()),
            };
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("Auto Save did not save {path}: {e}")),
        };
        let now = DiskStamp { len: bytes.len() as u64, modified: None, hash: hash_bytes(&bytes) };
        let own = own_write.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let ours = base.same_bytes(&now) || accepted.is_some_and(|a| a.same_bytes(&now)) || own.is_some_and(|(p, o)| p == path && o.same_bytes(&now));
        if ours { Ok(()) } else { Err(format!("Auto Save did not save {path}: another program changed it since it was opened or saved here")) }
    }

    /// Called by the worker after it wrote `bytes` to `path`.
    pub fn wrote(&self, path: &str, bytes: &[u8]) {
        if let Self::Expect { own_write, .. } = self {
            let st = std::fs::metadata(path).map(|m| (m.len(), m.modified().ok().and_then(nanos))).unwrap_or((bytes.len() as u64, None));
            *own_write.lock().unwrap_or_else(PoisonError::into_inner) = Some((path.to_string(), DiskStamp::of(st, bytes)));
        }
    }
}

/// A modification time as nanoseconds since the Unix epoch.
pub fn nanos(t: std::time::SystemTime) -> Option<u128> {
    t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_nanos())
}

/// `<folder>/<name> (my changes YYYY-MM-DD HHMMSS).fcproj`, numbered if that exists.
pub fn conflict_copy_path(s: &Session, path: &str, unix: i64, offset_seconds: i32) -> String {
    let p = std::path::Path::new(path);
    let stem = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Project".into());
    let dir = p.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let when = chrono::DateTime::from_timestamp(unix.saturating_add(i64::from(offset_seconds)), 0)
        .map(|t| t.format("%Y-%m-%d %H%M%S").to_string())
        .unwrap_or_else(|| unix.to_string());
    for n in 1..1000u32 {
        let name = if n == 1 { format!("{stem} (my changes {when}).fcproj") } else { format!("{stem} (my changes {when}) {n}.fcproj") };
        let candidate = dir.join(name).to_string_lossy().into_owned();
        if s.services.file_size(&candidate).is_err() {
            return candidate;
        }
    }
    dir.join(format!("{stem} (my changes {when} {}).fcproj", s.revision)).to_string_lossy().into_owned()
}

fn status(s: &mut Session) -> Value {
    let state = check(s);
    if let DiskState::Changed(now) = state
        && s.disk.conflict.is_none()
    {
        s.disk.conflict = Some(now);
    }
    json!({"path": s.path, "dirty": s.is_dirty(), "onDisk": state.name(), "conflict": s.disk.conflict.is_some(), "revision": s.revision})
}

/// The open undo group of a live run (`history.groupBegin`): its number and the project before it.
#[derive(Debug, Default)]
pub struct HistoryGroup {
    next: u64,
    open: Option<(u64, Arc<filmcraft_project::Project>)>,
}

fn group_begin(s: &mut Session) -> Value {
    s.history_group.next = s.history_group.next.wrapping_add(1);
    let id = s.history_group.next;
    s.history_group.open = Some((id, Arc::clone(&s.project)));
    // the first edit of the run starts a step of its own
    s.history.merge_key = None;
    json!({"group": id})
}

/// Fold the undo steps made since `history.groupBegin` into one step labelled `label`. When the
/// project from before the group is no longer in the undo list (undone past, or dropped by the
/// history limit), the steps are left as they are.
fn group_end(s: &mut Session, id: u64, label: &str) -> Value {
    let Some((open, before)) = s.history_group.open.take() else { return json!({"grouped": 0, "reason": "no group is open"}) };
    if open != id {
        // a newer group replaced this one: leave both runs' steps as they are
        return json!({"grouped": 0, "reason": "another group started meanwhile"});
    }
    let Some(i) = s.history.undo.iter().rposition(|(_, p, _)| Arc::ptr_eq(p, &before)) else {
        return json!({"grouped": 0, "reason": if Arc::ptr_eq(&s.project, &before) { "nothing changed" } else { "the steps are no longer in the undo history" }});
    };
    let n = s.history.undo.len() - i;
    s.history.undo.truncate(i + 1);
    if let Some(step) = s.history.undo.get_mut(i) {
        step.0 = label.to_string();
    }
    s.history.merge_key = None;
    json!({"grouped": n})
}

pub fn commands() -> Vec<CommandSpec> {
    let spec =
        |id, label, params, run| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal: false, named: NamedKeys::NONE };
    vec![
        spec("file.diskStatus", "Project File Status", "{}", |s, _| Ok(status(s))),
        spec("file.keepMine", "Keep My Version", "{}", |s, _| {
            let accepted = match check(s) {
                DiskState::Changed(now) => {
                    s.disk.accepted = Some(now);
                    true
                }
                _ => false,
            };
            s.disk.conflict = None;
            s.disk.ask_now = false;
            Ok(json!({"accepted": accepted}))
        }),
        spec("file.saveConflictCopy", "Save My Version as a Copy", r#"{"path":str?,"reload":bool=true}"#, |s, p| {
            let own = s.path.clone().ok_or_else(|| EngineError::Other("the project has no file".into()))?;
            let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            let copy = match p.get("path").and_then(Value::as_str) {
                Some(c) => c.to_string(),
                None => {
                    let offset = s.persistence.as_ref().map_or(0, |per| (per.local_offset)(unix));
                    conflict_copy_path(s, &own, unix, offset)
                }
            };
            if copy == own || matches!((std::fs::canonicalize(&copy), std::fs::canonicalize(&own)), (Ok(a), Ok(b)) if a == b) {
                return Err(EngineError::Other("the copy must go to another file".into()));
            }
            s.execute("file.saveCopy", json!({"path": copy}))?;
            let reload = p.get("reload").and_then(Value::as_bool).unwrap_or(true);
            if reload {
                s.execute("file.revert", json!({}))?;
            }
            Ok(json!({"copy": copy, "reloaded": reload}))
        }),
        spec("history.groupBegin", "Begin Undo Group", "{}", |s, _| Ok(group_begin(s))),
        spec("history.groupEnd", "End Undo Group", r#"{"group":u64,"label":str?}"#, |s, p| {
            let id =
                p.get("group").and_then(Value::as_u64).ok_or_else(|| EngineError::BadParams { cmd: "history.groupEnd".into(), msg: "need `group`".into() })?;
            let label = p.get("label").and_then(Value::as_str).unwrap_or("Script");
            Ok(group_end(s, id, label))
        }),
    ]
}

#[cfg(test)]
#[path = "disk_watch_tests.rs"]
mod tests;
