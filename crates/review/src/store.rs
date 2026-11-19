//! The files under `<data dir>/Review/`. The folder and the share records are private to this user
//! (0700 / 0600): a share record holds its link token.
//!
//! - `config.json`: port, bind, public base URL (written by the app).
//! - `server.json`: the running server's pid and port (written by the server).
//! - `notify`: rewritten by the server after every comment event; the app watches it.
//! - `shares/<id>/share.json`: the [`ShareRecord`] (written by the app).
//! - `shares/<id>/media/<rendition>.mp4`: the renders (`.partial.mp4` while encoding).
//! - `shares/<id>/events.jsonl`: the append-only comment log (written by the server, synced to disk
//!   before a comment is acknowledged).
//! - `shares/<id>/applied.json`: the ids of the events the app has applied (written by the app).

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{CommentEvent, ShareRecord};
use crate::text::is_hex;
use crate::token::ID_HEX;

/// Largest `share.json`, `config.json` or ledger read.
const MAX_SMALL_FILE: u64 = 1 << 20;
/// Largest comment log read (far above the per-share comment cap).
const MAX_LOG: u64 = 64 << 20;

/// Which addresses the server listens on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Bind {
    /// Every interface: phones on the same network (and a tunnel on this Mac) can reach it.
    #[default]
    Lan,
    /// 127.0.0.1 only (a tunnel on this Mac still can).
    Local,
}

/// `config.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReviewConfig {
    pub port: u16,
    pub bind: Bind,
    /// Base of the links when reviewers come through a tunnel (`https://name.example.net`); empty:
    /// `http://<this Mac's LAN address>:<port>`.
    pub public_base_url: String,
    /// Days until a new link expires (0: never).
    pub expire_days: u32,
    /// The tailscaled socket `review.funnel` asks (empty: Tailscale's default).
    pub tailscale_socket: String,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self { port: 8787, bind: Bind::Lan, public_base_url: String::new(), expire_days: 30, tailscale_socket: String::new() }
    }
}

/// `server.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub pid: u32,
    pub port: u16,
    pub bind: Bind,
    pub started_unix: i64,
}

/// The ids of the comment events the app has applied to its project.
pub type Applied = BTreeSet<String>;

/// `<data dir>/Review/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewDir {
    root: PathBuf,
}

/// Unix seconds now.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)).unwrap_or(0)
}

fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(cap.saturating_add(1)).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is too large", path.display())));
    }
    Ok(buf)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let b = read_capped(path, MAX_SMALL_FILE)?;
    serde_json::from_slice(&b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

/// Write `data` to `path` in one step (a temporary file next to it, synced, then renamed), readable
/// only by this user.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent folder"))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn private_dir(p: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(p)
}

