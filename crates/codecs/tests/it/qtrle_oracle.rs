//! QuickTime Animation (`rle `) in MOV against ffmpeg: stream info, then frames in play order and
//! in scrub order (seeks into the middle of a run of delta frames) equal ffmpeg's decode, alpha
//! included. Also with the `stss` box hidden, where every sample looks like a sync sample and the
//! decoder has to say which ones decoding can start from.

use crate::common;

use common::*;
use filmcraft_frame::PixelData;
use filmcraft_media::FrameRequest;

const W: usize = 96;
const H: usize = 54;
const N: usize = 30;

fn check_frames(src: &filmcraft_media::SharedSource, want: &[u8], order: &[usize], label: &str) {
    let rate = src.info().video.as_ref().unwrap().frame_rate;
    let per = W * H * 4;
    for &i in order {
        let f = src.video_frame(FrameRequest::full(rate.tick_of(i as i64))).unwrap();
        assert_eq!((f.width as usize, f.height as usize), (W, H));
        let PixelData::Rgba8(px) = &f.data else { panic!("{label}: frame {i} is not RGBA8") };
        assert!(px.as_slice() == &want[i * per..(i + 1) * per], "{label}: frame {i} differs from ffmpeg");
    }
}

#[test]
fn qtrle_mov_plays_and_scrubs_like_ffmpeg() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let src_graph = format!("testsrc2=s={W}x{H}:r=25,format=argb,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(X,N*3),0,mod(X*5+Y,256))'");
    let n = N.to_string();
    let Some(f) = fixture(&ff, "qtrle_argb_alpha.mov", &["-f", "lavfi", "-i", &src_graph, "-frames:v", &n, "-c:v", "qtrle", "-g", "8", "-f", "mov"]) else {
        panic!("could not write the qtrle fixture")
    };
    let want = ffmpeg_frames(&ff, &f, "rgba");
    assert_eq!(want.len(), W * H * 4 * N);
    let src = filmcraft_codecs::open_bytes("qtrle_argb_alpha.mov", bytes(&f)).unwrap();
    let v = src.info().video.as_ref().unwrap();
    assert_eq!(v.codec, "QuickTime Animation");
    assert_eq!(v.pixel_format, "ARGB 32-bit");
    assert!(v.has_alpha);
    assert!(
        want.as_chunks::<4>().0.iter().any(|p| p[3] == 0) && want.as_chunks::<4>().0.iter().any(|p| p[3] == 255),
        "the fixture has transparent and opaque pixels"
    );
    let play: Vec<usize> = (0..N).collect();
    check_frames(&src, &want, &play, "play");
    let scrub = [29, 3, 17, 9, 0, 22, 11, 11, 5, 28, 15, 1];
    let fresh = filmcraft_codecs::open_bytes("qtrle_argb_alpha.mov", bytes(&f)).unwrap();
    check_frames(&fresh, &want, &scrub, "scrub");

    // hide `stss` (rename it to `free`): every sample is then a sync sample to the container
    let mut bytes_nostss = std::fs::read(&f).unwrap();
    let at = bytes_nostss.windows(4).position(|w| w == b"stss").expect("ffmpeg writes stss for delta frames");
    bytes_nostss[at..at + 4].copy_from_slice(b"free");
    let blind = filmcraft_codecs::open_bytes("qtrle_nostss.mov", bytes_nostss.into()).unwrap();
    check_frames(&blind, &want, &scrub, "scrub without stss");
}

/// Every depth ffmpeg encodes opens with its label and decodes its first frame.
#[test]
fn qtrle_depth_labels() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    for (pix, label, alpha) in [("rgb24", "RGB 24-bit", false), ("rgb555be", "RGB 15-bit", false), ("gray", "8-bit grey", false), ("argb", "ARGB 32-bit", true)]
    {
        let name = format!("qtrle_label_{pix}.mov");
        let Some(f) =
            fixture(&ff, &name, &["-f", "lavfi", "-i", &format!("testsrc2=s=64x36:r=25,format={pix}"), "-frames:v", "3", "-c:v", "qtrle", "-f", "mov"])
        else {
            panic!("could not write {name}")
        };
        let src = filmcraft_codecs::open_bytes(&name, bytes(&f)).unwrap();
        let v = src.info().video.as_ref().unwrap();
        assert_eq!((v.codec.as_str(), v.pixel_format.as_str(), v.has_alpha), ("QuickTime Animation", label, alpha), "{pix}");
        let want = ffmpeg_frames(&ff, &f, "rgba");
        let got = src.video_frame(FrameRequest::full(filmcraft_time::Tick::ZERO)).unwrap();
        let PixelData::Rgba8(px) = &got.data else { panic!("{pix}: not RGBA8") };
        assert!(px.as_slice() == &want[..64 * 36 * 4], "{pix}: first frame differs from ffmpeg");
    }
}
