//! Tracks nothing can be seen of.
//!
//! A clip that fills the frame, opaque and in Normal mode, hides every track under it: B-roll
//! over the main camera, the top camera of a multi-camera source sequence played as it is, a
//! full-frame clip over a nested sequence. Those tracks used to be fetched, decoded, uploaded and
//! mixed all the same, so a sequence cost what all its layers cost however few of them showed.
//!
//! [`floor`] names the highest track that looks like such a cover, from the project alone (no
//! frame is decoded to find out). It is a prediction: media can be offline, a decoder can fail, a
//! picture can turn out to carry alpha. So the renderer draws that track first and looks at what
//! it got ([`frame_is_opaque`], [`crate::Image::is_opaque`]): only when the picture really is
//! opaque over the whole frame are the tracks below left out. When it is not, they are drawn after
//! all and the picture already made goes over them, so a wrong prediction costs nothing and can
//! never change the frame. Either way the result is the one drawing every track gives: an opaque
//! Normal layer replaces what is under it exactly (premultiplied over with alpha 1).
//!
//! The same code plans the GPU frames of the monitors and of exports, and renders the CPU
//! reference, so paused frames, playback and exports leave out the same tracks.
//! [`RenderOptions::occlusion`](crate::RenderOptions::occlusion) turns it off (the tests compare
//! the two).

use filmcraft_frame::{PixelData, VideoFrame};
use filmcraft_geom::{Affine, Vec2};
use filmcraft_project::{ItemKind, Project, Sequence, Track, TrackItem};
use filmcraft_time::Tick;

use crate::{Blend, RenderOptions, motion_matrix, opacity_blend, output_size, source_par, source_size};

/// Standard effects that only change colours: an opaque picture stays opaque under them, pixel
/// for pixel. A clip with any other enabled effect is not predicted to cover (most of the others
/// move, crop, key or blur the picture's edge). Only a prediction: what the effects made is
/// checked before anything is left out.
const COLOUR_EFFECTS: &[&str] = &[
    "brightness_contrast",
    "proc_amp",
    "tint",
    "black_white",
    "color_balance",
    "three_way_color",
    "leave_color",
    "change_to_color",
    "color_pass",
    "gamma_correction",
    "levels",
    "posterize",
    "asc_cdl",
    "channel_mix",
    "color_replace",
    "vignette",
    "video_limiter",
    "lumetri",
    "noise",
];

/// The lowest of the first `end` video tracks of `seq` that has to be drawn at `t`: the highest
/// one whose clip is predicted to cover the whole frame, or 0 when none is (or occlusion is off).
/// `nest` counts the nested sequences followed to reach `seq`.
pub(crate) fn floor(project: &Project, seq: &Sequence, t: Tick, opts: RenderOptions, nest: u32, end: usize) -> usize {
    if !opts.occlusion {
        return 0;
    }
    let tracks = seq.video_tracks.get(..end).unwrap_or(&seq.video_tracks);
    tracks.iter().rposition(|tr| tr.enabled && track_covers(project, seq, tr, t, opts, nest)).unwrap_or(0)
}

/// Whether the clip of `tr` at `t` is predicted to cover the frame. A transition is two clips
/// being mixed: never a cover.
fn track_covers(project: &Project, seq: &Sequence, tr: &Track, t: Tick, opts: RenderOptions, nest: u32) -> bool {
    !tr.transitions.iter().any(|x| x.range().contains(t)) && tr.item_at(t).is_some_and(|i| i.enabled && item_covers(project, seq, i, t, opts, nest))
}

fn item_covers(project: &Project, seq: &Sequence, item: &TrackItem, t: Tick, opts: RenderOptions, nest: u32) -> bool {
    let mt = item.effect_time_at(t);
    let (op, bl) = opacity_blend(item, mt);
    if op < 1.0 || bl != Blend::Normal || item.has_opacity_masks() {
        return false;
    }
    let standard = |e: &&filmcraft_project::EffectInstance| e.enabled && e.def().is_some_and(|d| !d.intrinsic) && !filmcraft_project::graphic::is_layer(e);
    if opts.effects && item.effects.iter().filter(standard).any(|e| !COLOUR_EFFECTS.contains(&e.effect.as_str())) {
        return false;
    }
    let Some(size) = source_size(project, item.item) else { return false };
    let (w, h) = output_size(seq, opts.scale);
    let s = f64::from(opts.scale);
    let placed = Affine::scale(s, s).then_apply(&motion_matrix(seq, item, size, source_par(project, item.item), mt));
    if !covers(size, &placed, w, h) {
        return false;
    }
    match project.item(item.item).map(|p| &p.kind) {
        Some(ItemKind::Media(_) | ItemKind::Subclip { .. }) => {
            project.resolve_media(item.item).and_then(|(_, m, _)| m.info.video.as_ref()).is_some_and(|v| !v.has_alpha)
        }
        Some(ItemKind::Sequence(nested)) if nest < crate::MAX_NEST_DEPTH => {
            let ft = item.source_time_at(t);
            match item.multicam_angle(nested) {
                // a multi-camera clip shows its angle's track, whether or not that track's output is on
                Some(angle) => nested
                    .angle_video_track_index(angle)
                    .and_then(|i| nested.video_tracks.get(i))
                    .is_some_and(|tr| track_covers(project, nested, tr, ft, opts, nest + 1)),
                None => nested.video_tracks.iter().any(|tr| tr.enabled && track_covers(project, nested, tr, ft, opts, nest + 1)),
            }
        }
        _ => false,
    }
}

