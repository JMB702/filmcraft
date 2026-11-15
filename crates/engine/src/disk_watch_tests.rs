use super::*;
use crate::Session;
use serde_json::json;

/// A saved project in a fresh folder, open in a session.
fn open_project(name: &str) -> (tempfile::TempDir, String, Session) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name).to_string_lossy().into_owned();
    let mut s = Session::default();
    s.execute("file.saveAs", json!({"path": path})).unwrap();
    let mut s2 = Session::default();
    s2.execute("file.open", json!({"path": path})).unwrap();
    drop(s);
    (dir, path, s2)
}

/// Another program writes the project: here, another session adding a bin and saving headless.
fn write_from_outside(path: &str, bin: &str) {
    let mut other = Session::default();
    other.execute("file.open", json!({"path": path})).unwrap();
    other.execute("file.newBin", json!({"name": bin})).unwrap();
    other.execute("file.save", json!({})).unwrap();
}

fn bins(s: &Session) -> Vec<String> {
    let v = crate::commands::inspect_project(s);
    v["root"]["children"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().map(str::to_string)).collect()
}

fn on_disk(s: &mut Session) -> String {
    s.execute("file.diskStatus", json!({})).unwrap()["onDisk"].as_str().unwrap().to_string()
}

#[test]
fn a_file_changed_by_another_program_is_noticed_and_a_touch_is_not() {
    let (_d, path, mut s) = open_project("p.fcproj");
    assert_eq!(on_disk(&mut s), "same");
    // rewritten with the same bytes (a sync tool, `touch`): not a change
    let bytes = std::fs::read(&path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(on_disk(&mut s), "same");
    write_from_outside(&path, "Outside");
    assert_eq!(on_disk(&mut s), "changed");
    assert!(s.disk.conflict.is_some());
    // reloading takes the other program's version and is clean again
    s.execute("file.revert", json!({})).unwrap();
    assert!(bins(&s).contains(&"Outside".to_string()), "{:?}", bins(&s));
    assert_eq!(on_disk(&mut s), "same");
    assert!(!s.is_dirty());
    std::fs::remove_file(&path).unwrap();
    assert_eq!(on_disk(&mut s), "missing");
}

#[test]
fn save_refuses_to_overwrite_a_newer_file_until_told() {
    let (_d, path, mut s) = open_project("p.fcproj");
    s.execute("file.newBin", json!({"name": "Mine"})).unwrap();
    write_from_outside(&path, "Theirs");
    let theirs = std::fs::read(&path).unwrap();
    let e = s.execute("file.save", json!({})).unwrap_err().to_string();
    assert!(e.contains("changed on disk"), "{e}");
    assert_eq!(std::fs::read(&path).unwrap(), theirs, "the newer file is untouched");
    assert!(s.is_dirty(), "my changes are still unsaved, not lost");
    assert!(s.disk.ask_now && s.disk.conflict.is_some(), "the app asks at once");
    // an explicit path naming the same file is the same save
    assert!(s.execute("file.save", json!({"path": path})).is_err());
    // Save As elsewhere is not guarded
    let other = std::path::Path::new(&path).with_file_name("other.fcproj").to_string_lossy().into_owned();
    s.execute("file.saveCopy", json!({"path": other})).unwrap();
    // force: the user said overwrite
    s.execute("file.save", json!({"force": true})).unwrap();
    assert!(!s.is_dirty());
    assert_eq!(on_disk(&mut s), "same");
}

#[test]
fn keep_mine_lets_the_next_save_replace_that_version_only() {
    let (_d, path, mut s) = open_project("p.fcproj");
    s.execute("file.newBin", json!({"name": "Mine"})).unwrap();
    write_from_outside(&path, "Theirs");
    assert_eq!(on_disk(&mut s), "changed");
    assert_eq!(s.execute("file.keepMine", json!({})).unwrap()["accepted"], true);
    assert_eq!(on_disk(&mut s), "same", "no longer reported");
    assert!(s.disk.conflict.is_none());
    // a further change by the other program is a new question
    write_from_outside(&path, "Theirs again");
    assert!(s.execute("file.save", json!({})).is_err());
    s.execute("file.keepMine", json!({})).unwrap();
    s.execute("file.save", json!({})).unwrap();
    let mut check = Session::default();
    check.execute("file.open", json!({"path": path})).unwrap();
    assert!(bins(&check).contains(&"Mine".to_string()));
    assert!(!bins(&check).contains(&"Theirs again".to_string()));
}

#[test]
fn save_mine_as_a_copy_keeps_both_versions() {
    let (_d, path, mut s) = open_project("Cut.fcproj");
    s.execute("file.newBin", json!({"name": "Mine"})).unwrap();
    write_from_outside(&path, "Theirs");
    let r = s.execute("file.saveConflictCopy", json!({})).unwrap();
    let copy = r["copy"].as_str().unwrap().to_string();
    assert!(copy.contains("Cut (my changes "), "{copy}");
    assert_ne!(copy, path);
    // the open project is now the other program's version, clean
    assert!(bins(&s).contains(&"Theirs".to_string()));
    assert!(!bins(&s).contains(&"Mine".to_string()));
    assert!(!s.is_dirty());
    assert_eq!(s.path.as_deref(), Some(path.as_str()));
    // and my version is in the copy
    let mut mine = Session::default();
    mine.execute("file.open", json!({"path": copy})).unwrap();
    assert!(bins(&mine).contains(&"Mine".to_string()));
    // a second copy at the same second gets another name
    let again = conflict_copy_path(&s, &path, r["copy"].as_str().map(|_| 0).unwrap_or(0), 0);
    let again2 = {
        std::fs::write(&again, b"x").unwrap();
        conflict_copy_path(&s, &path, 0, 0)
    };
    assert_ne!(again, again2);
}

#[test]
fn a_new_or_saved_as_project_is_tracked_from_its_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.fcproj").to_string_lossy().into_owned();
    let mut s = Session::default();
    assert_eq!(on_disk(&mut s), "none");
    s.execute("file.saveAs", json!({"path": path})).unwrap();
    assert_eq!(on_disk(&mut s), "same");
    write_from_outside(&path, "X");
    assert_eq!(on_disk(&mut s), "changed");
    s.execute("file.newProject", json!({})).ok();
    if s.path.is_none() {
        assert_eq!(on_disk(&mut s), "none");
    }
}

