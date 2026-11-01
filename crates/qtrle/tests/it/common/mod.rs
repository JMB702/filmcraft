//! Shared helpers. ffmpeg is used only as an external fixture generator and reference decoder
//! (never linked).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// ffmpeg, or `None` after printing `SKIPPED` (see `filmcraft_testkit::oracle`).
pub fn ffmpeg() -> Option<PathBuf> {
    filmcraft_testkit::ffmpeg_or_skip("qtrle oracle")
}

pub fn fixture_dir() -> PathBuf {
    filmcraft_testkit::fixtures_dir("qtrle")
}

fn run(ff: &Path, args: &[&str]) {
    let out = Command::new(ff).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).output().expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

/// An ffmpeg-encoded fixture: a lavfi source encoded with `-c:v qtrle`.
pub struct Spec {
    pub name: String,
    /// lavfi graph, ending in the input pixel format (`rgb24`, `rgb555be`, `argb`, `gray`).
    pub lavfi: String,
    pub frames: u32,
    /// Key frame interval.
    pub gop: u32,
}

/// Generate (or reuse) the fixture MOV.
pub fn make(ff: &Path, spec: &Spec) -> PathBuf {
    let path = fixture_dir().join(format!("{}.mov", spec.name));
    if !path.exists() {
        let tmp = filmcraft_testkit::temp_path(&path);
        let (frames, gop) = (spec.frames.to_string(), spec.gop.to_string());
        let t = tmp.to_str().unwrap().to_string();
        run(ff, &["-f", "lavfi", "-i", &spec.lavfi, "-frames:v", &frames, "-c:v", "qtrle", "-g", &gop, "-f", "mov", &t]);
        filmcraft_testkit::fixtures::publish(&tmp, &path).unwrap();
    }
    path
}

