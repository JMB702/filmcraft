//! Multi-layer playback scenarios for `bench_playback`: sequences with more than one video layer,
//! in the shapes real edits have. A scenario is named `<shape>-<codec>-<res>-<n>`:
//!
//! | shape | what plays |
//! |---|---|
//! | `cover` | `n` full-frame opaque clips stacked on V1..Vn (B-roll over the main camera: only the top one shows) |
//! | `pip` | V1 full frame, the clips above at 50 % in the corners (every layer shows; the last at 80 % opacity) |
//! | `mcam` | a multicam nest with one track per camera (`n` cameras), one multicam clip showing the last angle |
//! | `mcam-broll` | that nest twice: angle 1 on V1, the last angle over it on V2 |
//! | `mcam-punch` | the multicam clip scaled to 125 % (a punch-in) |
//! | `mcam-fx` | the multicam clip with Tint and Noise |
//! | `nest-pip` | a plain nested sequence (`n - 1` layers) on V1 under a picture-in-picture clip on V2 |
//! | `fx` | `pip` with Lumetri Color on V1 and Tint on the layers above |
//! | `gfx` | `cover` of `n - 1` clips under a full-frame ProRes 4444 title with alpha |
//! | `edit` | an edit as the footage-sync skill lays it out: multicam clips of a two-camera nest (a 1080p camera scaled up, the main camera) on V1, a tinted B-roll clip of it on V2, a title on V3 and a punch-in |
//!
//! `codec` is `h264`, `hevc10` or `hlg` (see `fixtures::layer_fixture`), `res` is 1080 or 2160
//! (the sequence is that size at 29.97 fps), `n` is 1..=4. `layers` alone names the whole matrix.

use filmcraft_engine::Session;
use filmcraft_project::{ItemId, MulticamSel, ParamValue, Project, SequenceSettings, TrackItem, TrackKind, find_effect, resolve_auto_points};
use filmcraft_time::{FrameRate, Tick, TimeRange};

use crate::fixtures::layer_fixture;
use crate::playback::import;

pub const SHAPES: &[&str] = &["mcam-broll", "mcam-punch", "mcam-fx", "nest-pip", "cover", "pip", "mcam", "fx", "gfx", "edit"];

const RATE: FrameRate = FrameRate::FPS_29_97;

/// A parsed scenario name.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub shape: String,
    pub codec: String,
    pub res: u32,
    pub n: usize,
}

/// `<shape>-<codec>-<res>-<n>`, or None when `name` is not a layer scenario.
pub fn parse(name: &str) -> Option<Spec> {
    let shape = SHAPES.iter().find(|s| name.starts_with(&format!("{s}-")))?;
    let mut rest = name[shape.len() + 1..].split('-');
    let codec = rest.next().filter(|c| matches!(*c, "h264" | "hevc10" | "hlg"))?.to_string();
    let res = rest.next()?.parse().ok().filter(|r| matches!(r, 1080 | 2160))?;
    let n = rest.next()?.parse().ok().filter(|n| (1..=4).contains(n))?;
    rest.next().is_none().then(|| Spec { shape: shape.to_string(), codec, res, n })
}

/// The scenarios `--scenario layers` runs: 1..=4 stacked layers at both sizes in both camera
/// codecs, then the shapes of real projects at 2160p.
pub fn matrix() -> Vec<String> {
    let mut v = Vec::new();
    for codec in ["h264", "hevc10"] {
        for res in [1080, 2160] {
            for n in 1..=4 {
                v.push(format!("cover-{codec}-{res}-{n}"));
            }
            for n in 2..=4 {
                v.push(format!("pip-{codec}-{res}-{n}"));
            }
        }
    }
    for codec in ["h264", "hevc10"] {
        for shape in ["mcam", "mcam-broll", "mcam-punch", "mcam-fx", "nest-pip", "fx", "gfx"] {
            let n = if shape == "nest-pip" { 3 } else { 2 };
            v.push(format!("{shape}-{codec}-2160-{n}"));
        }
        v.push(format!("mcam-{codec}-2160-4"));
        v.push(format!("edit-{codec}-2160-1"));
    }
    v.push("pip-hlg-2160-2".into());
    v
}

fn size(res: u32) -> (u32, u32) {
    if res == 2160 { (3840, 2160) } else { (1920, 1080) }
}

/// The frame size of a project item (media or sequence).
fn item_size(p: &Project, item: ItemId, fallback: (u32, u32)) -> (u32, u32) {
    match p.item(item).map(|i| &i.kind) {
        Some(filmcraft_project::ItemKind::Media(m)) => m.info.video.as_ref().map(|v| (v.width, v.height)).unwrap_or(fallback),
        Some(filmcraft_project::ItemKind::Sequence(q)) => (q.settings.width, q.settings.height),
        _ => fallback,
    }
}

