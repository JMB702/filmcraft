//! Adobe Premiere Pro projects (`.prproj`): import.
//!
//! A `.prproj` is a gzip-compressed XML document: a flat list of objects that reference each other
//! by number or UUID (see [`scan`] for the container, `graph` for the objects read and `map` for how
//! they chain into bins, sequences, tracks and clips). The format is undocumented; this reader was
//! written from projects saved by Premiere Pro 2026 (project version 45), never from Adobe code.
//!
//! What is imported: the bin tree, labels, media items (with the file paths Premiere stored, and
//! the project-relative path as a relink candidate), sequences (frame size and rate, audio sample
//! rate, start timecode, In / Out, work area, markers), video and audio tracks, clips (timeline
//! position and duration, source In, speed and reverse, frame holds, enabled state, clip gain, the
//! channels an audio clip plays, links between video and audio), audio and video transitions,
//! nested sequences, and multi-camera source sequences with each multi-camera clip's angle.
//! Everything else (effects and their parameters, Essential Sound settings, track mute / solo /
//! lock, captions and transcripts) is listed in the [`Report`].
//!
//! Real projects are large (hundreds of megabytes of XML): the document is walked once, and only
//! the objects the importer uses are parsed, one at a time.

mod graph;
mod map;
pub(crate) mod scan;

pub use map::Relink;

use crate::{Error, Format, ImportOptions, Imported, Report};

/// Whether `bytes` look like a Premiere project (gzip, or the XML itself).
pub fn sniff(bytes: &[u8]) -> bool {
    scan::sniff(bytes)
}

/// Import a Premiere project. Besides the fragment and the report, returns the media items whose
/// project-relative path differs from the absolute path Premiere stored: the caller (which can see
/// the file system) may use it when the absolute path is missing, as when a project folder was moved.
pub fn import(bytes: &[u8], opts: &ImportOptions) -> crate::Result<(Imported, Vec<Relink>, Report)> {
    let text = scan::document(bytes).map_err(|e| Error::parse(Format::Prproj, e))?;
    if !text.trim_start().starts_with('<') || !text.get(..text.len().min(4096)).is_some_and(|h| h.contains("<PremiereData")) {
        return Err(Error::parse(Format::Prproj, "not a Premiere project (no PremiereData element)"));
    }
    let g = graph::read(&text);
    drop(text);
    let mut report = Report::default();
    if let Some((at, why)) = g.truncated {
        report.warn(format!("the project file is damaged or truncated ({why} at byte {at}); everything before that point was imported"));
    }
    if g.broken > 0 {
        report.warn(format!("{} object(s) could not be read and were skipped", g.broken));
    }
    if g.duplicates > 0 {
        report.warn(format!("{} object(s) reuse an object id; the first of each was kept", g.duplicates));
    }
    if g.roots.is_empty() && g.sequences.is_empty() {
        return Err(Error::Empty);
    }
    let name = opts.name.clone().unwrap_or_else(|| "Premiere Project".into());
    let (mut imported, relinks) = map::build(&g, &name, opts, &mut report);
    skipped_report(&g, &mut report);
    crate::bound_start_timecodes(&mut imported, &mut report);
    Ok((imported, relinks, report))
}

/// One line per kind of Premiere data that the importer does not carry over.
fn skipped_report(g: &graph::Graph, report: &mut Report) {
    let count = |classes: &[&str]| -> usize { classes.iter().filter_map(|c| g.skipped.get(*c)).sum() };
    let video_fx = count(&["VideoFilterComponent"]);
    if video_fx > 0 {
        report
            .warn(format!("{video_fx} video effect instance(s) (Motion, Opacity, Transform, Text, …) were not imported; clips use default Motion and Opacity"));
    }
    let audio_fx = count(&["AudioFilterComponent"]);
    if audio_fx > 0 {
        report.warn(format!("{audio_fx} audio effect instance(s) (Volume, Channel Volume, Essential Sound processing, …) were not imported"));
    }
    if count(&["TranscriptClip", "ExternallyProvidedTranscriptDocument"]) > 0 {
        report.info("transcripts were not imported");
    }
    if count(&["CaptionDataTrack", "CaptionTrackItem", "DataClipTrackItem"]) > 0 {
        report.warn("caption and data tracks were not imported");
    }
    if count(&["GenerativeExtendSession"]) > 0 {
        report.info("Generative Extend sessions were not imported (their rendered results are)");
    }
    if count(&["AudioMixTrack", "AudioTrackInlet", "AudioFader"]) > 0 {
        report.info("track mixer settings (volume, pan, mute, solo) were not imported");
    }
}
