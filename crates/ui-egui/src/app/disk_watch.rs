//! The open project's file and the world outside: the live-channel announcement follows the
//! project path, and a project file another program changed is reloaded (nothing unsaved here)
//! or asked about ([`crate::panels::disk_conflict`]).
//!
//! A window in the background or behind others gets no frames, and a delayed repaint request is
//! not enough to wake it on every system, so a small thread ([`FileWaker`]) watches the file's
//! size and modification time and wakes the UI when they change. Control requests also look at
//! the file first, so an outside tool never reads the project from before its own change on disk.

use super::FilmcraftApp;
use filmcraft_engine::disk_watch::{self, DiskState};
use serde_json::json;

/// Seconds between two looks at the project file (one `stat` while it is unchanged).
const EVERY: f64 = 2.0;

impl FilmcraftApp {
    /// Tell the host which project file is open (the live-channel announcement).
    pub(super) fn tick_project_path(&mut self) {
        if let Some(hook) = self.hooks.project_path_changed.as_mut() {
            hook(self.session.path.as_deref());
        }
    }

    /// Every couple of seconds, when the window comes to the front, and at once after a refused
    /// save: has another program changed the project file? Reload it when nothing here is
    /// unsaved; otherwise leave the question for the dialog.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn tick_disk_watch(&mut self, ctx: &egui::Context, now: f64) {
        let (last_key, focus_key) = (egui::Id::unique("diskWatch.last"), egui::Id::unique("diskWatch.focused"));
        let focused = self.native_window_focused.unwrap_or(false);
        let was_focused: bool = ctx.data(|d| d.get_temp(focus_key)).unwrap_or(focused);
        ctx.data_mut(|d| d.insert_temp(focus_key, focused));
        if self.disk_waker.is_none() {
            // tried once: without the thread, the timer below still runs while frames do
            self.disk_waker = Some(FileWaker::start(ctx.clone(), std::time::Duration::from_secs_f64(EVERY)));
        }
        let waker = self.disk_waker.as_ref().and_then(Option::as_ref);
        let watched = if self.session.disk.base.is_some() { self.session.path.as_deref() } else { None };
        if let Some(w) = waker {
            w.watch(watched);
        }
        let poked = waker.is_some_and(FileWaker::take_poke);
        if watched.is_none() {
            return;
        }
        let last: f64 = ctx.data(|d| d.get_temp(last_key)).unwrap_or(f64::NEG_INFINITY);
        let asked = std::mem::take(&mut self.session.disk.ask_now);
        if asked || poked || now - last >= EVERY || (focused && !was_focused) {
            ctx.data_mut(|d| d.insert_temp(last_key, now));
            if asked {
                crate::panels::disk_conflict::opened_by_save(ctx);
            }
            self.check_project_file();
        }
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(EVERY));
    }

    /// One look at the project file (also what the control channel's tests drive).
    pub fn check_project_file(&mut self) {
        let DiskState::Changed(now) = disk_watch::check(&mut self.session) else { return };
        if self.session.disk.conflict.is_some() || self.session.is_dirty() {
            // unsaved work here: the dialog asks
            if self.session.disk.conflict.is_none() {
                log::info!("project file changed on disk while there are unsaved changes: asking");
            }
            self.session.disk.conflict = Some(now);
            return;
        }
        let name = self.session.project.name.clone();
        match self.session.execute("file.revert", json!({})) {
            Ok(_) => {
                log::info!("project file changed on disk: reloaded");
                self.status(tlf!("Reloaded “{name}”: another program changed it on disk", name));
            }
            Err(e) => {
                // a half-written file: try again once it changes
                self.session.disk.failed = Some(now);
                log::warn!("project file changed on disk but could not be reloaded: {e}");
                self.status(tlf!("“{name}” changed on disk but could not be reloaded: {e}", name, e));
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use waker::FileWaker;

#[cfg(not(target_arch = "wasm32"))]
mod waker {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, SystemTime};

    /// What the thread compares: size and modification time (`None`: the file can't be read).
    type Stamp = Option<(u64, Option<SystemTime>)>;

    fn stamp(path: &Path) -> Stamp {
        std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
    }

    #[derive(Default)]
    struct Shared {
        path: Mutex<Option<PathBuf>>,
        poked: AtomicBool,
        stop: AtomicBool,
    }

    /// Wakes the UI when the watched file's size or modification time changes, with or without
    /// frames. It only wakes: the engine decides whether the file really changed (its content
    /// hash) and what to do. Stops when dropped.
    pub(crate) struct FileWaker {
        shared: Arc<Shared>,
    }

    impl FileWaker {
        /// Look every `every` and call `wake` on a change. `None` if no thread could be started.
        pub(crate) fn start_with(every: Duration, wake: impl Fn() + Send + 'static) -> Option<Self> {
            let shared = Arc::new(Shared::default());
            let state = Arc::clone(&shared);
            let body = move || {
                let mut seen: Option<(PathBuf, Stamp)> = None;
                while !state.stop.load(Ordering::Acquire) {
                    std::thread::sleep(every);
                    let path = state.path.lock().unwrap_or_else(PoisonError::into_inner).clone();
                    let Some(path) = path else {
                        seen = None;
                        continue;
                    };
                    let now = stamp(&path);
                    let changed = seen.as_ref().is_some_and(|(p, s)| *p == path && *s != now);
                    seen = Some((path, now));
                    if changed && !state.stop.load(Ordering::Acquire) {
                        state.poked.store(true, Ordering::Release);
                        wake();
                    }
                }
            };
            let spawned = std::thread::Builder::new().name("filmcraft-project-watch".into()).spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
                    log::error!("project file watch stopped after a panic; the file is still checked while the window draws");
                }
            });
            match spawned {
                Ok(_) => Some(Self { shared }),
                Err(e) => {
                    log::warn!("project file watch thread: {e}");
                    None
                }
            }
        }

        /// Wake `ctx` (a repaint runs the app's logic even while its window is hidden).
        pub(crate) fn start(ctx: egui::Context, every: Duration) -> Option<Self> {
            Self::start_with(every, move || ctx.request_repaint())
        }

        /// The file to watch, or none. A new path starts from its current state.
        pub(crate) fn watch(&self, path: Option<&str>) {
            let mut p = self.shared.path.lock().unwrap_or_else(PoisonError::into_inner);
            if p.as_deref() != path.map(Path::new) {
                *p = path.map(PathBuf::from);
            }
        }

        /// Whether the file changed since the last call.
        pub(crate) fn take_poke(&self) -> bool {
            self.shared.poked.swap(false, Ordering::AcqRel)
        }
    }

    impl Drop for FileWaker {
        fn drop(&mut self) {
            self.shared.stop.store(true, Ordering::Release);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::AtomicUsize;
        use std::time::Instant;

        const EVERY: Duration = Duration::from_millis(20);

        fn wait_for(what: impl Fn() -> bool) -> bool {
            let end = Instant::now() + Duration::from_secs(5);
            while Instant::now() < end {
                if what() {
                    return true;
                }
                std::thread::sleep(EVERY);
            }
            false
        }

        /// No frames run here: the thread alone notices the change and wakes the UI.
        #[test]
        fn a_changed_file_wakes_the_ui_without_frames() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("Film.fcproj");
            std::fs::write(&file, b"one").unwrap();
            let woken = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&woken);
            let w = FileWaker::start_with(EVERY, move || {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
            w.watch(file.to_str());
            // a few looks at an unchanged file wake nothing
            std::thread::sleep(EVERY * 6);
            assert_eq!(woken.load(Ordering::SeqCst), 0);
            assert!(!w.take_poke());
            std::fs::write(&file, b"another program's version").unwrap();
            assert!(wait_for(|| woken.load(Ordering::SeqCst) > 0), "woken");
            assert!(w.take_poke());
            assert!(!w.take_poke(), "once per change");
            // a removed file is a change too; another path starts fresh
            std::fs::remove_file(&file).unwrap();
            assert!(wait_for(|| w.take_poke()), "removal noticed");
            let other = dir.path().join("Other.fcproj");
            std::fs::write(&other, b"x").unwrap();
            let before = woken.load(Ordering::SeqCst);
            w.watch(other.to_str());
            std::thread::sleep(EVERY * 6);
            assert_eq!(woken.load(Ordering::SeqCst), before, "switching files is not a change");
        }

        #[test]
        fn a_dropped_waker_stops_waking() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("Film.fcproj");
            std::fs::write(&file, b"one").unwrap();
            let woken = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&woken);
            let w = FileWaker::start_with(EVERY, move || {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
            w.watch(file.to_str());
            std::thread::sleep(EVERY * 3);
            drop(w);
            std::thread::sleep(EVERY * 2);
            std::fs::write(&file, b"changed after the window closed").unwrap();
            std::thread::sleep(EVERY * 6);
            assert_eq!(woken.load(Ordering::SeqCst), 0);
        }
    }
}
