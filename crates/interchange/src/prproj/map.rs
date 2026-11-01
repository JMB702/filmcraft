//! Premiere project graph → FilmCraft project fragment.
//!
//! How Premiere's objects chain together (each arrow is an `ObjectRef` / `ObjectURef`):
//!
//! - bins: `RootProjectItem` / `BinProjectItem` → items (bins and `ClipProjectItem`s);
//! - a project item: `ClipProjectItem` → `MasterClip` → `VideoClip` / `AudioClip` → source, where the
//!   source is a `Video/AudioMediaSource` → `Media` (the file) or a `Video/AudioSequenceSource` →
//!   `Sequence` (the project item is a sequence);
//! - a sequence: `Sequence` → `Video/AudioTrackGroup` → `Video/AudioClipTrack` → clip track items
//!   and transition track items;
//! - a clip on a track: `Video/AudioClipTrackItem` (timeline `Start` / `End`) → `SubClip` → its own
//!   `VideoClip` / `AudioClip` (`InPoint` = source in, `PlaybackSpeed`, multi-camera flag and
//!   selected angle) and the `MasterClip` it was edited from.
//!
//! The clip's `OutPoint` is not used: timeline duration × speed is authoritative (Premiere leaves
//! `OutPoint` past the edit for many clips).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use filmcraft_media::MediaKind;
use filmcraft_project::{
    BinId, Camera, ClipId, HexColor, ItemId, ItemKind, Label, Marker, MarkerId, MarkerKind, MulticamAudio, MulticamSel, MulticamSource, Track, TrackItem,
    TrackKind, Transition, TransitionAlign, TransitionId,
};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick, TimeRange};

use super::graph::{self, Graph, Rec, Ref, Source};
use crate::common::{
    Builder, MAX_IMPORTED_TRACKS, MediaSpec, empty_sequence, gain_to_db, kind_from_ext, plausible_rate, resolve_path, settings_for, transition_effect,
};
use crate::{ImportOptions, Imported, Report};

/// Deepest bin nesting followed.
const MAX_BIN_DEPTH: usize = 128;

/// A media item whose stored absolute path may have moved with the project: the path resolved
/// from the project-relative path Premiere also stores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relink {
    pub item: ItemId,
    pub relative: String,
}

struct Map<'g, 'r> {
    g: &'g Graph,
    b: Builder,
    report: &'r mut Report,
    base_dir: Option<String>,
    /// MasterClip UID → item.
    masters: HashMap<Ref, ItemId>,
    /// Sequence UID → item.
    sequences: HashMap<Ref, ItemId>,
    /// Sequence items in project order (bins first, then the rest).
    order: Vec<(Ref, ItemId)>,
    relinks: Vec<Relink>,
    /// Multi-camera use of each sequence: (sequence item, audio clip angles seen).
    multicam_sources: BTreeMap<ItemId, BTreeSet<u32>>,
    /// Track item ObjectID → (sequence, kind, clip) for links.
    placed: HashMap<Ref, (ItemId, ClipId)>,
    /// File path → the first media item made for it, so master clips no bin lists reuse it.
    by_path: HashMap<String, ItemId>,
    /// Media items made (each gets its own Builder key: Premiere may import one file twice).
    media_count: usize,
    /// Graphics made, for the report.
    graphics: usize,
}

pub(super) fn build(g: &Graph, name: &str, opts: &ImportOptions, report: &mut Report) -> (Imported, Vec<Relink>) {
    let mut m = Map {
        g,
        b: Builder::new(name),
        report,
        base_dir: opts.base_dir.clone(),
        masters: HashMap::new(),
        sequences: HashMap::new(),
        order: Vec::new(),
        relinks: Vec::new(),
        multicam_sources: BTreeMap::new(),
        placed: HashMap::new(),
        by_path: HashMap::new(),
        media_count: 0,
        graphics: 0,
    };
    let mut seen = HashSet::new();
    for root in &g.roots {
        if let Some(Rec::Bin(bin)) = g.uids.get(root) {
            for item in &bin.items {
                m.project_item(item, None, 0, false, &mut seen);
            }
        }
    }
    // sequences no bin lists (nested only, or orphaned) go in the root
    for uid in &g.sequences {
        let r = Ref::Uid(uid.clone());
        if !m.sequences.contains_key(&r) {
            m.reserve_sequence(&r, None, None, None);
        }
    }
    let order = m.order.clone();
    for (r, id) in &order {
        m.fill_sequence(r, *id);
    }
    m.multicam();
    m.break_cycles();
    if m.graphics > 0 {
        m.report.warn(format!("{} graphic(s) were imported as empty graphic clips: their text and shapes were not imported", m.graphics));
    }
    m.b.top = order.iter().map(|(_, id)| *id).collect();
    let relinks = std::mem::take(&mut m.relinks);
    (m.b.finish(), relinks)
}

