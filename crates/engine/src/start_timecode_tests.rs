//! Time-of-day timelines: Sequence ▸ Start Time (`sequence.settings` `startTimecode`), sequence
//! timecode parameters read against it, and drop-frame clip timecode (`clip.modifyTimecode`
//! `dropFrame`).

use filmcraft_project::ItemId;
use filmcraft_time::{FrameRate, Tick};
use serde_json::json;

use crate::Session;

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

/// The demo sequence at 29.97 fps, drop-frame or not.
fn ntsc(df: bool) -> Session {
    let mut s = demo();
    s.execute("sequence.settings", json!({"fps": 29.97, "dropFrame": df})).unwrap();
    s
}

/// A media item of the demo project, its frame rate forced to `rate` (as Interpret Footage would).
fn media_at(s: &mut Session, rate: FrameRate) -> ItemId {
    let id = s.project.items.values().find(|i| i.as_media().is_some_and(|m| m.info.video.is_some())).map(|i| i.id).unwrap();
    s.edit("test: interpret", |p, _| {
        p.item_mut(id).and_then(|i| i.as_media_mut()).unwrap().interpret.frame_rate = Some(rate);
        Ok(())
    })
    .unwrap();
    id
}

#[test]
fn drop_frame_start_time_round_trips_with_exact_frames() {
    let mut s = ntsc(true);
    let r = s.execute("sequence.settings", json!({"startTimecode": "16:49:16;19"})).unwrap();
    // 16:49:16;19 DF = (1009 min × 60 + 16 s) × 30 + 19 − 2 × (1009 − 100) dropped labels
    assert_eq!(r["startTimecode"], json!({"frames": 1_814_881, "timecode": "16;49;16;19"}));
    let q = s.active_sequence().unwrap();
    assert_eq!(q.start_timecode, 1_814_881);
    // reported by sequence.settings with nothing to change and by sequence.inspect
    assert_eq!(s.execute("sequence.settings", json!({})).unwrap()["startTimecode"]["timecode"], "16;49;16;19");
    assert_eq!(s.execute("sequence.inspect", json!({})).unwrap()["startTimecode"]["frames"], 1_814_881);
    // one undo step
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active_sequence().unwrap().start_timecode, 0);
}

#[test]
fn non_drop_frame_start_time_counts_nominal_frames() {
    let mut s = ntsc(false);
    let r = s.execute("sequence.settings", json!({"startTimecode": "16:49:16:19"})).unwrap();
    assert_eq!(r["startTimecode"], json!({"frames": 1_816_699, "timecode": "16:49:16:19"}));
    // a frame count is taken as is
    let r = s.execute("sequence.settings", json!({"startTimecode": 108_000})).unwrap();
    assert_eq!(r["startTimecode"]["timecode"], "01:00:00:00");
    // drop-frame and the start time in one call: the string is read drop-frame
    let r = s.execute("sequence.settings", json!({"dropFrame": true, "startTimecode": "16:49:16;19"})).unwrap();
    assert_eq!(r["startTimecode"]["frames"], 1_814_881);
}

#[test]
fn hostile_start_times_are_refused_and_change_nothing() {
    let mut s = ntsc(true);
    s.execute("sequence.settings", json!({"startTimecode": "01:00:00;00"})).unwrap();
    let before = s.active_sequence().unwrap().clone();
    for bad in [
        json!({"startTimecode": "abc"}),
        json!({"startTimecode": "-00:00:01;00"}),
        json!({"startTimecode": "+15"}),
        json!({"startTimecode": "24:00:00;00"}),
        json!({"startTimecode": "99999999999999999999"}),
        json!({"startTimecode": -1}),
        json!({"startTimecode": 1.5}),
        json!({"startTimecode": true}),
        json!({"startTimecode": i64::MAX}),
        json!({"startTimecode": [1]}),
    ] {
        assert!(s.execute("sequence.settings", bad.clone()).is_err(), "{bad}");
        assert_eq!(s.active_sequence().unwrap(), &before, "{bad}");
    }
    // drop-frame stays refused on a rate without it, start time or not
    let mut s = demo();
    s.execute("sequence.settings", json!({"fps": 25})).unwrap();
    assert!(s.execute("sequence.settings", json!({"dropFrame": true, "startTimecode": "10:00:00;00"})).is_err());
    assert_eq!(s.active_sequence().unwrap().start_timecode, 0);
}

#[test]
fn a_new_timebase_keeps_the_start_time_moment() {
    let mut s = ntsc(true);
    s.execute("sequence.settings", json!({"startTimecode": "01:00:00;00"})).unwrap();
    let at = s.active_sequence().unwrap().start_tick();
    s.execute("sequence.settings", json!({"fps": 59.94})).unwrap();
    let q = s.active_sequence().unwrap();
    assert_eq!(q.start_tick(), at);
    assert_eq!(q.start_timecode, 2 * 107_892);
}

