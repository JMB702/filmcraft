//! Tracks under a clip that covers the frame are not fetched, planned or mixed, and the picture
//! is the one drawing every track gives: on the CPU renderer and in the frame plan the GPU
//! compositor (monitors and exports) is given.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use filmcraft_media::{MediaInfo, MediaKind, MediaSource, VideoStreamInfo};
use filmcraft_project::{ClipId, Label, MediaClip, MediaRef, MulticamSel, ParamValue, SequenceSettings, TrackKind};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, TimeRange};

const RATE: FrameRate = FrameRate::FPS_24;
const W: u32 = 64;
const H: u32 = 36;

/// A video clip of one flat picture (8-bit 4:2:0, as a camera file decodes) that counts how
/// often a frame is asked of it.
struct Camera {
    info: MediaInfo,
    luma: u8,
    /// An alpha plane the file's description does not mention.
    surprise_alpha: bool,
    fetched: AtomicUsize,
}

impl MediaSource for Camera {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, _req: filmcraft_media::FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        self.fetched.fetch_add(1, Ordering::Relaxed);
        let v = self.info.video.as_ref().expect("video");
        let (w, h) = (v.width as usize, v.height as usize);
        let plane = |n: usize, x: u8| Arc::new(vec![x; n]);
        let alpha = self.surprise_alpha.then(|| plane(w * h, 128));
        Ok(Arc::new(VideoFrame {
            width: v.width,
            height: v.height,
            data: PixelData::Yuv8 {
                planes: [plane(w * h, self.luma), plane(w * h / 4, 128), plane(w * h / 4, 128)],
                chroma: filmcraft_frame::Chroma::C420,
                alpha,
            },
            color: filmcraft_color::ColorInfo::REC709,
            par: (1, 1),
            pts: Tick::ZERO,
        }))
    }
    fn audio(&self, _start: i64, _frames: usize, _rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        Err(filmcraft_media::MediaError::NoStream("audio"))
    }
}

struct Rig {
    p: Project,
    map: SourceMap,
    cameras: Vec<(ItemId, Arc<Camera>)>,
}

impl Rig {
    fn new() -> Rig {
        Rig { p: Project::new("occlusion"), map: SourceMap::default(), cameras: Vec::new() }
    }

    /// A `w`×`h` camera clip of grey level `luma`.
    fn camera_sized(&mut self, luma: u8, w: u32, h: u32, surprise_alpha: bool) -> ItemId {
        let info = MediaInfo {
            name: format!("cam{luma}"),
            kind: MediaKind::Movie,
            duration: Tick(10 * TICKS_PER_SECOND),
            video: Some(VideoStreamInfo {
                width: w,
                height: h,
                frame_rate: RATE,
                par: (1, 1),
                codec: "H.264".into(),
                pixel_format: "yuv420p".into(),
                color: filmcraft_color::ColorInfo::REC709,
                has_alpha: false,
                bitrate: None,
                hdr: None,
                color_assumed: None,
            }),
            audio_streams: Vec::new(),
            container: "MP4".into(),
            start_timecode: None,
            file_size: None,
        };
        let id = self.p.add_item(
            &info.name.clone(),
            Label::Iris,
            ItemKind::Media(Box::new(MediaClip {
                media: MediaRef::File { path: format!("/nowhere/cam{luma}.mp4") },
                info: info.clone(),
                interpret: Default::default(),
                mark_in: None,
                mark_out: None,
                markers: vec![],
                offline: false,
                proxy: None,
                identity: None,
            })),
            None,
        );
        let cam = Arc::new(Camera { info, luma, surprise_alpha, fetched: AtomicUsize::new(0) });
        self.map.0.insert(id, cam.clone() as SharedSource);
        self.cameras.push((id, cam));
        id
    }

    fn camera(&mut self, luma: u8) -> ItemId {
        self.camera_sized(luma, W, H, false)
    }