fn label(index: Option<usize>, default: Label) -> Label {
    index.and_then(|i| Label::ALL.get(i).copied()).unwrap_or(default)
}

/// Frame rate from ticks per frame.
fn rate_of(ticks: Option<i64>) -> Option<FrameRate> {
    let t = ticks.filter(|t| *t > 0)?;
    let r = FrameRate::new(TICKS_PER_SECOND, t);
    plausible_rate(r).then_some(r)
}

impl Map<'_, '_> {
    /// Import the project item `r` into `bin`. With `reuse`, a media item already made for the same
    /// file is used instead of a new one.
    fn project_item(&mut self, r: &Ref, bin: Option<BinId>, depth: usize, reuse: bool, seen: &mut HashSet<Ref>) {
        if depth > MAX_BIN_DEPTH {
            self.report.warn("bins nested deeper than 128 levels were not imported");
            return;
        }
        if !seen.insert(r.clone()) {
            return;
        }
        let g = self.g;
        match g.get(r) {
            Some(Rec::Bin(b)) => {
                let id = self.b.bin(if b.name.is_empty() { "Bin" } else { &b.name }, bin);
                for item in &b.items {
                    self.project_item(item, Some(id), depth + 1, false, seen);
                }
            }
            Some(Rec::ClipItem(ci)) => {
                let Some(master_ref) = &ci.master else {
                    self.report.warn("a project item without a master clip was skipped");
                    return;
                };
                if self.masters.contains_key(master_ref) {
                    return;
                }
                let Some(Rec::MasterClip(mc)) = g.get(master_ref) else {
                    self.report.warn(format!("project item \"{}\" has no master clip; skipped", ci.name));
                    return;
                };
                // Premiere keeps the name a project item had when it was duplicated; the master clip
                // carries the current one
                let name = if mc.name.is_empty() { ci.name.clone() } else { mc.name.clone() };
                if let Some(seq) = self.sequence_of(&mc.clips) {
                    if let Some(id) = self.sequences.get(&seq).copied() {
                        self.masters.insert(master_ref.clone(), id);
                    } else {
                        let id = self.reserve_sequence(&seq, Some(&name), bin, ci.label);
                        self.masters.insert(master_ref.clone(), id);
                    }
                } else if let Some(id) = self.media_item(&mc.clips, &name, bin, ci.label, reuse) {
                    self.masters.insert(master_ref.clone(), id);
                }
            }
            // a Generative Extend result: the sequence edited with and the media it rendered go in
            // the bin; the clip it extends is reused when it is already in the project
            Some(Rec::Compound { editing, original, rendered }) => {
                for part in editing.iter().chain(rendered) {
                    self.project_item(part, bin, depth + 1, false, seen);
                }
                if let Some(part) = original {
                    self.project_item(part, bin, depth + 1, true, seen);
                }
            }
            Some(_) => self.report.warn("an unexpected object in a bin was skipped"),
            None => self.report.warn("a bin lists an item that is not in the project"),
        }
    }

    /// The sequence a master clip's clips play, if it is a sequence.
    fn sequence_of(&self, clips: &[Ref]) -> Option<Ref> {
        clips.iter().find_map(|c| match self.g.get(c) {
            Some(Rec::Clip(clip)) => match clip.source.as_ref().and_then(|s| self.g.get(s)) {
                Some(Rec::Source(Source::Sequence { sequence: Some(s), .. })) => Some(s.clone()),
                _ => None,
            },
            _ => None,
        })
    }

