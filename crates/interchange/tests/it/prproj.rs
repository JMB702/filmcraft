//! Premiere Pro project import. Fixtures are synthetic documents written for these tests in the
//! structure Premiere saves; a real project is read only from `FILMCRAFT_PRPROJ_SAMPLE` (a path to a
//! `.prproj`), and nothing from it is printed unless `FILMCRAFT_PRPROJ_DUMP` is set.

use filmcraft_interchange::{Format, ImportOptions, Imported, Report, detect, import_with, prproj};
use filmcraft_project::{BinEntry, ItemId, ItemKind, MediaRef, MulticamAudio, Project, Sequence, TrackItem};
use filmcraft_time::Tick;

const SEC: i64 = 254_016_000_000;
/// Ticks per frame at 29.97 fps.
const NTSC: i64 = 8_475_667_200;
/// Ticks per sample at 48 kHz.
const KHZ48: i64 = 5_292_000;
const NO_TIME: i64 = -101_606_400_000_000_000;

/// A Premiere document under construction: top-level objects that reference each other by
/// `ObjectID` (numbers) and `ObjectUID` (names here, UUIDs in Premiere).
#[derive(Default)]
struct Doc {
    objects: Vec<String>,
    next: u64,
}

fn r(id: u64) -> String {
    format!("ObjectRef=\"{id}\"")
}

fn u(uid: &str) -> String {
    format!("ObjectURef=\"{uid}\"")
}

fn list(item: &str, refs: &[String]) -> String {
    refs.iter().enumerate().map(|(i, r)| format!("<{item} Index=\"{i}\" {r}/>")).collect()
}

impl Doc {
    fn obj(&mut self, class: &str, body: &str) -> u64 {
        self.next += 1;
        let id = self.next;
        self.objects.push(format!("\t<{class} ObjectID=\"{id}\" ClassID=\"00000000-0000-0000-0000-000000000000\" Version=\"1\">{body}</{class}>"));
        id
    }

    fn uobj(&mut self, class: &str, uid: &str, body: &str) {
        self.objects.push(format!("\t<{class} ObjectUID=\"{uid}\" ClassID=\"00000000-0000-0000-0000-000000000000\" Version=\"1\">{body}</{class}>"));
    }

