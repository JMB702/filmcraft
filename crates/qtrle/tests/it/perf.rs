//! Decode throughput at 2160p. Run with
//! `cargo test --release -p filmcraft-qtrle --test it perf:: -- --ignored --nocapture`.

use crate::common::Rng;
use filmcraft_qtrle::Decoder;
use std::time::Instant;

const W: usize = 3840;
const H: usize = 2160;

/// A 2160p ARGB key frame. `noise`: every pixel different (the incompressible worst case, about
/// 33 MB); else soft gradients with short repeats (motion graphics over transparency).
fn key_frame(noise: bool) -> Vec<u8> {
    let mut rng = Rng(0x1234_5678);
    let mut body = vec![0u8, 0];
    for y in 0..H {
        body.push(1);
        let mut x = 0;
        while x < W {
            let n = (W - x).min(if noise { 127 } else { 8 + (x + y) % 100 });
            if noise || (x / 64) % 3 != 0 {
                body.push(n as u8);
                for i in 0..n {
                    let v = if noise { rng.next() as u32 } else { (((x + i) * 255 / W) as u32) << 16 | ((y * 255 / H) as u32) << 8 | 0xff00_0000 };
                    body.extend_from_slice(&v.to_be_bytes());
                }
                x += n;
            } else {
                let n = n.max(2);
                body.push((-(n as i32)) as u8);
                body.extend_from_slice(&[0, 0, 0, 0]);
                x += n;
            }
        }
        body.push(0xff);
    }
    body.push(0);
    let mut s = (((body.len() + 4) as u32) | 0x4000_0000).to_be_bytes().to_vec();
    s.extend(body);
    s
}

#[test]
#[ignore]
fn decode_2160p_argb() {
    for noise in [true, false] {
        let s = key_frame(noise);
        let mut d = Decoder::new(W as u32, H as u32, 32, None).unwrap();
        // hold the previous picture, as the frame cache does, so each decode copies it
        let mut held = d.decode(&s).unwrap();
        let t = Instant::now();
        let n = 20;
        for _ in 0..n {
            held = d.decode(&s).unwrap();
        }
        std::hint::black_box(&held);
        let ms = t.elapsed().as_secs_f64() * 1e3 / n as f64;
        println!("2160p ARGB {} key frame, {:.1} MB: {ms:.1} ms/frame ({:.0} fps)", if noise { "noise" } else { "graphics" }, s.len() as f64 / 1e6, 1e3 / ms);
    }
}