    fn reserve_sequence(&mut self, r: &Ref, name: Option<&str>, bin: Option<BinId>, label_index: Option<usize>) -> ItemId {
        let g = self.g;
        let rec = match g.get(r) {
            Some(Rec::Sequence(s)) => Some(s),
            _ => None,
        };
        let name = name.map(str::to_string).or_else(|| rec.map(|s| s.name.clone())).filter(|n| !n.is_empty()).unwrap_or_else(|| "Sequence".into());
        let mut rate = None;
        let mut size = None;
        let mut sample_ticks = None;
        for grp in rec.map(|s| s.groups.as_slice()).unwrap_or_default() {
            if let Some(Rec::TrackGroup(tg)) = g.get(grp) {
                if tg.video {
                    rate = rate.or(rate_of(tg.rate));
                    size = size.or(tg.size);
                } else {
                    sample_ticks = sample_ticks.or(tg.rate);
                }
            }
        }
        let rate = rate.unwrap_or(FrameRate::FPS_29_97);
        let (w, h) = size.filter(|(w, h)| filmcraft_project::validate_frame_size(*w, *h).is_ok()).unwrap_or_else(|| {
            if size.is_some() {
                self.report.warn(format!("sequence \"{name}\" has an unsupported frame size; 1920x1080 was used"));
            }
            (1920, 1080)
        });
        let mut settings = settings_for(rate, w, h, false);
        if let Some(sr) = sample_ticks.and_then(|t| u32::try_from(TICKS_PER_SECOND / t).ok()).filter(|sr| (8_000..=384_000).contains(sr)) {
            settings.sample_rate = sr;
        }
        let id = self.b.reserve_sequence(&name, settings, bin);
        if let Some(item) = self.b.p.item_mut(id) {
            item.label = label(label_index, Label::Forest);
        }
        self.sequences.insert(r.clone(), id);
        self.order.push((r.clone(), id));
        id
    }

    /// Create the media item of a master clip whose clips play a file.
    fn media_item(&mut self, clips: &[Ref], name: &str, bin: Option<BinId>, label_index: Option<usize>, reuse: bool) -> Option<ItemId> {
        let g = self.g;
        let mut media: Option<&graph::Media> = None;
        let mut spec = MediaSpec::default();
        let mut duration: Option<i64> = None;
        let mut markers = None;
        for c in clips {
            let Some(Rec::Clip(clip)) = g.get(c) else { continue };
            markers = markers.or(clip.markers.as_ref());
            let Some(Rec::Source(Source::Media { media: Some(mr), duration: d })) = clip.source.as_ref().and_then(|s| g.get(s)) else { continue };
            duration = duration.max(*d);
            let Some(Rec::Media(md)) = g.get(mr) else { continue };
            media = media.or(Some(md));
            if clip.video {
                if let Some(Rec::Stream(vs)) = md.video.as_ref().and_then(|v| g.get(v)) {
                    let (w, h) = vs.size.unwrap_or((1920, 1080));
                    spec.video = Some((w, h, rate_of(vs.rate).unwrap_or(FrameRate::FPS_29_97)));
                    duration = duration.max(vs.duration);
                }
            } else if let Some(Rec::Stream(a)) = md.audio.as_ref().and_then(|a| g.get(a)) {
                let sr = a.rate.and_then(|t| u32::try_from(TICKS_PER_SECOND / t).ok()).filter(|sr| (1_000..=768_000).contains(sr)).unwrap_or(48_000);
                spec.audio = Some((sr, a.channels.unwrap_or(2).min(64)));
                duration = duration.max(a.duration);
            }
        }
        let Some(md) = media else {
            self.report.warn(format!("\"{name}\" is not a media file or sequence (unsupported item); skipped"));
            return None;
        };
        if md.synthetic {
            // Premiere-made media: the path is a type code. Graphics keep their canvas
            if md.title.as_deref() != Some("Graphic") || spec.audio.is_some() && spec.video.is_none() {
                self.report.warn(format!(
                    "\"{name}\" ({}) is made by Premiere and has no FilmCraft equivalent; skipped",
                    md.title.as_deref().unwrap_or("synthetic media")
                ));
                return None;
            }
            let (width, height, rate) = spec.video.unwrap_or((1920, 1080, FrameRate::FPS_29_97));
            let (width, height) = if filmcraft_project::validate_frame_size(width, height).is_ok() { (width, height) } else { (1920, 1080) };
            self.graphics += 1;
            return Some(self.b.p.add_item(name, label(label_index, Label::Rose), ItemKind::Graphic { width, height, rate }, bin));
        }
        let relative = md.relative.as_deref().map(|rel| resolve_path(rel, self.base_dir.as_deref()));
        let Some(path) = md.path.clone().or_else(|| relative.clone()) else {
            self.report.warn(format!("\"{name}\" has no file path; skipped"));
            return None;
        };
        if reuse && let Some(id) = self.by_path.get(&path) {
            return Some(*id);
        }
        spec.duration = duration.filter(|d| *d > 0).map(Tick);
        if kind_from_ext(&path) == MediaKind::Still {
            spec.kind = Some(MediaKind::Still);
        }
        self.media_count += 1;
        let key = format!("media:{}", self.media_count);
        let id = self.b.file_media(&key, name, &path, &spec, bin);
        let markers = self.markers(markers);
        if let Some(item) = self.b.p.item_mut(id) {
            item.label = label(label_index, if spec.video.is_some() { Label::Iris } else { Label::Caribbean });
            if let Some(m) = item.as_media_mut() {
                m.markers = markers;
            }
        }
        if let Some(rel) = relative.filter(|r| *r != path) {
            self.relinks.push(Relink { item: id, relative: rel });
        }
        self.by_path.entry(path).or_insert(id);
        Some(id)
    }

