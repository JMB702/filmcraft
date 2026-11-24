//! Clip markers: Add Marker (M) with clips selected marks the clips (on their master clip, or for a
//! nest clip in the nested sequence), with nothing selected it marks the sequence; clip markers
//! ride on their clips through moves, trims, razor and paste; a nest clip shows the nested
//! sequence's markers, trimmed to the part it plays, and follows changes made inside it.

use filmcraft_project::{ClipId, ItemId};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;

fn f(s: &Session, n: i64) -> Tick {
    s.sequence_rate().tick_of(n)
}

/// Media items of the demo project that have both picture and sound.
fn av_media(s: &Session) -> Vec<ItemId> {
    let mut v: Vec<ItemId> = s.project.items.values().filter(|i| i.has_video() && i.has_audio() && i.as_media().is_some()).map(|i| i.id).collect();
    v.sort();
    v
}

fn place(s: &mut Session, item: ItemId, track: usize, frame: i64, source_in: i64, frames: i64) -> (ClipId, ClipId) {
    let p = json!({"item": item.0, "track": format!("V{track}"), "audioTrack": format!("A{track}"), "frame": frame, "sourceIn": f(s, source_in).0, "duration": f(s, frames).0});
    s.execute("timeline.place", p).unwrap();
    s.execute("edit.deselectAll", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let v = q.video_tracks[track - 1].items.iter().find(|i| i.start == f(s, frame)).unwrap().id;
    let a = q.audio_tracks[track - 1].items.iter().find(|i| i.start == f(s, frame)).unwrap().id;
    (v, a)
}

/// A sequence with one A/V clip on V1/A1 at frame 24, 48 frames long, from media frame 10.
fn one() -> (Session, ItemId, ClipId, ClipId) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.execute("file.newSequence", json!({"name": "Clip markers", "video": 3, "audio": 3})).unwrap();
    let item = av_media(&s)[0];
    let (v, a) = place(&mut s, item, 1, 24, 10, 48);
    (s, item, v, a)
}

fn at(s: &mut Session, frame: i64) {
    s.execute("playhead.set", json!({"frame": frame})).unwrap();
}

fn select(s: &mut Session, clips: &[ClipId]) {
    s.execute("timeline.select", json!({"clips": clips.iter().map(|c| c.0).collect::<Vec<_>>()})).unwrap();
}

fn media_markers(s: &Session, item: ItemId) -> Vec<Tick> {
    s.project.item(item).and_then(|i| i.as_media()).map(|m| m.markers.iter().map(|k| k.start).collect()).unwrap_or_default()
}

fn seq_markers(s: &Session) -> Vec<Tick> {
    s.active_sequence().unwrap().markers.iter().map(|m| m.start).collect()
}

/// The clips' markers as `sequence.inspect` reports them: (clip, sequence frame, home).
fn shown(s: &mut Session) -> Vec<(u64, i64, String)> {
    let q = s.execute("sequence.inspect", json!({})).unwrap();
    let mut out = Vec::new();
    for kind in ["video", "audio"] {
        for t in q[kind].as_array().unwrap() {
            for it in t["items"].as_array().unwrap() {
                for m in it["markers"].as_array().unwrap() {
                    out.push((it["clip"].as_u64().unwrap(), m["startFrame"].as_i64().unwrap(), m["home"].as_str().unwrap().to_string()));
                }
            }
        }
    }
    out
}

fn frames_of(s: &mut Session, clip: ClipId) -> Vec<i64> {
    shown(s).into_iter().filter(|(c, _, _)| *c == clip.0).map(|(_, f, _)| f).collect()
}

