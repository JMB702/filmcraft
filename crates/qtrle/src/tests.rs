//! Unit tests on hand-built samples. The expected pixels were checked against ffmpeg's decoder
//! (an external oracle) on the same bytes in a MOV; see the README.

use super::*;

/// A sample: `lines` (each a skip byte, codes and data, ending with 0xff) after a header for all
/// lines (`part: None`) or for `count` lines from `start`, then the end-of-frame byte.
fn sample(lines: &[&[u8]], part: Option<(u16, u16)>) -> Vec<u8> {
    let mut body = match part {
        None => vec![0, 0],
        Some((start, count)) => [8u16, start, 0, count, 0].iter().flat_map(|v| v.to_be_bytes()).collect(),
    };
    for l in lines {
        body.extend_from_slice(l);
    }
    body.push(0);
    let mut s = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    s.extend(body);
    s
}

fn px(p: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
    let i = (y * w + x) * 4;
    [p[i], p[i + 1], p[i + 2], p[i + 3]]
}

#[test]
fn argb_key_and_delta_frames_keep_alpha() {
    let mut d = Decoder::new(4, 2, 32, None).unwrap();
    assert!(d.has_alpha());
    // line 0: copy two pixels, repeat one twice; line 1: repeat a half-transparent pixel 4 times
    let key = sample(&[&[1, 2, 0xff, 1, 2, 3, 0x00, 9, 9, 9, 0xfe, 0x80, 10, 20, 30, 0xff], &[1, 0xfc, 0x40, 1, 1, 1, 0xff]], None);
    assert!(d.is_key_frame(&key));
    let p = d.decode(&key).unwrap();
    assert_eq!(px(&p, 4, 0, 0), [1, 2, 3, 0xff]);
    assert_eq!(px(&p, 4, 1, 0), [9, 9, 9, 0]);
    assert_eq!(px(&p, 4, 3, 0), [10, 20, 30, 0x80]);
    assert_eq!(px(&p, 4, 3, 1), [1, 1, 1, 0x40]);
    // delta: line 1 only, skip 2 pixels, copy one, code 0 skips nothing more, copy one
    let delta = sample(&[&[3, 1, 0xff, 7, 7, 7, 0, 1, 1, 0xff, 8, 8, 8, 0xff]], Some((1, 1)));
    assert!(!d.is_key_frame(&delta));
    // all lines, but pixel 0 of line 1 skipped: not a key frame either
    let skips = sample(&[&[1, 0xfc, 0xff, 0, 0, 0, 0xff], &[2, 0xfd, 0xff, 0, 0, 0, 0xff]], Some((0, 2)));
    assert!(!d.is_key_frame(&skips));
    let short = sample(&[&[1, 0xfc, 0xff, 0, 0, 0, 0xff], &[1, 0xfd, 0xff, 0, 0, 0, 0xff]], None);
    assert!(!d.is_key_frame(&short), "line 1 stops a pixel short");
    let q = d.decode(&delta).unwrap();
    assert_eq!(px(&q, 4, 0, 0), [1, 2, 3, 0xff], "untouched line kept");
    assert_eq!(px(&q, 4, 1, 1), [1, 1, 1, 0x40], "skipped pixel kept");
    assert_eq!(px(&q, 4, 2, 1), [7, 7, 7, 0xff]);
    assert_eq!(px(&q, 4, 3, 1), [8, 8, 8, 0xff]);
    // the earlier picture is not changed by later samples
    assert_eq!(px(&p, 4, 2, 1), [1, 1, 1, 0x40]);
}

#[test]
fn unchanged_samples_repeat_the_picture() {
    let mut d = Decoder::new(2, 1, 24, None).unwrap();
    let a = d.decode(&sample(&[&[1, 0xfe, 5, 6, 7, 0xff]], None)).unwrap();
    for nop in [&[0u8, 0, 0, 7, 0, 0, 0][..], &[0, 0, 0, 4], &[], &[0x40, 0, 0, 6, 0, 0, 0, 0]] {
        assert_eq!(*d.decode(nop).unwrap(), *a);
    }
    assert!(!d.is_key_frame(&[0, 0, 0, 7, 0, 0, 0]));
}