    /// The item a track item's master clip maps to, creating a media item in the root for master
    /// clips no bin lists.
    fn item_for(&mut self, master: Option<&Ref>, clip: &graph::Clip, fallback_name: &str) -> Option<ItemId> {
        if let Some(m) = master
            && let Some(id) = self.masters.get(m)
        {
            return Some(*id);
        }
        let g = self.g;
        if let Some(Rec::Source(Source::Sequence { sequence: Some(s), .. })) = clip.source.as_ref().and_then(|s| g.get(s)) {
            return self.sequences.get(s).copied();
        }
        let (clips, name): (Vec<Ref>, String) = match master.and_then(|m| g.get(m)) {
            Some(Rec::MasterClip(mc)) => (mc.clips.clone(), if mc.name.is_empty() { fallback_name.to_string() } else { mc.name.clone() }),
            _ => (Vec::new(), fallback_name.to_string()),
        };
        let id = if clips.is_empty() { None } else { self.media_item(&clips, &name, None, None, true) }?;
        if let Some(m) = master {
            self.masters.insert(m.clone(), id);
        }
        Some(id)
    }

    fn fill_sequence(&mut self, r: &Ref, id: ItemId) {
        let g = self.g;
        let Some(Rec::Sequence(s)) = g.get(r) else {
            self.report.warn("a sequence object is missing; it was imported empty");
            return;
        };
        let Some(settings) = self.b.p.sequence(id).map(|q| q.settings.clone()) else { return };
        let frame = settings.frame_rate.frame_duration().0.max(1);
        let mut seq = empty_sequence(settings);
        if let Some(z) = s.zero_point.filter(|z| *z > 0) {
            seq.start_timecode = z / frame;
        }
        seq.mark_in = s.mark_in.filter(|t| *t >= 0).map(Tick);
        seq.mark_out = s.mark_out.filter(|t| *t > 0).map(Tick);
        if let (Some(a), Some(b)) = (s.work_in, s.work_out)
            && b > a
            && a >= 0
        {
            seq.work_area = Some(TimeRange::new(Tick(a), Tick(b - a)));
        }
        seq.markers = self.markers(s.markers.as_ref());
        for grp in &s.groups {
            let Some(Rec::TrackGroup(tg)) = g.get(grp) else { continue };
            let kind = if tg.video { TrackKind::Video } else { TrackKind::Audio };
            for (index, tr) in tg.tracks.iter().enumerate() {
                if index >= MAX_IMPORTED_TRACKS {
                    self.report.warn(format!("sequence \"{}\" has more than {MAX_IMPORTED_TRACKS} tracks of a kind; the rest were not imported", s.name));
                    break;
                }
                let track = self.track(tr, kind, index, id, &s.name);
                seq.tracks_mut(kind).push(track);
            }
        }
        // links: clips Premiere moves together
        for l in &s.links {
            let Some(Rec::Link(items)) = g.get(l) else { continue };
            let members: Vec<ClipId> = items.iter().filter_map(|i| self.placed.get(i)).filter(|(sq, _)| *sq == id).map(|(_, c)| *c).collect();
            if members.len() < 2 {
                continue;
            }
            let link = self.b.link_id();
            for t in seq.all_tracks_mut() {
                for ti in &mut t.items {
                    if members.contains(&ti.id) {
                        ti.link = Some(link);
                    }
                }
            }
        }
        self.b.put_sequence(id, seq);
    }

