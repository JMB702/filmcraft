//! The open project's file and the world outside: the live-channel announcement follows the
//! project path, and a project file another program changed is reloaded (nothing unsaved here)
//! or asked about ([`crate::panels::disk_conflict`]).

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
        if self.session.path.is_none() || self.session.disk.base.is_none() {
            return;
        }
        let last: f64 = ctx.data(|d| d.get_temp(last_key)).unwrap_or(f64::NEG_INFINITY);
        let asked = std::mem::take(&mut self.session.disk.ask_now);
        if asked || now - last >= EVERY || (focused && !was_focused) {
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
            self.session.disk.conflict = Some(now);
            return;
        }
        let name = self.session.project.name.clone();
        match self.session.execute("file.revert", json!({})) {
            Ok(_) => self.status(tlf!("Reloaded “{name}”: another program changed it on disk", name)),
            Err(e) => {
                // a half-written file: try again once it changes
                self.session.disk.failed = Some(now);
                self.status(tlf!("“{name}” changed on disk but could not be reloaded: {e}", name, e));
            }
        }
    }
}
