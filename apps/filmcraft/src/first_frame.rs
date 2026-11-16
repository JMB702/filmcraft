//! A start-up guard: the window must get its first frame.
//!
//! eframe creates the window hidden and shows it after painting the first frame. Everything after
//! that frame is woken through egui's repaint requests, and egui ignores a request while an
//! earlier one is still waiting for its frame. If that first frame never comes, nothing does:
//! no window, and a live channel that never answers (the request is queued for a frame that
//! never runs). It happened once on macOS, after a cold `open file.fcproj` on a heavily loaded
//! machine. The app sat idle in AppKit's event loop with its project open and no window, until
//! the app was asked to reopen (which sends window events and so a frame).
//!
//! [`watch`] does from inside what that reopen did from outside: until the first frame has been
//! painted it asks the window for a redraw, which goes to winit directly and so past egui's
//! pending request, and after a few seconds it shows the window, whose window events bring a
//! frame too. Both are no-ops for a start that is merely slow. It logs when it has to step in, so
//! a start that needed it can be told apart in the log.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// When [`watch`] steps in.
#[derive(Clone, Copy, Debug)]
pub struct Schedule {
    /// How often it looks.
    pub every: Duration,
    /// From when on it asks for a redraw, each time it looks.
    pub redraw_after: Duration,
    /// From when on it shows the window (once).
    pub show_after: Duration,
    /// When it stops looking.
    pub give_up: Duration,
}

impl Schedule {
    /// A first frame usually comes well within a second, a big project on a loaded machine within
    /// a few; nudging a slow start does no harm.
    pub const APP: Schedule = Schedule {
        every: Duration::from_millis(500),
        redraw_after: Duration::from_millis(1500),
        show_after: Duration::from_secs(4),
        give_up: Duration::from_secs(60),
    };
}

/// What [`watch`] asks of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nudge {
    Redraw,
    Show,
}

/// Watch for the first frame on a thread of its own. `painted` is set once a frame has been
/// painted; `nudge` asks the window for a redraw or to be shown, and returns `false` once the
/// window is gone (the watch then ends).
pub fn watch(painted: Arc<AtomicBool>, schedule: Schedule, nudge: impl FnMut(Nudge) -> bool + Send + 'static) {
    let spawned = std::thread::Builder::new().name("first-frame-watch".into()).spawn(move || {
        let started = Instant::now();
        let watched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(&painted, schedule, || started.elapsed(), nudge, std::thread::sleep);
        }));
        if watched.is_err() {
            log::warn!("the first-frame watch stopped after a panic");
        }
    });
    if let Err(e) = spawned {
        log::warn!("no first-frame watch: {e}");
    }
}

/// The watch itself: `elapsed` is the time since start, `sleep` waits `every` (tests pass a
/// clock of their own).
fn run(painted: &AtomicBool, s: Schedule, elapsed: impl Fn() -> Duration, mut nudge: impl FnMut(Nudge) -> bool, mut sleep: impl FnMut(Duration)) {
    let mut nudged = false;
    let mut shown = false;
    loop {
        if painted.load(Ordering::Acquire) {
            if nudged {
                log::info!("first frame painted {:.1} s after start, after the start-up nudge", elapsed().as_secs_f32());
            }
            return;
        }
        let waited = elapsed();
        if waited >= s.give_up {
            log::warn!("no frame painted {:.0} s after start; the first-frame watch gives up", waited.as_secs_f32());
            return;
        }
        if waited >= s.redraw_after {
            if !nudged {
                log::warn!("no frame painted {:.1} s after start: asking the window for one", waited.as_secs_f32());
                nudged = true;
            }
            if !nudge(Nudge::Redraw) {
                return;
            }
        }
        if waited >= s.show_after && !shown {
            log::warn!("still no frame {:.1} s after start: showing the window", waited.as_secs_f32());
            shown = true;
            if !nudge(Nudge::Show) {
                return;
            }
        }
        sleep(s.every);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Runs the watch with a clock that moves by `every` per sleep, so nothing waits; `paint_at`
    /// sets `painted` at that point. Returns each nudge with the time it was asked for.
    fn simulate(paint_at: Option<Duration>, window_goes_after: Option<usize>) -> Vec<(Nudge, Duration)> {
        let s = Schedule {
            every: Duration::from_millis(500),
            redraw_after: Duration::from_millis(1500),
            show_after: Duration::from_secs(4),
            give_up: Duration::from_secs(10),
        };
        let painted = AtomicBool::new(false);
        let clock = Cell::new(Duration::ZERO);
        let asked = RefCell::new(Vec::new());
        run(
            &painted,
            s,
            || clock.get(),
            |n| {
                asked.borrow_mut().push((n, clock.get()));
                window_goes_after.is_none_or(|k| asked.borrow().len() < k)
            },
            |d| {
                clock.set(clock.get() + d);
                if paint_at.is_some_and(|t| clock.get() >= t) {
                    painted.store(true, Ordering::Release);
                }
            },
        );
        asked.into_inner()
    }

    #[test]
    fn a_first_frame_in_time_is_left_alone() {
        assert!(simulate(Some(Duration::from_millis(1000)), None).is_empty());
    }

    #[test]
    fn a_missing_first_frame_gets_redraws_then_the_window_is_shown_once() {
        let asked = simulate(None, None);
        let shows: Vec<_> = asked.iter().filter(|(n, _)| *n == Nudge::Show).collect();
        assert_eq!(shows.len(), 1, "{asked:?}");
        assert!(shows[0].1 >= Duration::from_secs(4), "{asked:?}");
        let first = asked.first().copied();
        assert_eq!(first.map(|(n, _)| n), Some(Nudge::Redraw));
        assert!(first.is_some_and(|(_, t)| t >= Duration::from_millis(1500)), "{asked:?}");
        assert!(asked.iter().filter(|(n, _)| *n == Nudge::Redraw).count() > 5, "a redraw each time it looks: {asked:?}");
        assert!(asked.iter().all(|(_, t)| *t < Duration::from_secs(10)), "it gives up: {asked:?}");
    }

    #[test]
    fn the_watch_ends_with_the_first_frame() {
        let asked = simulate(Some(Duration::from_millis(2500)), None);
        assert!(!asked.is_empty() && asked.iter().all(|(n, _)| *n == Nudge::Redraw), "{asked:?}");
        assert!(asked.iter().all(|(_, t)| *t < Duration::from_millis(2500)), "{asked:?}");
    }

    #[test]
    fn the_watch_ends_when_the_window_is_gone() {
        assert_eq!(simulate(None, Some(1)).len(), 1);
    }
}
