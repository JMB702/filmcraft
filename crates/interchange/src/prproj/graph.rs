//! The object graph of a Premiere project, reduced to what the importer maps.
//!
//! Each top-level object of the document becomes one compact record, keyed by its `ObjectID`
//! (numbers, referenced by `ObjectRef`) or `ObjectUID` (UUIDs, referenced by `ObjectURef`). Objects
//! of classes the importer does not use are counted by class and dropped unparsed. Times are in
//! Premiere's ticks, 254 016 000 000 per second: the same unit as [`filmcraft_time::Tick`].

use std::collections::{BTreeMap, HashMap};

use roxmltree::Node;

use crate::xml::{child, child_text, children, text};

use super::scan::{self, Object, Truncated};

/// A reference to another object.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Ref {
    Id(u64),
    Uid(String),
}

/// Premiere's "no time set" value for marks (In / Out points).
pub(super) const NO_TIME: i64 = -101_606_400_000_000_000;

#[derive(Debug, Default)]
pub(super) struct Bin {
    pub name: String,
    pub items: Vec<Ref>,
    pub root: bool,
}

#[derive(Debug, Default)]
pub(super) struct ClipItem {
    pub name: String,
    pub label: Option<usize>,
    pub master: Option<Ref>,
}

#[derive(Debug, Default)]
pub(super) struct MasterClip {
    pub name: String,
    pub clips: Vec<Ref>,
}

#[derive(Debug, Default)]
pub(super) struct Clip {
    pub video: bool,
    pub source: Option<Ref>,
    pub in_point: Option<i64>,
    pub speed: Option<f64>,
    pub multicam: bool,
    pub angle: Option<i64>,
    pub markers: Option<Ref>,
    pub label: Option<usize>,
    pub frame_hold: Option<i64>,
    /// Linear clip gain (audio clips).
    pub gain: Option<f64>,
    /// `SecondaryContent` objects: the source channels an audio clip plays.
    pub channels: Vec<Ref>,
}

#[derive(Debug, Default)]
pub(super) struct SubClip {
    pub name: String,
    pub clip: Option<Ref>,
    pub master: Option<Ref>,
}

#[derive(Debug)]
pub(super) enum Source {
    Media { media: Option<Ref>, duration: Option<i64> },
    Sequence { sequence: Option<Ref> },
}

#[derive(Debug, Default)]
pub(super) struct Media {
    pub path: Option<String>,
    pub relative: Option<String>,
    pub title: Option<String>,
    /// Made by Premiere (graphics, transcripts, mattes): the "path" is a type code, not a file.
    pub synthetic: bool,
    pub video: Option<Ref>,
    pub audio: Option<Ref>,
}

#[derive(Debug, Default)]
pub(super) struct Stream {
    /// Ticks per frame (video) or per sample (audio).
    pub rate: Option<i64>,
    pub duration: Option<i64>,
    pub size: Option<(u32, u32)>,
    pub channels: Option<u32>,
}

#[derive(Debug, Default)]
pub(super) struct Sequence {
    pub name: String,
    pub zero_point: Option<i64>,
    pub mark_in: Option<i64>,
    pub mark_out: Option<i64>,
    pub work_in: Option<i64>,
    pub work_out: Option<i64>,
    pub markers: Option<Ref>,
    pub links: Vec<Ref>,
    pub groups: Vec<Ref>,
}

#[derive(Debug, Default)]
pub(super) struct TrackGroup {
    pub video: bool,
    pub tracks: Vec<Ref>,
    pub rate: Option<i64>,
    pub size: Option<(u32, u32)>,
}

#[derive(Debug, Default)]
pub(super) struct Track {
    pub items: Vec<Ref>,
    pub transitions: Vec<Ref>,
}

#[derive(Debug, Default)]
pub(super) struct TrackItem {
    pub start: i64,
    pub end: i64,
    pub subclip: Option<Ref>,
    pub disabled: bool,
    pub head: Option<Ref>,
    pub tail: Option<Ref>,
}

#[derive(Debug, Default)]
pub(super) struct TransitionItem {
    pub start: i64,
    pub end: i64,
    pub match_name: String,
    pub display_name: String,
    pub alignment: Option<i64>,
}

#[derive(Debug)]
pub(super) enum Rec {
    Bin(Bin),
    ClipItem(ClipItem),
    /// A project item made of several (a Generative Extend result): the sequence edited with
    /// (`EditingProjectItem`), the clip it extends (`MediaProjectItem`) and the media it rendered
    /// (`AdditionalProjectItemsVector`).
    Compound {
        editing: Option<Ref>,
        original: Option<Ref>,
        rendered: Vec<Ref>,
    },
    MasterClip(MasterClip),
    Clip(Clip),
    Secondary {
        channel: Option<u32>,
    },
    SubClip(SubClip),
    Source(Source),
    Media(Media),
    Stream(Stream),
    Sequence(Sequence),
    TrackGroup(TrackGroup),
    Track(Track),
    TrackItem(TrackItem),
    Transition(TransitionItem),
    Link(Vec<Ref>),
    Markers(Vec<Ref>),
    /// The marker's `DVAMarker` JSON.
    Marker(String),
}