/// How far inside the picture (in its own pixels) an output pixel's centre has to land to count
/// as covered: off the exact edge, where rounding in the placement could put it either side.
const EDGE: f64 = 1e-3;

/// Whether a `size` picture placed by `m` (its pixels to output pixels) covers the centre of
/// every pixel of a `w`×`h` output. The four corner pixels decide it: the placement is affine.
pub(crate) fn covers(size: (u32, u32), m: &Affine, w: usize, h: usize) -> bool {
    covers_within(size, m, w, h, EDGE)
}

/// [`covers`] for a picture that fades into transparency over its outermost half pixel (a nested
/// picture placed as a group: it is sampled with nothing around it, on both renderers): every
/// output pixel's centre has to land at least `inset` picture pixels inside it.
pub(crate) fn covers_within(size: (u32, u32), m: &Affine, w: usize, h: usize, inset: f64) -> bool {
    let Some(inv) = m.inverse() else { return false };
    let (sw, sh) = (f64::from(size.0), f64::from(size.1));
    let (x1, y1) = (w as f64 - 0.5, h as f64 - 0.5);
    [(0.5, 0.5), (x1, 0.5), (0.5, y1), (x1, y1)].into_iter().all(|(x, y)| {
        let p = inv.apply(Vec2::new(x, y));
        p.x >= inset && p.x <= sw - inset && p.y >= inset && p.y <= sh - inset
    })
}

/// Whether every pixel of a decoded frame is opaque for certain: video planes without an alpha
/// plane. RGBA frames are not looked into (a still with an alpha channel).
pub(crate) fn frame_is_opaque(f: &VideoFrame) -> bool {
    matches!(f.data, PixelData::Yuv8 { alpha: None, .. } | PixelData::Yuv16 { alpha: None, .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_covers_when_every_pixel_centre_lands_inside_it() {
        // placed as it is, or larger than the frame
        assert!(covers((1920, 1080), &Affine::IDENTITY, 1920, 1080));
        assert!(covers((1920, 1080), &Affine::scale(2.0, 2.0), 3840, 2160));
        assert!(covers((3840, 2160), &Affine::scale(0.5, 0.5), 1920, 1080));
        // a punch-in about the centre
        let punch = Affine::translate(960.0, 540.0).then_apply(&Affine::scale(1.25, 1.25)).then_apply(&Affine::translate(-960.0, -540.0));
        assert!(covers((1920, 1080), &punch, 1920, 1080));
        // one pixel short on the right, moved off the left edge, shrunk, turned
        assert!(!covers((1919, 1080), &Affine::IDENTITY, 1920, 1080));
        assert!(!covers((1920, 1080), &Affine::translate(1.0, 0.0), 1920, 1080));
        assert!(!covers((1920, 1080), &Affine::scale(0.5, 0.5), 1920, 1080));
        let turned = Affine::translate(960.0, 540.0).then_apply(&Affine::rotate_deg(10.0)).then_apply(&Affine::translate(-960.0, -540.0));
        assert!(!covers((1920, 1080), &turned, 1920, 1080));
        // a placement that cannot be inverted covers nothing
        assert!(!covers((1920, 1080), &Affine::scale(0.0, 1.0), 1920, 1080));
    }

    #[test]
    fn only_video_planes_without_alpha_are_known_to_be_opaque() {
        let plane = |n: usize, v: u8| std::sync::Arc::new(vec![v; n]);
        let yuv = |alpha| VideoFrame {
            width: 4,
            height: 4,
            data: PixelData::Yuv8 { planes: [plane(16, 16), plane(4, 128), plane(4, 128)], chroma: filmcraft_frame::Chroma::C420, alpha },
            color: filmcraft_color::ColorInfo::REC709,
            par: (1, 1),
            pts: Tick::ZERO,
        };
        assert!(frame_is_opaque(&yuv(None)));
        assert!(!frame_is_opaque(&yuv(Some(plane(16, 255)))));
        assert!(!frame_is_opaque(&VideoFrame::rgba_f32(1, 1, vec![0.0, 0.0, 0.0, 1.0])));
    }
}