    /// A sequence with one clip of each of `items` per track, bottom first.
    fn seq(&mut self, items: &[ItemId]) -> (ItemId, Vec<ClipId>) {
        let settings = SequenceSettings { width: W, height: H, frame_rate: RATE, ..Default::default() };
        let seq = self.p.new_sequence("s", settings, items.len(), 0, None);
        let mut clips = Vec::new();
        for (track, item) in items.iter().enumerate() {
            let size = self.p.source_size(*item).unwrap_or((W, H));
            let mut ti = self.p.make_track_item(*item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, RATE.tick_of(48)), RATE).unwrap();
            for e in &mut ti.effects {
                filmcraft_project::resolve_auto_points(e, (W, H), size);
            }
            clips.push(ti.id);
            self.p.sequence_mut(seq).unwrap().video_tracks[track].items.push(ti);
        }
        (seq, clips)
    }

    fn clip(&mut self, seq: ItemId, clip: ClipId) -> &mut TrackItem {
        self.p.sequence_mut(seq).unwrap().find_item_mut(clip).unwrap().1
    }

    fn set(&mut self, seq: ItemId, clip: ClipId, effect: &str, param: &str, value: ParamValue) {
        self.clip(seq, clip).effect_mut(effect).unwrap().params.get_mut(param).unwrap().value = value;
    }

    fn add_effect(&mut self, seq: ItemId, clip: ClipId, id: &str, params: &[(&str, f64)]) {
        let mut e = filmcraft_project::find_effect(id).unwrap().instance();
        for (k, v) in params {
            e.params.get_mut(*k).unwrap().value = ParamValue::Float(*v);
        }
        self.clip(seq, clip).effects.push(e);
    }

    /// How often each camera was asked for a frame since the last call, in the order they were made.
    fn fetched(&self) -> Vec<usize> {
        self.cameras.iter().map(|(_, c)| c.fetched.swap(0, Ordering::Relaxed)).collect()
    }

    fn opts(occlusion: bool) -> RenderOptions {
        RenderOptions { occlusion, ..Default::default() }
    }

    /// The frame on the CPU renderer and through the frame plan, with and without occlusion:
    /// each pair must be the same picture to the bit. Returns the fetches of the occluded CPU
    /// render and of the occluded plan, and the plan's layer count.
    fn check(&self, seq: ItemId) -> (Vec<usize>, Vec<usize>, usize) {
        let t = RATE.tick_of(3);
        self.fetched();
        let cpu = render_sequence(&self.p, seq, t, Self::opts(true), &self.map);
        let cpu_fetched = self.fetched();
        let all = render_sequence(&self.p, seq, t, Self::opts(false), &self.map);
        assert!(cpu.px == all.px, "CPU: leaving out hidden tracks changed the picture");
        self.fetched();
        let plan = plan::plan_frame(&self.p, seq, t, Self::opts(true), plan::DeviceCaps::ALL, &self.map);
        let plan_fetched = self.fetched();
        let full = plan::plan_frame(&self.p, seq, t, Self::opts(false), plan::DeviceCaps::ALL, &self.map);
        assert!(plan::execute_cpu(&plan).px == plan::execute_cpu(&full).px, "plan: leaving out hidden tracks changed the picture");
        let layers = match &plan {
            plan::FramePlan::Layers { layers, .. } => layers.len(),
            plan::FramePlan::Composite { steps, .. } => steps.iter().map(|s| if let plan::PlanStep::Layers(l) = s { l.len() } else { 1 }).sum(),
            plan::FramePlan::Image(_) => 1,
        };
        (cpu_fetched, plan_fetched, layers)
    }
}

fn float(v: f64) -> ParamValue {
    ParamValue::Float(v)
}

/// B-roll over B-roll over the main camera, all full frame: one picture shows, one is fetched.
/// Before, each frame decoded, uploaded and mixed all three.
#[test]
fn only_the_top_of_a_stack_of_full_frame_clips_is_fetched() {
    let mut r = Rig::new();
    let cams = [r.camera(40), r.camera(120), r.camera(200)];
    let (seq, _) = r.seq(&cams);
    let (cpu, plan, layers) = r.check(seq);
    assert_eq!(cpu, [0, 0, 1], "CPU renderer");
    assert_eq!(plan, [0, 0, 1], "frame plan");
    assert_eq!(layers, 1);
    // off: every track, as before
    r.fetched();
    let _ = render_sequence(&r.p, seq, RATE.tick_of(3), Rig::opts(false), &r.map);
    assert_eq!(r.fetched(), [1, 1, 1]);
}