    fn track(&mut self, r: &Ref, kind: TrackKind, index: usize, seq: ItemId, seq_name: &str) -> Track {
        let mut track = self.b.track(kind, index);
        let g = self.g;
        let Some(Rec::Track(t)) = g.get(r) else {
            self.report.warn(format!("a track of sequence \"{seq_name}\" is missing; it was imported empty"));
            return track;
        };
        // transition ObjectID → (outgoing clip, incoming clip)
        let mut ends: HashMap<Ref, (Option<ClipId>, Option<ClipId>)> = HashMap::new();
        for item in &t.items {
            let Some(Rec::TrackItem(ti)) = g.get(item) else {
                self.report.warn("a clip whose object is missing was skipped");
                continue;
            };
            let Some(clip) = self.clip(ti, kind, seq) else { continue };
            if let Some(h) = &ti.head {
                ends.entry(h.clone()).or_default().1 = Some(clip.id);
            }
            if let Some(tl) = &ti.tail {
                ends.entry(tl.clone()).or_default().0 = Some(clip.id);
            }
            self.placed.insert(item.clone(), (seq, clip.id));
            track.items.push(clip);
        }
        for tr in &t.transitions {
            let Some(Rec::Transition(x)) = g.get(tr) else { continue };
            if x.end <= x.start || x.start < 0 {
                self.report.warn("a transition with an invalid range was skipped");
                continue;
            }
            let (from, to) = ends.get(tr).copied().unwrap_or_default();
            if from.is_none() && to.is_none() {
                self.report.warn(format!("transition \"{}\" has no adjacent clip; skipped", x.display_name));
                continue;
            }
            // the display name is the one people know ("Cross Dissolve"); the match name is internal
            let name = if x.display_name.is_empty() { x.match_name.as_str() } else { x.display_name.as_str() };
            let audio = kind == TrackKind::Audio;
            let effect = if audio && (x.match_name.eq_ignore_ascii_case("Custom Fade") || name.eq_ignore_ascii_case("Custom Fade")) {
                self.report.info("audio \"Custom Fade\" transitions (fade handles) were imported as Constant Power");
                transition_effect("Constant Power", true, self.report)
            } else {
                transition_effect(name, audio, self.report)
            };
            let duration = x.end - x.start;
            let align = match x.alignment {
                Some(0) => TransitionAlign::StartAtCut,
                Some(a) if a >= duration => TransitionAlign::EndAtCut,
                _ if to.is_none() => TransitionAlign::EndAtCut,
                _ if from.is_none() => TransitionAlign::StartAtCut,
                _ => TransitionAlign::CenterAtCut,
            };
            track.transitions.push(Transition {
                id: TransitionId(self.b.alloc()),
                effect,
                start: Tick(x.start),
                duration: Tick(duration),
                from,
                to,
                align,
                reverse: false,
            });
        }
        track
    }