#[test]
fn m_with_a_clip_selected_marks_the_clip_and_its_linked_sound_in_one_undo_step() {
    let (mut s, item, v, a) = one();
    select(&mut s, &[v]);
    at(&mut s, 40);
    let undo = s.history.undo.len();
    let r = s.execute("markers.add", json!({"name": "Clap"})).unwrap();
    assert_eq!(r["markers"].as_array().unwrap().len(), 1, "the picture and its linked sound share one marker: {r}");
    assert!(seq_markers(&s).is_empty(), "no sequence marker");
    // the marker is on the master clip, at the media time the playhead shows: 10 + (40 - 24)
    assert_eq!(media_markers(&s, item), vec![f(&s, 26)]);
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    let mut got = shown(&mut s);
    got.sort();
    let mut want = vec![(v.0, 40, "source".to_string()), (a.0, 40, "source".to_string())];
    want.sort();
    assert_eq!(got, want, "the picture and its sound show it at the playhead");
    s.execute("edit.undo", json!({})).unwrap();
    assert!(media_markers(&s, item).is_empty() && shown(&mut s).is_empty(), "undo takes it away");
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(media_markers(&s, item), vec![f(&s, 26)], "redo puts it back");
}

#[test]
fn m_with_nothing_selected_or_no_selected_clip_under_the_playhead_marks_the_sequence() {
    let (mut s, item, v, _) = one();
    at(&mut s, 40);
    s.execute("markers.add", json!({})).unwrap();
    assert_eq!((seq_markers(&s), media_markers(&s, item)), (vec![f(&s, 40)], vec![]), "nothing selected: a sequence marker");
    // selected, but the playhead is past the clip (it ends at 72)
    select(&mut s, &[v]);
    at(&mut s, 80);
    s.execute("markers.add", json!({})).unwrap();
    assert_eq!((seq_markers(&s), media_markers(&s, item)), (vec![f(&s, 40), f(&s, 80)], vec![]), "no selected clip under the playhead: a sequence marker");
    // the ruler's Add Marker (selection: false) marks the sequence over a selected clip
    at(&mut s, 30);
    s.execute("markers.add", json!({"selection": false})).unwrap();
    assert_eq!(media_markers(&s, item), vec![], "selection:false leaves the clip alone");
    assert_eq!(seq_markers(&s).len(), 3);
    // a caller naming a time marks the sequence, a leftover selection notwithstanding...
    s.execute("markers.add", json!({"frame": 35})).unwrap();
    assert_eq!((seq_markers(&s).len(), media_markers(&s, item)), (4, vec![]), "an explicit time is a sequence marker");
    // ...unless it asks for the selection
    s.execute("markers.add", json!({"frame": 35, "selection": true})).unwrap();
    assert_eq!(media_markers(&s, item), vec![f(&s, 21)], "selection:true marks the selected clip at that time");
}

