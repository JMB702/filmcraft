//! Clip markers: the markers a clip in a sequence shows, and adding, finding and clearing them.
//!
//! As in Premiere, a clip marker belongs to the clip's source, not to the one clip instance:
//!
//! - a clip of a media file (or of a subclip) shows the markers of its master clip, the media
//!   item, that fall inside the part of the media the clip plays. Adding a marker to a clip in the
//!   Timeline adds it there, so the Source monitor shows it and every other clip of that media that
//!   covers the time shows it too, and linked sound shows the marker its picture got;
//! - a nest clip (a nested or multi-camera source sequence) shows the sequence markers of the
//!   sequence it plays, at the matching times, trimmed to the part it plays. They are worked out from
//!   the nested sequence every time (never copied), so they follow every change made inside it, and
//!   every nest clip of that sequence shows them. Adding a marker to a nest clip adds a sequence
//!   marker inside the nested sequence;
//! - markers stored on the clip itself (`TrackItem::markers`, read from interchange files whose
//!   clip markers belong to one clip) show too. Clips whose source can't hold markers (graphics,
//!   adjustment layers, generated items) keep their markers there.
//!
//! All of these are in source time, so they move with the clip when it is moved, trimmed, rippled,
//! razored or pasted, and the project file keeps them where it already kept sequence and source
//! markers.

use std::collections::{BTreeMap, BTreeSet};

use filmcraft_project::{ClipId, ItemId, ItemKind, Label, Marker, MarkerId, MarkerKind, Project, Sequence, TrackItem};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{bad, marker_author_p, marker_color_p, sequence_time_p, str_p};
use crate::{EngineError, Result, Session};

/// The most clips one `markers.add` may name.
const MAX_CLIPS: usize = 10_000;

/// Where a clip marker is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MarkerHome {
    /// The master clip: the markers of this media item (a subclip's root media).
    Source(ItemId),
    /// The sequence markers of this nested sequence.
    Nest(ItemId),
    /// The clip's own markers.
    Clip(ClipId),
}

impl MarkerHome {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Source(_) => "source",
            Self::Nest(_) => "nest",
            Self::Clip(_) => "clip",
        }
    }
    /// The `markers.edit` / `markers.delete` parameters that address a marker kept here.
    pub fn params(self) -> Value {
        match self {
            Self::Source(id) => json!({"target": "source", "item": id.0}),
            Self::Nest(id) => json!({"target": "program", "sequence": id.0}),
            Self::Clip(id) => json!({"target": "clip", "clip": id.0}),
        }
    }
}

/// A marker as one clip shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipMarker {
    pub clip: ClipId,
    pub home: MarkerHome,
    /// The stored marker (its `start` and `duration` are in the home's time).
    pub marker: Marker,
    /// Sequence time where it shows on the clip.
    pub start: Tick,
    /// Sequence duration it covers on the clip (0 for a point marker), trimmed to the clip.
    pub duration: Tick,
}

impl ClipMarker {
    pub fn to_json(&self, rate: filmcraft_time::FrameRate) -> Value {
        let m = &self.marker;
        let owner = match self.home {
            MarkerHome::Source(i) | MarkerHome::Nest(i) => i.0,
            MarkerHome::Clip(c) => c.0,
        };
        json!({
            "id": m.id.0, "home": self.home.kind(), "owner": owner,
            "start": self.start.0, "startFrame": rate.frame_at(self.start), "duration": self.duration.0,
            "sourceStart": m.start.0, "sourceDuration": m.duration.0,
            "kind": format!("{:?}", m.kind), "name": m.name, "comment": m.comment, "color": m.color_value(), "author": m.author, "url": m.url,
        })
    }
}

/// Where markers added to a clip of `item` are kept (`None`: on the clip itself).
pub fn home_of(project: &Project, item: ItemId) -> Option<MarkerHome> {
    match &project.item(item)?.kind {
        ItemKind::Media(_) | ItemKind::Subclip { .. } => project.resolve_media(item).map(|(root, _, _)| MarkerHome::Source(root)),
        ItemKind::Sequence(_) => Some(MarkerHome::Nest(item)),
        _ => None,
    }
}