    fn clip(&mut self, ti: &graph::TrackItem, kind: TrackKind, seq: ItemId) -> Option<TrackItem> {
        let g = self.g;
        if ti.end <= ti.start || ti.start < 0 {
            self.report.warn("a clip with an invalid timeline range was skipped");
            return None;
        }
        let Some(Rec::SubClip(sc)) = ti.subclip.as_ref().and_then(|s| g.get(s)) else {
            self.report.warn("a clip without its source description was skipped");
            return None;
        };
        let Some(Rec::Clip(clip)) = sc.clip.as_ref().and_then(|c| g.get(c)) else {
            self.report.warn(format!("clip \"{}\" has no source clip; skipped", sc.name));
            return None;
        };
        let Some(item) = self.item_for(sc.master.as_ref(), clip, &sc.name) else {
            self.report.warn(format!("clip \"{}\": its source is not in the project; skipped", sc.name));
            return None;
        };
        if item == seq {
            self.report.warn(format!("clip \"{}\" nests its own sequence; skipped", sc.name));
            return None;
        }
        let mut c = self.b.clip(item, kind, &sc.name, Tick(ti.start), Tick(ti.end - ti.start), Tick(clip.in_point.unwrap_or(0).max(0)));
        c.label = label(clip.label, c.label);
        c.custom_label_color = None;
        c.enabled = !ti.disabled;
        if let Some(sp) = clip.speed {
            if sp == 0.0 || sp.abs() > 1e4 {
                self.report.warn(format!("clip \"{}\" has an unusable speed ({sp}); 100% was used", sc.name));
            } else {
                c.speed = sp.abs();
                c.reverse = sp < 0.0;
            }
        }
        if let Some(h) = clip.frame_hold.filter(|h| *h >= 0) {
            c.frame_hold = Some(Tick(h));
        }
        if let Some(gain) = clip.gain.filter(|g| *g > 0.0 && (*g - 1.0).abs() > 1e-6) {
            c.gain_db = gain_to_db(gain).clamp(-96.0, 96.0);
        }
        let nested = self.b.p.item(item).and_then(|i| i.as_sequence()).is_some();
        if clip.multicam && nested {
            let angle = clip.angle.and_then(|a| u32::try_from(a).ok()).unwrap_or(0);
            c.multicam = Some(MulticamSel { enabled: true, angle });
            let seen = self.multicam_sources.entry(item).or_default();
            if kind == TrackKind::Audio {
                seen.insert(angle);
            }
        }
        if kind == TrackKind::Audio && !nested {
            let channels: Vec<u16> = clip
                .channels
                .iter()
                .filter_map(|s| match g.get(s) {
                    Some(Rec::Secondary { channel: Some(ch) }) => u16::try_from(*ch).ok(),
                    _ => None,
                })
                .take(32)
                .collect();
            if !channels.is_empty() && channels != [0, 1] {
                c.source_channels = channels;
            }
        }
        Some(c)
    }

    fn markers(&mut self, r: Option<&Ref>) -> Vec<Marker> {
        let g = self.g;
        let Some(Rec::Markers(list)) = r.and_then(|r| g.get(r)) else { return Vec::new() };
        let mut out = Vec::new();
        for m in list {
            let Some(Rec::Marker(json)) = g.get(m) else { continue };
            match marker(json) {
                Some(mut mk) => {
                    mk.id = MarkerId(self.b.alloc());
                    out.push(mk);
                }
                None => self.report.warn("a marker that could not be read was skipped"),
            }
        }
        out.sort_by_key(|m| m.start);
        out
    }

    /// Mark the sequences multi-camera clips play as multi-camera sources: one camera per video
    /// track, with the audio tracks that carry the same master clips as that camera's video.
    fn multicam(&mut self) {
        let sources: Vec<(ItemId, BTreeSet<u32>)> = self.multicam_sources.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (id, audio_angles) in sources {
            let Some(seq) = self.b.p.sequence(id) else { continue };
            let master_of = |t: &Track| -> BTreeSet<ItemId> { t.items.iter().map(|i| i.item).collect() };
            let audio_sets: Vec<(usize, BTreeSet<ItemId>)> = seq.audio_tracks.iter().enumerate().map(|(i, t)| (i, master_of(t))).collect();
            let mut cameras = Vec::new();
            let mut used = BTreeSet::new();
            for (vi, vt) in seq.video_tracks.iter().enumerate() {
                let video_items = master_of(vt);
                let audio_tracks: Vec<_> =
                    audio_sets.iter().filter(|(ai, set)| !set.is_empty() && !used.contains(ai) && set.is_subset(&video_items)).map(|(ai, _)| *ai).collect();
                used.extend(audio_tracks.iter().copied());
                let name = vt
                    .items
                    .first()
                    .and_then(|ti| self.b.p.item(ti.item))
                    .map(|i| crate::common::file_stem(&i.name).to_string())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| format!("Camera {}", vi + 1));
                cameras.push(Camera {
                    name,
                    video_track: Some(vt.id),
                    audio_tracks: audio_tracks.iter().filter_map(|ai| seq.audio_tracks.get(*ai).map(|t| t.id)).collect(),
                    enabled: true,
                    source: vt.items.first().map(|ti| ti.item),
                });
            }
            let unassigned = seq.audio_tracks.iter().enumerate().filter(|(i, t)| !used.contains(i) && !t.items.is_empty()).count();
            let audio = if audio_angles.iter().any(|a| *a != 0) {
                MulticamAudio::SwitchAudio
            } else if audio_angles.is_empty() {
                MulticamAudio::AllCameras
            } else {
                MulticamAudio::Camera1
            };
            let name = self.b.p.item(id).map(|i| i.name.clone()).unwrap_or_default();
            self.report.info(format!(
                "multi-camera source \"{name}\": {} camera(s), audio {}; camera audio matched by source clip{}",
                cameras.len(),
                audio.name(),
                if unassigned > 0 {
                    format!(", {unassigned} audio track(s) belong to no camera and stay silent in the multi-camera clip")
                } else {
                    String::new()
                }
            ));
            if let Some(s) = self.b.p.sequence_mut(id) {
                s.multicam = Some(MulticamSource { cameras, audio, sync: String::new() });
            }
        }
    }

    /// Nested sequences must not contain themselves: drop clips that close a cycle.
    fn break_cycles(&mut self) {
        let nests = |p: &filmcraft_project::Project, id: ItemId| -> Vec<ItemId> {
            p.sequence(id)
                .map(|s| s.all_tracks().flat_map(|t| t.items.iter().map(|i| i.item)).filter(|i| p.sequence(*i).is_some()).collect())
                .unwrap_or_default()
        };
        let ids: Vec<ItemId> = self.order.iter().map(|(_, id)| *id).collect();
        for &start in &ids {
            // does `start` reach itself?
            let mut stack: Vec<ItemId> = nests(&self.b.p, start);
            let mut seen = HashSet::new();
            let mut cyclic = BTreeSet::new();
            while let Some(n) = stack.pop() {
                if n == start {
                    cyclic.insert(n);
                    continue;
                }
                if seen.len() > 100_000 || !seen.insert(n) {
                    continue;
                }
                stack.extend(nests(&self.b.p, n));
            }
            if cyclic.is_empty() {
                continue;
            }
            // remove the direct nests of `start` that lead back to it
            let direct: Vec<ItemId> = nests(&self.b.p, start).into_iter().filter(|d| reaches(&self.b.p, *d, start, &nests)).collect();
            if let Some(s) = self.b.p.sequence_mut(start) {
                for t in s.all_tracks_mut() {
                    t.items.retain(|i| !direct.contains(&i.item));
                }
            }
            self.report.warn("clips of nested sequences that contain each other were removed");
        }
    }
}