    fn xml(&self) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<PremiereData Version=\"3\">\n\t<Project ObjectRef=\"1\"/>\n{}\n</PremiereData>\n",
            self.objects.join("\n")
        )
    }

    fn bin(&mut self, class: &str, uid: &str, name: &str, items: &[&str]) {
        let items: Vec<String> = items.iter().map(|i| u(i)).collect();
        self.uobj(class, uid, &format!("<ProjectItem Version=\"1\"><Name>{name}</Name></ProjectItem><ProjectItemContainer Version=\"1\"><Items Version=\"1\">{}</Items></ProjectItemContainer>", list("Item", &items)));
    }

    fn video_stream(&mut self, w: u32, h: u32, duration: i64) -> u64 {
        self.obj("VideoStream", &format!("<FrameRate>{NTSC}</FrameRate><Duration>{duration}</Duration><FrameRect>0,0,{w},{h}</FrameRect>"))
    }

    fn audio_stream(&mut self, channels: usize, duration: i64) -> u64 {
        let layout: Vec<String> = (0..channels).map(|i| format!("{{\"channellabel\":{}}}", i + 100)).collect();
        self.obj(
            "AudioStream",
            &format!("<FrameRate>{KHZ48}</FrameRate><Duration>{duration}</Duration><AudioChannelLayout>[{}]</AudioChannelLayout>", layout.join(",")),
        )
    }

    fn media(&mut self, uid: &str, path: &str, relative: &str, video: Option<u64>, audio: Option<u64>) {
        let v = video.map(|v| format!("<VideoStream {}/>", r(v))).unwrap_or_default();
        let a = audio.map(|a| format!("<AudioStream {}/>", r(a))).unwrap_or_default();
        self.uobj(
            "Media",
            uid,
            &format!("<ActualMediaFilePath>{path}</ActualMediaFilePath><FilePath>{path}</FilePath><RelativePath>{relative}</RelativePath>{v}{a}"),
        );
    }

    fn media_source(&mut self, video: bool, media: &str, duration: i64) -> u64 {
        let class = if video { "VideoMediaSource" } else { "AudioMediaSource" };
        self.obj(class, &format!("<MediaSource Version=\"1\"><Media {}/></MediaSource><OriginalDuration>{duration}</OriginalDuration>", u(media)))
    }

    fn sequence_source(&mut self, video: bool, sequence: &str) -> u64 {
        let class = if video { "VideoSequenceSource" } else { "AudioSequenceSource" };
        self.obj(class, &format!("<SequenceSource Version=\"1\"><Sequence {}/></SequenceSource><OriginalDuration>0</OriginalDuration>", u(sequence)))
    }

    /// A `VideoClip` / `AudioClip`: `inner` goes in its `Clip`, `outer` beside it.
    fn clip(&mut self, video: bool, source: u64, inner: &str, outer: &str) -> u64 {
        let class = if video { "VideoClip" } else { "AudioClip" };
        self.obj(class, &format!("<Clip Version=\"1\"><Source {}/>{inner}</Clip>{outer}", r(source)))
    }

    fn master(&mut self, uid: &str, name: &str, clips: &[u64]) {
        let clips: Vec<String> = clips.iter().map(|c| r(*c)).collect();
        self.uobj("MasterClip", uid, &format!("<Name>{name}</Name><Clips Version=\"1\">{}</Clips>", list("Clip", &clips)));
    }

    fn clip_item(&mut self, uid: &str, name: &str, master: &str, label: Option<usize>) {
        let label = label.map(|l| format!("<Node Version=\"1\"><Properties Version=\"1\"><Column.PropertyText.Label>BE.Prefs.LabelColors.{l}</Column.PropertyText.Label></Properties></Node>")).unwrap_or_default();
        self.uobj("ClipProjectItem", uid, &format!("<ProjectItem Version=\"1\"><Name>{name}</Name>{label}</ProjectItem><MasterClip {}/>", u(master)));
    }

    /// A media file in the project: its master clip (one clip per stream) and its project item.
    fn file(&mut self, uid: &str, name: &str, video: Option<(u32, u32)>, audio: Option<usize>, seconds: i64) {
        let duration = seconds * SEC;
        let v = video.map(|(w, h)| self.video_stream(w, h, duration));
        let a = audio.map(|c| self.audio_stream(c, duration));
        let media = format!("{uid}-media");
        self.media(&media, &format!("/Volumes/Shoot/{name}"), &format!("../Shoot/{name}"), v, a);
        let mut clips = Vec::new();
        if v.is_some() {
            let s = self.media_source(true, &media, duration);
            clips.push(self.clip(true, s, "", ""));
        }
        if a.is_some() {
            let s = self.media_source(false, &media, duration);
            clips.push(self.clip(false, s, "", ""));
        }
        self.master(&format!("{uid}-master"), name, &clips);
        self.clip_item(uid, name, &format!("{uid}-master"), None);
    }

    /// A sequence's project item: a master clip whose clips play it.
    fn sequence_item(&mut self, uid: &str, name: &str, sequence: &str) {
        let vs = self.sequence_source(true, sequence);
        let v = self.clip(true, vs, "", "");
        let as_ = self.sequence_source(false, sequence);
        let a = self.clip(false, as_, "", "");
        self.master(&format!("{uid}-master"), name, &[v, a]);
        self.clip_item(uid, name, &format!("{uid}-master"), None);
    }

    /// A clip on a track: a track item, its sub-clip and the sub-clip's own clip.
    #[allow(clippy::too_many_arguments)]
    fn track_item(&mut self, video: bool, start: i64, end: i64, master: &str, source: u64, inner: &str, outer: &str, extra: &str) -> u64 {
        let clip = self.clip(video, source, inner, outer);
        let sub = self.obj("SubClip", &format!("<Name>clip</Name><Clip {}/><MasterClip {}/>", r(clip), u(master)));
        let class = if video { "VideoClipTrackItem" } else { "AudioClipTrackItem" };
        self.obj(
            class,
            &format!(
                "<ClipTrackItem Version=\"1\"><TrackItem Version=\"1\"><Start>{start}</Start><End>{end}</End></TrackItem><SubClip {}/>{extra}</ClipTrackItem>",
                r(sub)
            ),
        )
    }

    fn transition(&mut self, video: bool, start: i64, end: i64, display: &str, alignment: i64) -> u64 {
        let class = if video { "VideoTransitionTrackItem" } else { "AudioTransitionTrackItem" };
        self.obj(
            class,
            &format!("<TransitionTrackItem Version=\"1\"><TrackItem Version=\"1\"><Start>{start}</Start><End>{end}</End></TrackItem><DisplayName>{display}</DisplayName><Alignment>{alignment}</Alignment></TransitionTrackItem>"),
        )
    }

    fn track(&mut self, video: bool, items: &[u64], transitions: &[u64]) -> u64 {
        let items: Vec<String> = items.iter().map(|i| r(*i)).collect();
        let transitions: Vec<String> = transitions.iter().map(|i| r(*i)).collect();
        let class = if video { "VideoClipTrack" } else { "AudioClipTrack" };
        self.obj(
            class,
            &format!(
                "<ClipTrack Version=\"1\"><ClipItems Version=\"1\"><TrackItems Version=\"1\">{}</TrackItems></ClipItems><TransitionItems Version=\"1\"><TrackItems Version=\"1\">{}</TrackItems></TransitionItems></ClipTrack>",
                list("TrackItem", &items),
                list("TrackItem", &transitions)
            ),
        )
    }

    fn group(&mut self, video: bool, tracks: &[u64]) -> u64 {
        let tracks: Vec<String> = tracks.iter().map(|t| r(*t)).collect();
        if video {
            self.obj(
                "VideoTrackGroup",
                &format!(
                    "<TrackGroup Version=\"1\"><Tracks Version=\"1\">{}</Tracks><FrameRate>{NTSC}</FrameRate></TrackGroup><FrameRect>0,0,1920,1080</FrameRect>",
                    list("Track", &tracks)
                ),
            )
        } else {
            self.obj(
                "AudioTrackGroup",
                &format!("<TrackGroup Version=\"1\"><Tracks Version=\"1\">{}</Tracks><FrameRate>{KHZ48}</FrameRate></TrackGroup>", list("Track", &tracks)),
            )
        }
    }

    fn sequence(&mut self, uid: &str, name: &str, groups: &[u64], props: &str, markers: Option<u64>, links: &[u64]) {
        let groups: Vec<String> = groups.iter().map(|g| r(*g)).collect();
        let links: Vec<String> = links.iter().map(|l| r(*l)).collect();
        let markers = markers.map(|m| format!("<MarkerOwner Version=\"1\"><Markers {}/></MarkerOwner>", r(m))).unwrap_or_default();
        self.uobj(
            "Sequence",
            uid,
            &format!(
                "<Node Version=\"1\"><Properties Version=\"1\">{props}</Properties></Node><Name>{name}</Name>{markers}<TrackGroups Version=\"1\">{}</TrackGroups><PersistentGroupContainer Version=\"1\"><LinkContainer Version=\"1\"><Links Version=\"1\">{}</Links></LinkContainer></PersistentGroupContainer>",
                list("TrackGroup", &groups),
                list("Link", &links)
            ),
        );
    }

    fn link(&mut self, items: &[u64]) -> u64 {
        let items: Vec<String> = items.iter().map(|i| r(*i)).collect();
        self.obj("Link", &format!("<TrackItemGroup Version=\"1\"><TrackItems Version=\"1\">{}</TrackItems></TrackItemGroup>", list("TrackItem", &items)))
    }

    fn param(&mut self, point: bool, id: u32, name: &str, start: &str, keyframes: &str) -> u64 {
        let class = if point { "PointComponentParam" } else { "VideoComponentParam" };
        let k = if keyframes.is_empty() { String::new() } else { format!("<IsTimeVarying>true</IsTimeVarying><Keyframes>{keyframes}</Keyframes>") };
        self.obj(class, &format!("<ParameterID>{id}</ParameterID><Name>{name}</Name><StartKeyframe>-91445760000000000,{start},0,0,0,0,0,0</StartKeyframe>{k}"))
    }

    fn filter(&mut self, match_name: &str, display: &str, params: &[u64]) -> u64 {
        let params: Vec<String> = params.iter().map(|p| r(*p)).collect();
        self.obj(
            "VideoFilterComponent",
            &format!(
                "<Component Version=\"7\"><Params Version=\"1\">{}</Params><DisplayName>{display}</DisplayName></Component><MatchName>{match_name}</MatchName>",
                list("Param", &params)
            ),
        )
    }

    fn volume(&mut self, level: &str, keyframes: &str) -> u64 {
        let k = if keyframes.is_empty() { String::new() } else { format!("<IsTimeVarying>true</IsTimeVarying><Keyframes>{keyframes}</Keyframes>") };
        let mute = self.obj("AudioComponentParam", "<Name>Mute</Name>");
        let lvl = self.obj("AudioComponentParam", &format!("<StartKeyframe>-91445760000000000,{level},0,0,0,0,0,0</StartKeyframe>{k}<Name>Level</Name>"));
        self.obj(
            "AudioFilterComponent",
            &format!("<AudioComponent Version=\"3\"><Component Version=\"7\"><Params Version=\"1\">{}</Params></Component></AudioComponent><FilterMatchName>Internal Volume Stereo</FilterMatchName>", list("Param", &[r(mute), r(lvl)])),
        )
    }

    fn chain(&mut self, video: bool, filters: &[u64]) -> u64 {
        let filters: Vec<String> = filters.iter().map(|f| r(*f)).collect();
        let class = if video { "VideoComponentChain" } else { "AudioComponentChain" };
        self.obj(class, &format!("<ComponentChain Version=\"3\"><Components Version=\"1\">{}</Components></ComponentChain>", list("Component", &filters)))
    }

    fn markers(&mut self, json: &[String]) -> u64 {
        let ids: Vec<String> = json
            .iter()
            .map(|j| r(self.obj("Marker", &format!("<DVAMarker>{}</DVAMarker>", j.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;")))))
            .collect();
        self.obj("Markers", &format!("<Markers Version=\"1\">{}</Markers>", list("Marker", &ids)))
    }
}

/// The two sources of the multi-camera sequence, their audio, a separate recorder, a graphic, a
/// multi-camera source sequence, an edit that cuts it, and a Generative Extend result.
fn project() -> Doc {
    let mut d = Doc::default();
    d.file("cam-a", "A001.mov", Some((1920, 1080)), Some(2), 60);
    d.file("cam-b", "B001.mov", Some((1920, 1080)), Some(2), 60);
    d.file("lav", "lav.wav", None, Some(2), 60);
    // a graphic: Premiere-made media whose "path" is a type code
    let gs = d.video_stream(1920, 1080, 3600 * SEC);
    d.uobj("Media", "graphic-media", &format!("<FilePath>1196574294</FilePath><Title>Graphic</Title><Infinite>true</Infinite><VideoStream {}/>", r(gs)));
    let gsrc = d.media_source(true, "graphic-media", 3600 * SEC);
    let gclip = d.clip(true, gsrc, "", "");
    d.master("graphic-master", "Lower Third", &[gclip]);
    d.clip_item("graphic", "Lower Third", "graphic-master", Some(4));

    // multi-camera source: V1 / A1 camera A, V2 / A2 camera B, A3 the recorder
    let cam = |d: &mut Doc, video: bool, master: &str, media: &str| {
        let s = d.media_source(video, media, 60 * SEC);
        d.track_item(video, 0, 60 * SEC, master, s, "", "", "")
    };
    let v1 = cam(&mut d, true, "cam-a-master", "cam-a-media");
    let v2 = cam(&mut d, true, "cam-b-master", "cam-b-media");
    let a1 = cam(&mut d, false, "cam-a-master", "cam-a-media");
    let a2 = cam(&mut d, false, "cam-b-master", "cam-b-media");
    let a3 = cam(&mut d, false, "lav-master", "lav-media");
    let tv1 = d.track(true, &[v1], &[]);
    let tv2 = d.track(true, &[v2], &[]);
    let ta1 = d.track(false, &[a1], &[]);
    let ta2 = d.track(false, &[a2], &[]);
    let ta3 = d.track(false, &[a3], &[]);
    let gv = d.group(true, &[tv1, tv2]);
    let ga = d.group(false, &[ta1, ta2, ta3]);
    d.sequence("mc-seq", "Interview MC", &[gv, ga], "", None, &[]);
    d.sequence_item("mc", "Interview MC", "mc-seq");

    // the edit: a multi-camera clip on angle 2 (index 1), then camera A at 200% and disabled,
    // joined by a dissolve; the multi-camera audio linked to its video; the recorder at +6 dB on
    // its right channel with a fade out; a graphic on V2
    let dissolve = d.transition(true, 4 * SEC + SEC / 2, 5 * SEC + SEC / 2, "Cross Dissolve", SEC / 2);
    let fade = d.transition(false, 7 * SEC, 8 * SEC, "Constant Power", SEC);
    let mcv = d.sequence_source(true, "mc-seq");
    let e1 = d.track_item(
        true,
        0,
        5 * SEC,
        "mc-master",
        mcv,
        &format!("<InPoint>{}</InPoint><IsMulticam>true</IsMulticam><SelectedTrackIndex>1</SelectedTrackIndex>", 2 * SEC),
        "",
        &format!("<TailTransition {}/>", r(dissolve)),
    );
    // camera A's clip: Motion scale animated 100 → 150 over source 1 s → 3 s (clip 0 → 1 s at
    // 200 %), position a quarter in from the left, Opacity 75 %, a Transform and an unsupported effect
    let position = d.param(true, 1, "Position", "0.25:0.5", "");
    let scale = d.param(false, 2, "Scale", "100.", &format!("{SEC},100.,0,0,0,0,0,0;{},150.,5,0,0,0.333,0,0.333;", 3 * SEC));
    let rotation = d.param(false, 5, "Rotation", "10.", "");
    let motion = d.filter("AE.ADBE Motion", "Motion", &[position, scale, rotation]);
    let opacity_p = d.param(false, 1, "Opacity", "75.", "");
    let opacity = d.filter("AE.ADBE Opacity", "Opacity", &[opacity_p]);
    let tscale = d.param(false, 3, "Scale Height", "120.", "");
    let tpos = d.param(true, 2, "Position", "0.5:0.75", "");
    let transform = d.filter("AE.ADBE Geometry2", "Transform", &[tpos, tscale]);
    let glow = d.filter("AE.ADBE Glo2", "Glow", &[]);
    let vchain = d.chain(true, &[motion, opacity, transform, glow]);
    let cav = d.media_source(true, "cam-a-media", 60 * SEC);
    let e2 = d.track_item(
        true,
        5 * SEC,
        8 * SEC,
        "cam-a-master",
        cav,
        &format!("<InPoint>{SEC}</InPoint><PlaybackSpeed>2.</PlaybackSpeed>"),
        "",
        &format!("<IsMuted>true</IsMuted><HeadTransition {}/><ComponentOwner Version=\"1\"><Components {}/></ComponentOwner>", r(dissolve), r(vchain)),
    );
    let gsrc2 = d.media_source(true, "graphic-media", 3600 * SEC);
    let e3 = d.track_item(true, SEC, 3 * SEC, "graphic-master", gsrc2, "", "", "");
    let mca = d.sequence_source(false, "mc-seq");
    let e4 = d.track_item(
        false,
        0,
        5 * SEC,
        "mc-master",
        mca,
        &format!("<InPoint>{}</InPoint><IsMulticam>true</IsMulticam><SelectedTrackIndex>1</SelectedTrackIndex>", 2 * SEC),
        "",
        "",
    );
    // the recorder's volume: −6 dB, then a keyframed dip to −12 dB
    let vol = d.volume("0.0891250938", &format!("0,0.0891250938,0,0;{},0.0446683592,0,0;", 2 * SEC));
    let achain = d.chain(false, &[vol]);
    let right = d.obj("SecondaryContent", "<ChannelIndex>1</ChannelIndex>");
    let lsrc = d.media_source(false, "lav-media", 60 * SEC);
    let e5 = d.track_item(
        false,
        0,
        8 * SEC,
        "lav-master",
        lsrc,
        "",
        &format!(
            "<Gain>2.</Gain><SecondaryContents Version=\"1\"><SecondaryContentItem Index=\"0\"><Second {}/></SecondaryContentItem></SecondaryContents>",
            r(right)
        ),
        &format!("<TailTransition {}/><ComponentOwner Version=\"1\"><Components {}/></ComponentOwner>", r(fade), r(achain)),
    );
    let ev1 = d.track(true, &[e1, e2], &[dissolve]);
    let ev2 = d.track(true, &[e3], &[]);
    let ea1 = d.track(false, &[e4], &[]);
    let ea2 = d.track(false, &[e5], &[fade]);
    let gv = d.group(true, &[ev1, ev2]);
    let ga = d.group(false, &[ea1, ea2]);
    let link = d.link(&[e1, e4]);
    let markers = d.markers(&[format!(
        r#"{{"DVAMarker":{{"mComment":"check focus","mCuePointList":[{{"mKey":"marker_guid","mValue":"x"}},{{"mKey":"keywordExtDVAv1","mValue":"{{\"color\":255}}"}}],"mDuration":{{"ticks":0}},"mName":"Cue","mStartTime":{{"ticks":{SEC}}},"mType":"Comment"}}}}"#
    )]);
    let props = format!(
        "<MZ.ZeroPoint>{}</MZ.ZeroPoint><MZ.InPoint>{SEC}</MZ.InPoint><MZ.OutPoint>{NO_TIME}</MZ.OutPoint><MZ.WorkInPoint>0</MZ.WorkInPoint><MZ.WorkOutPoint>{}</MZ.WorkOutPoint>",
        3600 * SEC,
        6 * SEC
    );
    d.sequence("edit-seq", "Edit", &[gv, ga], &props, Some(markers), &[link]);
    d.sequence_item("edit", "Edit", "edit-seq");

    // Generative Extend: the sequence it was made in, the clip it extends (a second project item
    // of the recorder's file) and the media it rendered
    let es = d.media_source(false, "lav-media", 60 * SEC);
    let ei = d.track_item(false, 0, 60 * SEC, "lav-master", es, "", "", "");
    let et = d.track(false, &[ei], &[]);
    let eg = d.group(false, &[et]);
    let ev = d.group(true, &[]);
    d.sequence("ext-seq", "lav-extended", &[ev, eg], "", None, &[]);
    d.sequence_item("ext", "lav-extended", "ext-seq");
    let src = d.media_source(false, "lav-media", 60 * SEC);
    let c = d.clip(false, src, "", "");
    d.master("lav2-master", "lav.wav", &[c]);
    d.clip_item("lav2", "lav.wav", "lav2-master", None);
    d.file("rendered", "lav-extended.wav", None, Some(2), 62);
    d.uobj("CompoundProjectItem", "compound", &format!("<ProjectItem Version=\"1\"></ProjectItem><EditingProjectItem {}/><MediaProjectItem {}/><AdditionalProjectItemsVector Version=\"1\">{}</AdditionalProjectItemsVector>", u("ext"), u("lav2"), list("AdditionalProjectItem", &[u("rendered")])));

    d.bin("BinProjectItem", "footage", "Footage", &["cam-a", "cam-b", "lav", "graphic", "compound"]);
    d.bin("BinProjectItem", "sequences", "Sequences", &["mc", "edit"]);
    d.bin("RootProjectItem", "root", "Root", &["footage", "sequences"]);
    d
}

fn import(doc: &str) -> (Imported, Vec<prproj::Relink>, Report) {
    prproj::import(doc.as_bytes(), &ImportOptions { base_dir: Some("/Projects/Show".into()), ..Default::default() }).unwrap()
}

fn find<'p>(p: &'p Project, name: &str) -> (ItemId, &'p Sequence) {
    p.items.values().find(|i| i.name == name).and_then(|i| i.as_sequence().map(|s| (i.id, s))).unwrap()
}

fn named(p: &Project, name: &str) -> Vec<ItemId> {
    p.items.values().filter(|i| i.name == name).map(|i| i.id).collect()
}

fn bin_names(p: &Project) -> Vec<String> {
    p.root.children.iter().filter_map(|c| if let BinEntry::Bin(b) = c { Some(b.name.clone()) } else { None }).collect()
}

/// A gzip member of `data` in stored (uncompressed) deflate blocks, as a stand-in for a saved
/// project: the reader inflates either.
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];
    let chunks: Vec<&[u8]> = data.chunks(65_535).collect();
    for (i, c) in chunks.iter().enumerate() {
        gz.push(u8::from(i + 1 == chunks.len()));
        let n = c.len() as u16;
        gz.extend_from_slice(&n.to_le_bytes());
        gz.extend_from_slice(&(!n).to_le_bytes());
        gz.extend_from_slice(c);
    }
    gz.extend_from_slice(&[0, 0, 0, 0]);
    gz.extend_from_slice(&(data.len() as u32).to_le_bytes());
    gz
}

