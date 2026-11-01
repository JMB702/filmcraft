//! Decoder output against ffmpeg's QuickTime Animation decoder (oracle only; skipped without
//! ffmpeg). Every frame must match exactly.

use crate::common::*;
use filmcraft_qtrle::{Decoder, color_table};
use std::path::Path;

fn specs() -> Vec<Spec> {
    // animated alpha: a ramp across the picture that moves with time
    let alpha = "format=argb,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='mod(X*3+Y+N*17,256)'";
    let mut v = Vec::new();
    for pix in ["rgb24", "rgb555be", "argb", "gray"] {
        let tag = pix.trim_end_matches("be");
        v.push(Spec { name: format!("testsrc2_160x90_{tag}"), lavfi: format!("testsrc2=s=160x90:r=25,format={pix}"), frames: 24, gop: 10 });
        // ffmpeg's encoder needs a width that is a multiple of 4 for grey
        let w = if pix == "gray" { 36 } else { 37 };
        v.push(Spec { name: format!("mandel_odd_{w}x23_{tag}"), lavfi: format!("mandelbrot=s={w}x23:r=25,format={pix}"), frames: 6, gop: 4 });
    }
    v.push(Spec { name: "testsrc2_320x180_argb_alpha".into(), lavfi: format!("testsrc2=s=320x180:r=25,{alpha}"), frames: 12, gop: 5 });
    v.push(Spec { name: "still_bars_128x72_argb".into(), lavfi: "smptebars=s=128x72:r=25,format=argb".into(), frames: 5, gop: 100 });
    v
}

/// Decode every sample of `mov` and compare with ffmpeg's frames.
fn check(ff: &Path, mov: &Path, name: &str) -> usize {
    let (w, h, depth, entry) = sample_entry(mov);
    let table = color_table(&entry);
    let mut d = Decoder::new(w, h, depth, table.as_deref()).unwrap();
    let pkts = packets(ff, mov);
    let want = reference(ff, mov);
    let per = (w * h * 4) as usize;
    assert_eq!(want.len(), per * pkts.len(), "{name}: frame count");
    for (i, p) in pkts.iter().enumerate() {
        let got = d.decode(p).unwrap_or_else(|e| panic!("{name}: frame {i}: {e}"));
        let exp = &want[i * per..(i + 1) * per];
        if got.as_slice() != exp {
            let k = got.iter().zip(exp).position(|(a, b)| a != b).unwrap();
            let (x, y) = ((k / 4) as u32 % w, (k / 4) as u32 / w);
            panic!("{name}: frame {i} differs first at ({x},{y}): {:?} vs ffmpeg {:?}", &got[k / 4 * 4..k / 4 * 4 + 4], &exp[k / 4 * 4..k / 4 * 4 + 4]);
        }
    }
    pkts.len()
}

#[test]
fn ffmpeg_encoded_clips_match_exactly() {
    let Some(ff) = ffmpeg() else { return };
    for spec in specs() {
        let mov = make(&ff, &spec);
        let (_, _, depth, _) = sample_entry(&mov);
        let n = check(&ff, &mov, &spec.name);
        println!("{:<30} depth {depth:>2}: {n} frames exact", spec.name);
    }
}

/// Indexed depths (which ffmpeg can't encode): random well-formed streams with key frames, partial
/// updates, skips and repeats, in a hand-built MOV, with default and stored colour tables.
#[test]
fn indexed_streams_match_exactly() {
    let Some(ff) = ffmpeg() else { return };
    let dir = fixture_dir();
    let table: Vec<[u8; 3]> = (0..256u32).map(|i| [(i * 7) as u8, (255 - i) as u8, (i * i / 3) as u8]).collect();
    // 1-bit is left out: ffmpeg's 1-bit decoding rejects simple well-formed lines (see the README)
    let cases: [(u16, Option<&[[u8; 3]]>); 8] =
        [(2, None), (4, None), (8, None), (34, None), (36, None), (40, None), (8, Some(&table)), (4, Some(&table[..16]))];
    for (n, (depth, table)) in cases.into_iter().enumerate() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ n as u64);
        let (w, h) = (96usize, 20usize);
        let samples: Vec<Vec<u8>> = (0..8).map(|i| random_sample(&mut rng, w, h, depth, i % 4 == 0)).collect();
        let name = format!("crafted_depth{depth}{}", if table.is_some() { "_ctab" } else { "" });
        let mov = dir.join(format!("{name}.mov"));
        write_mov(&mov, w as u16, h as u16, depth, &samples, table);
        check(&ff, &mov, &name);
        println!("{name}: 8 frames exact");
    }
}

/// A real file (for example a 4K Premiere preview render), never committed: set
/// `FILMCRAFT_QTRLE_SAMPLE=/path/to/file.mov` (and optionally `FILMCRAFT_QTRLE_FRAMES=n`, default
/// 3). Compares the first frames with ffmpeg and prints the decode time per frame.
#[test]
fn real_file_sample() {
    let Some(path) = std::env::var_os("FILMCRAFT_QTRLE_SAMPLE") else { return };
    let Some(ff) = ffmpeg() else { return };
    let frames: usize = std::env::var("FILMCRAFT_QTRLE_FRAMES").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
    let mov = Path::new(&path);
    let (w, h, depth, entry) = sample_entry(mov);
    let mut d = Decoder::new(w, h, depth, color_table(&entry).as_deref()).unwrap();
    let n = frames.to_string();
    // the first `frames` samples and ffmpeg's frames, without reading the whole file
    let out = std::process::Command::new(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(mov)
        .args(["-map", "0:v:0", "-frames:v", &n, "-c", "copy", "-f", "rawvideo", "-"])
        .output()
        .unwrap();
    let mut pkts = Vec::new();
    let mut data = &out.stdout[..];
    while data.len() >= 4 {
        let s = (u32::from_be_bytes(data[..4].try_into().unwrap()) & 0x3fff_ffff) as usize;
        pkts.push(data[..s].to_vec());
        data = &data[s..];
    }
    let want = std::process::Command::new(&ff)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(mov)
        .args(["-map", "0:v:0", "-frames:v", &n, "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .output()
        .unwrap()
        .stdout;
    let per = (w * h * 4) as usize;
    for (i, p) in pkts.iter().enumerate() {
        let t = std::time::Instant::now();
        let key = d.is_key_frame(p);
        let key_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = std::time::Instant::now();
        let got = d.decode(p).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let exact = got.as_slice() == &want[i * per..(i + 1) * per];
        let opaque = got.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
        println!(
            "{w}x{h} depth {depth} frame {i}: {} bytes, {ms:.1} ms, exact {exact}, all opaque {opaque}, key frame {key} ({key_ms:.1} ms to tell)",
            p.len()
        );
        assert!(exact, "frame {i} differs from ffmpeg");
    }
}