fn reaches(p: &filmcraft_project::Project, from: ItemId, to: ItemId, nests: &dyn Fn(&filmcraft_project::Project, ItemId) -> Vec<ItemId>) -> bool {
    let mut stack = vec![from];
    let mut seen = HashSet::new();
    while let Some(n) = stack.pop() {
        if n == to {
            return true;
        }
        if seen.len() > 100_000 || !seen.insert(n) {
            continue;
        }
        stack.extend(nests(p, n));
    }
    false
}

/// A marker from its `DVAMarker` JSON.
fn marker(json: &str) -> Option<Marker> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let m = v.get("DVAMarker")?;
    let ticks = |key: &str| m.get(key).and_then(|t| t.get("ticks")).and_then(|t| t.as_i64().or_else(|| t.as_str().and_then(|s| s.parse().ok())));
    let start = ticks("mStartTime").unwrap_or(0).max(0);
    let duration = ticks("mDuration").unwrap_or(0).max(0);
    let text = |key: &str| m.get(key).and_then(|t| t.as_str()).unwrap_or("").to_string();
    let kind = match m.get("mType").and_then(|t| t.as_str()).unwrap_or("Comment") {
        "Chapter" => MarkerKind::Chapter,
        "Segmentation" => MarkerKind::Segmentation,
        "WebLink" => MarkerKind::WebLink,
        "FlashCuePoint" => MarkerKind::FlashCue,
        _ => MarkerKind::Comment,
    };
    // the colour is a cue point value: `{"color": n}`, n packed with red in the low byte
    let color = m.get("mCuePointList").and_then(|l| l.as_array()).and_then(|l| {
        l.iter().find_map(|c| {
            let value: serde_json::Value = serde_json::from_str(c.get("mValue")?.as_str()?).ok()?;
            let n = value.get("color")?.as_u64()?;
            Some(HexColor([(n & 0xff) as u8, (n >> 8 & 0xff) as u8, (n >> 16 & 0xff) as u8]))
        })
    });
    Some(Marker {
        url: String::new(),
        author: String::new(),
        custom_color: color,
        id: MarkerId(0),
        start: Tick(start),
        duration: Tick(duration),
        name: text("mName"),
        comment: text("mComment"),
        kind,
        color: Label::Green,
    })
}