fn home_markers<'a>(project: &'a Project, seq: &'a Sequence, home: MarkerHome) -> &'a [Marker] {
    match home {
        MarkerHome::Source(id) => project.item(id).and_then(|i| i.as_media()).map_or(&[], |m| &m.markers),
        MarkerHome::Nest(id) => project.sequence(id).map_or(&[], |q| &q.markers),
        MarkerHome::Clip(c) => seq.find_item(c).map_or(&[], |(_, it)| &it.markers),
    }
}

/// `rel` source ticks at `speed` as sequence ticks (exact at 100%).
fn on_timeline(rel: Tick, speed: f64) -> Tick {
    if speed == 1.0 { rel } else { Tick((rel.0 as f64 / speed).floor() as i64) }
}

/// Where the source span `[a, a + d)` shows on `it`, trimmed to the part the clip plays: the
/// sequence start and duration, or `None` when it falls outside the clip.
fn place(it: &TrackItem, a: Tick, d: Tick) -> Option<(Tick, Tick)> {
    if let Some(h) = it.frame_hold {
        let covers = if d > Tick::ZERO { a <= h && h.0 < a.0.saturating_add(d.0) } else { a == h };
        return covers.then(|| (it.start, if d > Tick::ZERO { it.duration } else { Tick::ZERO }));
    }
    let speed = it.speed.abs();
    if !(speed > 0.0 && speed.is_finite()) || it.duration <= Tick::ZERO {
        return None;
    }
    let (lo, hi) = (it.source_in, it.source_out());
    if d <= Tick::ZERO {
        if a < lo || a >= hi {
            return None;
        }
        let rel = if it.reverse { hi - Tick(1) - a } else { a - lo };
        let t = it.start + on_timeline(rel, speed);
        return (t < it.end()).then_some((t, Tick::ZERO));
    }
    let (s0, s1) = (a.max(lo), Tick(a.0.saturating_add(d.0)).min(hi));
    if s1 <= s0 {
        return None;
    }
    let (r0, r1) = if it.reverse { (hi - s1, hi - s0) } else { (s0 - lo, s1 - lo) };
    let t0 = it.start + on_timeline(r0, speed);
    let t1 = (it.start + on_timeline(r1, speed)).min(it.end());
    (t0 < it.end()).then(|| (t0, (t1 - t0).max(Tick::ZERO)))
}

/// The markers clip `it` of sequence `seq` shows, in sequence time, sorted by time: its source's
/// (master clip or nested sequence) inside the part it plays, then its own.
pub fn clip_markers(project: &Project, seq: &Sequence, it: &TrackItem) -> Vec<ClipMarker> {
    let mut out = Vec::new();
    let master = home_of(project, it.item);
    if let Some(home) = master {
        for m in home_markers(project, seq, home) {
            if let Some((start, duration)) = place(it, m.start, m.duration) {
                out.push(ClipMarker { clip: it.id, home, marker: m.clone(), start, duration });
            }
        }
    }
    for m in &it.markers {
        // an interchange file may repeat the master clip's markers on each clip: show them once
        let repeat =
            out.iter().any(|c| c.marker.start == m.start && c.marker.duration == m.duration && c.marker.name == m.name && c.marker.comment == m.comment);
        if !repeat && let Some((start, duration)) = place(it, m.start, m.duration) {
            out.push(ClipMarker { clip: it.id, home: MarkerHome::Clip(it.id), marker: m.clone(), start, duration });
        }
    }
    out.sort_by_key(|c| (c.start, c.marker.id.0));
    out
}

/// The clip markers of `clips` in the active sequence.
fn markers_of(s: &Session, clips: &BTreeSet<ClipId>) -> Vec<ClipMarker> {
    let Some(q) = s.active_sequence() else { return Vec::new() };
    q.all_tracks().flat_map(|t| &t.items).filter(|it| clips.contains(&it.id)).flat_map(|it| clip_markers(&s.project, q, it)).collect()
}

