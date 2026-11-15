//! Finding the FilmCraft apps running for this user, so outside tools can edit the project that is
//! open in one of them ("live"), instead of writing its file behind the app's back.
//!
//! Every app started normally serves its control channel on a Unix domain socket and announces it
//! with a small JSON file in a private per-user directory ([`live_dir`]):
//!
//! ```text
//! ~/Library/Application Support/FilmCraft/live/      (0700, owned by the user)
//!     <pid>.json                                       (0600: pid, socket, token, open project)
//!     <pid>.sock                                       (the socket itself)
//! ```
//!
//! Only the user can enter the directory, so only the user's processes can reach a socket in it;
//! nothing listens on the network. Each request must still carry the token from the announcement
//! (64 random hex characters), so a socket that ends up somewhere less private is still closed to
//! anyone who cannot read the announcement. Each app has its own pid-named pair of files, so
//! several apps can run at once. An app removes its files when it quits; the files of an app that
//! crashed stay behind, and readers treat them as absent: the process is gone, or the socket
//! refuses connections (which also covers a reused pid). See `docs/control-protocol.md`.
//!
//! Windows has no live channel yet (no per-user socket there): [`live_dir`] is `None`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Overrides the live directory (tests, sandboxes). Both the app and the CLI read it.
pub const DIR_ENV: &str = "FILMCRAFT_LIVE_DIR";
/// `0`, `off`, `false` or `no` turns the live channel off in the app (like `--no-live`).
pub const SWITCH_ENV: &str = "FILMCRAFT_LIVE";
/// Version of the announcement format.
pub const FORMAT: u32 = 1;
/// Longest socket path used as is. `sun_path` holds 104 bytes on macOS and 108 on Linux,
/// including the terminating NUL; a longer path goes to a private folder in the temp directory.
const MAX_SOCKET_PATH: usize = 100;
/// Largest announcement read; a real one is a few hundred bytes.
const MAX_ANNOUNCEMENT: u64 = 64 * 1024;

/// What a running app writes about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announcement {
    pub format: u32,
    pub pid: u32,
    /// The Unix domain socket serving the control channel.
    pub socket: PathBuf,
    /// The credential every request must carry (64 hex characters).
    pub token: String,
    /// The project file open in the app now (`None`: an unsaved project or none). A hint for
    /// people and tools; ask the app (`file.diskStatus`) for the authoritative answer.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub started_unix: i64,
}

/// Is the live channel turned off by [`SWITCH_ENV`]?
pub fn disabled_by_env(value: Option<&str>) -> bool {
    value.map(|v| v.trim().to_ascii_lowercase()).is_some_and(|v| matches!(v.as_str(), "0" | "off" | "false" | "no"))
}

/// Where apps announce themselves: [`DIR_ENV`], else a `live` folder in the per-user FilmCraft
/// folder (`~/Library/Application Support/FilmCraft/live` on macOS; `$XDG_RUNTIME_DIR/filmcraft`
/// on Linux when the session has one, else `~/.local/share/filmcraft/live`). It does not follow
/// `--data-dir` or a portable install, so the CLI finds the app wherever either was started from.
/// `None` where there is no live channel (Windows) or no home folder.
pub fn live_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(d) = env(DIR_ENV) {
        return Some(d);
    }
    if !cfg!(unix) {
        return None;
    }
    if cfg!(target_os = "macos") {
        return env("HOME").map(|h| h.join("Library/Application Support/FilmCraft/live"));
    }
    if let Some(run) = env("XDG_RUNTIME_DIR") {
        return Some(run.join("filmcraft"));
    }
    env("XDG_DATA_HOME").map(|d| d.join("filmcraft/live")).or_else(|| env("HOME").map(|h| h.join(".local/share/filmcraft/live")))
}

/// The announcement file of process `pid`.
pub fn announcement_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.json"))
}

/// 64 random hex characters from the operating system.
#[cfg(unix)]
pub fn random_token() -> io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
mod unix {
    use super::{Announcement, FORMAT, MAX_ANNOUNCEMENT, MAX_SOCKET_PATH, Path, PathBuf, io};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

    fn uid() -> u32 {
        rustix::process::getuid().as_raw()
    }

    /// Why a directory or file may not be trusted, or `None`.
    fn untrusted(meta: &std::fs::Metadata, what: &str) -> Option<String> {
        if meta.uid() != uid() {
            return Some(format!("{what} belongs to another user"));
        }
        if meta.mode() & 0o077 != 0 {
            return Some(format!("{what} can be read or written by other users (mode {:o})", meta.mode() & 0o777));
        }
        None
    }

