//! Interchange documents (CMX 3600 EDL, FCP7 XML, FCPXML, OTIO, AAF, OMF, and Premiere Pro projects
//! for import) ↔ the session's project.

use serde_json::{Value, json};

use filmcraft_interchange::{ExportOptions, Format, ImportOptions};
use filmcraft_project::{BinId, ItemId, ItemKind, MediaRef};

use crate::{EngineError, Result, Session};

/// File extensions of the interchange documents `file.import` reads (lower case). The Media
/// Browser lists the same set.
pub const EXTENSIONS: [&str; 8] = ["edl", "xml", "fcpxml", "otio", "aaf", "omf", "omfi", "prproj"];

/// Whether `path` / `bytes` are a Premiere Pro project (File ▸ Open converts them).
pub fn is_premiere(path: &str, bytes: &[u8]) -> bool {
    std::path::Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("prproj"))
        && filmcraft_interchange::prproj::sniff(bytes)
}

/// The interchange format of a file, if it is one (by content, with the extension as a hint).
pub fn detect(path: &str, bytes: &[u8]) -> Option<Format> {
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if !ext.as_deref().is_some_and(|e| EXTENSIONS.contains(&e)) {
        return None;
    }
    filmcraft_interchange::detect(bytes, ext.as_deref())
}

fn report_json(r: &filmcraft_interchange::Report) -> Vec<String> {
    r.entries.iter().map(|e| if e.count > 1 { format!("{} (×{})", e.message, e.count) } else { e.message.clone() }).collect()
}

/// How long an import may spend opening its media files, together. The files are probed on the
/// thread that runs the command (the UI thread for File > Open), so a file on a stalled network
/// share must not hold it: past this, the remaining files are left to open when they are played
/// (frames are rendered on worker threads), as a saved project's files are.
pub const LINK_MEDIA_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Open `file` on a helper thread, waiting at most `wait`. None: it took longer (the helper
/// finishes or stays blocked on its own; its result is dropped). Hosts that cannot start threads
/// (the browser) open it here.
fn open_within(s: &Session, file: &str, wait: std::time::Duration) -> Option<std::result::Result<filmcraft_media::SharedSource, filmcraft_media::MediaError>> {
    let (pool, services, owned) = (s.media.clone(), s.services.clone(), file.to_string());
    let (tx, rx) = std::sync::mpsc::channel();
    let job = move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pool.open_file(&owned, &*services)))
            .unwrap_or_else(|_| Err(filmcraft_media::MediaError::Decode(format!("{owned}: the opener failed"))));
        let _ = tx.send(r);
    };
    if std::thread::Builder::new().name("filmcraft-link-media".into()).spawn(job).is_err() {
        return Some(s.media.open_file(file, &*s.services));
    }
    rx.recv_timeout(wait).ok()
}

fn is_file_media(item: &filmcraft_project::ProjectItem) -> bool {
    matches!(&item.kind, ItemKind::Media(m) if matches!(m.media, MediaRef::File { .. }))
}

/// Write embedded audio without replacing an occupied path; return the path used. A numbered
/// name changes only the file name: the folder keeps its separators as written (`/` or `\`).
fn write_embedded_audio(s: &Session, path: &str, wav: &[u8]) -> Result<String> {
    let (folder, name) = path.split_at(path.rfind(['/', '\\']).map_or(0, |i| i + 1));
    if matches!(name, "" | "." | "..") {
        return Err(EngineError::Other(format!("{path}: missing audio file name")));
    }
    let (stem, extension) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    for number in 1..=1_000 {
        let candidate = if number == 1 { path.to_owned() } else { format!("{folder}{stem} ({number}){extension}") };
        match s.services.write_file_new(&candidate, wav) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(EngineError::Other(format!("{candidate}: {e}"))),
        }
    }
    Err(EngineError::Other(format!("{path}: no unused embedded-audio filename after 1000 attempts")))
}