#[test]
fn bins_media_and_sequences() {
    let (imported, relinks, report) = import(&project().xml());
    let p = &imported.project;
    assert_eq!(bin_names(p), ["Footage", "Sequences"], "{report}");
    for name in ["A001.mov", "B001.mov"] {
        let ids = named(p, name);
        assert_eq!(ids.len(), 1, "{name}");
        let item = p.item(ids[0]).unwrap();
        let ItemKind::Media(m) = &item.kind else { panic!("{name} is media") };
        assert_eq!(m.media, MediaRef::File { path: format!("/Volumes/Shoot/{name}") });
        assert!(m.info.video.is_some() && m.info.audio().is_some(), "{name}");
    }
    // the project-relative path is offered for relinking
    assert!(relinks.iter().any(|l| l.relative == "/Projects/Shoot/A001.mov"), "{relinks:?}");
    // the graphic keeps its canvas; its text is reported
    let g = p.item(named(p, "Lower Third")[0]).unwrap();
    assert!(matches!(g.kind, ItemKind::Graphic { width: 1920, height: 1080, .. }));
    assert_eq!(g.label, filmcraft_project::Label::ALL[4]);
    assert!(report.to_string().contains("graphic"), "{report}");
    // Generative Extend: the rendered file and the sequence come in; the extended clip is the
    // recorder file already imported, not a second item
    assert_eq!(named(p, "lav.wav").len(), 1);
    assert_eq!(named(p, "lav-extended.wav").len(), 1);
    assert_eq!(named(p, "lav-extended").len(), 1);
    // the sequences in project order (the Footage bin, holding the extend sequence, comes first)
    let order: Vec<&str> = imported.sequences.iter().filter_map(|id| p.item(*id)).map(|i| i.name.as_str()).collect();
    assert_eq!(order, ["lav-extended", "Interview MC", "Edit"]);
}