/// The selected clips of the active sequence that are under sequence time `t`: the clips Add
/// Marker (M) puts a clip marker on. Empty: it adds a sequence marker.
pub fn selected_under(s: &Session, t: Tick) -> Vec<ClipId> {
    let Some(q) = s.active_sequence() else { return Vec::new() };
    let sel = &s.state.selection;
    q.all_tracks().flat_map(|tr| &tr.items).filter(|it| sel.contains(&it.id) && it.start <= t && t < it.end()).map(|it| it.id).collect()
}

/// The clip marker of a selected clip under the playhead that starts on the playhead's frame:
/// Add Marker (M) on it opens it instead of stacking another.
pub fn at_playhead(s: &Session) -> Option<ClipMarker> {
    let t = s.playhead();
    let rate = s.sequence_rate();
    let clips: BTreeSet<ClipId> = selected_under(s, t).into_iter().collect();
    markers_of(s, &clips).into_iter().find(|c| rate.frame_at(c.start) == rate.frame_at(t))
}

/// Sequence times Go to Next / Previous Marker stops at: the sequence markers and the clip
/// markers of the selected clips.
pub fn stops(s: &Session) -> Vec<Tick> {
    let Some(q) = s.active_sequence() else { return Vec::new() };
    let mut v: Vec<Tick> = q.markers.iter().map(|m| m.start).collect();
    let sel: BTreeSet<ClipId> = s.state.selection.iter().copied().collect();
    v.extend(markers_of(s, &sel).iter().map(|c| c.start));
    v.sort();
    v.dedup();
    v
}

fn ids_p(p: &Value, cmd: &str) -> Result<Option<Vec<ClipId>>> {
    let Some(v) = p.get("clips").filter(|v| !v.is_null()) else { return Ok(None) };
    let a = v.as_array().filter(|a| a.len() <= MAX_CLIPS).ok_or_else(|| bad(cmd, format!("clips must be a list of at most {MAX_CLIPS} clip ids")))?;
    let ids = a.iter().map(|v| v.as_u64().map(ClipId).ok_or_else(|| bad(cmd, "clips must be clip ids"))).collect::<Result<Vec<_>>>()?;
    Ok(Some(ids))
}