/// The video samples of a MOV, split by each sample's leading size field (ffmpeg's rawvideo
/// muxer concatenates the packets).
pub fn packets(ff: &Path, mov: &Path) -> Vec<Vec<u8>> {
    let out = Command::new(ff)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(mov)
        .args(["-map", "0:v:0", "-c", "copy", "-f", "rawvideo", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut v = Vec::new();
    let mut d = &out.stdout[..];
    while d.len() >= 4 {
        let n = (u32::from_be_bytes(d[..4].try_into().unwrap()) & 0x3fff_ffff) as usize;
        assert!(n >= 4 && n <= d.len(), "bad sample size {n}");
        v.push(d[..n].to_vec());
        d = &d[n..];
    }
    v
}

/// The `rle ` sample entry payload (after its box header): (width, height, depth, payload).
pub fn sample_entry(mov: &Path) -> (u32, u32, u16, Vec<u8>) {
    let d = std::fs::read(mov).unwrap();
    let i = d.windows(4).position(|w| w == b"stsd").expect("stsd");
    // 'stsd', version and flags, entry count, then the entry's size and format
    let size = u32::from_be_bytes(d[i + 12..i + 16].try_into().unwrap()) as usize;
    assert_eq!(&d[i + 16..i + 20], b"rle ");
    let p = d[i + 20..i + 12 + size].to_vec();
    let be16 = |o: usize| u16::from_be_bytes([p[o], p[o + 1]]);
    (be16(24) as u32, be16(26) as u32, be16(74), p)
}

/// ffmpeg's decode of every frame as RGBA8.
pub fn reference(ff: &Path, mov: &Path) -> Vec<u8> {
    let out = Command::new(ff)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(mov)
        .args(["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn full(kind: &[u8; 4], flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = flags.to_be_bytes().to_vec();
    p.extend_from_slice(payload);
    bx(kind, &p)
}

fn be(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A minimal QuickTime file with one `rle ` track of `samples` (25 fps). `table`: a colour table
/// stored in the sample description (color table ID 0); else ID -1.
pub fn write_mov(path: &Path, w: u16, h: u16, depth: u16, samples: &[Vec<u8>], table: Option<&[[u8; 3]]>) {
    let mut entry = vec![0u8; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0, 0, 0, 0]);
    entry.extend_from_slice(b"appl");
    entry.extend(be(&[0, 0x400]));
    entry.extend_from_slice(&w.to_be_bytes());
    entry.extend_from_slice(&h.to_be_bytes());
    entry.extend(be(&[0x48_0000, 0x48_0000, 0]));
    entry.extend_from_slice(&1u16.to_be_bytes());
    let mut name = [0u8; 32];
    name[0] = 9;
    name[1..10].copy_from_slice(b"Animation");
    entry.extend_from_slice(&name);
    entry.extend_from_slice(&depth.to_be_bytes());
    entry.extend_from_slice(&(if table.is_some() { 0i16 } else { -1 }).to_be_bytes());
    if let Some(t) = table {
        entry.extend(be(&[0]));
        entry.extend_from_slice(&0u16.to_be_bytes());
        entry.extend_from_slice(&(t.len() as u16 - 1).to_be_bytes());
        for (i, c) in t.iter().enumerate() {
            for v in [i as u16, c[0] as u16 * 257, c[1] as u16 * 257, c[2] as u16 * 257] {
                entry.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
    let n = samples.len() as u32;
    let stsd = full(b"stsd", 0, &[be(&[1]), bx(b"rle ", &entry)].concat());
    let stts = full(b"stts", 0, &be(&[1, n, 1]));
    let stsc = full(b"stsc", 0, &be(&[1, 1, n, 1]));
    let stsz = full(b"stsz", 0, &[be(&[0, n]), be(&samples.iter().map(|s| s.len() as u32).collect::<Vec<_>>())].concat());
    let ftyp = bx(b"ftyp", b"qt  \0\0\x02\0qt  ");
    let matrix = be(&[0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000]);
    let moov = |off: u32| {
        let stbl = bx(b"stbl", &[stsd.clone(), stts.clone(), stsc.clone(), stsz.clone(), full(b"stco", 0, &be(&[1, off]))].concat());
        let dinf = bx(b"dinf", &full(b"dref", 0, &[be(&[1]), full(b"alis", 1, &[])].concat()));
        let vmhd = full(b"vmhd", 1, &[0, 0x40, 0x80, 0, 0x80, 0, 0x80, 0]);
        let minf = bx(b"minf", &[vmhd, dinf, stbl].concat());
        let hdlr = full(b"hdlr", 0, &[b"mhlr".to_vec(), b"vide".to_vec(), b"appl".to_vec(), be(&[0, 0]), vec![0]].concat());
        let mdhd = full(b"mdhd", 0, &[be(&[0, 0, 25, n]), vec![0, 0, 0, 0]].concat());
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        let tkhd = full(b"tkhd", 3, &[be(&[0, 0, 1, 0, n * 24]), vec![0; 16], matrix.clone(), be(&[u32::from(w) << 16, u32::from(h) << 16])].concat());
        let trak = bx(b"trak", &[tkhd, mdia].concat());
        let mvhd = full(b"mvhd", 0, &[be(&[0, 0, 600, n * 24, 0x1_0000]), vec![1, 0], vec![0; 10], matrix.clone(), vec![0; 24], be(&[2])].concat());
        bx(b"moov", &[mvhd, trak].concat())
    };
    let off = (ftyp.len() + moov(0).len() + 8) as u32;
    let mdat = bx(b"mdat", &samples.concat());
    std::fs::write(path, [ftyp, moov(off), mdat].concat()).unwrap();
}

/// A small xorshift generator for reproducible streams and mutations.
pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    pub fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// (pixels, bytes) of a coding unit at `depth` (the decoder's own rule, restated for the tests).
pub fn unit(depth: u16) -> (usize, usize) {
    match depth {
        1 | 33 => (16, 2),
        2 | 34 => (16, 4),
        4 | 36 => (8, 4),
        8 | 40 => (4, 4),
        16 => (1, 2),
        24 => (1, 3),
        _ => (1, 4),
    }
}

/// A random but well-formed sample for a `w`×`h` picture (`w` a multiple of the unit): every
/// run stays inside its line. `key`: all lines, no skips; else a random band of lines with skips.
pub fn random_sample(rng: &mut Rng, w: usize, h: usize, depth: u16, key: bool) -> Vec<u8> {
    let (upx, ub) = unit(depth);
    let units = w / upx;
    let (start, count) = if key {
        (0, h)
    } else {
        let s = rng.below(h);
        (s, 1 + rng.below(h - s))
    };
    let mut body: Vec<u8> = if key { vec![0, 0] } else { [8u16, start as u16, 0, count as u16, 0].iter().flat_map(|v| v.to_be_bytes()).collect() };
    for _ in 0..count {
        let mut x = if key { 0 } else { rng.below(3).min(units) };
        body.push(x as u8 + 1);
        while x < units {
            let left = units - x;
            match rng.below(if key { 2 } else { 3 }) {
                0 => {
                    let n = 1 + rng.below(left.min(127));
                    body.push(n as u8);
                    body.extend((0..n * ub).map(|_| rng.byte()));
                    x += n;
                }
                1 if left >= 2 => {
                    let n = 2 + rng.below(left.min(128) - 1);
                    body.push((-(n as i32)) as u8);
                    body.extend((0..ub).map(|_| rng.byte()));
                    x += n;
                }
                1 => continue,
                _ => {
                    let n = rng.below(left.min(254)) + 1;
                    body.push(0);
                    body.push(n as u8 + 1);
                    x += n;
                }
            }
            if !key && rng.below(4) == 0 {
                break; // leave the rest of the line as it was
            }
        }
        body.push(0xff);
    }
    body.push(0);
    let mut s = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    s.extend(body);
    s
}