/// Import a document: merge its bins, media and sequences into the project (one undo step), then
/// link each media file that exists on disk. Returns the new sequences. The document's items go
/// into `bin` (the Project panel's current bin, as for media files); the root when it is `None`
/// or no longer exists.
pub fn import(s: &mut Session, path: &str, bytes: &[u8], format: Format, bin: Option<BinId>) -> Result<Value> {
    let parsed = parse(s, path, bytes, format)?;
    merge(s, path, format, parsed, bin)
}

/// A document read into a project fragment, not yet merged.
pub struct Parsed {
    fragment: filmcraft_interchange::Imported,
    report: filmcraft_interchange::Report,
    name: String,
}

fn options(path: &str) -> ImportOptions {
    let p = std::path::Path::new(path);
    ImportOptions {
        base_dir: p.parent().map(|d| d.to_string_lossy().to_string()),
        name: p.file_stem().map(|n| n.to_string_lossy().to_string()),
        ..Default::default()
    }
}

/// Read a document. Nothing in the session changes, except embedded audio written next to an AAF
/// or OMF document.
pub fn parse(s: &mut Session, path: &str, bytes: &[u8], format: Format) -> Result<Parsed> {
    let opts = options(path);
    let (fragment, report) = match format {
        // AAF / OMF may embed audio: write it next to the document (the media items point there)
        Format::Aaf | Format::Omf => {
            let r = if format == Format::Aaf { filmcraft_interchange::aaf::import(bytes, &opts) } else { filmcraft_interchange::omf::import(bytes, &opts) };
            let (mut fragment, extracted, report) = r.map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
            for m in extracted {
                let media = fragment
                    .project
                    .item_mut(m.item)
                    .and_then(|item| item.as_media_mut())
                    .ok_or_else(|| EngineError::Other(format!("{}: embedded audio has no media item", m.path)))?;
                let written_path = write_embedded_audio(s, &m.path, &m.wav)?;
                if written_path != m.path {
                    media.media = MediaRef::File { path: written_path };
                }
            }
            (fragment, report)
        }
        // a Premiere project stores each file's absolute path and its path relative to the
        // project: use the relative one when only it exists (the project folder was moved)
        Format::Prproj => {
            let (mut fragment, relinks, report) =
                filmcraft_interchange::prproj::import(bytes, &opts).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
            for r in relinks {
                if let Some(media) = fragment.project.item_mut(r.item).and_then(|item| item.as_media_mut())
                    && let MediaRef::File { path: stored } = &media.media
                    && s.services.file_size(stored).is_err()
                    && s.services.file_size(&r.relative).is_ok()
                {
                    media.media = MediaRef::File { path: r.relative };
                }
            }
            (fragment, report)
        }
        _ => filmcraft_interchange::import_with(bytes, format, &opts).map_err(|e| EngineError::Other(format!("{path}: {e}")))?,
    };
    for item in fragment.project.items.values() {
        if let ItemKind::Sequence(sequence) = &item.kind {
            sequence.settings.validate().map_err(|e| EngineError::Other(format!("{path}: invalid imported sequence settings: {e}")))?;
        }
    }
    let name = opts.name.unwrap_or_else(|| format.name().to_string());
    Ok(Parsed { fragment, report, name })
}

/// Merge a read document into the project and link its media (see [`import`]).
pub fn merge(s: &mut Session, path: &str, format: Format, parsed: Parsed, bin: Option<BinId>) -> Result<Value> {
    merge_within(s, path, format, parsed, bin, LINK_MEDIA_BUDGET)
}