#[test]
fn sequence_settings_marks_and_markers() {
    let (imported, _, _) = import(&project().xml());
    let (_, edit) = find(&imported.project, "Edit");
    assert_eq!((edit.settings.width, edit.settings.height), (1920, 1080));
    assert_eq!(edit.settings.frame_rate, filmcraft_time::FrameRate::FPS_29_97);
    assert_eq!(edit.settings.sample_rate, 48_000);
    // 01:00:00;00 is 107 892 frames at 29.97
    assert_eq!(edit.start_timecode, 107_892);
    assert_eq!(edit.mark_in, Some(Tick(SEC)));
    assert_eq!(edit.mark_out, None);
    assert_eq!(edit.work_area.map(|w| (w.start, w.duration)), Some((Tick(0), Tick(6 * SEC))));
    assert_eq!(edit.markers.len(), 1);
    let m = &edit.markers[0];
    assert_eq!((m.name.as_str(), m.comment.as_str(), m.start), ("Cue", "check focus", Tick(SEC)));
    assert_eq!(m.custom_color, Some(filmcraft_project::HexColor([255, 0, 0])));
}

fn clips(s: &Sequence, video: bool, track: usize) -> Vec<&TrackItem> {
    let tracks = if video { &s.video_tracks } else { &s.audio_tracks };
    tracks.get(track).map(|t| t.items.iter().collect()).unwrap_or_default()
}

