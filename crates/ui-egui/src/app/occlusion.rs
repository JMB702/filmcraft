//! Drawing a window that the window system wrongly reports as hidden.
//!
//! eframe draws nothing while the window system reports the window minimized or fully covered
//! (`ViewportInfo::visible() == Some(false)`): it runs only `App::logic` and keeps the input for
//! later. On macOS that report comes only from AppKit's occlusion notifications. If one saying
//! "hidden" is never followed by one saying "shown", the window stays on screen with its last
//! picture, takes no clicks or keys, and never repaints, while everything that runs in `logic`
//! (the live channel, the engine, saving) carries on. That happened once, mid-session, after a
//! reload from disk and a run of marker edits; the cause of the missing notification is unknown.
//!
//! A window that is really hidden cannot be clicked, scrolled or typed into, so input the person
//! gives it is proof the report is stale. [`Guard`] then has the window painted anyway, on every
//! pass, until the window system reports a change again (which it is trusted with). It logs each
//! report and when it steps in, so a recurrence shows in `filmcraft.log`.

/// Whether the person is using the window in this input: a press, a key, text, scrolling or a
/// zoom. A pointer just moving over the window or leaving it is not proof (a pointer crossing a
/// covered window still produces moves on some systems).
pub(crate) fn used(events: &[egui::Event]) -> bool {
    events.iter().any(|event| match event {
        egui::Event::PointerButton { pressed, .. } | egui::Event::Key { pressed, .. } => *pressed,
        egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::MouseWheel { .. } | egui::Event::Zoom(_) | egui::Event::Rotate(_) => true,
        _ => false,
    })
}

/// Tracks what the window system reports about the window's visibility and whether to distrust it.
#[derive(Debug, Default)]
pub(crate) struct Guard {
    /// The last report: `Some(false)` hidden, `Some(true)` shown, `None` not reported yet.
    reported: Option<bool>,
    /// The report says hidden, yet the window was used since: paint it anyway.
    distrust: bool,
    /// Distrust just began: the input that proved it needs a pass of its own.
    started: bool,
}

impl Guard {
    /// Note this pass's report (`ViewportInfo::visible`) and whether its input shows the window
    /// in use; returns whether the window should be painted although it is reported hidden.
    pub(crate) fn observe(&mut self, visible: Option<bool>, used: bool) -> bool {
        if visible != self.reported {
            if self.reported.is_some() || visible == Some(false) {
                log::info!("the window system reports the window {}", if visible == Some(false) { "hidden" } else { "shown" });
            }
            if self.distrust {
                log::info!("the window system reported a change again: trusting it");
            }
            self.reported = visible;
            self.distrust = false;
        }
        if visible != Some(false) {
            return false;
        }
        if used && !self.distrust {
            log::warn!("the window is being used while the window system reports it hidden: drawing it anyway");
            self.distrust = true;
            self.started = true;
        }
        self.distrust
    }

    /// In `App::logic`: ask for this window to be painted on the next pass despite the report,
    /// and for that pass to come at once when distrust has just begun (the input is waiting).
    pub(crate) fn apply(&mut self, ctx: &egui::Context) {
        if self.distrust {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
        }
        if std::mem::take(&mut self.started) {
            ctx.request_repaint();
        }
    }

    /// The last report from the window system (`None` before the first one).
    pub(crate) fn reported(&self) -> Option<bool> {
        self.reported
    }

    /// Whether the window is painted against a stale "hidden" report.
    pub(crate) fn distrusting(&self) -> bool {
        self.distrust
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click() -> egui::Event {
        egui::Event::PointerButton { pos: egui::pos2(10.0, 10.0), button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }
    }

    #[test]
    fn a_shown_window_is_left_to_eframe() {
        let mut guard = Guard::default();
        assert!(!guard.observe(None, true));
        assert!(!guard.observe(Some(true), true));
    }

    #[test]
    fn a_hidden_window_nobody_uses_stays_unpainted() {
        let mut guard = Guard::default();
        assert!(!guard.observe(Some(false), false));
        assert!(!guard.observe(Some(false), false));
        assert!(!guard.distrusting());
    }

    #[test]
    fn using_a_window_reported_hidden_paints_it_until_the_next_report() {
        let mut guard = Guard::default();
        guard.observe(Some(true), false);
        assert!(!guard.observe(Some(false), false), "a real hide is trusted");
        assert!(guard.observe(Some(false), used(&[click()])), "a click proves the report stale");
        assert!(guard.observe(Some(false), false), "and it keeps painting without more input");
        assert!(!guard.observe(Some(true), false), "a new report from the window system is trusted");
        assert!(!guard.observe(Some(false), false), "including a later hide");
        assert_eq!(guard.reported(), Some(false));
    }

    #[test]
    fn only_real_use_counts() {
        let moved = egui::Event::PointerMoved(egui::pos2(1.0, 1.0));
        let release =
            egui::Event::PointerButton { pos: egui::pos2(1.0, 1.0), button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE };
        assert!(!used(&[moved.clone(), egui::Event::PointerGone, release]));
        let key = egui::Event::Key { key: egui::Key::M, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        assert!(used(&[moved, key]));
        assert!(used(&[egui::Event::Text("m".into())]));
        assert!(used(&[click()]));
    }
}