/// A clip of `item` at `start` s for `secs` s (from the same time of its source), placed as a new
/// clip is (centred, 100 %) in a `seq`-sized sequence.
fn clip(p: &mut Project, item: ItemId, seq: (u32, u32), start: f64, secs: f64) -> TrackItem {
    let at = RATE.snap(Tick::from_seconds_f64(start));
    let range = TimeRange::new(at, RATE.snap(Tick::from_seconds_f64(secs)));
    let src = item_size(p, item, seq);
    let mut v = p.make_track_item(item, TrackKind::Video, at, range, RATE).expect("track item");
    for e in &mut v.effects {
        resolve_auto_points(e, seq, src);
    }
    v
}

fn motion(v: &mut TrackItem, scale: f64, pos: Option<(f64, f64)>) {
    if let Some(m) = v.effect_mut("motion") {
        if let Some(x) = m.params.get_mut("scale") {
            x.value = ParamValue::Float(scale);
        }
        if let (Some(x), Some((px, py))) = (m.params.get_mut("position"), pos) {
            x.value = ParamValue::Vec2(filmcraft_geom::Vec2::new(px, py));
        }
    }
}

fn opacity(v: &mut TrackItem, pct: f64) {
    if let Some(o) = v.effect_mut("opacity").and_then(|o| o.params.get_mut("opacity")) {
        o.value = ParamValue::Float(pct);
    }
}

fn effect(v: &mut TrackItem, id: &str, params: &[(&str, f64)]) {
    let mut e = find_effect(id).unwrap_or_else(|| panic!("effect {id}")).instance();
    for (k, val) in params {
        if let Some(x) = e.params.get_mut(*k) {
            x.value = ParamValue::Float(*val);
        }
    }
    v.effects.push(e);
}

/// The Tint and Noise of a graded B-roll clip.
fn tint_noise(v: &mut TrackItem) {
    effect(v, "tint", &[("amount", 100.0)]);
    effect(v, "noise", &[("amount", 6.0)]);
}

fn multicam(mut v: TrackItem, angle: usize) -> TrackItem {
    v.multicam = Some(MulticamSel { enabled: true, angle: angle as u32 });
    v
}

/// A new sequence with `tracks` (bottom first), each a list of clips.
fn sequence(p: &mut Project, name: &str, (w, h): (u32, u32), tracks: Vec<Vec<TrackItem>>) -> ItemId {
    let settings = SequenceSettings { width: w, height: h, frame_rate: RATE, ..SequenceSettings::default() };
    let seq = p.new_sequence(name, settings, tracks.len().max(1), 1, None);
    let q = p.sequence_mut(seq).expect("seq");
    for (track, items) in q.video_tracks.iter_mut().zip(tracks) {
        track.items = items;
    }
    seq
}

/// Where the `i`-th picture-in-picture layer sits (the centre of a quadrant).
fn corner(i: usize, (w, h): (u32, u32)) -> (f64, f64) {
    let (w, h) = (w as f64, h as f64);
    [(w * 0.27, h * 0.27), (w * 0.73, h * 0.73), (w * 0.73, h * 0.27), (w * 0.27, h * 0.73)][i % 4]
}