#[test]
fn clips_timing_speed_links_and_transitions() {
    let (imported, _, report) = import(&project().xml());
    let p = &imported.project;
    let (mc_id, _) = find(p, "Interview MC");
    let (_, edit) = find(p, "Edit");
    assert_eq!((edit.video_tracks.len(), edit.audio_tracks.len()), (2, 2));
    let v1 = clips(edit, true, 0);
    assert_eq!(v1.len(), 2);
    // the multi-camera clip: nested source, source In 2 s, angle index 1
    assert_eq!((v1[0].item, v1[0].start, v1[0].duration, v1[0].source_in), (mc_id, Tick(0), Tick(5 * SEC), Tick(2 * SEC)));
    assert_eq!(v1[0].multicam.map(|m| m.angle), Some(1));
    // camera A at 200 %, disabled
    assert_eq!((v1[1].start, v1[1].duration, v1[1].source_in), (Tick(5 * SEC), Tick(3 * SEC), Tick(SEC)));
    assert_eq!(v1[1].speed, 2.0);
    assert!(!v1[1].enabled && v1[0].enabled);
    // the dissolve joins them, centred on the cut
    let t = &edit.video_tracks[0].transitions;
    assert_eq!(t.len(), 1, "{report}");
    assert_eq!((t[0].from, t[0].to), (Some(v1[0].id), Some(v1[1].id)));
    assert_eq!((t[0].start, t[0].duration), (Tick(4 * SEC + SEC / 2), Tick(SEC)));
    assert_eq!(t[0].align, filmcraft_project::TransitionAlign::CenterAtCut);
    // the graphic on V2
    let v2 = clips(edit, true, 1);
    assert_eq!((v2[0].start, v2[0].duration), (Tick(SEC), Tick(2 * SEC)));
    // multi-camera audio linked to its video
    let a1 = clips(edit, false, 0);
    assert_eq!(a1[0].multicam.map(|m| m.angle), Some(1));
    assert!(a1[0].link.is_some());
    assert_eq!(a1[0].link, v1[0].link);
    assert!(v1[1].link.is_none());
    // the recorder: +6 dB, right channel only, fading out at its end
    let a2 = clips(edit, false, 1);
    assert!((a2[0].gain_db - 6.02).abs() < 0.01, "{}", a2[0].gain_db);
    assert_eq!(a2[0].source_channels, [1]);
    let fade = &edit.audio_tracks[1].transitions;
    assert_eq!(fade.len(), 1);
    assert_eq!((fade[0].from, fade[0].to, fade[0].align), (Some(a2[0].id), None, filmcraft_project::TransitionAlign::EndAtCut));
}

