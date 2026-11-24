//! Trimming the same edge of several selected clips together: `timeline.trim` with `clips` (the
//! Timeline's drag on one selected clip's edge) and trim mode's selected edit points. Every edge
//! moves by one delta in one undo step, and all of them stop at the first limit any of them reaches.

use filmcraft_edit::{Edge, TrimMode};
use filmcraft_project::ClipId;
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;

fn f(s: &Session, n: i64) -> Tick {
    s.sequence_rate().tick_of(n)
}

fn range(s: &Session, c: ClipId) -> (Tick, Tick) {
    s.active_sequence().unwrap().find_item(c).map(|(_, i)| (i.start, i.end())).unwrap()
}

/// Three 48-frame A/V clips, clip k on V(k+1) with its linked sound on A(k+1), starting at frame
/// 24·(k+1), with `head[k]` frames of media before its In point and at least 20 after its Out point.
fn three(head: [i64; 3]) -> (Session, Vec<(ClipId, ClipId)>) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.execute("file.newSequence", json!({"name": "Multi-trim", "video": 3, "audio": 3})).unwrap();
    let item = s.project.items.values().find(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).unwrap();
    let media = crate::media_duration(&s.project, &s.media, item).unwrap();
    assert!(media >= f(&s, 48 + 48 + 20), "the demo clip has room for the tail handle");
    for (k, h) in head.into_iter().enumerate() {
        let p = json!({"item": item.0, "track": format!("V{}", k + 1), "audioTrack": format!("A{}", k + 1), "frame": 24 * (k as i64 + 1), "sourceIn": f(&s, h).0, "duration": f(&s, 48).0});
        s.execute("timeline.place", p).unwrap();
    }
    s.execute("edit.deselectAll", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let clips = (0..3).map(|k| (q.video_tracks[k].items[0].id, q.audio_tracks[k].items[0].id)).collect();
    (s, clips)
}

/// Every clip of the fixture, pictures and sounds.
fn all(clips: &[(ClipId, ClipId)]) -> Vec<ClipId> {
    clips.iter().flat_map(|(v, a)| [*v, *a]).collect()
}

fn trim(s: &mut Session, clip: ClipId, extra: &[ClipId], edge: &str, mode: &str, frames: i64) -> crate::Result<Value> {
    let p = json!({"clip": clip.0, "clips": extra.iter().map(|c| c.0).collect::<Vec<_>>(), "edge": edge, "mode": mode, "deltaFrames": frames});
    s.execute("timeline.trim", p)
}

/// Where each clip is after its `edge` moved by `d`.
fn moved(before: &[(Tick, Tick)], edge: &str, d: Tick) -> Vec<(Tick, Tick)> {
    before.iter().map(|&(a, b)| if edge == "in" { (a + d, b) } else { (a, b + d) }).collect()
}

#[test]
fn extending_and_shortening_one_edge_trims_every_selected_clip_in_one_undo_step() {
    for edge in ["in", "out"] {
        for frames in [-5, 5] {
            let (mut s, clips) = three([48; 3]);
            let ids = all(&clips);
            let before: Vec<_> = ids.iter().map(|c| range(&s, *c)).collect();
            let (project, undo) = (s.project.clone(), s.history.undo.len());
            let r = trim(&mut s, clips[1].0, &[clips[0].0, clips[2].0], edge, "regular", frames).unwrap();
            let d = f(&s, frames);
            assert_eq!(r["delta"], json!(d.0), "{edge} {frames}");
            let after: Vec<_> = ids.iter().map(|c| range(&s, *c)).collect();
            assert_eq!(after, moved(&before, edge, d), "{edge} {frames}: every picture and its sound");
            assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
            let edited = s.project.clone();
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(*s.project, *project, "undo puts every clip back");
            s.execute("edit.redo", json!({})).unwrap();
            assert_eq!(*s.project, *edited);
        }
    }
}

#[test]
fn a_ripple_trim_of_several_edges_ripples_each_track_once() {
    for (edge, frames) in [("out", 5), ("out", -5), ("in", -5), ("in", 5)] {
        let (mut s, clips) = three([48; 3]);
        let item = s.active_sequence().unwrap().video_tracks[0].items[0].item;
        // a later clip on every track
        for k in 1..=3 {
            let p = json!({"item": item.0, "track": format!("V{k}"), "audioTrack": format!("A{k}"), "frame": 200, "sourceIn": 0, "duration": f(&s, 24).0});
            s.execute("timeline.place", p).unwrap();
        }
        let q = s.active_sequence().unwrap();
        let later: Vec<ClipId> = q.video_tracks.iter().chain(q.audio_tracks.iter()).map(|t| t.items[1].id).collect();
        let later_before: Vec<Tick> = later.iter().map(|c| range(&s, *c).0).collect();
        let dur = |s: &Session, c: ClipId| {
            let (a, b) = range(s, c);
            b - a
        };
        let ids = all(&clips);
        let durs: Vec<Tick> = ids.iter().map(|c| dur(&s, *c)).collect();
        let undo = s.history.undo.len();
        let r = trim(&mut s, clips[0].0, &[clips[1].0, clips[2].0], edge, "ripple", frames).unwrap();
        let d = f(&s, frames);
        assert_eq!(r["delta"], json!(d.0));
        // the clip gets longer by what its edge gave (an In edge moving earlier adds to its head)
        let grow = if edge == "in" { -d } else { d };
        for (c, d0) in ids.iter().zip(&durs) {
            assert_eq!(dur(&s, *c), *d0 + grow, "{edge} {frames}: clip {}", c.0);
        }
        for (c, t0) in later.iter().zip(&later_before) {
            assert_eq!(range(&s, *c).0, *t0 + grow, "{edge} {frames}: the later clip on each track moves once");
        }
        assert_eq!(s.history.undo.len(), undo + 1);
    }
}