/// [`merge`], spending at most `budget` opening the media files.
pub(crate) fn merge_within(s: &mut Session, path: &str, format: Format, parsed: Parsed, bin: Option<BinId>, budget: std::time::Duration) -> Result<Value> {
    let Parsed { fragment, report, name } = parsed;
    let before: std::collections::HashSet<ItemId> = s.project.items.keys().copied().collect();
    let seqs = s.edit(&format!("Import {name}"), |proj, _| {
        let seqs = filmcraft_interchange::merge_into(proj, fragment, bin);
        // A document that sets only some Motion parameters leaves the others at their "auto"
        // (NaN) defaults. Resolve them as placing a clip does, for every format. A media file's
        // picture size is only what the document says so far (an EDL says nothing: the importer
        // assumes the sequence size), so its clips' anchors wait for Link Media below.
        proj.resolve_placed_auto_points(|seq| !before.contains(&seq), |source| !is_file_media(source));
        Ok(seqs)
    })?;
    // Link media: probe each new file-backed item that exists, through the host's reader as a
    // direct import does (only the container index is read now, not the whole file: #461).
    let new_media: Vec<(ItemId, String)> = s
        .project
        .items
        .values()
        .filter(|i| !before.contains(&i.id))
        .filter_map(|i| match &i.kind {
            ItemKind::Media(m) => match &m.media {
                MediaRef::File { path } => Some((i.id, path.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect();
    // The document's media paths are opened as stored paths of the document.
    s.media.add_stored_paths(new_media.iter().map(|(_, p)| p.clone()), Some(path));
    let mut linked = 0;
    let mut offline = Vec::new();
    let mut deferred = Vec::new();
    let mut report = report_json(&report);
    let deadline = web_time::Instant::now() + budget;
    for (id, mpath) in new_media {
        let file = match s.media.follow(&mpath) {
            Ok(file) => file,
            Err(why) => {
                log::warn!("{mpath} is not opened: {why}");
                offline.push(mpath);
                continue;
            }
        };
        let wait = deadline.saturating_duration_since(web_time::Instant::now());
        let opened = if wait.is_zero() { None } else { open_within(s, &file, wait) };
        let Some(opened) = opened else {
            deferred.push(mpath);
            continue;
        };
        match opened {
            Ok(src) => {
                let info = src.info().clone();
                let identity = crate::relink::identity_of(&*s.services, &file).ok();
                let rebase = (format == Format::Edl).then_some(info.start_timecode).flatten();
                s.edit("Link Media", |proj, _| {
                    if let Some(item) = proj.items.get_mut(&id)
                        && let ItemKind::Media(m) = &mut item.kind
                    {
                        let rate = info.video.as_ref().map(|v| v.frame_rate);
                        m.info = info.clone();
                        m.offline = false;
                        m.identity = identity;
                        if let (Some(tc), Some(rate)) = (rebase, rate) {
                            filmcraft_interchange::rebase_source_timecode(proj, id, tc, rate);
                        }
                    }
                    // the picture size is now the file's own: centre the anchors of its clips in it
                    proj.resolve_placed_auto_points(|seq| !before.contains(&seq), |source| source.id == id);
                    Ok(())
                })?;
                s.media.insert_file(id, &mpath, src);
                linked += 1;
            }
            Err(e) => {
                // a cloud placeholder was not read (that would wait for its download): say so
                let reason = crate::media_pool::offline_reason(&e);
                if reason == filmcraft_render::offline::OfflineReason::NotDownloaded {
                    report.push(e.to_string());
                }
                s.media.set_offline(id, reason, &mpath, &e.to_string());
                offline.push(mpath);
            }
        }
    }
    if !deferred.is_empty() {
        log::warn!("media not probed within {budget:?}, opened when played: {}", deferred.join(", "));
        report.push(format!("{} media file(s) took too long to open; they open when played", deferred.len()));
    }
    if let Some(&first) = seqs.first() {
        s.state.active_sequence = Some(first);
        if !s.state.open_sequences.contains(&first) {
            s.state.open_sequences.push(first);
        }
    }
    Ok(json!({
        "format": format.name(),
        "sequences": seqs.iter().map(|i| i.0).collect::<Vec<_>>(),
        "linkedMedia": linked,
        "offlineMedia": offline,
        "deferredMedia": deferred,
        "report": report,
    }))
}

/// `file.exportInterchange {format: "edl"|"xml"|"fcpxml"|"otio"|"aaf"|"omf" = "xml", path, sequence?}`
pub fn export(s: &mut Session, p: &Value) -> Result<Value> {
    // FCP7 XML only when no format is asked for: a format we don't know is an error, not XML.
    // The names are the documented ones (each format's extension, in either case) and nothing
    // else, so the error can list exactly what is accepted.
    let format = match p.get("format") {
        None | Some(Value::Null) => Format::Fcp7Xml,
        Some(v) => v.as_str().and_then(|name| Format::ALL.into_iter().find(|f| f.extension().eq_ignore_ascii_case(name))).ok_or_else(|| {
            let names: Vec<&str> = Format::ALL.iter().map(|f| f.extension()).collect();
            crate::commands::bad("file.exportInterchange", format!("unknown format {v}: use one of {}", names.join(", ")))
        })?,
    };
    let path = p.get("path").and_then(Value::as_str).ok_or_else(|| EngineError::Other("need `path`".into()))?.to_string();
    let seq = p.get("sequence").and_then(Value::as_u64).map(ItemId).or(s.state.active_sequence).ok_or(EngineError::NoSequence)?;
    let opts = ExportOptions {
        relative_to: std::path::Path::new(&path).parent().map(|d| d.to_string_lossy().to_string()),
        name: s.project.item(seq).map(|i| i.name.clone()),
        ..Default::default()
    };
    let (bytes, report) = filmcraft_interchange::export(&s.project, seq, format, &opts).map_err(|e| EngineError::Other(e.to_string()))?;
    s.services.write_file(&path, &bytes).map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(json!({"path": path, "bytes": bytes.len(), "report": report_json(&report)}))
}

#[cfg(test)]
mod embedded_audio_tests {
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex};

    use crate::{Services, Session};

    use super::write_embedded_audio;

    #[derive(Default)]
    struct VirtualFiles(Mutex<HashMap<String, Vec<u8>>>);

    impl Services for VirtualFiles {
        fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
            self.0.lock().unwrap().get(path).cloned().ok_or_else(|| io::ErrorKind::NotFound.into())
        }

        fn write_file(&self, path: &str, data: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().insert(path.to_owned(), data.to_vec());
            Ok(())
        }

        fn write_file_new(&self, path: &str, data: &[u8]) -> io::Result<()> {
            let mut files = self.0.lock().unwrap();
            if files.contains_key(path) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            files.insert(path.to_owned(), data.to_vec());
            Ok(())
        }
    }

    #[test]
    fn virtual_extraction_keeps_existing_media() {
        let files = Arc::new(VirtualFiles::default());
        files.0.lock().unwrap().insert("/files/cut Audio.wav".into(), b"first".to_vec());
        files.0.lock().unwrap().insert("/files/cut Audio (2).wav".into(), b"second".to_vec());
        let session = Session::new(files.clone());
        let path = write_embedded_audio(&session, "/files/cut Audio.wav", b"new audio").unwrap();
        assert_eq!(path, "/files/cut Audio (3).wav");
        let stored = files.0.lock().unwrap();
        assert_eq!(stored["/files/cut Audio.wav"], b"first");
        assert_eq!(stored["/files/cut Audio (2).wav"], b"second");
        assert_eq!(stored["/files/cut Audio (3).wav"], b"new audio");
    }

    #[test]
    fn numbered_names_keep_the_path_separators() {
        for path in ["C:/files/cut Audio.wav", "C:\\files\\cut Audio.wav"] {
            let files = Arc::new(VirtualFiles::default());
            files.0.lock().unwrap().insert(path.into(), b"first".to_vec());
            let session = Session::new(files.clone());
            let numbered = write_embedded_audio(&session, path, b"new audio").unwrap();
            assert_eq!(numbered, path.replace("cut Audio.wav", "cut Audio (2).wav"));
            assert_eq!(files.0.lock().unwrap()[path], b"first");
        }
    }
}