/// The parsed object graph.
#[derive(Debug, Default)]
pub(super) struct Graph {
    pub ids: HashMap<u64, Rec>,
    pub uids: HashMap<String, Rec>,
    /// Classes of the objects that were not read, with counts.
    pub skipped: BTreeMap<String, usize>,
    /// Objects of a read class that could not be parsed.
    pub broken: usize,
    pub duplicates: usize,
    /// Where (and why) the document ended early, if it did.
    pub truncated: Option<(usize, &'static str)>,
    /// Root bins, in document order.
    pub roots: Vec<String>,
    /// Sequences in document order.
    pub sequences: Vec<String>,
}

impl Graph {
    pub fn get(&self, r: &Ref) -> Option<&Rec> {
        match r {
            Ref::Id(i) => self.ids.get(i),
            Ref::Uid(u) => self.uids.get(u),
        }
    }
}

/// Classes the importer reads. Everything else is counted and skipped without parsing.
const READ: &[&str] = &[
    "RootProjectItem",
    "BinProjectItem",
    "ClipProjectItem",
    "CompoundProjectItem",
    "MasterClip",
    "VideoClip",
    "AudioClip",
    "SecondaryContent",
    "SubClip",
    "VideoMediaSource",
    "AudioMediaSource",
    "VideoSequenceSource",
    "AudioSequenceSource",
    "Media",
    "VideoStream",
    "AudioStream",
    "Sequence",
    "VideoTrackGroup",
    "AudioTrackGroup",
    "VideoClipTrack",
    "AudioClipTrack",
    "VideoClipTrackItem",
    "AudioClipTrackItem",
    "VideoTransitionTrackItem",
    "AudioTransitionTrackItem",
    "Link",
    "Markers",
    "Marker",
];

/// Largest number of XML nodes one object may have. The biggest objects of real projects (sequences
/// with long property lists, media with XMP) have a few thousand.
const MAX_OBJECT_NODES: u32 = 2_000_000;

/// Read the graph of a document.
pub(super) fn read(text: &str) -> Graph {
    let mut g = Graph::default();
    let walked = scan::objects(text, |o| g.add(o));
    if let Err(Truncated { at, why }) = walked {
        g.truncated = Some((at, why));
    }
    g
}

impl Graph {
    fn add(&mut self, o: Object<'_>) {
        if !READ.contains(&o.name) {
            if !o.text.ends_with("/>") || o.text.contains("ObjectID=") || o.text.contains("ObjectUID=") {
                *self.skipped.entry(o.name.to_string()).or_insert(0) += 1;
            }
            return;
        }
        let opts = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: MAX_OBJECT_NODES, ..Default::default() };
        let Ok(doc) = roxmltree::Document::parse_with_options(o.text, opts) else {
            self.broken += 1;
            return;
        };
        let n = doc.root_element();
        let key = if let Some(id) = n.attribute("ObjectID") {
            match id.trim().parse::<u64>() {
                Ok(i) => Ref::Id(i),
                Err(_) => {
                    self.broken += 1;
                    return;
                }
            }
        } else if let Some(uid) = n.attribute("ObjectUID") {
            Ref::Uid(uid.trim().to_ascii_lowercase())
        } else {
            // a reference (`<Project ObjectRef="1"/>`), not an object
            return;
        };
        let Some(rec) = record(o.name, n) else {
            self.broken += 1;
            return;
        };
        if let (Ref::Uid(u), Rec::Bin(b)) = (&key, &rec)
            && b.root
        {
            self.roots.push(u.clone());
        }
        if let (Ref::Uid(u), Rec::Sequence(_)) = (&key, &rec) {
            self.sequences.push(u.clone());
        }
        let fresh = match key {
            Ref::Id(i) => self.ids.insert(i, rec).is_none(),
            Ref::Uid(u) => self.uids.insert(u, rec).is_none(),
        };
        if !fresh {
            self.duplicates += 1;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reading one object
// ---------------------------------------------------------------------------------------------

fn at<'a, 'i>(n: Node<'a, 'i>, path: &[&str]) -> Option<Node<'a, 'i>> {
    let mut cur = n;
    for p in path {
        cur = child(cur, p)?;
    }
    Some(cur)
}

fn at_text<'a>(n: Node<'a, '_>, path: &[&str]) -> Option<&'a str> {
    at(n, path).map(text)
}

fn at_i64(n: Node<'_, '_>, path: &[&str]) -> Option<i64> {
    at_text(n, path).and_then(|t| t.parse::<i64>().ok())
}

fn at_f64(n: Node<'_, '_>, path: &[&str]) -> Option<f64> {
    at_text(n, path).and_then(|t| t.parse::<f64>().ok()).filter(|v| v.is_finite())
}

fn at_bool(n: Node<'_, '_>, path: &[&str]) -> bool {
    at_text(n, path).is_some_and(|t| t.eq_ignore_ascii_case("true") || t == "1")
}

/// The reference an element carries (`ObjectRef` or `ObjectURef`).
pub(super) fn reference(n: Node<'_, '_>) -> Option<Ref> {
    if let Some(r) = n.attribute("ObjectRef") {
        return r.trim().parse::<u64>().ok().map(Ref::Id);
    }
    n.attribute("ObjectURef").map(|u| Ref::Uid(u.trim().to_ascii_lowercase()))
}

fn ref_at(n: Node<'_, '_>, path: &[&str]) -> Option<Ref> {
    at(n, path).and_then(reference)
}

/// References of the `item` children of the element at `path`, in `Index` order.
fn refs_at(n: Node<'_, '_>, path: &[&str], item: &'static str) -> Vec<Ref> {
    let Some(list) = at(n, path) else { return Vec::new() };
    let mut v: Vec<(i64, Ref)> = children(list, item)
        .enumerate()
        .filter_map(|(k, c)| {
            let index = c.attribute("Index").and_then(|i| i.trim().parse::<i64>().ok()).unwrap_or(k as i64);
            reference(c).or_else(|| child(c, "Second").and_then(reference)).map(|r| (index, r))
        })
        .collect();
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, r)| r).collect()
}

/// A label index from `BE.Prefs.LabelColors.N`.
fn label(value: Option<&str>) -> Option<usize> {
    value?.trim().strip_prefix("BE.Prefs.LabelColors.")?.parse::<usize>().ok()
}

/// `x,y,width,height` → `(width, height)`.
fn rect(value: Option<&str>) -> Option<(u32, u32)> {
    let v: Vec<u32> = value?.split(',').filter_map(|p| p.trim().parse::<u32>().ok()).collect();
    match v.as_slice() {
        [_, _, w, h] if *w > 0 && *h > 0 => Some((*w, *h)),
        _ => None,
    }
}

/// Channel count of an `AudioChannelLayout` JSON array.
fn channel_count(value: Option<&str>) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(value?).ok()?;
    v.as_array().map(|a| a.len()).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0)
}