#[test]
fn a_ripple_trim_refuses_two_edges_on_one_track_but_a_regular_trim_takes_them() {
    let (mut s, clips) = three([48; 3]);
    let item = s.active_sequence().unwrap().video_tracks[0].items[0].item;
    s.execute("timeline.place", json!({"item": item.0, "track": "V1", "audioTrack": "A1", "frame": 200, "sourceIn": 0, "duration": f(&s, 24).0})).unwrap();
    let second = s.active_sequence().unwrap().video_tracks[0].items[1].id;
    let (project, undo) = (s.project.clone(), s.history.undo.len());
    let e = trim(&mut s, clips[0].0, &[second], "out", "ripple", -3).unwrap_err().to_string();
    assert!(e.contains("one edge per track") && e.contains("V1"), "{e}");
    assert_eq!(*s.project, *project, "nothing moved");
    assert_eq!(s.history.undo.len(), undo);
    assert!(
        crate::commands::trim_delta_group(&s, clips[0].0, &[second], Edge::Out, TrimMode::Ripple, f(&s, -3), true).is_err(),
        "the drag shows the same refusal"
    );

    let (a, b) = (range(&s, clips[0].0), range(&s, second));
    trim(&mut s, clips[0].0, &[second], "out", "regular", -3).unwrap();
    assert_eq!(range(&s, clips[0].0), (a.0, a.1 - f(&s, 3)));
    assert_eq!(range(&s, second), (b.0, b.1 - f(&s, 3)));
}

#[test]
fn the_first_limit_any_clip_reaches_stops_all_of_them() {
    // media: the middle clip has 2 frames before its In point
    for mode in ["regular", "ripple"] {
        let (mut s, clips) = three([48, 2, 48]);
        let ids = all(&clips);
        let before: Vec<_> = ids.iter().map(|c| range(&s, *c)).collect();
        let shown = crate::commands::trim_delta_group(
            &s,
            clips[0].0,
            &[clips[1].0, clips[2].0],
            Edge::In,
            if mode == "ripple" { TrimMode::Ripple } else { TrimMode::Regular },
            f(&s, -5),
            true,
        )
        .unwrap();
        assert_eq!(shown, f(&s, -2), "{mode}: the drag stops where the edit will");
        let r = trim(&mut s, clips[0].0, &[clips[1].0, clips[2].0], "in", mode, -5).unwrap();
        assert_eq!(r["delta"], json!(f(&s, -2).0), "{mode}");
        let durs: Vec<Tick> = ids.iter().map(|c| range(&s, *c).1 - range(&s, *c).0).collect();
        assert!(durs.iter().zip(&before).all(|(d, b)| *d == b.1 - b.0 + f(&s, 2)), "{mode}: every clip gained 2 frames, none more");
    }

    // a neighbour 3 frames after the last clip's Out point, on a regular trim
    let (mut s, clips) = three([48; 3]);
    let item = s.active_sequence().unwrap().video_tracks[0].items[0].item;
    s.execute("timeline.place", json!({"item": item.0, "track": "V3", "audioTrack": "A3", "frame": 72 + 48 + 3, "sourceIn": 0, "duration": f(&s, 24).0}))
        .unwrap();
    let ids = all(&clips);
    let before: Vec<_> = ids.iter().map(|c| range(&s, *c)).collect();
    let r = trim(&mut s, clips[0].0, &[clips[1].0, clips[2].0], "out", "regular", 10).unwrap();
    assert_eq!(r["delta"], json!(f(&s, 3).0));
    assert_eq!(ids.iter().map(|c| range(&s, *c)).collect::<Vec<_>>(), moved(&before, "out", f(&s, 3)));

    // zero length: every clip keeps its last frame
    let (mut s, clips) = three([48; 3]);
    let ids = all(&clips);
    let r = trim(&mut s, clips[2].0, &[clips[0].0, clips[1].0], "out", "regular", -100).unwrap();
    assert_eq!(r["delta"], json!(f(&s, -47).0));
    assert!(ids.iter().all(|c| range(&s, *c).1 - range(&s, *c).0 == f(&s, 1)));
}