fn float(ti: &TrackItem, effect: &str, param: &str) -> Option<f64> {
    ti.effect(effect).and_then(|e| e.param(param)).and_then(|p| p.value.as_f64())
}

#[test]
fn motion_opacity_transform_and_volume() {
    let (imported, _, report) = import(&project().xml());
    let (_, edit) = find(&imported.project, "Edit");
    let cam = clips(edit, true, 0)[1];
    // keyframes move from source time to clip time: (source − In 1 s) / 200 %
    let scale = cam.effect("motion").and_then(|e| e.param("scale")).unwrap();
    let keys: Vec<(Tick, f64)> = scale.keyframes.iter().map(|k| (k.time, k.value.as_f64().unwrap())).collect();
    assert_eq!(keys, [(Tick(0), 100.0), (Tick(SEC), 150.0)]);
    assert_eq!(scale.keyframes[1].interp, filmcraft_project::Interpolation::Bezier);
    // Position is a fraction of the 1920×1080 sequence frame
    let pos = cam.effect("motion").and_then(|e| e.param("position")).and_then(|p| p.value.as_vec2()).unwrap();
    assert_eq!((pos.x, pos.y), (480.0, 540.0));
    assert_eq!(float(cam, "motion", "rotation"), Some(10.0));
    assert_eq!(float(cam, "opacity", "opacity"), Some(75.0));
    // Transform works in the clip's own picture
    assert_eq!(float(cam, "transform", "scale_height"), Some(120.0));
    let tpos = cam.effect("transform").and_then(|e| e.param("position")).and_then(|p| p.value.as_vec2()).unwrap();
    assert_eq!((tpos.x, tpos.y), (960.0, 810.0));
    assert!(report.to_string().contains("effect \"Glow\" is not supported"), "{report}");
    // the multi-camera clip kept default Motion
    assert_eq!(float(clips(edit, true, 0)[0], "motion", "scale"), Some(100.0));
    // Volume: Premiere's level is linear with 0 dB at 10^(−15/20)
    let lav = clips(edit, false, 1)[0];
    let level = lav.effect("volume").and_then(|e| e.param("level")).unwrap();
    let keys: Vec<(Tick, f64)> = level.keyframes.iter().map(|k| (k.time, (k.value.as_f64().unwrap() * 100.0).round() / 100.0)).collect();
    assert_eq!(keys, [(Tick(0), -6.0), (Tick(2 * SEC), -12.0)]);
}

#[test]
fn multicam_source_cameras_and_audio() {
    let (imported, _, report) = import(&project().xml());
    let (_, mc) = find(&imported.project, "Interview MC");
    let m = mc.multicam.as_ref().expect("the sequence the multi-camera clips play is a multi-camera source");
    assert_eq!(m.cameras.len(), 2);
    assert_eq!(m.cameras[0].name, "A001");
    assert_eq!(m.cameras[0].video_track, Some(mc.video_tracks[0].id));
    assert_eq!(m.cameras[0].audio_tracks, [mc.audio_tracks[0].id]);
    assert_eq!(m.cameras[1].audio_tracks, [mc.audio_tracks[1].id]);
    // the edit switches audio with the angle; the recorder belongs to no camera
    assert_eq!(m.audio, MulticamAudio::SwitchAudio);
    assert!(report.to_string().contains("1 audio track(s) belong to no camera"), "{report}");
    let (_, edit) = find(&imported.project, "Edit");
    assert!(edit.multicam.is_none());
}

#[test]
fn compressed_and_plain_documents_import_alike() {
    let xml = project().xml();
    let gz = gzip_stored(xml.as_bytes());
    assert_eq!(detect(&gz, Some("prproj")), Some(Format::Prproj));
    assert_eq!(detect(&gz, None), Some(Format::Prproj));
    assert_eq!(detect(xml.as_bytes(), Some("xml")), Some(Format::Prproj));
    let (a, _) = import_with(&gz, Format::Prproj, &ImportOptions::default()).unwrap();
    let (b, _) = import_with(xml.as_bytes(), Format::Prproj, &ImportOptions::default()).unwrap();
    assert_eq!(a.project.items.len(), b.project.items.len());
    assert_eq!(a.sequences.len(), 3);
    // an FCP7 XML is not mistaken for a Premiere project
    assert_ne!(detect(b"<?xml version=\"1.0\"?><xmeml version=\"4\"></xmeml>", Some("xml")), Some(Format::Prproj));
}

