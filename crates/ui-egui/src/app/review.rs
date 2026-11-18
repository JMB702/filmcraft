//! Review links in the window: comments reviewers post become markers within about a second.
//!
//! The review server rewrites `Review/notify` after every comment; a [`FileWaker`] thread looks at
//! it four times a second and wakes the UI (even a hidden window), which then applies the new
//! comments (`filmcraft_engine::review::sync`). Opening another project applies the comments that
//! waited for it; a slow timer covers a missed wake-up.

use super::FilmcraftApp;
use super::disk_watch::FileWaker;
use std::time::Duration;

/// How often the waker looks at the notify file.
const WAKE_EVERY: Duration = Duration::from_millis(250);
/// Seconds between syncs when nothing woke us.
const FALLBACK: f64 = 10.0;

impl FilmcraftApp {
    pub(super) fn tick_review(&mut self, ctx: &egui::Context, now: f64) {
        let Some(dir) = self.session.review.dir.clone() else { return };
        if self.review_waker.is_none() {
            self.review_waker = Some(FileWaker::start(ctx.clone(), WAKE_EVERY));
        }
        let waker = self.review_waker.as_ref().and_then(Option::as_ref);
        if let Some(w) = waker {
            w.watch(dir.notify_path().to_str());
        }
        let poked = waker.is_some_and(FileWaker::take_poke);
        let (path_key, time_key) = (egui::Id::unique("review.syncedPath"), egui::Id::unique("review.syncedAt"));
        let synced_path: Option<Option<String>> = ctx.data(|d| d.get_temp(path_key));
        let last: f64 = ctx.data(|d| d.get_temp(time_key)).unwrap_or(f64::NEG_INFINITY);
        let opened = synced_path.as_ref() != Some(&self.session.path);
        if poked || opened || now - last >= FALLBACK {
            ctx.data_mut(|d| {
                d.insert_temp(path_key, self.session.path.clone());
                d.insert_temp(time_key, now);
            });
            match filmcraft_engine::review::sync(&mut self.session) {
                Ok(v) => {
                    if v["applied"].as_u64().unwrap_or(0) > 0 {
                        // the project changed outside a command: tell auto-save and repaint
                        self.session.sync_persistence();
                        ctx.request_repaint();
                    }
                }
                Err(e) => log::warn!("review comments: {e}"),
            }
        }
        if self.session.review.rendering() {
            ctx.request_repaint_after(Duration::from_millis(500));
        } else if self.session.path.is_some() {
            ctx.request_repaint_after(Duration::from_secs_f64(FALLBACK));
        }
    }
}
