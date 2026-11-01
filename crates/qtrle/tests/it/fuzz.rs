//! Robustness: mutated and random samples must never panic (errors are fine), at every depth,
//! with the decoder carrying its picture from one hostile sample to the next.

use crate::common::*;
use filmcraft_qtrle::Decoder;

const DEPTHS: [u16; 11] = [1, 2, 4, 8, 16, 24, 32, 33, 34, 36, 40];

fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut s = seed.to_vec();
    match rng.below(6) {
        0 => s.truncate(rng.below(s.len() + 1)),
        1 => {
            for _ in 0..1 + rng.below(8) {
                let i = rng.below(s.len());
                if let Some(b) = s.get_mut(i) {
                    *b ^= 1 << rng.below(8);
                }
            }
        }
        2 => {
            // corrupt the size field or the partial-update header
            let i = rng.below(14.min(s.len()));
            if let Some(b) = s.get_mut(i) {
                *b = rng.byte();
            }
        }
        3 => {
            let i = rng.below(s.len());
            s.splice(i..i, (0..rng.below(64)).map(|_| rng.byte()));
        }
        4 => s = (0..rng.below(300)).map(|_| rng.byte()).collect(),
        _ => {
            // big counts everywhere
            for b in s.iter_mut().skip(6) {
                if rng.below(5) == 0 {
                    *b = [0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff][rng.below(6)];
                }
            }
        }
    }
    s
}

#[test]
fn mutated_samples_never_panic() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut errors = 0usize;
    let mut runs = 0usize;
    for depth in DEPTHS {
        for (w, h) in [(48usize, 10usize), (17, 3), (1, 1)] {
            let seeds: Vec<Vec<u8>> = (0..6).map(|i| random_sample(&mut rng, w / unit(depth).0 * unit(depth).0, h, depth, i % 3 == 0)).collect();
            let mut d = Decoder::new(w as u32, h as u32, depth, None).unwrap();
            for _ in 0..1500 {
                let seed = &seeds[rng.below(seeds.len())];
                let s = mutate(&mut rng, seed);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.decode(&s).map(|p| p.len())));
                match r {
                    Ok(Ok(n)) => assert_eq!(n, w * h * 4),
                    Ok(Err(_)) => errors += 1,
                    Err(_) => panic!("panic at depth {depth} {w}x{h} on {:02x?}", s),
                }
                runs += 1;
            }
        }
    }
    println!("{runs} mutated samples, {errors} rejected, no panic");
}

/// Real encoder output as seeds, when ffmpeg is present.
#[test]
fn mutated_ffmpeg_samples_never_panic() {
    let Some(ff) = ffmpeg() else { return };
    let mut rng = Rng(7);
    for pix in ["rgb24", "rgb555be", "argb", "gray"] {
        let tag = pix.trim_end_matches("be");
        let spec = Spec { name: format!("fuzz_seed_{tag}"), lavfi: format!("testsrc2=s=40x24:r=25,format={pix}"), frames: 4, gop: 2 };
        let mov = make(&ff, &spec);
        let (w, h, depth, _) = sample_entry(&mov);
        let seeds = packets(&ff, &mov);
        let mut d = Decoder::new(w, h, depth, None).unwrap();
        for _ in 0..3000 {
            let k = rng.below(seeds.len());
            let s = mutate(&mut rng, &seeds[k]);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.decode(&s).map(|_| ())));
            assert!(r.is_ok(), "panic at depth {depth} on {s:02x?}");
        }
    }
}
