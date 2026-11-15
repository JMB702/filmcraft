//! Where a run goes: into the running app that has the project open (live), or into a headless
//! session of its own that reads and writes the file.
//!
//! `filmcraft-cli --project P …` asks the apps running for this user ([`filmcraft_automation::live`])
//! whether one has P open. If one does, the commands run there: the user sees them at once and can
//! undo them, and P is never rewritten behind the app's back. Otherwise the run is headless, as
//! before. `--live` insists on an app (and without `--project` takes the only one running);
//! `--headless` never asks.

use std::path::Path;
use std::time::Duration;

use filmcraft_automation::BridgeClient;
use filmcraft_automation::live::{self, Announcement};
use serde_json::{Value, json};

use crate::args::Args;

/// How long an app may take to say which project it has open before it counts as not answering.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(5);

/// The decision.
pub enum Route {
    /// Run in this app, which has `project` open (`None`: `--live` without `--project`).
    Live { app: Announcement, client: BridgeClient, project: Option<String>, dirty: bool },
    /// Run in a session of our own.
    Headless {
        /// `--headless` was given while this app has the project open (pid): the run goes on,
        /// and the app notices the file changed.
        open_in: Option<u32>,
    },
}

/// The file an app has open and whether it has unsaved changes, asked over its live channel.
async fn ask(client: &BridgeClient, timeout: Duration) -> Result<(Option<String>, bool), String> {
    let r: Value = tokio::time::timeout(timeout, client.execute("file.diskStatus", json!({})))
        .await
        .map_err(|_| "not answering".to_string())?
        .map_err(|e| e.to_string())?;
    Ok((r["path"].as_str().map(str::to_string), r["dirty"].as_bool().unwrap_or(false)))
}

/// Decide where the run `a` goes, looking for apps in `dir` (`None`: no live channel here).
pub async fn route(a: &Args, dir: Option<&Path>, timeout: Duration) -> Result<Route, String> {
    let (want_live, headless) = (a.flag("--live"), a.flag("--headless"));
    if want_live && headless {
        return Err("--live and --headless contradict each other".into());
    }
    if want_live && a.flag("--demo") {
        return Err("--live runs in the app's open project; --demo is headless".into());
    }
    let project = a.opt("--project");
    // nothing to look for: the demo, or a run with no project and no --live
    if project.is_none() && !want_live {
        return Ok(Route::Headless { open_in: None });
    }
    let apps = match dir {
        #[cfg(unix)]
        Some(dir) => live::running(dir).0,
        _ => Vec::new(),
    };
    let Some(project) = project else {
        // --live without --project: the one app running
        return match apps.as_slice() {
            [] => Err("--live: no FilmCraft is running (start the app, or run without --live)".into()),
            [app] => {
                let client = BridgeClient::live(app);
                let (path, dirty) = ask(&client, timeout).await.map_err(|e| format!("--live: FilmCraft (pid {}) is {e}", app.pid))?;
                Ok(Route::Live { app: app.clone(), client, project: path, dirty })
            }
            many => Err(format!(
                "--live: {} FilmCraft windows are running; name the project with --project ({})",
                many.len(),
                many.iter().map(|a| a.project.clone().unwrap_or_else(|| format!("pid {}: unsaved project", a.pid))).collect::<Vec<_>>().join(", ")
            )),
        };
    };
    // the apps that say they have the project open; each is asked to be sure
    let mut silent = None;
    for app in apps.iter().filter(|app| app.project.as_deref().is_some_and(|p| live::same_file(p, project))) {
        if headless {
            return Ok(Route::Headless { open_in: Some(app.pid) });
        }
        let client = BridgeClient::live(app);
        match ask(&client, timeout).await {
            Ok((Some(path), dirty)) if live::same_file(&path, project) => {
                return Ok(Route::Live { app: app.clone(), client, project: Some(path), dirty });
            }
            // it opened another project since it wrote its announcement
            Ok(_) => {}
            Err(e) => silent = Some((app.pid, e)),
        }
    }
    if let Some((pid, why)) = silent {
        return Err(format!(
            "FilmCraft (pid {pid}) has {project} open but is {why}; not editing the file behind its back. Try again, or pass --headless to edit the file anyway (the app then offers to reload it)"
        ));
    }
    if want_live {
        return Err(format!("--live: no running FilmCraft has {project} open (open it there, or run without --live)"));
    }
    Ok(Route::Headless { open_in: None })
}

#[cfg(all(test, unix))]
mod tests;