#[test]
fn linked_sound_follows_its_picture_unless_alt_was_held() {
    let (mut s, clips) = three([48; 3]);
    let sounds: Vec<_> = clips.iter().map(|c| range(&s, c.1)).collect();
    let p = json!({"clip": clips[0].0.0, "clips": [clips[1].0.0, clips[2].0.0], "edge": "out", "mode": "regular", "deltaFrames": 4, "linked": false});
    s.execute("timeline.trim", p).unwrap();
    for (k, c) in clips.iter().enumerate() {
        assert_eq!(range(&s, c.1), sounds[k], "Alt: the sound stays");
        assert_eq!(range(&s, c.0).1, sounds[k].1 + f(&s, 4), "the picture trims");
    }
}

#[test]
fn a_locked_track_is_skipped_with_its_clips_sound() {
    let (mut s, clips) = three([48; 3]);
    s.execute("timeline.setTrack", json!({"track": "V2", "locked": true})).unwrap();
    let before: Vec<_> = all(&clips).iter().map(|c| range(&s, *c)).collect();
    trim(&mut s, clips[0].0, &[clips[1].0, clips[2].0], "out", "regular", 5).unwrap();
    let d = f(&s, 5);
    for (k, c) in all(&clips).iter().enumerate() {
        let want = if k / 2 == 1 { before[k] } else { (before[k].0, before[k].1 + d) };
        assert_eq!(range(&s, *c), want, "clip {k}: V2's clip and its sound stay");
    }
    // grabbing the locked clip itself is refused, as a single trim is
    let project = s.project.clone();
    assert!(trim(&mut s, clips[1].0, &[clips[0].0], "out", "regular", 5).is_err());
    assert_eq!(*s.project, *project);
}

#[test]
fn hostile_clip_lists_are_refused_before_the_edit() {
    let (mut s, clips) = three([48; 3]);
    let (project, undo) = (s.project.clone(), s.history.undo.len());
    let many: Vec<u64> = std::iter::repeat_n(clips[1].0.0, 10_001).collect();
    for bad in [json!("x"), json!(7), json!([-1]), json!([1.5]), json!(["7"]), json!([null]), json!([u64::MAX]), json!([999_999_999]), json!(many)] {
        let p = json!({"clip": clips[0].0.0, "clips": bad, "edge": "out", "mode": "ripple", "deltaFrames": 3});
        assert!(s.execute("timeline.trim", p).is_err(), "{bad}");
        assert_eq!(*s.project, *project);
        assert_eq!(s.history.undo.len(), undo);
    }
    // repeats, the grabbed clip itself and its own sound are harmless
    let p = json!({"clip": clips[0].0.0, "clips": [clips[0].0.0, clips[0].1.0, clips[1].0.0, clips[1].0.0], "edge": "out", "mode": "ripple", "deltaFrames": 3});
    s.execute("timeline.trim", p).unwrap();
    assert_eq!(range(&s, clips[1].1).1, f(&s, 48 + 48 + 3), "trimmed once");
}

#[test]
fn a_single_clip_trims_exactly_as_before() {
    let (mut s, clips) = three([48; 3]);
    let before: Vec<_> = all(&clips).iter().map(|c| range(&s, *c)).collect();
    s.execute("timeline.trim", json!({"clip": clips[0].0.0, "edge": "out", "mode": "regular", "deltaFrames": 5})).unwrap();
    let d = f(&s, 5);
    for (k, c) in all(&clips).iter().enumerate() {
        let want = if k < 2 { (before[k].0, before[k].1 + d) } else { before[k] };
        assert_eq!(range(&s, *c), want);
    }
}

#[test]
fn selected_edit_points_on_several_tracks_trim_together_and_stop_together() {
    for kind in ["trim", "ripple"] {
        let (mut s, clips) = three([48, 2, 48]);
        for (n, c) in clips.iter().enumerate() {
            s.execute("trim.selectEditPoint", json!({"clip": c.0.0, "edge": "in", "kind": kind, "add": n != 0})).unwrap();
        }
        assert_eq!(s.state.edit_points.len(), 6, "{kind}: each picture's edge and its sound's");
        let ids = all(&clips);
        let before: Vec<_> = ids.iter().map(|c| range(&s, *c)).collect();
        let undo = s.history.undo.len();
        let r = s.execute("trim.nudge", json!({"frames": -5})).unwrap();
        assert_eq!(r["applied"], json!([f(&s, -2).0]), "{kind}");
        let durs: Vec<Tick> = ids.iter().map(|c| range(&s, *c).1 - range(&s, *c).0).collect();
        assert!(durs.iter().zip(&before).all(|(d, b)| *d == b.1 - b.0 + f(&s, 2)), "{kind}: all of them stopped at the middle clip's media");
        assert_eq!(s.history.undo.len(), undo + 1, "{kind}: one undo step");
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(ids.iter().map(|c| range(&s, *c)).collect::<Vec<_>>(), before);
    }
}