#[test]
fn size_field_flag_bits_are_ignored() {
    let mut s = sample(&[&[1, 0xfe, 5, 6, 7, 0xff]], None);
    s[0] |= 0x40;
    let mut d = Decoder::new(2, 1, 24, None).unwrap();
    assert_eq!(px(&d.decode(&s).unwrap(), 2, 1, 0), [5, 6, 7, 255]);
}

#[test]
fn rgb555_widens_by_bit_replication() {
    let mut d = Decoder::new(4, 1, 16, None).unwrap();
    let p = d.decode(&sample(&[&[1, 4, 0x7f, 0xff, 0x04, 0x21, 0x42, 0x10, 0x80, 0x1f, 0xff]], None)).unwrap();
    assert_eq!(px(&p, 4, 0, 0), [0xff, 0xff, 0xff, 0xff]);
    assert_eq!(px(&p, 4, 1, 0), [0x08, 0x08, 0x08, 0xff]);
    assert_eq!(px(&p, 4, 2, 0), [0x84, 0x84, 0x84, 0xff]);
    assert_eq!(px(&p, 4, 3, 0), [0x00, 0x00, 0xff, 0xff], "top bit unused");
}

#[test]
fn indexed_8_bit_units_are_groups_of_four() {
    // grey 8-bit: index i is 255 - i
    let mut d = Decoder::new(16, 1, 40, None).unwrap();
    d.decode(&sample(&[&[1, 0xfc, 10, 10, 10, 10, 0xff]], None)).unwrap();
    // skip byte 3 skips two groups (8 pixels); code 0 then skip byte 2 skips one more group
    let p = d.decode(&sample(&[&[3, 1, 1, 2, 3, 4, 0, 2, 0xff]], Some((0, 1)))).unwrap();
    let row: Vec<u8> = (0..16).map(|x| px(&p, 16, x, 0)[0]).collect();
    assert_eq!(row, [245, 245, 245, 245, 245, 245, 245, 245, 254, 253, 252, 251, 245, 245, 245, 245]);
    let p = d.decode(&sample(&[&[1, 0, 4, 1, 9, 0, 9, 0, 0xff]], Some((0, 1)))).unwrap();
    let row: Vec<u8> = (0..16).map(|x| px(&p, 16, x, 0)[0]).collect();
    assert_eq!(row[12..], [246, 255, 246, 255]);
}