/// Build the session of a layer scenario (None when its fixtures can't be made).
pub fn session(spec: &Spec) -> Option<(Session, ItemId)> {
    let Spec { shape, codec, res, n } = spec;
    let (n, sz) = (*n, size(*res));
    let secs = crate::fixtures::LAYER_SECS as f64;
    let mut s = Session::default();
    // the camera clips, and what else the shape needs
    let cams = if shape == "gfx" { n.saturating_sub(1).max(1) } else { n };
    let media: Vec<ItemId> = (0..cams).map(|i| layer_fixture(codec, *res, i).map(|p| import(&mut s, &p))).collect::<Option<_>>()?;
    let title = if matches!(shape.as_str(), "gfx" | "edit") { Some(import(&mut s, &layer_fixture("gfx", *res, 0)?)) } else { None };
    // `edit`: the second camera is a 1080p H.264 camcorder, scaled up inside the nest
    let small = if shape == "edit" { Some(import(&mut s, &layer_fixture("h264", 1080, 3)?)) } else { None };
    let seq = s
        .edit("bench layers", |p: &mut Project, st| {
            let full = |p: &mut Project, i: usize| clip(p, media[i], sz, 0.0, secs);
            let pip = |p: &mut Project, i: usize| {
                let mut v = clip(p, media[i], sz, 0.0, secs);
                if i > 0 {
                    motion(&mut v, 50.0, Some(corner(i - 1, sz)));
                    if i + 1 == n {
                        opacity(&mut v, 80.0);
                    }
                }
                v
            };
            // a nest with one camera per track
            let cameras = |p: &mut Project| {
                let tracks = (0..n).map(|i| vec![full(p, i)]).collect();
                sequence(p, "Cameras", sz, tracks)
            };
            let tracks: Vec<Vec<TrackItem>> = match shape.as_str() {
                "cover" => (0..n).map(|i| vec![full(p, i)]).collect(),
                "pip" => (0..n).map(|i| vec![pip(p, i)]).collect(),
                "fx" => (0..n)
                    .map(|i| {
                        let mut v = pip(p, i);
                        if i == 0 {
                            effect(&mut v, "lumetri", &[("temperature", 18.0), ("contrast", 22.0)]);
                        } else {
                            effect(&mut v, "tint", &[("amount", 70.0)]);
                        }
                        vec![v]
                    })
                    .collect(),
                "gfx" => {
                    let mut t: Vec<Vec<TrackItem>> = (0..cams).map(|i| vec![full(p, i)]).collect();
                    // the title is 4 s long: twice, end to end
                    let title = title.expect("title");
                    t.push(vec![clip(p, title, sz, 0.0, 4.0), {
                        let mut v = clip(p, title, sz, 0.0, 4.0);
                        v.start = RATE.snap(Tick::from_seconds_f64(4.0));
                        v
                    }]);
                    t
                }
                "mcam" | "mcam-punch" | "mcam-fx" => {
                    let nest = cameras(p);
                    let mut v = multicam(clip(p, nest, sz, 0.0, secs), n - 1);
                    match shape.as_str() {
                        "mcam-punch" => motion(&mut v, 125.0, None),
                        "mcam-fx" => tint_noise(&mut v),
                        _ => {}
                    }
                    vec![vec![v]]
                }
                "mcam-broll" => {
                    let nest = cameras(p);
                    vec![vec![multicam(clip(p, nest, sz, 0.0, secs), 0)], vec![multicam(clip(p, nest, sz, 0.0, secs), n - 1)]]
                }
                "nest-pip" => {
                    // the nest holds every layer but the last, which is the picture-in-picture above it
                    let inner = (0..n.saturating_sub(1).max(1)).map(|i| vec![pip(p, i)]).collect();
                    let nest = sequence(p, "Nested", sz, inner);
                    let mut top = clip(p, media[n - 1], sz, 0.0, secs);
                    motion(&mut top, 40.0, Some(corner(1, sz)));
                    vec![vec![clip(p, nest, sz, 0.0, secs)], vec![top]]
                }
                "edit" => {
                    let small = small.expect("camcorder");
                    let mut cam1 = clip(p, small, sz, 0.0, secs);
                    motion(&mut cam1, f64::from(sz.0) / 1920.0 * 100.0, None);
                    let cam2 = full(p, 0);
                    let nest = sequence(p, "Rough", sz, vec![vec![cam1], vec![cam2]]);
                    let title = title.expect("title");
                    // V1: the main camera, then the camcorder, then a punch-in on the main camera
                    let mut punch = multicam(clip(p, nest, sz, 6.0, 4.0), 1);
                    motion(&mut punch, 125.0, None);
                    let v1 = vec![multicam(clip(p, nest, sz, 0.0, 4.0), 1), multicam(clip(p, nest, sz, 4.0, 2.0), 0), punch];
                    // V2: graded B-roll over the main camera from 1.5 s to 3.5 s
                    let mut broll = multicam(clip(p, nest, sz, 1.5, 2.0), 0);
                    tint_noise(&mut broll);
                    // V3: a title from 4.5 s (the fixture's first second)
                    let mut t = clip(p, title, sz, 0.0, 1.0);
                    t.start = RATE.snap(Tick::from_seconds_f64(4.5));
                    vec![v1, vec![broll], vec![t]]
                }
                _ => return Err(filmcraft_engine::EngineError::Other(format!("unknown shape {shape}"))),
            };
            let seq = sequence(p, "Bench", sz, tracks);
            st.active_sequence = Some(seq);
            st.open_sequences = vec![seq];
            st.playheads.insert(seq, Tick::ZERO);
            Ok(seq)
        })
        .ok()?;
    Some((s, seq))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse() {
        assert_eq!(parse("cover-h264-2160-2"), Some(Spec { shape: "cover".into(), codec: "h264".into(), res: 2160, n: 2 }));
        assert_eq!(parse("mcam-broll-hevc10-1080-3").map(|s| s.shape), Some("mcam-broll".into()));
        assert_eq!(parse("mcam-hlg-2160-4").map(|s| (s.shape, s.n)), Some(("mcam".into(), 4)));
        for bad in ["stack3", "cover-h264-720-2", "cover-vp9-1080-2", "cover-h264-1080-9", "cover-h264-1080-2-x", "pip-h264-1080"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert!(matrix().iter().all(|n| parse(n).is_some()));
    }
}