fn time(value: Option<i64>) -> Option<i64> {
    value.filter(|t| *t != NO_TIME)
}

fn record(class: &str, n: Node<'_, '_>) -> Option<Rec> {
    Some(match class {
        "RootProjectItem" | "BinProjectItem" => Rec::Bin(Bin {
            name: at_text(n, &["ProjectItem", "Name"]).unwrap_or("").to_string(),
            items: refs_at(n, &["ProjectItemContainer", "Items"], "Item"),
            root: class == "RootProjectItem",
        }),
        "ClipProjectItem" => Rec::ClipItem(ClipItem {
            name: at_text(n, &["ProjectItem", "Name"]).unwrap_or("").to_string(),
            label: label(at_text(n, &["ProjectItem", "Node", "Properties", "Column.PropertyText.Label"])),
            master: ref_at(n, &["MasterClip"]),
        }),
        "CompoundProjectItem" => Rec::Compound {
            editing: ref_at(n, &["EditingProjectItem"]),
            original: ref_at(n, &["MediaProjectItem"]),
            rendered: refs_at(n, &["AdditionalProjectItemsVector"], "AdditionalProjectItem"),
        },
        "MasterClip" => Rec::MasterClip(MasterClip { name: child_text(n, "Name").unwrap_or("").to_string(), clips: refs_at(n, &["Clips"], "Clip") }),
        "VideoClip" | "AudioClip" => {
            let c = child(n, "Clip")?;
            Rec::Clip(Clip {
                video: class == "VideoClip",
                source: ref_at(c, &["Source"]),
                in_point: at_i64(c, &["InPoint"]),
                speed: at_f64(c, &["PlaybackSpeed"]),
                multicam: at_bool(c, &["IsMulticam"]),
                angle: at_i64(c, &["SelectedTrackIndex"]),
                markers: ref_at(c, &["MarkerOwner", "Markers"]),
                label: label(at_text(c, &["Node", "Properties", "asl.clip.label.name"])),
                frame_hold: at_i64(n, &["FrameHoldStart"]).filter(|_| at_i64(n, &["FrameHold"]).is_some_and(|h| h != 0)),
                gain: at_f64(n, &["Gain"]),
                channels: refs_at(n, &["SecondaryContents"], "SecondaryContentItem"),
            })
        }
        "SecondaryContent" => Rec::Secondary { channel: at_i64(n, &["ChannelIndex"]).and_then(|c| u32::try_from(c).ok()) },
        "SubClip" => {
            Rec::SubClip(SubClip { name: child_text(n, "Name").unwrap_or("").to_string(), clip: ref_at(n, &["Clip"]), master: ref_at(n, &["MasterClip"]) })
        }
        "VideoMediaSource" | "AudioMediaSource" => {
            Rec::Source(Source::Media { media: ref_at(n, &["MediaSource", "Media"]), duration: at_i64(n, &["OriginalDuration"]) })
        }
        "VideoSequenceSource" | "AudioSequenceSource" => Rec::Source(Source::Sequence { sequence: ref_at(n, &["SequenceSource", "Sequence"]) }),
        "Media" => {
            let nonempty = |name: &str| child_text(n, name).filter(|t| !t.is_empty()).map(str::to_string);
            Rec::Media(Media {
                path: nonempty("ActualMediaFilePath").or_else(|| nonempty("FilePath")),
                relative: nonempty("RelativePath"),
                title: nonempty("Title"),
                synthetic: child_text(n, "Infinite").is_some_and(|t| t.eq_ignore_ascii_case("true"))
                    || nonempty("FilePath").is_some_and(|p| p.bytes().all(|c| c.is_ascii_digit())),
                video: ref_at(n, &["VideoStream"]),
                audio: ref_at(n, &["AudioStream"]),
            })
        }
        "VideoStream" | "AudioStream" => Rec::Stream(Stream {
            rate: at_i64(n, &["FrameRate"]).filter(|r| *r > 0),
            duration: at_i64(n, &["Duration"]).filter(|d| *d >= 0),
            size: rect(child_text(n, "FrameRect")),
            channels: channel_count(child_text(n, "AudioChannelLayout")),
        }),
        "Sequence" => {
            let props = at(n, &["Node", "Properties"]);
            let prop = |name: &str| props.and_then(|p| at_i64(p, &[name]));
            Rec::Sequence(Sequence {
                name: child_text(n, "Name").unwrap_or("").to_string(),
                zero_point: prop("MZ.ZeroPoint"),
                mark_in: time(prop("MZ.InPoint")),
                mark_out: time(prop("MZ.OutPoint")),
                work_in: time(prop("MZ.WorkInPoint")),
                work_out: time(prop("MZ.WorkOutPoint")),
                markers: ref_at(n, &["MarkerOwner", "Markers"]),
                links: refs_at(n, &["PersistentGroupContainer", "LinkContainer", "Links"], "Link"),
                groups: refs_at(n, &["TrackGroups"], "TrackGroup"),
            })
        }
        "VideoTrackGroup" | "AudioTrackGroup" => Rec::TrackGroup(TrackGroup {
            video: class == "VideoTrackGroup",
            tracks: refs_at(n, &["TrackGroup", "Tracks"], "Track"),
            rate: at_i64(n, &["TrackGroup", "FrameRate"]).filter(|r| *r > 0),
            size: rect(child_text(n, "FrameRect")),
        }),
        "VideoClipTrack" | "AudioClipTrack" => Rec::Track(Track {
            items: refs_at(n, &["ClipTrack", "ClipItems", "TrackItems"], "TrackItem"),
            transitions: refs_at(n, &["ClipTrack", "TransitionItems", "TrackItems"], "TrackItem"),
        }),
        "VideoClipTrackItem" | "AudioClipTrackItem" => {
            let c = child(n, "ClipTrackItem")?;
            Rec::TrackItem(TrackItem {
                start: at_i64(c, &["TrackItem", "Start"]).unwrap_or(0),
                end: at_i64(c, &["TrackItem", "End"])?,
                subclip: ref_at(c, &["SubClip"]),
                disabled: at_bool(c, &["IsMuted"]),
                head: ref_at(c, &["HeadTransition"]),
                tail: ref_at(c, &["TailTransition"]),
            })
        }
        "VideoTransitionTrackItem" | "AudioTransitionTrackItem" => {
            let t = child(n, "TransitionTrackItem")?;
            Rec::Transition(TransitionItem {
                start: at_i64(t, &["TrackItem", "Start"]).unwrap_or(0),
                end: at_i64(t, &["TrackItem", "End"])?,
                match_name: child_text(t, "MatchName").unwrap_or("").to_string(),
                display_name: child_text(t, "DisplayName").unwrap_or("").to_string(),
                alignment: at_i64(t, &["Alignment"]),
            })
        }
        "Link" => Rec::Link(refs_at(n, &["TrackItemGroup", "TrackItems"], "TrackItem")),
        "Markers" => Rec::Markers(refs_at(n, &["Markers"], "Marker")),
        "Marker" => Rec::Marker(child_text(n, "DVAMarker").unwrap_or("").to_string()),
        _ => return None,
    })
}