    /// Create `dir` (and its parents) if needed and make sure it is private: a directory owned by
    /// this user that no one else may enter. A directory this user owns is tightened to 0700; one
    /// owned by someone else, or a symlink, is refused.
    pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!("{} is not a directory", dir.display())));
        }
        if meta.uid() != uid() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} belongs to another user", dir.display())));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Is `dir` private to this user (see [`ensure_private_dir`])? The reason when it is not.
    pub fn check_private_dir(dir: &Path) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        untrusted(&meta, &dir.display().to_string()).map_or(Ok(()), Err)
    }

    /// The socket for process `pid`: in `dir` when the path fits in a socket address, else in a
    /// private folder in the temp directory named after `dir`.
    pub fn socket_path(dir: &Path, pid: u32) -> io::Result<PathBuf> {
        let name = format!("{pid}.sock");
        let path = dir.join(&name);
        if path.as_os_str().len() <= MAX_SOCKET_PATH {
            return Ok(path);
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        dir.hash(&mut h);
        let short = std::env::temp_dir().join(format!("filmcraft-live-{}-{:08x}", uid(), h.finish() as u32));
        ensure_private_dir(&short)?;
        let path = short.join(name);
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(io::Error::other(format!("no socket path short enough for {}", dir.display())));
        }
        Ok(path)
    }

    /// Write `a` as `file` (0600), replacing it atomically.
    pub fn write_announcement(file: &Path, a: &Announcement) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(a).map_err(io::Error::other)?;
        let tmp = file.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, file).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    /// Read one announcement, refusing a file that isn't this user's or that others can read
    /// (the token is in it).
    pub fn read_announcement(file: &Path) -> Result<Announcement, String> {
        use std::io::Read;
        let shown = file.display();
        let meta = std::fs::symlink_metadata(file).map_err(|e| format!("{shown}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{shown} is not a regular file"));
        }
        if let Some(why) = untrusted(&meta, &shown.to_string()) {
            return Err(why);
        }
        let mut text = String::new();
        std::fs::File::open(file).and_then(|f| f.take(MAX_ANNOUNCEMENT).read_to_string(&mut text)).map_err(|e| format!("{shown}: {e}"))?;
        let a: Announcement = serde_json::from_str(&text).map_err(|e| format!("{shown}: {e}"))?;
        if a.format != FORMAT {
            return Err(format!("{shown}: announcement format {} (this build reads {FORMAT})", a.format));
        }
        if a.token.len() != 64 || !a.token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{shown}: malformed token"));
        }
        Ok(a)
    }

    /// Is process `pid` running? (A pid that can't be a process is not.)
    pub fn pid_alive(pid: u32) -> bool {
        let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else { return false };
        match rustix::process::test_kill_process(pid) {
            Ok(()) => true,
            // it exists but belongs to someone else: not one of ours, and not a stale file either
            Err(e) => e == rustix::io::Errno::PERM,
        }
    }

    /// Does the socket accept a connection? A dead app's socket file refuses, as does a socket
    /// path that no longer exists.
    pub fn socket_answers(socket: &Path) -> bool {
        std::os::unix::net::UnixStream::connect(socket).is_ok()
    }
}

#[cfg(unix)]
pub use unix::{check_private_dir, ensure_private_dir, pid_alive, read_announcement, socket_answers, socket_path, write_announcement};

/// One entry of the live directory that was not used, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub file: PathBuf,
    pub why: String,
}

/// The apps running for this user in `dir`, plus the entries that were skipped (stale, wrong
/// permissions, unreadable), oldest first. A directory that isn't private yields nothing.
#[cfg(unix)]
pub fn running(dir: &Path) -> (Vec<Announcement>, Vec<Skipped>) {
    let mut live = Vec::new();
    let mut skipped = Vec::new();
    if let Err(why) = check_private_dir(dir) {
        if std::fs::symlink_metadata(dir).is_ok() {
            skipped.push(Skipped { file: dir.to_path_buf(), why });
        }
        return (live, skipped);
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return (live, skipped) };
    for e in entries.flatten() {
        let file = e.path();
        if file.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        match read_announcement(&file) {
            Ok(a) if !pid_alive(a.pid) => skipped.push(Skipped { file, why: format!("stale: process {} is not running", a.pid) }),
            Ok(a) if !socket_answers(&a.socket) => skipped.push(Skipped { file, why: format!("stale: {} does not answer", a.socket.display()) }),
            Ok(a) => live.push(a),
            Err(why) => skipped.push(Skipped { file, why }),
        }
    }
    live.sort_by_key(|a| (a.started_unix, a.pid));
    (live, skipped)
}

/// Remove the announcements (and their sockets) of apps that are no longer running. Files that
/// are not ours (another owner, wrong permissions) are left alone.
#[cfg(unix)]
pub fn remove_stale(dir: &Path) {
    if check_private_dir(dir).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let file = e.path();
        if file.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(a) = read_announcement(&file)
            && a.pid != std::process::id()
            && (!pid_alive(a.pid) || !socket_answers(&a.socket))
        {
            let _ = std::fs::remove_file(&a.socket);
            let _ = std::fs::remove_file(&file);
        }
    }
}

/// Do `a` and `b` name the same project file? Compared as written first, then with symlinks and
/// `.`/`..` resolved, so `./p.fcproj` and its absolute path match.
pub fn same_file(a: &str, b: &str) -> bool {
    a == b || matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

#[cfg(all(test, unix))]
mod tests;