#[test]
fn timecode_parameters_are_the_displayed_time() {
    let mut s = ntsc(true);
    s.execute("sequence.settings", json!({"startTimecode": "16:49:16;19"})).unwrap();
    let rate = s.sequence_rate();
    // one second after the start
    s.execute("playhead.set", json!({"timecode": "16:49:17;19"})).unwrap();
    assert_eq!(s.playhead(), rate.tick_of(30));
    // ticks / frames / seconds still count from the first frame
    s.execute("playhead.set", json!({"frame": 45})).unwrap();
    assert_eq!(s.playhead(), rate.tick_of(45));
    // a time before the start is before the sequence: refused by commands that need a time ≥ 0
    let r = s.execute("markers.add", json!({"timecode": "16:49:15;00"}));
    assert!(r.is_err(), "{r:?}");
    let m = s.execute("markers.add", json!({"timecode": "16:49:18;19", "name": "two"})).unwrap();
    let js = s.execute("sequence.inspect", json!({})).unwrap();
    let marker = js["markers"].as_array().unwrap().iter().find(|x| x["id"] == m["marker"]).unwrap().clone();
    assert_eq!(marker["start"], rate.tick_of(60).0);
}

#[test]
fn drop_frame_clip_timecode_round_trips() {
    let mut s = demo();
    let id = media_at(&mut s, FrameRate::FPS_29_97);
    // non-drop (the default): the nominal count, as before
    let r = s.execute("clip.modifyTimecode", json!({"item": id.0, "timecode": "19:13:05;27"})).unwrap();
    assert_eq!(r["startTimecode"], 2_075_577);
    assert_eq!(r["dropFrame"], false);
    // drop-frame
    let r = s.execute("clip.modifyTimecode", json!({"item": id.0, "timecode": "19:13:05;27", "dropFrame": true})).unwrap();
    assert_eq!(r["startTimecode"], 2_073_501);
    assert_eq!((r["dropFrame"].as_bool(), r["timecode"].as_str()), (Some(true), Some("19;13;05;27")));
    let m = s.project.item(id).unwrap().as_media().unwrap().clone();
    assert!(m.timecode_drop_frame());
    // Media Start / End and the item clock show it drop-frame
    assert_eq!(m.clock().timecode(Tick::ZERO), "19;13;05;27");
    let fields = crate::panels::metadata_fields(&s.project, id);
    let start = fields.iter().find(|f| f.name == "Media Start").unwrap();
    assert_eq!(start.value, "19;13;05;27");
    // the flag alone keeps the frame count (the moment) and changes how it reads
    let r = s.execute("clip.modifyTimecode", json!({"item": id.0, "dropFrame": false})).unwrap();
    assert_eq!((r["startTimecode"].as_i64(), r["dropFrame"].as_bool()), (Some(2_073_501), Some(false)));
    // Use the file's timecode: the file's start and counting come back
    s.execute("clip.modifyTimecode", json!({"item": id.0, "reset": true})).unwrap();
    let m = s.project.item(id).unwrap().as_media().unwrap();
    assert_eq!((m.info.start_timecode, m.interpret.timecode_drop_frame), (None, None));
}

#[test]
fn hostile_clip_timecode_is_refused() {
    let mut s = demo();
    let id = media_at(&mut s, FrameRate::FPS_25);
    // no drop-frame form at 25 fps
    assert!(s.execute("clip.modifyTimecode", json!({"item": id.0, "timecode": "10:00:00;00", "dropFrame": true})).is_err());
    for bad in [
        json!({"item": id.0, "dropFrame": "yes"}),
        json!({"item": id.0, "frame": "12"}),
        json!({"item": id.0, "frame": -1}),
        json!({"item": id.0, "frame": i64::MAX}),
        json!({"item": id.0, "timecode": "nope"}),
    ] {
        let before = s.project.item(id).unwrap().clone();
        assert!(s.execute("clip.modifyTimecode", bad.clone()).is_err(), "{bad}");
        assert_eq!(s.project.item(id).unwrap(), &before, "{bad}");
    }
}

#[test]
fn old_projects_load_without_the_new_fields() {
    let s = ntsc(true);
    let mut v: serde_json::Value = serde_json::from_str(&s.project.to_json()).unwrap();
    // strip every timecode_drop_frame as a project from before it existed would have none
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.remove("timecode_drop_frame");
                m.values_mut().for_each(strip);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut v);
    let p = filmcraft_project::Project::from_json(&v.to_string()).unwrap();
    assert!(p.items.values().filter_map(|i| i.as_media()).all(|m| !m.timecode_drop_frame()));
}

#[test]
fn interchange_keeps_the_start_time() {
    use filmcraft_interchange::{ExportOptions, Format, export, import};
    let mut s = ntsc(true);
    s.execute("sequence.settings", json!({"startTimecode": "16:49:16;19"})).unwrap();
    let seq = s.state.active_sequence.unwrap();
    // the EDL's first record starts at the sequence's Start Time
    let (edl, _) = export(&s.project, seq, Format::Edl, &ExportOptions::default()).unwrap();
    let edl = String::from_utf8(edl).unwrap();
    let first = edl.lines().find(|l| l.starts_with("001")).unwrap();
    assert!(first.split_whitespace().any(|f| f == "16;49;16;19"), "{edl}");
    // and the start time survives a round trip through XML and AAF
    for format in [Format::Fcp7Xml, Format::Aaf] {
        let (bytes, _) = export(&s.project, seq, format, &ExportOptions::default()).unwrap();
        let (back, _) = import(&bytes, format, None).unwrap();
        let q = back.project.sequence(back.sequences[0]).unwrap();
        assert_eq!(q.start_timecode, 1_814_881, "{format:?}");
    }
}