#[test]
fn several_selected_clips_each_get_a_marker_on_their_own_source() {
    let (mut s, item, v, _) = one();
    let media = av_media(&s);
    assert!(media.len() >= 2, "the demo project has two A/V media files");
    let other = media[1];
    let (v2, a2) = place(&mut s, other, 2, 30, 0, 48);
    // a third clip, selected but not under the playhead, gets none
    let (v3, _) = place(&mut s, item, 3, 100, 0, 24);
    select(&mut s, &[v, v2, v3]);
    at(&mut s, 50);
    let r = s.execute("markers.add", json!({})).unwrap();
    assert_eq!(r["markers"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(media_markers(&s, item), vec![f(&s, 10 + 26)]);
    assert_eq!(media_markers(&s, other), vec![f(&s, 20)]);
    assert_eq!(frames_of(&mut s, v), vec![50]);
    assert_eq!(frames_of(&mut s, a2), vec![50]);
    assert!(frames_of(&mut s, v3).is_empty(), "the clip not under the playhead has none (its media range ends before the marker)");
    assert!(seq_markers(&s).is_empty());
    // explicit clips over the control channel
    at(&mut s, 60);
    let r = s.execute("markers.add", json!({"clips": [v2.0]})).unwrap();
    assert_eq!(r["clips"], json!([v2.0]));
    assert_eq!(media_markers(&s, other), vec![f(&s, 20), f(&s, 30)]);
}

#[test]
fn clip_markers_ride_on_their_clip_through_move_trim_razor_and_paste_and_survive_a_save() {
    let (mut s, item, v, a) = one();
    select(&mut s, &[v]);
    at(&mut s, 40);
    s.execute("markers.add", json!({"name": "Clap"})).unwrap();
    s.execute("edit.deselectAll", json!({})).unwrap();
    // move: 10 frames later, on V2
    s.execute("timeline.move", json!({"moves": [{"clip": v.0, "track": "V2", "time": f(&s, 34).0}]})).unwrap();
    assert_eq!((frames_of(&mut s, v), frames_of(&mut s, a)), (vec![50], vec![50]), "moved with the clip and its sound");
    // trim the In point 5 frames later: the marker stays on its frame
    s.execute("timeline.trim", json!({"clip": v.0, "edge": "in", "deltaFrames": 5})).unwrap();
    assert_eq!(frames_of(&mut s, v), vec![50]);
    // trim the In point past it: it's outside the part the clip plays
    s.execute("timeline.trim", json!({"clip": v.0, "edge": "in", "deltaFrames": 15})).unwrap();
    assert!(frames_of(&mut s, v).is_empty(), "trimmed away");
    s.execute("timeline.trim", json!({"clip": v.0, "edge": "in", "deltaFrames": -15})).unwrap();
    assert_eq!(frames_of(&mut s, v), vec![50], "trimmed back");
    // razor at 45: the marker stays on the second part, at the same frame
    s.execute("timeline.razor", json!({"time": f(&s, 45).0, "track": "V2"})).unwrap();
    let q = s.active_sequence().unwrap();
    let parts: Vec<ClipId> = q.video_tracks[1].items.iter().map(|i| i.id).collect();
    assert_eq!(parts.len(), 2);
    assert_eq!((frames_of(&mut s, parts[0]), frames_of(&mut s, parts[1])), (vec![], vec![50]));
    // copy the second part and paste it at 200: the copy shows the marker too
    select(&mut s, &[parts[1]]);
    s.execute("edit.copy", json!({})).unwrap();
    s.execute("edit.deselectAll", json!({})).unwrap();
    at(&mut s, 200);
    s.execute("edit.paste", json!({})).unwrap();
    let pasted: Vec<i64> = shown(&mut s).into_iter().map(|(_, f, _)| f).filter(|f| *f >= 200).collect();
    assert!(pasted.contains(&(200 + 50 - 45)), "the pasted copy carries the marker: {pasted:?}");
    // save and reload
    let bytes = filmcraft_format::encode(&s.project, false);
    let back = filmcraft_format::decode(&bytes).unwrap().project;
    assert_eq!(
        back.item(item).and_then(|i| i.as_media()).map(|m| m.markers.iter().map(|k| (k.start, k.name.clone())).collect::<Vec<_>>()),
        Some(vec![(f(&s, 26), "Clap".to_string())])
    );
}

/// An "Inner" sequence (one 60-frame clip) with sequence markers at frames 10 and 30 and a range
/// marker over 40..50, nested twice in "Outer": whole at frame 0, and from inner frame 20 for 25
/// frames at frame 100. Returns (session, inner, first nest clip, second nest clip).
fn nested() -> (Session, ItemId, ClipId, ClipId) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.execute("file.newSequence", json!({"name": "Inner", "video": 1, "audio": 1})).unwrap();
    let inner = s.state.active_sequence.unwrap();
    let item = av_media(&s)[0];
    place(&mut s, item, 1, 0, 0, 60);
    for (frame, dur) in [(10, 0), (30, 0), (40, 10)] {
        s.execute("markers.add", json!({"time": f(&s, frame).0, "durationFrames": dur})).unwrap();
    }
    assert_eq!(seq_markers(&s).len(), 3);
    s.execute("file.newSequence", json!({"name": "Outer", "video": 2, "audio": 2})).unwrap();
    let p = json!({"item": inner.0, "track": "V1", "audioTrack": "A1", "frame": 0});
    s.execute("timeline.place", p).unwrap();
    let p = json!({"item": inner.0, "track": "V2", "audioTrack": "A2", "frame": 100, "sourceIn": f(&s, 20).0, "duration": f(&s, 25).0});
    s.execute("timeline.place", p).unwrap();
    s.execute("edit.deselectAll", json!({})).unwrap();
    let q = s.active_sequence().unwrap();
    let (a, b) = (q.video_tracks[0].items[0].id, q.video_tracks[1].items[0].id);
    (s, inner, a, b)
}

#[test]
fn a_nest_clip_shows_the_nested_sequences_markers_trimmed_to_the_part_it_plays() {
    let (mut s, inner, first, second) = nested();
    assert_eq!(frames_of(&mut s, first), vec![10, 30, 40]);
    // the second plays inner 20..45: 30 shows at 110, the range 40..50 is cut to 40..45 at 120
    assert_eq!(frames_of(&mut s, second), vec![110, 120]);
    let q = s.execute("sequence.inspect", json!({})).unwrap();
    let ms = q["video"][1]["items"][0]["markers"].as_array().unwrap().clone();
    assert!(ms.iter().all(|m| m["home"] == json!("nest") && m["owner"] == json!(inner.0)), "{ms:?}");
    assert_eq!(ms[1]["duration"].as_i64(), Some(f(&s, 5).0), "the range is trimmed to the clip");
    assert!(seq_markers(&s).is_empty(), "the outer sequence has no markers of its own");
}

#[test]
fn nest_clips_follow_changes_to_the_nested_sequences_markers() {
    let (mut s, inner, first, second) = nested();
    let outer = s.state.active_sequence.unwrap();
    // add one inside the nest (as if editing it): both nest clips show it at once
    let id = s.project.sequence(inner).unwrap().markers[1].id;
    s.execute("markers.edit", json!({"target": "program", "sequence": inner.0, "marker": id.0, "frame": 25})).unwrap();
    assert_eq!(frames_of(&mut s, first), vec![10, 25, 40]);
    assert_eq!(frames_of(&mut s, second), vec![105, 120]);
    s.execute("sequence.open", json!({"item": inner.0})).unwrap();
    s.execute("markers.add", json!({"time": f(&s, 35).0})).unwrap();
    s.execute("sequence.open", json!({"item": outer.0})).unwrap();
    assert_eq!(frames_of(&mut s, first), vec![10, 25, 35, 40]);
    assert_eq!(frames_of(&mut s, second), vec![105, 115, 120]);
    // M on a selected nest clip adds a sequence marker inside the nested sequence
    select(&mut s, &[second]);
    at(&mut s, 102);
    s.execute("markers.add", json!({})).unwrap();
    let inside: Vec<Tick> = s.project.sequence(inner).unwrap().markers.iter().map(|m| m.start).collect();
    assert!(inside.contains(&f(&s, 22)), "inner frame 20 + 2: {inside:?}");
    assert!(frames_of(&mut s, first).contains(&22), "the other nest clip of that sequence shows it too");
    assert!(seq_markers(&s).is_empty());
}

#[test]
fn go_to_next_and_previous_marker_stop_at_the_selected_clips_markers() {
    let (mut s, _, v, _) = one();
    at(&mut s, 30);
    s.execute("markers.add", json!({})).unwrap(); // sequence marker at 30
    select(&mut s, &[v]);
    at(&mut s, 50);
    s.execute("markers.add", json!({})).unwrap(); // clip marker at 50
    at(&mut s, 0);
    let frame = |s: &Session| s.sequence_rate().frame_at(s.playhead());
    s.execute("markers.goNext", json!({})).unwrap();
    assert_eq!(frame(&s), 30);
    s.execute("markers.goNext", json!({})).unwrap();
    assert_eq!(frame(&s), 50, "the selected clip's marker is a stop");
    s.execute("markers.goPrev", json!({})).unwrap();
    assert_eq!(frame(&s), 30);
    s.execute("edit.deselectAll", json!({})).unwrap();
    s.execute("markers.goNext", json!({})).unwrap();
    assert_eq!(frame(&s), 30, "nothing after the sequence marker with no clip selected");
}

#[test]
fn clearing_markers_with_a_clip_selected_clears_its_clip_markers_only() {
    let (mut s, item, v, _) = one();
    at(&mut s, 30);
    s.execute("markers.add", json!({})).unwrap();
    select(&mut s, &[v]);
    for frame in [40, 50, 60] {
        at(&mut s, frame);
        s.execute("markers.add", json!({})).unwrap();
    }
    assert_eq!(media_markers(&s, item).len(), 3);
    at(&mut s, 50);
    s.execute("markers.clearCurrent", json!({})).unwrap();
    assert_eq!(frames_of(&mut s, v), vec![40, 60], "Clear Selected Marker took the one at the playhead");
    s.execute("markers.clearAll", json!({})).unwrap();
    assert!(media_markers(&s, item).is_empty(), "Clear Markers cleared the clip's");
    assert_eq!(seq_markers(&s), vec![f(&s, 30)], "and left the sequence marker");
    s.execute("edit.deselectAll", json!({})).unwrap();
    s.execute("markers.clearAll", json!({})).unwrap();
    assert!(seq_markers(&s).is_empty());
}

#[test]
fn a_clips_own_markers_show_and_are_edited_through_target_clip() {
    let (mut s, _, v, _) = one();
    let seq = s.state.active_sequence.unwrap();
    let marker = filmcraft_project::Marker {
        url: String::new(),
        author: String::new(),
        custom_color: None,
        id: filmcraft_project::MarkerId(900_000),
        start: f(&s, 20),
        duration: Tick::ZERO,
        name: "Own".into(),
        comment: String::new(),
        kind: Default::default(),
        color: filmcraft_project::Label::Iris,
    };
    std::sync::Arc::make_mut(&mut s.project).sequence_mut(seq).unwrap().find_item_mut(v).unwrap().1.markers.push(marker);
    assert_eq!(shown(&mut s), vec![(v.0, 34, "clip".to_string())]);
    s.execute("markers.edit", json!({"target": "clip", "clip": v.0, "marker": 900_000, "name": "Renamed"})).unwrap();
    assert_eq!(s.active_sequence().unwrap().find_item(v).unwrap().1.markers[0].name, "Renamed");
    s.execute("markers.delete", json!({"target": "clip", "clip": v.0, "marker": 900_000})).unwrap();
    assert!(shown(&mut s).is_empty());
    assert!(s.execute("markers.edit", json!({"target": "clip", "clip": 1_234_567, "marker": 900_000, "name": "x"})).is_err());
}

#[test]
fn hostile_markers_add_parameters_are_refused_without_an_edit() {
    let (mut s, _, v, _) = one();
    at(&mut s, 40);
    let undo = s.history.undo.len();
    let n: Vec<Value> = (0..10_001).map(|i| json!(i)).collect();
    for p in [
        json!({"clips": "x"}),
        json!({"clips": [-1]}),
        json!({"clips": [1.5]}),
        json!({"clips": ["7"]}),
        json!({"clips": []}),
        json!({"clips": [u64::MAX]}),
        json!({"clips": n}),
        json!({"selection": "yes"}),
        json!({"clips": [v.0], "time": f(&s, 200).0}),
    ] {
        assert!(s.execute("markers.add", p.clone()).is_err(), "{p}");
    }
    assert_eq!(s.history.undo.len(), undo, "no undo step");
    assert!(seq_markers(&s).is_empty() && shown(&mut s).is_empty());
}
