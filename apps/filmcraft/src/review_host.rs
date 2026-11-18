//! Review links in the desktop app: the review server is this same program started as
//! `filmcraft --review-server <Review folder>`, in its own process group so it keeps taking
//! comments after FilmCraft quits. It stops by itself when no link is live.

use std::path::Path;
use std::time::{Duration, Instant};

use filmcraft_review::server;
use filmcraft_review::store::ReviewDir;

/// `--review-server <dir>` as the first arguments: run the server and return its exit code.
pub fn child_main(args: &[std::ffi::OsString]) -> Option<i32> {
    if args.first().is_none_or(|a| a != "--review-server") {
        return None;
    }
    let Some(dir) = args.get(1).filter(|d| !d.is_empty()) else {
        eprintln!("filmcraft --review-server <Review folder>");
        return Some(2);
    };
    Some(server::run_main(ReviewDir::new(Path::new(dir))))
}

/// Start the review server for `dir` unless one already answers on its port.
pub fn ensure_server(dir: &ReviewDir) -> Result<(), String> {
    let cfg = dir.config();
    if server::probe(cfg.port, dir) {
        return Ok(());
    }
    dir.ensure().map_err(|e| format!("{}: {e}", dir.root().display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("this program's path: {e}"))?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.root().join("server.log")).map_err(|e| format!("server.log: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--review-server").arg(dir.root()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    // its own process group: a Ctrl-C or a signal to FilmCraft's group leaves it running
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| format!("could not start the review server: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if server::probe(cfg.port, dir) {
            // not waited for: it outlives this process
            drop(child);
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("the review server exited ({status}); see {}", dir.root().join("server.log").display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(child);
    Err(format!("the review server did not answer on port {} in time; see {}", cfg.port, dir.root().join("server.log").display()))
}

/// Give a session its Review folder and launcher, and restart the server in the background when
/// a link is still live (after the Mac restarted, for one).
pub fn install(session: &mut filmcraft_engine::Session, data_dir: &Path) {
    let dir = ReviewDir::in_data_dir(data_dir);
    session.review.dir = Some(dir.clone());
    session.review.launcher = Some(std::sync::Arc::new(ensure_server));
    let now = filmcraft_review::store::now_unix();
    if dir.shares().iter().any(|s| s.is_live(now)) {
        let spawned = std::thread::Builder::new().name("review-server-start".into()).spawn(move || {
            if let Err(e) = ensure_server(&dir) {
                log::warn!("review links: {e}");
            }
        });
        if let Err(e) = spawned {
            log::warn!("review links: {e}");
        }
    }
}
