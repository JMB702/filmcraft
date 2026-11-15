//! "The project changed on disk": another program wrote the open project's file while this window
//! has unsaved changes, or Save found a newer file than the one it would replace. Nothing is
//! dropped without the user choosing it: Reload from Disk (discard mine), Keep Mine (the next save
//! replaces the file's version), or Save Mine as a Copy (beside the project, then reload).
//!
//! Automation ids: `diskConflict.reload`, `diskConflict.keep`, `diskConflict.copy`.

use crate::FilmcraftApp;
use egui::{Frame, Margin, RichText};
use serde_json::json;

fn from_save_key() -> egui::Id {
    egui::Id::unique("diskConflict.fromSave")
}

/// The question comes from a refused Save: Keep Mine then saves.
pub(crate) fn opened_by_save(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(from_save_key(), true));
}

/// What the user chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Reload,
    KeepMine,
    SaveCopy,
}

/// Carry out `choice` (the buttons, and tests).
pub fn answer(app: &mut FilmcraftApp, ctx: &egui::Context, choice: Choice) {
    let from_save = ctx.data_mut(|d| d.remove_temp::<bool>(from_save_key())).unwrap_or(false);
    let name = app.session.project.name.clone();
    let r = match choice {
        Choice::Reload => app.session.execute("file.revert", json!({})).map(|_| tlf!("Reloaded “{name}” from disk", name)),
        Choice::KeepMine => app.session.execute("file.keepMine", json!({})).and_then(|_| {
            if from_save {
                app.session.execute("file.save", json!({})).map(|_| tlf!("Saved your version of “{name}” over the newer file", name))
            } else {
                Ok(tl!("Kept your version; saving will replace the file's newer version").to_string())
            }
        }),
        Choice::SaveCopy => app.session.execute("file.saveConflictCopy", json!({})).map(|v| {
            let copy = v["copy"].as_str().unwrap_or_default().to_string();
            tlf!("Saved your version as “{copy}” and reloaded the project from disk", copy)
        }),
    };
    match r {
        Ok(msg) => {
            app.session.disk.conflict = None;
            app.status(msg);
        }
        // the question stays, with the reason
        Err(e) => app.ui.status = e.to_string(),
    }
}

/// The dialog, while a question is open.
pub fn show(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if app.session.disk.conflict.is_none() {
        return;
    }
    let t = app.tokens;
    let name = app.session.project.name.clone();
    let path = app.session.path.clone().unwrap_or_default();
    let from_save: bool = ctx.data(|d| d.get_temp(from_save_key())).unwrap_or(false);
    let mut choice = None;
    egui::Modal::new(egui::Id::unique("disk-conflict-modal")).frame(super::file_dialogs::modal_frame(&t)).show(ctx, |ui| {
        ui.set_width(520.0);
        Frame::new().inner_margin(Margin { left: 28, right: 28, top: 24, bottom: 20 }).show(ui, |ui| {
            ui.label(RichText::new(tl!("Project Changed on Disk")).size(18.0).strong().color(t.text));
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new(tlf!("Another program changed “{name}” since you opened or saved it here, and you have unsaved changes.", name)).size(13.5).color(t.text));
            ui.add_space(4.0);
            ui.label(RichText::new(path).size(12.0).color(t.text_dim));
            ui.add_space(8.0);
            ui.label(RichText::new(tl!("Reload from Disk discards your unsaved changes. Keep Mine replaces the newer file when you save. Save Mine as a Copy keeps both: yours in a new file next to the project, then the project reloads.")).size(12.5).color(t.text_dim));
            ui.add_space(18.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if super::file_dialogs::button(app, ui, "diskConflict.copy", tl!("Save Mine as a Copy"), true) {
                    choice = Some(Choice::SaveCopy);
                }
                let keep = if from_save { tl!("Save Mine (Replace File)") } else { tl!("Keep Mine") };
                if super::file_dialogs::button(app, ui, "diskConflict.keep", keep, false) {
                    choice = Some(Choice::KeepMine);
                }
                if super::file_dialogs::button(app, ui, "diskConflict.reload", tl!("Reload from Disk"), false) {
                    choice = Some(Choice::Reload);
                }
            });
        });
    });
    // no Escape: each way out is a decision about someone's changes
    if let Some(c) = choice {
        answer(app, ctx, c);
    }
}
