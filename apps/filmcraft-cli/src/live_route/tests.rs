use super::*;
use filmcraft_automation::live::{FORMAT, random_token, write_announcement};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A stand-in for a running app: answers `file.diskStatus` with `open` (and `dirty`), records
/// every other command, and answers nothing at all when `silent`.
struct FakeApp {
    dir: PathBuf,
    seen: Arc<Mutex<Vec<String>>>,
}

fn private_dir() -> tempfile::TempDir {
    let d = tempfile::Builder::new().prefix("fccli").tempdir().unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

async fn fake_app(dir: &Path, name: &str, open: Option<&str>, hint: Option<&str>, dirty: bool, silent: bool) -> FakeApp {
    let socket = dir.join(format!("{name}.sock"));
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let token = random_token().unwrap();
    let a = Announcement {
        format: FORMAT,
        pid: std::process::id(),
        socket,
        token: token.clone(),
        project: hint.map(str::to_string),
        app_version: "test".into(),
        started_unix: 0,
    };
    write_announcement(&dir.join(format!("{name}.json")), &a).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let open = open.map(str::to_string);
    tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else { return };
            let (log, open, token) = (Arc::clone(&log), open.clone(), token.clone());
            tokio::spawn(async move {
                let (r, mut w) = s.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if silent {
                        continue;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(req["token"], token.as_str(), "every request carries the app's token");
                    let cmd = req["params"]["command"].as_str().unwrap_or_default().to_string();
                    let result = if cmd == "file.diskStatus" {
                        json!({"path": open, "dirty": dirty})
                    } else {
                        log.lock().unwrap().push(cmd);
                        json!({"group": 1})
                    };
                    let reply = format!("{}\n", json!({"id": req["id"], "ok": true, "result": result}));
                    if w.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    FakeApp { dir: dir.to_path_buf(), seen }
}

fn args(list: &[&str]) -> Args {
    Args::parse(list.iter().map(|s| s.to_string()))
}

const SHORT: Duration = Duration::from_millis(300);

async fn decide(dir: &Path, list: &[&str]) -> Result<Route, String> {
    route(&args(list), Some(dir), SHORT).await
}

fn is_live(r: &Result<Route, String>) -> bool {
    matches!(r, Ok(Route::Live { .. }))
}

fn is_headless(r: &Result<Route, String>) -> bool {
    matches!(r, Ok(Route::Headless { .. }))
}

#[tokio::test]
async fn an_app_with_the_project_open_gets_the_run() {
    let d = private_dir();
    let p = d.path().join("p.fcproj");
    std::fs::write(&p, b"x").unwrap();
    let p = p.to_str().unwrap();
    let _app = fake_app(d.path(), "1", Some(p), Some(p), false, false).await;
    let r = decide(d.path(), &["--project", p, "run", "s.jsonl"]).await;
    assert!(is_live(&r));
    // however the path is spelt
    let dotted = d.path().join(".").join("p.fcproj");
    assert!(is_live(&decide(d.path(), &["--project", dotted.to_str().unwrap(), "exec", "x"]).await));
    // --live says the same
    assert!(is_live(&decide(d.path(), &["--live", "--project", p, "exec", "x"]).await));
    // --live without --project: the only app
    let r = decide(d.path(), &["--live", "exec", "x"]).await;
    let Ok(Route::Live { project, dirty, .. }) = r else { panic!("live") };
    assert_eq!(project.as_deref(), Some(p));
    assert!(!dirty);
}

#[tokio::test]
async fn a_project_not_open_anywhere_runs_headless() {
    let d = private_dir();
    let (p, q) = (d.path().join("p.fcproj"), d.path().join("q.fcproj"));
    std::fs::write(&p, b"x").unwrap();
    std::fs::write(&q, b"x").unwrap();
    let (p, q) = (p.to_str().unwrap(), q.to_str().unwrap());
    // no app at all
    assert!(is_headless(&decide(d.path(), &["--project", p, "exec", "x"]).await));
    // an app with another project
    let _app = fake_app(d.path(), "1", Some(q), Some(q), false, false).await;
    assert!(is_headless(&decide(d.path(), &["--project", p, "exec", "x"]).await));
    // an app whose announcement is behind: it has opened q since
    let _late = fake_app(d.path(), "2", Some(q), Some(p), false, false).await;
    assert!(is_headless(&decide(d.path(), &["--project", p, "exec", "x"]).await));
    // --live insists
    let e = decide(d.path(), &["--live", "--project", p, "exec", "x"]).await.err().unwrap();
    assert!(e.contains("no running FilmCraft has"), "{e}");
    // no project, no --live: headless as always (the demo, a new project)
    assert!(is_headless(&decide(d.path(), &["--demo", "exec", "x"]).await));
    // no live channel on this system
    assert!(is_headless(&route(&args(&["--project", p, "exec", "x"]), None, SHORT).await));
}

#[tokio::test]
async fn headless_is_forced_with_a_note() {
    let d = private_dir();
    let p = d.path().join("p.fcproj");
    std::fs::write(&p, b"x").unwrap();
    let p = p.to_str().unwrap();
    let app = fake_app(d.path(), "1", Some(p), Some(p), false, false).await;
    let r = decide(d.path(), &["--headless", "--project", p, "--save", "exec", "x"]).await;
    let Ok(Route::Headless { open_in }) = r else { panic!("headless") };
    assert_eq!(open_in, Some(std::process::id()), "the CLI says the app has it open");
    assert!(app.seen.lock().unwrap().is_empty(), "nothing was sent to the app");
    assert!(decide(d.path(), &["--headless", "--live", "--project", p, "exec", "x"]).await.is_err());
    assert!(decide(d.path(), &["--live", "--demo", "exec", "x"]).await.is_err());
    let _ = app.dir;
}

#[tokio::test]
async fn an_app_that_has_the_project_but_does_not_answer_stops_the_run() {
    let d = private_dir();
    let p = d.path().join("p.fcproj");
    std::fs::write(&p, b"x").unwrap();
    let p = p.to_str().unwrap();
    let _app = fake_app(d.path(), "1", Some(p), Some(p), false, true).await;
    let e = decide(d.path(), &["--project", p, "--save", "exec", "x"]).await.err().unwrap();
    assert!(e.contains("not answering") && e.contains("behind its back"), "{e}");
}

#[tokio::test]
async fn several_apps_need_a_project_name() {
    let d = private_dir();
    let (p, q) = (d.path().join("p.fcproj"), d.path().join("q.fcproj"));
    std::fs::write(&p, b"x").unwrap();
    std::fs::write(&q, b"x").unwrap();
    let (p, q) = (p.to_str().unwrap(), q.to_str().unwrap());
    let a = fake_app(d.path(), "1", Some(p), Some(p), false, false).await;
    let b = fake_app(d.path(), "2", Some(q), Some(q), true, false).await;
    let e = decide(d.path(), &["--live", "exec", "x"]).await.err().unwrap();
    assert!(e.contains("2 FilmCraft windows"), "{e}");
    // each project goes to its own app
    let Ok(Route::Live { project, dirty, .. }) = decide(d.path(), &["--project", q, "exec", "x"]).await else { panic!("live") };
    assert_eq!((project.as_deref(), dirty), (Some(q), true));
    let Ok(Route::Live { client, .. }) = decide(d.path(), &["--project", p, "exec", "x"]).await else { panic!("live") };
    client.execute("file.newBin", json!({"name": "A"})).await.unwrap();
    assert_eq!(*a.seen.lock().unwrap(), ["file.newBin"]);
    assert!(b.seen.lock().unwrap().is_empty());
}
