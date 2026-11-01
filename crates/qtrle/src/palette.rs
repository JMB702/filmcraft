//! Colour tables for the indexed depths: the default tables QuickTime uses when a sample
//! description says color table ID -1, the grey ramps of depths 33–40, and the `ctab` stored in a
//! sample description (color table ID 0).

/// 256 straight-alpha RGBA entries; indices a table leaves out are opaque black.
pub type Palette = [[u8; 4]; 256];

const BLACK: [u8; 4] = [0, 0, 0, 255];

/// The 16-colour default table (depth 4).
const DEFAULT_4: [[u8; 3]; 16] = [
    [0xff, 0xff, 0xff],
    [0xfc, 0xf3, 0x05],
    [0xff, 0x64, 0x02],
    [0xdd, 0x08, 0x06],
    [0xf2, 0x08, 0x84],
    [0x46, 0x00, 0xa5],
    [0x00, 0x00, 0xd4],
    [0x02, 0xab, 0xea],
    [0x1f, 0xb7, 0x14],
    [0x00, 0x64, 0x11],
    [0x56, 0x2c, 0x05],
    [0x90, 0x71, 0x3a],
    [0xc0, 0xc0, 0xc0],
    [0x80, 0x80, 0x80],
    [0x40, 0x40, 0x40],
    [0x00, 0x00, 0x00],
];

/// The 4-colour default table (depth 2).
const DEFAULT_2: [[u8; 3]; 4] = [[0xff, 0xff, 0xff], [0xac, 0xac, 0xac], [0x55, 0x55, 0x55], [0x00, 0x00, 0x00]];

/// The 1-bit table: 0 is white, 1 black.
const DEFAULT_1: [[u8; 3]; 2] = [[0xff, 0xff, 0xff], [0x00, 0x00, 0x00]];

fn from_rgb(colors: &[[u8; 3]]) -> Palette {
    let mut p = [BLACK; 256];
    for (d, c) in p.iter_mut().zip(colors) {
        *d = [c[0], c[1], c[2], 255];
    }
    p
}

/// The 256-colour default table (depth 8): a 6×6×6 cube of 0xff, 0xcc … 0x00 with red slowest
/// (white first, black left out), then ten-step ramps of red, green, blue and grey, then black.
fn default_8() -> Palette {
    const CUBE: [u8; 6] = [0xff, 0xcc, 0x99, 0x66, 0x33, 0x00];
    const RAMP: [u8; 10] = [0xee, 0xdd, 0xbb, 0xaa, 0x88, 0x77, 0x55, 0x44, 0x22, 0x11];
    let mut colors = Vec::with_capacity(256);
    for r in CUBE {
        for g in CUBE {
            for b in CUBE {
                colors.push([r, g, b]);
            }
        }
    }
    colors.pop(); // black goes last
    for v in RAMP {
        colors.push([v, 0, 0]);
    }
    for v in RAMP {
        colors.push([0, v, 0]);
    }
    for v in RAMP {
        colors.push([0, 0, v]);
    }
    for v in RAMP {
        colors.push([v, v, v]);
    }
    colors.push([0, 0, 0]);
    from_rgb(&colors)
}

/// A grey ramp of `2^bits` levels from white (index 0) to black.
fn grey(bits: u32) -> Palette {
    let top = (1u32 << bits) - 1;
    let colors: Vec<[u8; 3]> = (0..=top)
        .map(|i| {
            let v = (255 - i * 255 / top) as u8;
            [v, v, v]
        })
        .collect();
    from_rgb(&colors)
}

/// The table for an indexed `depth` (1, 2, 4, 8, or 33–40 for grey): the sample description's own
/// table when it has one, else the default.
pub(crate) fn for_depth(depth: u16, table: Option<&[[u8; 3]]>) -> Palette {
    match (depth, table) {
        (1 | 2 | 4 | 8, Some(t)) if !t.is_empty() => from_rgb(t),
        (1 | 33, _) => from_rgb(&DEFAULT_1),
        (2, _) => from_rgb(&DEFAULT_2),
        (4, _) => from_rgb(&DEFAULT_4),
        (34, _) => grey(2),
        (36, _) => grey(4),
        (40, _) => grey(8),
        _ => default_8(),
    }
}

/// The colour table stored in a QuickTime visual sample description, when its color table ID is 0.
///
/// `entry` is the sample entry payload after the 8-byte box header: 6 reserved bytes, the data
/// reference index, the 70 bytes of video fields (depth and color table ID last), then the table:
/// seed (4 bytes), flags (2), size (2, entries − 1) and per entry a value and 16-bit red, green
/// and blue. Entries are taken in order (at most 256); components keep their high byte.
pub fn color_table(entry: &[u8]) -> Option<Vec<[u8; 3]>> {
    let id = i16::from_be_bytes([*entry.get(76)?, *entry.get(77)?]);
    if id != 0 {
        return None;
    }
    let size = u16::from_be_bytes([*entry.get(84)?, *entry.get(85)?]);
    let count = usize::from(size).saturating_add(1).min(256);
    let table = entry.get(86..)?;
    let colors: Vec<[u8; 3]> = table.as_chunks::<8>().0.iter().take(count).map(|e| [e[2], e[4], e[6]]).collect();
    (!colors.is_empty()).then_some(colors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tables() {
        let p = default_8();
        assert_eq!(p[0], [0xff, 0xff, 0xff, 255]);
        assert_eq!(p[1], [0xff, 0xff, 0xcc, 255]);
        assert_eq!(p[214], [0x00, 0x00, 0x33, 255]);
        assert_eq!(p[215], [0xee, 0x00, 0x00, 255]);
        assert_eq!(p[245], [0xee, 0xee, 0xee, 255]);
        assert_eq!(p[255], [0, 0, 0, 255]);
        assert_eq!(grey(4)[1], [0xee, 0xee, 0xee, 255]);
        assert_eq!(grey(2)[2], [0x55, 0x55, 0x55, 255]);
        assert_eq!(grey(8)[200], [55, 55, 55, 255]);
        assert_eq!(for_depth(1, None)[1], BLACK);
    }

    #[test]
    fn stored_table() {
        let mut e = vec![0u8; 86];
        e[84..86].copy_from_slice(&1u16.to_be_bytes());
        for c in [[0u16, 0x0a0a, 0x1414, 0x1e1e], [1, 0xffff, 0, 0x8080]] {
            for v in c {
                e.extend_from_slice(&v.to_be_bytes());
            }
        }
        assert_eq!(color_table(&e), Some(vec![[10, 20, 30], [255, 0, 128]]));
        e[76..78].copy_from_slice(&(-1i16).to_be_bytes());
        assert_eq!(color_table(&e), None);
        // truncated or empty tables
        assert_eq!(color_table(&e[..80]), None);
        let mut short = e.clone();
        short[76..78].copy_from_slice(&0i16.to_be_bytes());
        assert_eq!(color_table(&short[..90]), None);
    }
}