#[test]
fn indexed_4_and_2_bit_units() {
    // default 16-colour table: index 1 yellow, 2 orange; units of 8 pixels
    let mut d = Decoder::new(24, 1, 4, None).unwrap();
    let p = d.decode(&sample(&[&[2, 1, 0x12, 0x00, 0x00, 0x0f, 0xff]], None)).unwrap();
    assert_eq!(px(&p, 24, 0, 0), [0xff, 0xff, 0xff, 0xff], "never coded: index 0");
    assert_eq!(px(&p, 24, 8, 0), [0xfc, 0xf3, 0x05, 0xff]);
    assert_eq!(px(&p, 24, 9, 0), [0xff, 0x64, 0x02, 0xff]);
    assert_eq!(px(&p, 24, 15, 0), [0, 0, 0, 0xff]);
    // 2-bit grey, a 16-pixel unit repeated twice
    let mut d = Decoder::new(40, 1, 34, None).unwrap();
    let p = d.decode(&sample(&[&[1, 0xfe, 0xe4, 0xe4, 0xe4, 0xe4, 0xff]], None)).unwrap();
    let row: Vec<u8> = (0..40).map(|x| px(&p, 40, x, 0)[0]).collect();
    assert_eq!(row[..8], [0, 0x55, 0xaa, 0xff, 0, 0x55, 0xaa, 0xff]);
    assert_eq!(row[28..], [0, 0x55, 0xaa, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
}

#[test]
fn indexed_1_bit_units_are_16_pixel_words() {
    let mut d = Decoder::new(32, 1, 1, None).unwrap();
    let p = d.decode(&sample(&[&[2, 1, 0xf0, 0x0f, 0xff]], None)).unwrap();
    let row: String = (0..32).map(|x| if px(&p, 32, x, 0)[0] == 0 { '#' } else { '.' }).collect();
    assert_eq!(row, "................####........####");
}

#[test]
fn stored_colour_table() {
    let mut d = Decoder::new(4, 1, 8, Some(&[[10, 20, 30], [200, 100, 50]])).unwrap();
    let p = d.decode(&sample(&[&[1, 1, 0, 1, 2, 255, 0xff]], None)).unwrap();
    assert_eq!(px(&p, 4, 0, 0), [10, 20, 30, 255]);
    assert_eq!(px(&p, 4, 1, 0), [200, 100, 50, 255]);
    assert_eq!(px(&p, 4, 2, 0), [0, 0, 0, 255], "missing entries are black");
}

#[test]
fn runs_past_the_edge_are_clipped_to_their_line() {
    let mut d = Decoder::new(3, 2, 24, None).unwrap();
    // 5 pixels copied into a 3-pixel line, then a repeat far past it, then a huge skip
    let s = sample(&[&[1, 5, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 5, 0x81, 9, 9, 9, 0, 255, 0xff], &[1, 0xff]], None);
    let p = d.decode(&s).unwrap();
    assert_eq!(px(&p, 3, 2, 0), [3, 3, 3, 255]);
    assert_eq!(px(&p, 3, 0, 1), [0, 0, 0, 255], "nothing spilled into the next line");
    // lines below the picture and a start line past the bottom are read and dropped
    let s = sample(&[&[1, 0xfe, 6, 6, 6, 0xff], &[1, 0xfe, 7, 7, 7, 0xff]], Some((1, 2)));
    let p = d.decode(&s).unwrap();
    assert_eq!(px(&p, 3, 1, 1), [6, 6, 6, 255]);
    d.decode(&sample(&[&[1, 0xfe, 6, 6, 6, 0xff]], Some((60_000, 1)))).unwrap();
}

#[test]
fn truncated_samples_are_errors() {
    let full = sample(&[&[1, 0xfe, 5, 6, 7, 0xff], &[1, 2, 1, 1, 1, 2, 2, 2, 0xff]], None);
    for n in 8..full.len() - 1 {
        let mut d = Decoder::new(2, 2, 24, None).unwrap();
        let mut s = full[..n].to_vec();
        s[..4].copy_from_slice(&(n as u32).to_be_bytes());
        assert_eq!(d.decode(&s), Err(Error::Truncated), "{n} bytes");
    }
    // a size field shorter than the sample cuts it
    let mut s = full.clone();
    s[..4].copy_from_slice(&10u32.to_be_bytes());
    assert_eq!(Decoder::new(2, 2, 24, None).unwrap().decode(&s), Err(Error::Truncated));
    // a partial-update header cut short
    assert_eq!(header(&[0, 0, 0, 9, 0, 8, 0, 0, 0]), Err(Error::Truncated));
}

#[test]
fn reset_and_hostile_dimensions() {
    let mut d = Decoder::new(1, 1, 32, None).unwrap();
    d.decode(&sample(&[&[1, 1, 0xff, 1, 2, 3, 0xff]], None)).unwrap();
    d.reset();
    assert_eq!(*d.decode(&[]).unwrap(), vec![0, 0, 0, 0]);
    for (w, h, depth) in [(0, 1, 32), (1, 0, 32), (16_385, 1, 32), (16_384, 16_384, 32), (8, 8, 3), (8, 8, 48), (8, 8, 0)] {
        assert!(Decoder::new(w, h, depth, None).is_err(), "{w}x{h} depth {depth}");
    }
    assert_eq!(depth_label(36).as_deref(), Some("4-bit grey"));
    assert_eq!(depth_label(32).as_deref(), Some("ARGB 32-bit"));
}
