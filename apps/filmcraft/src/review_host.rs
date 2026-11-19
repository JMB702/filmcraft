//! Review links in the desktop app: the review server is this same program started as
//! `filmcraft --review-server <Review folder>`, in its own process group so it keeps taking
//! comments after FilmCraft quits. It stops by itself when no link is live.

use std::path::Path;
use std::time::{Duration, Instant};

use filmcraft_review::server;
use filmcraft_review::store::ReviewDir;
use filmcraft_review::tunnel::{MAX_OUTPUT, ProbeOutput};

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
    session.review.tunnel = Some(std::sync::Arc::new(tailscale_probe));
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

/// The `tailscale` program: the usual install places first (an app started from the Dock has no
/// shell `PATH`), then `PATH`.
fn tailscale_tool() -> Option<std::path::PathBuf> {
    let fixed = ["/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/bin/tailscale"];
    let from_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).map(|d| d.join("tailscale")).collect::<Vec<_>>()).unwrap_or_default();
    fixed.iter().map(std::path::PathBuf::from).chain(from_path).find(|p| p.is_file())
}

/// Run `tailscale [--socket S] <args>` and return its output, or why it failed. Gives up after a
/// few seconds and reads at most [`MAX_OUTPUT`] bytes.
fn run_tailscale(tool: &Path, socket: &str, args: &[&str]) -> Result<String, String> {
    use std::io::Read;
    let mut cmd = std::process::Command::new(tool);
    if !socket.is_empty() {
        cmd.arg(format!("--socket={socket}"));
    }
    cmd.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("could not run tailscale: {e}"))?;
    let read_all = |r: Option<Box<dyn Read + Send>>| {
        std::thread::Builder::new().name("tailscale-output".into()).spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                // capped: one byte past the limit marks it as too long
                let _ = r.take(MAX_OUTPUT as u64 + 1).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = read_all(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let err = read_all(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let deadline = Instant::now() + Duration::from_secs(4);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let collect = |h: std::io::Result<std::thread::JoinHandle<Vec<u8>>>| h.ok().and_then(|h| h.join().ok()).unwrap_or_default();
    let (out, err) = (collect(out), collect(err));
    match status {
        None => Err("tailscale did not answer in time".into()),
        Some(st) if st.success() => String::from_utf8(out).map_err(|_| "tailscale printed something that is not text".into()),
        // `status --json` can exit non-zero while still printing the state (signed out, say)
        Some(_) if out.first() == Some(&b'{') => String::from_utf8(out).map_err(|_| "tailscale printed something that is not text".into()),
        Some(st) => {
            let msg = String::from_utf8_lossy(&err);
            let msg = msg.trim();
            Err(if msg.is_empty() { format!("tailscale exited with {st}") } else { msg.chars().take(400).collect() })
        }
    }
}

/// `review.funnel`'s probe: two read-only status commands. Never signs in or changes anything.
fn tailscale_probe(socket: &str) -> ProbeOutput {
    let Some(tool) = tailscale_tool() else {
        return ProbeOutput::not_installed();
    };
    let status = run_tailscale(&tool, socket, &["status", "--json"]);
    let serve = if status.is_ok() { run_tailscale(&tool, socket, &["funnel", "status", "--json"]) } else { Err("skipped".into()) };
    ProbeOutput { tool: Some(tool.display().to_string()), status, serve }
}