#[test]
fn exporting_a_premiere_project_is_refused() {
    let (imported, _, _) = import(&project().xml());
    let seq = imported.sequences[0];
    assert!(filmcraft_interchange::export(&imported.project, seq, Format::Prproj, &Default::default()).is_err());
}

#[test]
fn nested_sequences_that_contain_each_other_are_broken_up() {
    let mut d = Doc::default();
    let nest = |d: &mut Doc, seq: &str, other: &str, master: &str| {
        let src = d.sequence_source(true, other);
        let ti = d.track_item(true, 0, SEC, master, src, "", "", "");
        let t = d.track(true, &[ti], &[]);
        let g = d.group(true, &[t]);
        d.sequence(seq, seq, &[g], "", None, &[]);
    };
    nest(&mut d, "a", "b", "b-item-master");
    nest(&mut d, "b", "a", "a-item-master");
    d.sequence_item("a-item", "a", "a");
    d.sequence_item("b-item", "b", "b");
    d.bin("RootProjectItem", "root", "Root", &["a-item", "b-item"]);
    let (imported, _, report) = import(&d.xml());
    let p = &imported.project;
    let nested = |id: ItemId| p.sequence(id).map(|s| s.all_tracks().map(|t| t.items.len()).sum::<usize>()).unwrap_or(0);
    let (a, _) = find(p, "a");
    let (b, _) = find(p, "b");
    assert!(nested(a) + nested(b) < 2, "{report}");
    assert!(report.to_string().contains("contain each other"), "{report}");
}

#[test]
fn deep_bins_and_dangling_references_are_reported_not_fatal() {
    let mut d = Doc::default();
    d.file("clip", "shot.mov", Some((1280, 720)), None, 5);
    let mut child = "clip".to_string();
    for depth in 0..300 {
        let uid = format!("bin{depth}");
        d.bin("BinProjectItem", &uid, "deep", &[&child]);
        child = uid;
    }
    // a track item whose sub-clip is missing, a clip of a master clip that isn't there
    let gone = d.obj(
        "VideoClipTrackItem",
        &format!(
            "<ClipTrackItem Version=\"1\"><TrackItem Version=\"1\"><Start>0</Start><End>{SEC}</End></TrackItem><SubClip ObjectRef=\"999999\"/></ClipTrackItem>"
        ),
    );
    let src = d.media_source(true, "nowhere", SEC);
    let orphan = d.track_item(true, SEC, 2 * SEC, "no-master", src, "", "", "");
    let t = d.track(true, &[gone, orphan, 424_242], &[]);
    let g = d.group(true, &[t]);
    d.sequence("s", "s", &[g, 777_777], "", None, &[]);
    d.bin("RootProjectItem", "root", "Root", &[&child, "missing-item", "s"]);
    let (imported, _, report) = import(&d.xml());
    let text = report.to_string();
    assert!(text.contains("deeper than 128"), "{text}");
    assert!(text.contains("not in the project"), "{text}");
    let (_, s) = find(&imported.project, "s");
    assert!(s.video_tracks[0].items.is_empty(), "{text}");
}

#[test]
fn truncated_and_damaged_documents_never_panic() {
    let xml = project().xml();
    let bytes = xml.as_bytes();
    // every cut point (stepped), and the same cuts compressed
    let mut step = 0;
    while step < bytes.len() {
        let cut = &bytes[..step];
        let r = std::panic::catch_unwind(|| prproj::import(cut, &ImportOptions::default()));
        assert!(r.is_ok(), "panicked on the document cut at {step}");
        let gz = gzip_stored(cut);
        let r = std::panic::catch_unwind(|| prproj::import(&gz[..gz.len().saturating_sub(step % 23)], &ImportOptions::default()));
        assert!(r.is_ok(), "panicked on the compressed document cut at {step}");
        step += 37;
    }
    // a cut inside the objects is reported, and what came before it imports
    let half = &bytes[..bytes.len() * 3 / 4];
    if let Ok((_, _, report)) = prproj::import(half, &ImportOptions::default()) {
        assert!(report.to_string().contains("truncated"), "{report}");
    }
    // flipped bytes: markup, digits and invalid UTF-8 in the wrong places
    let mut seed = 0x2545_f491_u32;
    for _ in 0..1500 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let mut doc = bytes.to_vec();
        let at = seed as usize % doc.len();
        doc[at] = [b'<', b'>', b'"', b'/', b'9', b'-', 0xff, b' '][(seed >> 24) as usize % 8];
        let r = std::panic::catch_unwind(|| prproj::import(&doc, &ImportOptions::default()));
        assert!(r.is_ok(), "panicked with byte {at} changed");
    }
}

#[test]
fn hostile_numbers_are_clamped_or_reported() {
    let mut d = Doc::default();
    d.file("clip", "shot.mov", Some((99_999, 99_999)), Some(2), 5);
    let src = d.media_source(true, "clip-media", SEC);
    let a = d.track_item(true, -5, SEC, "clip-master", src, "<PlaybackSpeed>0</PlaybackSpeed>", "", "");
    let b = d.track_item(
        true,
        SEC,
        i64::MAX,
        "clip-master",
        src,
        &format!("<InPoint>{}</InPoint><PlaybackSpeed>1e300</PlaybackSpeed>", i64::MIN),
        "<Gain>-3</Gain>",
        "",
    );
    let c = d.track_item(true, 3 * SEC, 2 * SEC, "clip-master", src, "", "", "");
    let t = d.track(true, &[a, b, c], &[]);
    let g = d.obj(
        "VideoTrackGroup",
        &format!(
            "<TrackGroup Version=\"1\"><Tracks Version=\"1\">{}</Tracks><FrameRate>-1</FrameRate></TrackGroup><FrameRect>0,0,0,99999999999</FrameRect>",
            list("Track", &[r(t)])
        ),
    );
    let markers = d.markers(&[
        "{\"DVAMarker\":{\"mStartTime\":{\"ticks\":-99},\"mCuePointList\":[{\"mValue\":\"{\\\"color\\\":99999999999}\"}]}}".to_string(),
        "not json".into(),
    ]);
    d.sequence("s", "s", &[g], &format!("<MZ.ZeroPoint>{}</MZ.ZeroPoint>", i64::MAX), Some(markers), &[]);
    d.bin("RootProjectItem", "root", "Root", &["clip", "s"]);
    let (imported, _, report) = import(&d.xml());
    let (_, s) = find(&imported.project, "s");
    assert!(s.settings.validate().is_ok(), "{report}");
    for ti in s.all_tracks().flat_map(|t| &t.items) {
        assert!(ti.start >= Tick(0) && ti.duration > Tick(0) && ti.speed > 0.0 && ti.speed.is_finite(), "{ti:?}");
    }
    assert!(report.to_string().contains("invalid timeline range"), "{report}");
    assert!(s.markers.iter().all(|m| m.start >= Tick(0)));
}

