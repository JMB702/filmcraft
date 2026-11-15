use super::*;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

fn private_dir() -> tempfile::TempDir {
    let d = tempfile::Builder::new().prefix("fclive").tempdir().unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn announcement(pid: u32, socket: PathBuf, project: Option<&str>) -> Announcement {
    Announcement {
        format: FORMAT,
        pid,
        socket,
        token: random_token().unwrap(),
        project: project.map(str::to_string),
        app_version: "test".into(),
        started_unix: 0,
    }
}

/// A pid that was just used and is now gone.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn a_running_app_is_found_with_its_project() {
    let dir = private_dir();
    let socket = dir.path().join("a.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let a = announcement(std::process::id(), socket, Some("/x/p.fcproj"));
    write_announcement(&dir.path().join("a.json"), &a).unwrap();
    let mode = std::fs::metadata(dir.path().join("a.json")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the token is readable by this user only");
    let (live, skipped) = running(dir.path());
    assert_eq!(live, vec![a]);
    assert!(skipped.is_empty(), "{skipped:?}");
}

#[test]
fn two_apps_are_both_found_and_told_apart() {
    let dir = private_dir();
    let (s1, s2) = (dir.path().join("1.sock"), dir.path().join("2.sock"));
    let (_l1, _l2) = (UnixListener::bind(&s1).unwrap(), UnixListener::bind(&s2).unwrap());
    let mut a = announcement(std::process::id(), s1, Some("/x/one.fcproj"));
    let mut b = announcement(std::process::id(), s2, Some("/x/two.fcproj"));
    a.started_unix = 1;
    b.started_unix = 2;
    write_announcement(&dir.path().join("1.json"), &a).unwrap();
    write_announcement(&dir.path().join("2.json"), &b).unwrap();
    let (live, _) = running(dir.path());
    assert_eq!(live, vec![a.clone(), b.clone()], "oldest first");
    assert_ne!(a.token, b.token, "each app has its own credential");
}

#[test]
fn a_stale_announcement_is_absent_and_removed() {
    let dir = private_dir();
    // the process is gone
    let gone = announcement(dead_pid(), dir.path().join("gone.sock"), None);
    write_announcement(&dir.path().join("gone.json"), &gone).unwrap();
    // the process runs (a reused pid), but its socket refuses: a crashed app's socket file
    let socket = dir.path().join("dead.sock");
    drop(UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    // pid 1 (launchd/init) is always running, like a pid reused by another program
    let refused = announcement(1, socket.clone(), None);
    write_announcement(&dir.path().join("refused.json"), &refused).unwrap();
    let (live, skipped) = running(dir.path());
    assert!(live.is_empty(), "{live:?}");
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(skipped.iter().all(|s| s.why.starts_with("stale")), "{skipped:?}");
    remove_stale(dir.path());
    assert!(!dir.path().join("gone.json").exists());
    assert!(!dir.path().join("refused.json").exists());
    assert!(!socket.exists(), "the dead socket goes with its announcement");
}

#[test]
fn files_with_wrong_permissions_are_not_trusted() {
    let dir = private_dir();
    let socket = dir.path().join("a.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let file = dir.path().join("a.json");
    write_announcement(&file, &announcement(std::process::id(), socket, None)).unwrap();
    // others may read the token: refused
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (live, skipped) = running(dir.path());
    assert!(live.is_empty());
    assert!(skipped[0].why.contains("other users"), "{skipped:?}");
    // a directory others can enter: nothing in it is used
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let (live, skipped) = running(dir.path());
    assert!(live.is_empty());
    assert!(skipped[0].why.contains("other users"), "{skipped:?}");
    // the app tightens a directory it owns before using it
    ensure_private_dir(dir.path()).unwrap();
    assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(running(dir.path()).0.len(), 1);
}

#[test]
fn hostile_announcements_are_skipped() {
    let dir = private_dir();
    let write = |name: &str, text: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    write("junk.json", "{not json");
    write("short.json", r#"{"format":1,"pid":1,"socket":"/x","token":"ab"}"#);
    write("future.json", &format!(r#"{{"format":99,"pid":1,"socket":"/x","token":"{}"}}"#, "a".repeat(64)));
    write("huge.json", &" ".repeat(200 * 1024));
    std::os::unix::fs::symlink("/etc/hosts", dir.path().join("link.json")).unwrap();
    let (live, skipped) = running(dir.path());
    assert!(live.is_empty());
    assert_eq!(skipped.len(), 5, "{skipped:?}");
}

#[test]
fn a_missing_live_dir_is_no_app_and_is_created_private() {
    let dir = private_dir();
    let live = dir.path().join("a/b/live");
    assert_eq!(running(&live), (vec![], vec![]));
    ensure_private_dir(&live).unwrap();
    assert!(check_private_dir(&live).is_ok());
}

#[test]
fn a_long_live_dir_gets_a_short_private_socket_folder() {
    let dir = private_dir();
    let long = dir.path().join("x".repeat(120));
    let p = socket_path(&long, 42).unwrap();
    assert!(p.as_os_str().len() <= MAX_SOCKET_PATH, "{}", p.display());
    assert!(check_private_dir(p.parent().unwrap()).is_ok());
    let short = socket_path(dir.path(), 42).unwrap();
    assert_eq!(short, dir.path().join("42.sock"));
}

#[test]
fn tokens_are_random_hex() {
    let (a, b) = (random_token().unwrap(), random_token().unwrap());
    assert_eq!(a.len(), 64);
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b);
}

#[test]
fn the_switch_turns_live_off() {
    for v in ["0", "off", "OFF", " false ", "no"] {
        assert!(disabled_by_env(Some(v)), "{v}");
    }
    for v in [None, Some(""), Some("1"), Some("on")] {
        assert!(!disabled_by_env(v), "{v:?}");
    }
}

#[test]
fn project_paths_match_however_they_are_spelt() {
    let dir = private_dir();
    let p = dir.path().join("p.fcproj");
    std::fs::write(&p, b"x").unwrap();
    let dotted = dir.path().join(".").join("p.fcproj");
    assert!(same_file(p.to_str().unwrap(), dotted.to_str().unwrap()));
    assert!(!same_file(p.to_str().unwrap(), dir.path().join("q.fcproj").to_str().unwrap()));
}
