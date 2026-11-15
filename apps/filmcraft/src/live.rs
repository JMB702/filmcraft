//! The live channel: every normally started app serves its control channel on a Unix domain
//! socket in a private per-user folder and announces it there
//! ([`filmcraft_automation::live`]), so `filmcraft-cli --project P …` runs its commands in this
//! app when P is open here, instead of rewriting P behind its back. No flags are needed;
//! `--no-live` or `FILMCRAFT_LIVE=0` turns it off. `--control <port>` still works beside it.
//!
//! Only this user can reach the socket: it sits in a 0700 folder they own, and every request must
//! carry the token from the 0600 announcement. Nothing listens on the network. Windows has no live
//! channel yet.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;

use filmcraft_automation::live::{self, Announcement};
use filmcraft_ui_egui::ControlRequest;

/// This app's announcement; dropping it (when the app quits) removes the files.
pub struct Live {
    file: PathBuf,
    announcement: Announcement,
}

impl Live {
    /// The open project changed (opened, saved as, closed): rewrite the announcement.
    pub fn set_project(&mut self, project: Option<&str>) {
        if self.announcement.project.as_deref() == project {
            return;
        }
        self.announcement.project = project.map(str::to_string);
        #[cfg(unix)]
        if let Err(e) = live::write_announcement(&self.file, &self.announcement) {
            log::warn!("live channel: could not update {}: {e}", self.file.display());
        }
    }

    #[cfg(test)]
    pub fn socket(&self) -> &std::path::Path {
        &self.announcement.socket
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // only our own files: another app may have reused the name after a crash of ours
        #[cfg(unix)]
        if live::read_announcement(&self.file).is_ok_and(|a| a.pid == self.announcement.pid && a.token == self.announcement.token) {
            let _ = std::fs::remove_file(&self.file);
            let _ = std::fs::remove_file(&self.announcement.socket);
        }
    }
}

/// Start serving the live channel into `tx` (the queue the `--control` server shares). The reason
/// when it can't (logged by the caller; the app works without it).
#[cfg(unix)]
pub fn start(ctx: egui::Context, tx: Sender<ControlRequest>, ready: Arc<AtomicBool>) -> Result<Live, String> {
    let dir = live::live_dir().ok_or("no folder for the live channel (HOME is not set)")?;
    start_in(&dir, ctx, tx, ready)
}

#[cfg(unix)]
fn start_in(dir: &std::path::Path, ctx: egui::Context, tx: Sender<ControlRequest>, ready: Arc<AtomicBool>) -> Result<Live, String> {
    use std::os::unix::fs::PermissionsExt;
    let shown = dir.display();
    live::ensure_private_dir(dir).map_err(|e| format!("{shown}: {e}"))?;
    // the files of apps that crashed
    live::remove_stale(dir);
    let pid = std::process::id();
    let socket = live::socket_path(dir, pid).map_err(|e| e.to_string())?;
    // a socket left by an earlier process with this pid (it crashed): nothing listens on it
    if !live::socket_answers(&socket) {
        let _ = std::fs::remove_file(&socket);
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    // the folder already keeps others out; the socket says so too
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    let token = live::random_token().map_err(|e| format!("no random token: {e}"))?;
    let announcement = Announcement {
        format: live::FORMAT,
        pid,
        socket: socket.clone(),
        token: token.clone(),
        project: None,
        app_version: env!("CARGO_PKG_VERSION").into(),
        started_unix: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
    };
    let file = live::announcement_path(dir, pid);
    // announced only once it is bound: a reader that finds the file can connect
    if let Err(e) = live::write_announcement(&file, &announcement) {
        let _ = std::fs::remove_file(&socket);
        return Err(format!("{}: {e}", file.display()));
    }
    crate::control_server::spawn_accept_loop(listener, tx, ctx, token, ready);
    log::info!("live channel at {}", socket.display());
    Ok(Live { file, announcement })
}

#[cfg(not(unix))]
pub fn start(_ctx: egui::Context, _tx: Sender<ControlRequest>, _ready: Arc<AtomicBool>) -> Result<Live, String> {
    Err("the live channel is not available on this system yet".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;

    fn private_dir() -> tempfile::TempDir {
        let d = tempfile::Builder::new().prefix("fclive").tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    /// The app announces a socket only it and this user can use, answers authenticated requests
    /// on it, follows the open project, and removes its files when it goes.
    #[test]
    fn the_live_channel_is_announced_served_and_removed() {
        let dir = private_dir();
        let (tx, rx) = std::sync::mpsc::channel::<ControlRequest>();
        let ready = Arc::new(AtomicBool::new(true));
        let mut l = start_in(dir.path(), egui::Context::default(), tx, ready).unwrap();
        let (found, skipped) = live::running(dir.path());
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(found.len(), 1);
        let a = found[0].clone();
        assert_eq!(a.pid, std::process::id());
        assert_eq!(std::fs::metadata(&a.socket).unwrap().permissions().mode() & 0o077, 0);
        // an answer from the "app"
        std::thread::spawn(move || {
            while let Ok(req) = rx.recv() {
                let method = req.method.clone();
                let _ = req.reply.send(serde_json::json!({"ok": true, "result": method}));
            }
        });
        let mut s = std::os::unix::net::UnixStream::connect(&a.socket).unwrap();
        writeln!(s, "{}", serde_json::json!({"id": 1, "token": a.token, "method": "app.ping"})).unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        assert!(line.contains("\"result\":\"app.ping\""), "{line}");
        // a wrong token is refused before anything runs
        let mut s = std::os::unix::net::UnixStream::connect(&a.socket).unwrap();
        writeln!(s, "{}", serde_json::json!({"id": 1, "token": "0".repeat(64), "method": "app.ping"})).unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        assert!(line.contains("authentication failed"), "{line}");

        l.set_project(Some("/x/p.fcproj"));
        assert_eq!(live::running(dir.path()).0[0].project.as_deref(), Some("/x/p.fcproj"));
        let socket = l.socket().to_path_buf();
        drop(l);
        assert!(live::running(dir.path()).0.is_empty());
        assert!(!socket.exists());
        assert!(!live::announcement_path(dir.path(), std::process::id()).exists());
    }
}