#[test]
fn not_a_premiere_project() {
    for doc in [&b""[..], b"<xmeml/>", b"<?xml version=\"1.0\"?><PremiereData Version=\"3\"></PremiereData>", &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 0xff, 0xff]]
    {
        assert!(prproj::import(doc, &ImportOptions::default()).is_err(), "{:?}", String::from_utf8_lossy(doc));
    }
    // a forged gzip trailer asking for 4 GB is refused before allocating
    let mut gz = gzip_stored(b"<PremiereData/>");
    let n = gz.len();
    gz[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(prproj::import(&gz, &ImportOptions::default()).is_err());
}

/// Bin tree, one line per entry, for eyeballing a real import.
fn tree(p: &Project) -> String {
    fn walk(p: &Project, b: &filmcraft_project::Bin, depth: usize, out: &mut String) {
        for c in &b.children {
            match c {
                BinEntry::Bin(sub) => {
                    out.push_str(&format!("{}[{}]\n", "  ".repeat(depth), sub.name));
                    walk(p, sub, depth + 1, out);
                }
                BinEntry::Item(id) => {
                    let Some(it) = p.item(*id) else { continue };
                    let kind = match &it.kind {
                        ItemKind::Sequence(s) => format!(
                            "sequence {}x{} {} V{:?} A{:?}{}",
                            s.settings.width,
                            s.settings.height,
                            s.settings.frame_rate,
                            s.video_tracks.iter().map(|t| t.items.len()).collect::<Vec<_>>(),
                            s.audio_tracks.iter().map(|t| t.items.len()).collect::<Vec<_>>(),
                            s.multicam.as_ref().map(|m| format!(" multicam {} cams {:?}", m.cameras.len(), m.audio)).unwrap_or_default()
                        ),
                        ItemKind::Media(m) => format!("media {:?}", m.media),
                        _ => "other".into(),
                    };
                    out.push_str(&format!("{}- {} ({kind})\n", "  ".repeat(depth), it.name));
                }
            }
        }
    }
    let mut s = String::new();
    walk(p, &p.root, 0, &mut s);
    s
}

#[test]
fn real_project_from_env() {
    let Ok(path) = std::env::var("FILMCRAFT_PRPROJ_SAMPLE") else { return };
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(detect(&bytes, Some("prproj")), Some(Format::Prproj));
    let base = std::path::Path::new(&path).parent().map(|d| d.to_string_lossy().into_owned());
    let t = std::time::Instant::now();
    let (imported, relinks, report) = prproj::import(&bytes, &ImportOptions { base_dir: base, ..Default::default() }).unwrap();
    let elapsed = t.elapsed();
    let p = &imported.project;
    let sequences = p.items.values().filter(|i| i.as_sequence().is_some()).count();
    assert!(sequences > 0);
    assert!(!imported.sequences.is_empty());
    for item in p.items.values() {
        if let Some(s) = item.as_sequence() {
            assert!(s.settings.validate().is_ok());
        }
    }
    // every multi-camera source is cut by timeline clips with Multi-Camera on and an angle that
    // names one of its cameras (the Multi-Camera view shows angles only for such clips)
    let mut multicam_clips = 0usize;
    for (id, cameras) in p.items.values().filter_map(|i| i.as_sequence().and_then(|s| s.multicam.as_ref()).map(|m| (i.id, m.cameras.len()))) {
        let cuts: Vec<&filmcraft_project::MulticamSel> = p
            .items
            .values()
            .filter_map(|i| i.as_sequence())
            .flat_map(|s| s.all_tracks().flat_map(|t| &t.items))
            .filter(|ti| ti.item == id)
            .filter_map(|ti| ti.multicam.as_ref())
            .collect();
        assert!(!cuts.is_empty() && cuts.iter().all(|m| m.enabled && (m.angle as usize) < cameras), "{cameras} cameras");
        multicam_clips += cuts.len();
    }
    if std::env::var("FILMCRAFT_PRPROJ_DUMP").is_ok() {
        println!(
            "import took {elapsed:?}; {} items, {sequences} sequences, {} relink candidates, {multicam_clips} multi-camera clips",
            p.items.len(),
            relinks.len()
        );
        print!("{}", tree(p));
        print!("{report}");
        // effect counts over every clip
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for seq in p.items.values().filter_map(|i| i.as_sequence()) {
            for ti in seq.all_tracks().flat_map(|t| &t.items) {
                for e in &ti.effects {
                    let changed = e.params.iter().any(|(id, v)| {
                        v.is_animated()
                            || filmcraft_project::find_effect(&e.effect)
                                .and_then(|d| d.param(id))
                                .is_some_and(|d| d.default != v.value && !matches!(d.default, filmcraft_project::ParamValue::Vec2(_)))
                    }) || e.params.iter().any(|(_, v)| v.value.as_vec2().is_some_and(|p| !p.x.is_nan()));
                    if changed {
                        *counts.entry(e.effect.as_str()).or_default() += 1;
                    }
                    if e.params.values().any(|v| v.is_animated()) {
                        *counts.entry("(animated)").or_default() += 1;
                    }
                }
            }
        }
        println!("clips with changed effects: {counts:?}");
    }
}