#[test]
fn a_live_run_is_one_undo_step() {
    let (_d, _path, mut s) = open_project("p.fcproj");
    let before = s.history.undo.len();
    let g = s.execute("history.groupBegin", json!({})).unwrap()["group"].as_u64().unwrap();
    for n in ["A", "B", "C"] {
        s.execute("file.newBin", json!({"name": n})).unwrap();
    }
    assert_eq!(s.history.undo.len(), before + 3);
    let r = s.execute("history.groupEnd", json!({"group": g, "label": "filmcraft-cli run"})).unwrap();
    assert_eq!(r["grouped"], 3);
    assert_eq!(s.history.undo.len(), before + 1);
    assert_eq!(s.history.undo.last().unwrap().0, "filmcraft-cli run");
    s.execute("edit.undo", json!({})).unwrap();
    let b = bins(&s);
    assert!(!b.iter().any(|n| ["A", "B", "C"].contains(&n.as_str())), "{b:?}");
    s.execute("edit.redo", json!({})).unwrap();
    assert!(bins(&s).contains(&"C".to_string()));
}

#[test]
fn undo_groups_that_cannot_fold_leave_the_steps_alone() {
    let (_d, _path, mut s) = open_project("p.fcproj");
    // nothing changed
    let g = s.execute("history.groupBegin", json!({})).unwrap()["group"].as_u64().unwrap();
    assert_eq!(s.execute("history.groupEnd", json!({"group": g})).unwrap()["grouped"], 0);
    // another run began meanwhile: neither folds the other's steps
    let g1 = s.execute("history.groupBegin", json!({})).unwrap()["group"].as_u64().unwrap();
    s.execute("file.newBin", json!({"name": "A"})).unwrap();
    let g2 = s.execute("history.groupBegin", json!({})).unwrap()["group"].as_u64().unwrap();
    s.execute("file.newBin", json!({"name": "B"})).unwrap();
    let n = s.history.undo.len();
    assert_eq!(s.execute("history.groupEnd", json!({"group": g1})).unwrap()["grouped"], 0);
    assert_eq!(s.execute("history.groupEnd", json!({"group": g2})).unwrap()["grouped"], 0, "g1's end closed the group");
    assert_eq!(s.history.undo.len(), n);
    // hostile parameters are errors, not panics
    assert!(s.execute("history.groupEnd", json!({})).is_err());
    assert!(s.execute("history.groupEnd", json!({"group": -1})).is_err());
    assert_eq!(s.execute("history.groupEnd", json!({"group": u64::MAX})).unwrap()["grouped"], 0);
}

#[test]
fn autosave_never_writes_over_a_newer_file() {
    let (_d, path, mut s) = open_project("p.fcproj");
    s.execute("file.newBin", json!({"name": "Mine"})).unwrap();
    let guard = autosave_expectation(&s);
    assert!(guard.allows(&path).is_ok(), "the file is as it was opened");
    // the worker's own write is not another program's
    let mut other = Session::default();
    other.execute("file.open", json!({"path": path})).unwrap();
    other.execute("file.newBin", json!({"name": "Autosaved"})).unwrap();
    let bytes = filmcraft_format::encode(&other.project, false);
    std::fs::write(&path, &bytes).unwrap();
    guard.wrote(&path, &bytes);
    assert!(guard.allows(&path).is_ok());
    assert_eq!(on_disk(&mut s), "same", "the watcher takes the worker's write for ours");
    // another program's write stops it
    write_from_outside(&path, "Theirs");
    let e = guard.allows(&path).unwrap_err();
    assert!(e.contains("another program changed it"), "{e}");
    // while a question is open the worker doesn't write at all
    assert_eq!(on_disk(&mut s), "changed");
    s.execute("file.diskStatus", json!({})).unwrap();
    assert!(matches!(autosave_expectation(&s), AutosaveGuard::Blocked));
    assert!(autosave_expectation(&s).allows(&path).is_err());
}

#[test]
fn recovered_work_over_a_newer_file_is_reported() {
    let (_d, path, mut s) = open_project("p.fcproj");
    // the recovered snapshot is older than the file: the file is newer than the work
    recovered(&mut s, Some(&path), 0);
    assert_eq!(on_disk(&mut s), "changed");
    // the snapshot is newer than the file: the work was made over it
    recovered(&mut s, Some(&path), i64::MAX / 2);
    assert_eq!(on_disk(&mut s), "same");
    // no file, nothing to compare with
    recovered(&mut s, None, 0);
    assert!(s.disk.base.is_none());
}