/// A picture-in-picture on top covers nothing; the full-frame clip under it does.
#[test]
fn a_clip_that_does_not_fill_the_frame_hides_nothing_but_the_one_under_it_can() {
    let mut r = Rig::new();
    let cams = [r.camera(40), r.camera(120), r.camera(200)];
    let (seq, clips) = r.seq(&cams);
    r.set(seq, clips[2], "motion", "scale", float(50.0));
    let (cpu, plan, layers) = r.check(seq);
    assert_eq!(cpu, [0, 1, 1]);
    assert_eq!(plan, [0, 1, 1]);
    assert_eq!(layers, 2);
}

/// Anything through which the tracks below show keeps them: opacity under 100 %, a blend mode,
/// an effect that can cut the picture, a transition, a disabled clip or track.
#[test]
fn tracks_that_show_through_are_all_drawn() {
    let build = |f: &dyn Fn(&mut Rig, ItemId, ClipId)| {
        let mut r = Rig::new();
        let cams = [r.camera(40), r.camera(200)];
        let (seq, clips) = r.seq(&cams);
        f(&mut r, seq, clips[1]);
        let (cpu, plan, _) = r.check(seq);
        (cpu, plan)
    };
    let both = (vec![1, 1], vec![1, 1]);
    assert_eq!(build(&|r, seq, top| r.set(seq, top, "opacity", "opacity", float(80.0))), both, "opacity");
    assert_eq!(build(&|r, seq, top| r.set(seq, top, "opacity", "blend", ParamValue::Choice(Blend::Screen.index()))), both, "blend mode");
    assert_eq!(build(&|r, seq, top| r.add_effect(seq, top, "crop", &[("left", 25.0)])), both, "crop");
    assert_eq!(build(&|r, seq, top| r.set(seq, top, "motion", "rotation", float(5.0))), both, "rotation");
    // a disabled clip or track shows nothing and hides nothing
    assert_eq!(build(&|r, seq, top| r.clip(seq, top).enabled = false), (vec![1, 0], vec![1, 0]), "disabled clip");
    assert_eq!(build(&|r, seq, _| r.p.sequence_mut(seq).unwrap().video_tracks[1].enabled = false), (vec![1, 0], vec![1, 0]), "disabled track");
}

/// A colour effect leaves an opaque picture opaque: graded B-roll still hides the camera under it.
#[test]
fn a_graded_clip_still_hides_what_is_under_it() {
    for effect in [("tint", "amount", 70.0), ("noise", "amount", 6.0)] {
        let mut r = Rig::new();
        let cams = [r.camera(40), r.camera(200)];
        let (seq, clips) = r.seq(&cams);
        r.add_effect(seq, clips[1], effect.0, &[(effect.1, effect.2)]);
        let (cpu, plan, layers) = r.check(seq);
        assert_eq!((cpu, plan, layers), (vec![0, 1], vec![0, 1], 1), "{}", effect.0);
    }
}

/// The prediction is made from the project; what is drawn decides. A clip whose media is offline
/// draws nothing, and one whose picture has an alpha plane the file did not announce is not
/// opaque: the tracks below are drawn after all, under it.
#[test]
fn a_wrong_prediction_draws_every_track() {
    // offline: no source for the top clip
    let mut r = Rig::new();
    let cams = [r.camera(40), r.camera(120), r.camera(200)];
    let (seq, _) = r.seq(&cams);
    r.map.0.remove(&cams[2]);
    let (cpu, plan, layers) = r.check(seq);
    // the clip under the offline one covers in its place
    assert_eq!((cpu, plan, layers), (vec![0, 1, 0], vec![0, 1, 0], 1));
    let img = render_sequence(&r.p, seq, RATE.tick_of(3), Rig::opts(true), &r.map);
    assert!(img.is_opaque() && img.get(5, 5)[0] > 0.1, "the camera under the offline clip shows");

    // an alpha plane nobody announced: half transparent over the camera below
    let mut r = Rig::new();
    let cams = [r.camera(40), r.camera_sized(200, W, H, true)];
    let (seq, _) = r.seq(&cams);
    let (cpu, plan, layers) = r.check(seq);
    assert_eq!((cpu, plan, layers), (vec![1, 1], vec![1, 1], 2));
    // the planned layers are bottom first: the camera, then the half-transparent clip over it
    let plan::FramePlan::Layers { layers, .. } = plan::plan_frame(&r.p, seq, RATE.tick_of(3), Rig::opts(true), plan::DeviceCaps::ALL, &r.map) else {
        panic!("layers")
    };
    assert!(crate::occlusion::frame_is_opaque(&layers[0].frame) && !crate::occlusion::frame_is_opaque(&layers[1].frame));
}