/// Add Marker in the Timeline with clips selected (or `clips` named): a clip marker on each of
/// them under the marker time, kept by its source. `Ok(None)`: no clip takes it, so the caller
/// adds a sequence marker.
pub(crate) fn add(s: &mut Session, p: &Value) -> Result<Option<Value>> {
    const CMD: &str = "markers.add";
    let named = ids_p(p, CMD)?;
    let given = sequence_time_p(s, p, "", CMD)?;
    // M marks the selected clips under the playhead. A caller that names a time marks the sequence
    // there unless it asks for the selection (or names clips): a selection left over from an
    // earlier step must not turn an agent's sequence marker into a clip marker.
    let use_selection = match p.get("selection") {
        None | Some(Value::Null) => given.is_none(),
        Some(v) => v.as_bool().ok_or_else(|| bad(CMD, "selection must be true or false"))?,
    };
    if named.is_none() && !use_selection {
        return Ok(None);
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let t = given.unwrap_or_else(|| s.playhead());
    let clips: Vec<ClipId> = match named {
        Some(ids) => {
            if ids.is_empty() {
                return Err(bad(CMD, "clips must name at least one clip"));
            }
            for c in &ids {
                let (_, it) = q.find_item(*c).ok_or_else(|| bad(CMD, format!("no clip {} in the active sequence", c.0)))?;
                if !(it.start <= t && t < it.end()) {
                    return Err(bad(CMD, format!("clip {} is not under the marker time", c.0)));
                }
            }
            ids
        }
        None => selected_under(s, t),
    };
    if clips.is_empty() {
        return Ok(None);
    }
    // one marker per home and source time: linked sound shares its picture's
    let mut want: BTreeMap<(MarkerHome, Tick), Vec<ClipId>> = BTreeMap::new();
    for c in &clips {
        let Some((_, it)) = q.find_item(*c) else { continue };
        let home = home_of(&s.project, it.item).unwrap_or(MarkerHome::Clip(it.id));
        let src = it.source_time_at(t);
        if src < Tick::ZERO {
            return Err(bad(CMD, format!("clip {} shows no source frame at the marker time", c.0)));
        }
        let e = want.entry((home, src)).or_default();
        if !e.contains(c) {
            e.push(*c);
        }
    }
    let name = str_p(p, "name").unwrap_or("").to_string();
    let comment = str_p(p, "comment").unwrap_or("").to_string();
    if name.len() > 65_536 || comment.len() > 65_536 {
        return Err(bad(CMD, "marker text must be at most 65536 bytes"));
    }
    let (color, custom_color) = marker_color_p(p, CMD)?.unwrap_or((Label::Green, None));
    let author = marker_author_p(p, CMD)?.unwrap_or_default();
    let duration = match p.get("durationFrames").filter(|v| !v.is_null()) {
        None => Tick::ZERO,
        Some(v) => {
            let f = v.as_i64().filter(|f| (0..=10_000_000).contains(f)).ok_or_else(|| bad(CMD, "durationFrames must be a whole number from 0 to 10000000"))?;
            s.sequence_rate().tick_of(f)
        }
    };
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let added = s.edit("Add Marker", |pr, _| {
        let mut added = Vec::new();
        for (home, src) in want.keys() {
            if pr.next_id == u64::MAX {
                return Err(bad(CMD, "project ids are exhausted"));
            }
            let id = MarkerId(pr.alloc_id());
            let list = home_list(pr, seq_id, *home)?;
            list.push(Marker {
                url: String::new(),
                author: author.clone(),
                custom_color,
                id,
                start: *src,
                duration,
                name: name.clone(),
                comment: comment.clone(),
                kind: MarkerKind::Comment,
                color,
            });
            list.sort_by_key(|m| m.start);
            added.push(id);
        }
        Ok(added)
    })?;
    let homes: Vec<Value> = want.keys().map(|(h, _)| h.params()).collect();
    Ok(Some(json!({
        "marker": added.first().map(|m| m.0),
        "markers": added.iter().map(|m| m.0).collect::<Vec<_>>(),
        "clips": clips.iter().map(|c| c.0).collect::<Vec<_>>(),
        "homes": homes,
    })))
}

/// The marker list a home keeps, in the project being edited (`seq`: the active sequence).
pub(crate) fn home_list(pr: &mut Project, seq: ItemId, home: MarkerHome) -> Result<&mut Vec<Marker>> {
    let gone = || EngineError::Other("the marker's clip or source no longer exists".into());
    match home {
        MarkerHome::Source(id) => pr.item_mut(id).and_then(|i| i.as_media_mut()).map(|m| &mut m.markers).ok_or_else(gone),
        MarkerHome::Nest(id) => pr.sequence_mut(id).map(|q| &mut q.markers).ok_or_else(gone),
        MarkerHome::Clip(c) => pr.sequence_mut(seq).and_then(|q| q.find_item_mut(c)).map(|(_, it)| &mut it.markers).ok_or_else(gone),
    }
}

/// Clear Selected Marker / Clear Markers with clips selected: the clip markers of the selected
/// clips (at the playhead's frame, or all of those the clips show). `Ok(None)` when nothing is
/// selected: the caller clears sequence markers.
pub(crate) fn clear(s: &mut Session, all: bool) -> Result<Option<Value>> {
    if s.state.selection.is_empty() {
        return Ok(None);
    }
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let rate = s.sequence_rate();
    let t = s.playhead();
    let sel: BTreeSet<ClipId> = s.state.selection.iter().copied().collect();
    let shown = markers_of(s, &sel);
    let doomed: BTreeSet<(MarkerHome, MarkerId)> =
        shown.iter().filter(|c| all || rate.frame_at(c.start) == rate.frame_at(t)).map(|c| (c.home, c.marker.id)).collect();
    if doomed.is_empty() {
        // Clear Selected Marker on a frame with no clip marker clears the sequence marker there
        return Ok(if all { Some(json!({"count": 0})) } else { None });
    }
    let n = doomed.len();
    s.edit(if all { "Clear Clip Markers" } else { "Clear Clip Marker" }, |pr, _| {
        for (home, id) in &doomed {
            home_list(pr, seq_id, *home)?.retain(|m| m.id != *id);
        }
        Ok(())
    })?;
    Ok(Some(json!({"count": n})))
}
