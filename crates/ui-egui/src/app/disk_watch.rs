//! The open project's file and the world outside: the live-channel announcement follows the
//! project path.

use super::FilmcraftApp;

impl FilmcraftApp {
    /// Tell the host which project file is open (the live-channel announcement).
    pub(super) fn tick_project_path(&mut self) {
        if let Some(hook) = self.hooks.project_path_changed.as_mut() {
            hook(self.session.path.as_deref());
        }
    }
}
