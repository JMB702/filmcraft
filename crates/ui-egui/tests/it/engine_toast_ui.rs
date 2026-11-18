//! An engine toast (#2443) reaches the status bar, for a message and for an error.

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;

fn harness() -> Harness<'static, FilmcraftApp> {
    let app = FilmcraftApp::new(Session::default());
    let mut harness = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_max_steps(10_000).build_eframe(move |_cc| app);
    for _ in 0..3 {
        harness.step();
    }
    harness
}

#[test]
fn engine_toasts_show_in_the_status_bar() {
    let mut h = harness();
    h.state_mut().session.toast("Saved the thing");
    for _ in 0..2 {
        h.step();
    }
    assert_eq!(h.state().ui.status, "Saved the thing");
    h.state_mut().session.error_toast("test", "It failed");
    for _ in 0..2 {
        h.step();
    }
    assert_eq!(h.state().ui.status, "It failed");
}

/// While auto-saves keep failing, the status bar keeps a warning up (the toast is only for the
/// first failure of a streak); a written auto-save takes it down.
#[test]
fn failing_auto_save_keeps_a_status_bar_warning() {
    let dir = std::env::temp_dir().join(format!("fc-autosave-warning-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = harness();
    h.state_mut().session.start_autosave(filmcraft_engine::autosave::AutosaveConfig::new(&dir)).unwrap();
    h.state_mut().session.persistence.as_mut().unwrap().status.auto_save_failing =
        Some("auto-save to /Volumes/X/Auto-Save: Operation not supported (os error 45)".into());
    for _ in 0..2 {
        h.step();
    }
    assert!(h.state().ui.status.starts_with("Auto-save is failing"), "{}", h.state().ui.status);
    assert!(h.state().ui.status.contains("os error 45"));
    // clicking the bar dismisses a status, but this one comes back while the failure lasts
    h.state_mut().ui.status.clear();
    h.step();
    assert!(h.state().ui.status.starts_with("Auto-save is failing"));
    h.state_mut().session.persistence.as_mut().unwrap().status.auto_save_failing = None;
    h.state_mut().ui.status.clear();
    h.step();
    assert!(h.state().ui.status.is_empty());
    h.state_mut().session.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