impl ReviewDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The Review folder of a FilmCraft data folder.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("Review"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the folder (0700) if needed.
    pub fn ensure(&self) -> io::Result<()> {
        private_dir(&self.root.join("shares"))
    }

    pub fn config(&self) -> ReviewConfig {
        read_json(&self.root.join("config.json")).unwrap_or_default()
    }

    pub fn save_config(&self, c: &ReviewConfig) -> io::Result<()> {
        self.ensure()?;
        write_atomic(&self.root.join("config.json"), &serde_json::to_vec_pretty(c).map_err(io::Error::other)?)
    }

    pub fn server_info(&self) -> Option<ServerInfo> {
        read_json(&self.root.join("server.json")).ok()
    }

    pub fn save_server_info(&self, s: &ServerInfo) -> io::Result<()> {
        write_atomic(&self.root.join("server.json"), &serde_json::to_vec(s).map_err(io::Error::other)?)
    }

    /// Remove `server.json` if it is this process's.
    pub fn clear_server_info(&self, pid: u32) {
        if self.server_info().is_some_and(|s| s.pid == pid) {
            let _ = std::fs::remove_file(self.root.join("server.json"));
        }
    }

    /// The file the server rewrites after every comment event.
    pub fn notify_path(&self) -> PathBuf {
        self.root.join("notify")
    }

    /// Tell a watching app that a comment event was written.
    pub fn bump_notify(&self) -> io::Result<()> {
        let n = std::fs::read_to_string(self.notify_path()).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
        write_atomic(&self.notify_path(), n.wrapping_add(1).to_string().as_bytes())
    }

    /// A share's folder; `None` for an id that isn't one (so no path ever comes from outside).
    pub fn share_dir(&self, id: &str) -> Option<PathBuf> {
        is_hex(id, ID_HEX).then(|| self.root.join("shares").join(id))
    }

    /// A rendition's final file.
    pub fn media_path(&self, id: &str, rendition: &str) -> Option<PathBuf> {
        if !crate::model::RENDITIONS.contains(&rendition) {
            return None;
        }
        Some(self.share_dir(id)?.join("media").join(format!("{rendition}.mp4")))
    }

    /// Where a rendition is encoded before it is renamed to [`ReviewDir::media_path`].
    pub fn partial_media_path(&self, id: &str, rendition: &str) -> Option<PathBuf> {
        if !crate::model::RENDITIONS.contains(&rendition) {
            return None;
        }
        Some(self.share_dir(id)?.join("media").join(format!("{rendition}.partial.mp4")))
    }

    /// The poster frame (JPEG), if written.
    pub fn poster_path(&self, id: &str) -> Option<PathBuf> {
        Some(self.share_dir(id)?.join("media").join("poster.jpg"))
    }

    pub fn save_share(&self, s: &ShareRecord) -> io::Result<()> {
        let dir = self.share_dir(&s.id).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad share id"))?;
        private_dir(&dir.join("media"))?;
        write_atomic(&dir.join("share.json"), &serde_json::to_vec_pretty(s).map_err(io::Error::other)?)
    }

    pub fn load_share(&self, id: &str) -> io::Result<ShareRecord> {
        let dir = self.share_dir(id).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad share id"))?;
        let s: ShareRecord = read_json(&dir.join("share.json"))?;
        if s.id != id {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "share id does not match its folder"));
        }
        Ok(s)
    }

    /// Every readable share (unreadable ones are skipped).
    pub fn shares(&self) -> Vec<ShareRecord> {
        let Ok(rd) = std::fs::read_dir(self.root.join("shares")) else { return Vec::new() };
        let mut v: Vec<ShareRecord> = rd.flatten().filter_map(|e| e.file_name().to_str().and_then(|id| self.load_share(id).ok())).collect();
        v.sort_by(|a, b| a.created_unix.cmp(&b.created_unix).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// Append one event to a share's comment log and sync it to disk before returning.
    pub fn append_event(&self, id: &str, e: &CommentEvent) -> io::Result<()> {
        let dir = self.share_dir(id).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad share id"))?;
        let mut line = serde_json::to_vec(e).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut o = std::fs::OpenOptions::new();
        o.append(true).create(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(dir.join("events.jsonl"))?;
        f.write_all(&line)?;
        f.sync_data()
    }

    /// The comment log. A last line without its newline (a write cut short) and lines that don't
    /// parse are skipped.
    pub fn read_events(&self, id: &str) -> io::Result<Vec<CommentEvent>> {
        let dir = self.share_dir(id).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad share id"))?;
        let b = match read_capped(&dir.join("events.jsonl"), MAX_LOG) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let complete = match b.iter().rposition(|&c| c == b'\n') {
            Some(i) => &b[..i],
            None => return Ok(Vec::new()),
        };
        Ok(complete.split(|&c| c == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect())
    }

    pub fn applied(&self, id: &str) -> Applied {
        self.share_dir(id).and_then(|d| read_json(&d.join("applied.json")).ok()).unwrap_or_default()
    }

    pub fn save_applied(&self, id: &str, a: &Applied) -> io::Result<()> {
        let dir = self.share_dir(id).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad share id"))?;
        write_atomic(&dir.join("applied.json"), &serde_json::to_vec(a).map_err(io::Error::other)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventKind, SHARE_FORMAT};

    pub(crate) fn share(id: &str) -> ShareRecord {
        ShareRecord {
            format: SHARE_FORMAT,
            id: id.into(),
            token: "ab".repeat(32),
            title: "Seq".into(),
            project: "/p.fcproj".into(),
            sequence: 3,
            rate_num: 25,
            rate_den: 1,
            drop_frame: false,
            start_timecode: 0,
            range_start: 0,
            duration_frames: 250,
            width: 1920,
            height: 1080,
            renditions: vec![],
            created_unix: 1,
            expires_unix: None,
            revoked: false,
        }
    }

    fn ev(id: &str) -> CommentEvent {
        CommentEvent { id: id.into(), kind: EventKind::Add, comment: id.into(), author: "A".into(), text: "t".into(), frame: 1, device: "d".into(), at_unix: 1 }
    }

    #[test]
    fn shares_round_trip_and_bad_ids_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let d = ReviewDir::new(t.path().join("Review"));
        d.ensure().unwrap();
        let s = share("0123456789abcdef");
        d.save_share(&s).unwrap();
        assert_eq!(d.load_share(&s.id).unwrap(), s);
        assert_eq!(d.shares(), vec![s.clone()]);
        assert!(d.share_dir("../../etc").is_none());
        assert!(d.media_path(&s.id, "../share").is_none());
        assert!(d.media_path(&s.id, "hd").is_some());
        assert!(d.load_share("0123456789abcdeF").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.share_dir(&s.id).unwrap().join("share.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the share record (it holds the token) is private");
        }
    }

    #[test]
    fn the_log_survives_a_torn_last_line_and_garbage() {
        let t = tempfile::tempdir().unwrap();
        let d = ReviewDir::new(t.path());
        let id = "0123456789abcdef";
        d.save_share(&share(id)).unwrap();
        assert!(d.read_events(id).unwrap().is_empty());
        d.append_event(id, &ev("e1")).unwrap();
        d.append_event(id, &ev("e2")).unwrap();
        let log = d.share_dir(id).unwrap().join("events.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{not json}\n{\"id\":\"half").unwrap();
        drop(f);
        assert_eq!(d.read_events(id).unwrap().iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e1", "e2"]);
    }

    #[test]
    fn the_ledger_and_config_round_trip() {
        let t = tempfile::tempdir().unwrap();
        let d = ReviewDir::new(t.path());
        let id = "0123456789abcdef";
        d.save_share(&share(id)).unwrap();
        assert!(d.applied(id).is_empty());
        let a: Applied = ["e1".to_string()].into_iter().collect();
        d.save_applied(id, &a).unwrap();
        assert_eq!(d.applied(id), a);
        assert_eq!(d.config(), ReviewConfig::default());
        let c = ReviewConfig { public_base_url: "https://x.example".into(), ..Default::default() };
        d.save_config(&c).unwrap();
        assert_eq!(d.config(), c);
        d.bump_notify().unwrap();
        d.bump_notify().unwrap();
        assert_eq!(std::fs::read_to_string(d.notify_path()).unwrap(), "2");
    }
}
