//! `filmcraft-cli --project P` runs in the app that has P open (live), and edits P itself only
//! when no app has it open or `--headless` says so. The "app" is a stand-in serving the live
//! channel from this test, announced in a private folder (`FILMCRAFT_LIVE_DIR`).

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use filmcraft_automation::live::{Announcement, FORMAT, random_token, write_announcement};
use serde_json::{Value, json};

struct Env {
    dir: tempfile::TempDir,
    live: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live");
        std::fs::create_dir(&live).unwrap();
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o700)).unwrap();
        let project = dir.path().join("Film.fcproj");
        let env = Self { dir, live, project };
        let out = env.cli(&["exec", "file.saveAs", &format!("path={}", env.project.display())]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        env
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        crate::filmcraft_command()
            .args(args)
            .env("FILMCRAFT_LIVE_DIR", &self.live)
            .env("FILMCRAFT_DATA_DIR", self.dir.path().join("data"))
            .env_remove("FILMCRAFT_CONTROL_TOKEN")
            .output()
            .unwrap()
    }

    fn script(&self) -> String {
        let p = self.dir.path().join("build.jsonl");
        std::fs::write(
            &p,
            "# placeholder edit\n{\"id\":\"file.newBin\",\"params\":{\"name\":\"Live\"}}\n{\"id\":\"file.newBin\",\"params\":{\"name\":\"Live 2\"}}\n",
        )
        .unwrap();
        p.to_string_lossy().into_owned()
    }

    fn project(&self) -> &str {
        self.project.to_str().unwrap()
    }

    /// The bins in the project file, read headless.
    fn bins_on_disk(&self) -> String {
        let out = self.cli(&["--headless", "--project", self.project(), "--compact", "inspect", "project"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// A stand-in app with `open` open, `dirty` or not, recording the commands it is sent.
fn fake_app(live: &Path, open: &str, dirty: bool) -> Arc<Mutex<Vec<Value>>> {
    let socket = live.join("app.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let token = random_token().unwrap();
    let a = Announcement {
        format: FORMAT,
        pid: std::process::id(),
        socket,
        token: token.clone(),
        project: Some(open.to_string()),
        app_version: "test".into(),
        started_unix: 0,
    };
    write_announcement(&live.join("app.json"), &a).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let open = open.to_string();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let (log, open, token) = (Arc::clone(&log), open.clone(), token.clone());
            std::thread::spawn(move || {
                let mut w = s.try_clone().unwrap();
                for line in BufReader::new(s).lines().map_while(Result::ok) {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    if req["token"] != token.as_str() {
                        let _ = writeln!(w, "{}", json!({"ok": false, "error": "control authentication failed"}));
                        return;
                    }
                    let cmd = &req["params"]["command"];
                    let result = match cmd.as_str() {
                        Some("file.diskStatus") => json!({"path": open, "dirty": dirty, "onDisk": "same", "conflict": dirty}),
                        Some("history.groupBegin") => json!({"group": 7}),
                        _ => json!({}),
                    };
                    if cmd != "file.diskStatus" {
                        log.lock().unwrap().push(req["params"].clone());
                    }
                    let _ = writeln!(w, "{}", json!({"id": req["id"], "ok": true, "result": result}));
                }
            });
        }
    });
    seen
}

fn commands(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
    seen.lock().unwrap().iter().map(|p| p["command"].as_str().unwrap_or_default().to_string()).collect()
}

#[test]
fn a_script_runs_in_the_app_that_has_the_project_open_as_one_undo_step() {
    let env = Env::new();
    let before = std::fs::read(&env.project).unwrap();
    let seen = fake_app(&env.live, env.project(), false);
    let out = env.cli(&["--project", env.project(), "--save", "run", &env.script()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("running in FilmCraft (pid"), "{stderr}");
    assert!(!stderr.contains("asking its user"), "{stderr}");
    assert_eq!(commands(&seen), ["history.groupBegin", "file.newBin", "file.newBin", "history.groupEnd", "file.save"]);
    let end = &seen.lock().unwrap()[3];
    assert_eq!(end["params"]["group"], 7);
    assert_eq!(end["params"]["label"], "filmcraft-cli run build.jsonl");
    // the file was left to the app
    assert_eq!(std::fs::read(&env.project).unwrap(), before);
}

#[test]
fn the_app_is_not_saved_over_its_own_unsaved_changes() {
    let env = Env::new();
    let seen = fake_app(&env.live, env.project(), true);
    let out = env.cli(&["--project", env.project(), "--save", "exec", "file.newBin", "name=Live"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("not saved"), "{stderr}");
    // the stand-in app is also asking about a newer file: the run says whose version it sees
    assert!(stderr.contains("it is asking its user which to keep"), "{stderr}");
    assert_eq!(commands(&seen), ["file.newBin"]);
    // --save-as elsewhere is a copy; the app keeps its project
    let copy = env.dir.path().join("Copy.fcproj");
    let out = env.cli(&["--project", env.project(), "--save-as", copy.to_str().unwrap(), "exec", "file.newBin", "name=X"]);
    assert!(out.status.success());
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["command"], "file.saveCopy");
    assert_eq!(last["params"]["path"], copy.to_str().unwrap());
}

#[test]
fn without_an_app_the_file_is_edited_headless() {
    let env = Env::new();
    let out = env.cli(&["--project", env.project(), "--save", "run", &env.script()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(!stderr.contains("running in FilmCraft"), "{stderr}");
    assert!(env.bins_on_disk().contains("Live 2"));
    // --live insists on an app
    let out = env.cli(&["--live", "--project", env.project(), "exec", "file.newBin", "name=Y"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no running FilmCraft has"));
}

#[test]
fn headless_is_forced_and_the_app_is_left_alone() {
    let env = Env::new();
    let seen = fake_app(&env.live, env.project(), false);
    let out = env.cli(&["--headless", "--project", env.project(), "--save", "run", &env.script()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("--headless: FilmCraft (pid"), "{stderr}");
    assert!(commands(&seen).is_empty(), "nothing went to the app");
    assert!(env.bins_on_disk().contains("Live 2"));
}