/// A multi-camera clip shows one angle. B-roll of another angle over it hides it: one camera is
/// fetched, not every camera of the source sequence twice.
#[test]
fn multicam_clips_over_each_other_fetch_one_angle() {
    let mut r = Rig::new();
    let cams = [r.camera(40), r.camera(120), r.camera(200)];
    let (source, _) = r.seq(&cams);
    let (seq, clips) = r.seq(&[source, source]);
    r.clip(seq, clips[0]).multicam = Some(MulticamSel { enabled: true, angle: 0 });
    r.clip(seq, clips[1]).multicam = Some(MulticamSel { enabled: true, angle: 1 });
    let (cpu, plan, layers) = r.check(seq);
    assert_eq!((cpu, plan, layers), (vec![0, 1, 0], vec![0, 1, 0], 1));
    // graded B-roll (the clip is rendered on the CPU with its effects) still hides the angle below
    r.add_effect(seq, clips[1], "noise", &[("amount", 6.0)]);
    let (cpu, plan, layers) = r.check(seq);
    assert_eq!((cpu, plan, layers), (vec![0, 1, 0], vec![0, 1, 0], 1));
}

/// A nested sequence shown as it is contributes its layers; a cover inside it hides the tracks
/// under it in the nest and the tracks under the nest.
#[test]
fn a_cover_inside_a_nest_hides_the_tracks_under_the_nest() {
    let mut r = Rig::new();
    let (below, a, b) = (r.camera(20), r.camera(120), r.camera(200));
    let (nest, _) = r.seq(&[a, b]);
    let (seq, _) = r.seq(&[below, nest]);
    let (cpu, plan, layers) = r.check(seq);
    assert_eq!((cpu, plan, layers), (vec![0, 0, 1], vec![0, 0, 1], 1));
}

/// A larger clip scaled down to fill the frame exactly, and a smaller one scaled up, cover it on
/// the compositor (it samples inside the picture at the frame's edge).
#[test]
fn a_clip_scaled_to_fill_the_frame_covers_it_in_the_plan() {
    let mut r = Rig::new();
    let (below, big, small) = (r.camera(20), r.camera_sized(120, W * 2, H * 2, false), r.camera_sized(200, W / 2, H / 2, false));
    let (seq, clips) = r.seq(&[below, big]);
    r.set(seq, clips[1], "motion", "scale", float(50.0));
    let plan = |r: &Rig, seq| {
        r.fetched();
        let _ = plan::plan_frame(&r.p, seq, RATE.tick_of(3), Rig::opts(true), plan::DeviceCaps::ALL, &r.map);
        r.fetched()
    };
    assert_eq!(plan(&r, seq), [0, 1, 0]);
    let (seq, clips) = r.seq(&[below, small]);
    r.set(seq, clips[1], "motion", "scale", float(200.0));
    assert_eq!(plan(&r, seq), [0, 0, 1]);
}

/// A punch-in on a multi-camera clip (a group in the plan) still hides the track under it, and a
/// shrunk one does not.
#[test]
fn a_punch_in_on_a_multicam_clip_hides_the_track_under_it() {
    let mut r = Rig::new();
    let (below, a, b) = (r.camera(20), r.camera(120), r.camera(200));
    let (source, _) = r.seq(&[a, b]);
    let (seq, clips) = r.seq(&[below, source]);
    r.clip(seq, clips[1]).multicam = Some(MulticamSel { enabled: true, angle: 1 });
    r.set(seq, clips[1], "motion", "scale", float(125.0));
    let (cpu, plan, _) = r.check(seq);
    assert_eq!((cpu, plan), (vec![0, 0, 1], vec![0, 0, 1]));
    let planned = plan::plan_frame(&r.p, seq, RATE.tick_of(3), Rig::opts(true), plan::DeviceCaps::ALL, &r.map);
    assert!(
        matches!(&planned, plan::FramePlan::Composite { steps, .. } if matches!(steps.as_slice(), [plan::PlanStep::Group(_)])),
        "one group, nothing under it"
    );
    r.set(seq, clips[1], "motion", "scale", float(60.0));
    let (cpu, plan, _) = r.check(seq);
    assert_eq!((cpu, plan), (vec![1, 0, 1], vec![1, 0, 1]));
}
