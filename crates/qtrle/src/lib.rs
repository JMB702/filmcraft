//! Clean-room QuickTime Animation (`rle `) video decoder.
//!
//! - [`Decoder`]: decodes the samples of one track in order into a straight-alpha RGBA8 picture
//!   (row-major, `width * 4` bytes per row). Depths 1, 2, 4 and 8 (indexed: the sample
//!   description's colour table or the default one), 16 (RGB 5-5-5), 24 (RGB), 32 (ARGB, alpha
//!   kept) and 33–40 (grey, 1/2/4/8 bits). Key frames and delta frames: a delta frame only
//!   rewrites the lines and pixels it codes, the rest keeps the previous picture.
//! - [`color_table`]: the colour table of a sample description; [`Decoder::is_key_frame`]:
//!   whether a sample codes every pixel (a frame decoding can start from).
//!
//! Every byte is untrusted: reads are bounds-checked, writes are clipped to the picture, and a
//! sample that runs out of data is an error (the lines decoded so far stay in the picture).
//! See the crate README for the bitstream description and what was verified how.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod palette;

use std::sync::Arc;

pub use palette::{Palette, color_table};

/// Largest picture side, and largest picture (8192×8192): the canvas is 4 bytes per pixel.
pub const MAX_SIDE: u32 = 16_384;
pub const MAX_PIXELS: u64 = 8192 * 8192;

/// Decoder errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("truncated QuickTime Animation frame")]
    Truncated,
    #[error("unsupported QuickTime Animation stream: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How a depth stores its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Palette indices of 1, 2, 4 or 8 bits, packed most significant first.
    Indexed(u8),
    /// 16-bit big-endian `xRRRRRGG GGGBBBBB`.
    Rgb555,
    /// R, G, B.
    Rgb24,
    /// A, R, G, B (straight alpha).
    Argb32,
}

impl Format {
    /// The format of a sample description depth (`None`: not a depth this codec has).
    pub fn from_depth(depth: u16) -> Option<Format> {
        Some(match depth {
            1 | 33 => Format::Indexed(1),
            2 | 34 => Format::Indexed(2),
            4 | 36 => Format::Indexed(4),
            8 | 40 => Format::Indexed(8),
            16 => Format::Rgb555,
            24 => Format::Rgb24,
            32 => Format::Argb32,
            _ => return None,
        })
    }

    /// (pixels, bytes) of one coding unit: what a skip, copy or repeat counts in. Indexed pixels
    /// travel in 32-bit groups, except 1-bit pixels, which travel in 16-bit words.
    fn unit(self) -> (usize, usize) {
        match self {
            Format::Indexed(1) => (16, 2),
            Format::Indexed(bits) => (32 / usize::from(bits.max(1)), 4),
            Format::Rgb555 => (1, 2),
            Format::Rgb24 => (1, 3),
            Format::Argb32 => (1, 4),
        }
    }
}

/// A short label for a depth ("RGB 24-bit", "ARGB 32-bit", "8-bit indexed", "8-bit grey").
pub fn depth_label(depth: u16) -> Option<String> {
    Some(match (Format::from_depth(depth)?, depth > 32) {
        (Format::Indexed(b), false) => format!("{b}-bit indexed"),
        (Format::Indexed(b), true) => format!("{b}-bit grey"),
        (Format::Rgb555, _) => "RGB 15-bit".into(),
        (Format::Rgb24, _) => "RGB 24-bit".into(),
        (Format::Argb32, _) => "ARGB 32-bit".into(),
    })
}

/// The lines a sample rewrites: `None` for a sample that changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update {
    /// First line (0-based).
    pub start: u16,
    /// Number of lines (`None`: to the bottom of the picture).
    pub lines: Option<u16>,
    /// Where the line data starts.
    offset: usize,
    /// Where the chunk ends (its size field, cut to the sample).
    end: usize,
}

/// Parse a sample's header. A sample shorter than 8 bytes, or whose size field says so, repeats
/// the previous picture. The size field's top two bits are flags (encoders set bit 30).
pub fn header(sample: &[u8]) -> Result<Option<Update>> {
    let be16 = |o: usize| sample.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let Some(size) = sample.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) & 0x3fff_ffff) else {
        return Ok(None);
    };
    if sample.len() < 8 || size < 8 {
        return Ok(None);
    }
    let end = usize::try_from(size).unwrap_or(usize::MAX).min(sample.len());
    let flags = be16(4).ok_or(Error::Truncated)?;
    if flags & 0x0008 == 0 {
        return Ok(Some(Update { start: 0, lines: None, offset: 6, end }));
    }
    let (Some(start), Some(lines)) = (be16(6), be16(10)) else {
        return Err(Error::Truncated);
    };
    Ok(Some(Update { start, lines: Some(lines), offset: 14, end }))
}

/// Bytes of a sample, read front to back.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.data.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }
}

/// Decodes one track's samples, in order, into an RGBA8 picture.
pub struct Decoder {
    width: usize,
    height: usize,
    format: Format,
    palette: Box<Palette>,
    picture: Arc<Vec<u8>>,
    blank: [u8; 4],
}

impl Decoder {
    /// A decoder for a `width`×`height` track of `depth` (the sample description's depth field),
    /// with the sample description's colour table, if any ([`color_table`]).
    pub fn new(width: u32, height: u32, depth: u16, table: Option<&[[u8; 3]]>) -> Result<Self> {
        let format = Format::from_depth(depth).ok_or_else(|| Error::Unsupported(format!("depth {depth}")))?;
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
            return Err(Error::Unsupported(format!("{width}x{height} picture")));
        }
        let palette = Box::new(palette::for_depth(depth, table));
        // before any key frame: transparent with alpha, else what an empty canvas would show
        let blank = match format {
            Format::Argb32 => [0; 4],
            Format::Indexed(_) => palette[0],
            _ => [0, 0, 0, 255],
        };
        let (width, height) = (width as usize, height as usize);
        let picture = Arc::new(blank.repeat(width * height));
        Ok(Self { width, height, format, palette, picture, blank })
    }

    pub fn width(&self) -> u32 {
        self.width as u32
    }
    pub fn height(&self) -> u32 {
        self.height as u32
    }
    pub fn format(&self) -> Format {
        self.format
    }
    /// Whether pictures carry alpha (32-bit ARGB).
    pub fn has_alpha(&self) -> bool {
        self.format == Format::Argb32
    }

    /// Whether `sample` codes every pixel of the picture, so decoding can start there: it updates
    /// every line, and no line skips pixels or stops short. Reads the sample without decoding it.
    pub fn is_key_frame(&self, sample: &[u8]) -> bool {
        let Ok(Some(u)) = header(sample) else { return false };
        if u.start != 0 || u.lines.is_some_and(|n| usize::from(n) < self.height) {
            return false;
        }
        let (unit_px, unit_bytes) = self.format.unit();
        let mut r = Reader { data: sample.get(..u.end).unwrap_or(sample), pos: u.offset };
        (0..self.height).all(|_| line_is_complete(&mut r, self.width, unit_px, unit_bytes) == Ok(true))
    }

    /// Forget the previous picture (before decoding from a key frame after a seek).
    pub fn reset(&mut self) {
        let blank = self.blank;
        Arc::make_mut(&mut self.picture).as_chunks_mut::<4>().0.fill(blank);
    }

    /// Decode the next sample and return the picture (straight-alpha RGBA8, `width * 4` bytes a
    /// row). On an error the picture keeps the lines decoded before it.
    pub fn decode(&mut self, sample: &[u8]) -> Result<Arc<Vec<u8>>> {
        if let Some(u) = header(sample)? {
            let data = sample.get(..u.end).unwrap_or(sample);
            let mut r = Reader { data, pos: u.offset };
            let lines = u.lines.map_or(self.height, usize::from);
            let picture = Arc::make_mut(&mut self.picture);
            let mut lines_out = Lines { width: self.width, format: self.format, palette: &self.palette, picture };
            lines_out.decode(&mut r, usize::from(u.start), lines)?;
        }
        Ok(Arc::clone(&self.picture))
    }
}

/// Read one line; whether it starts with no skip, has no skip codes and reaches the right edge.
fn line_is_complete(r: &mut Reader, width: usize, unit_px: usize, unit_bytes: usize) -> Result<bool> {
    if r.byte()? != 1 {
        return Ok(false);
    }
    let mut x = 0usize;
    loop {
        let code = r.byte()? as i8;
        match code {
            -1 => return Ok(x >= width),
            0 => return Ok(false),
            1.. => {
                r.take(code as usize * unit_bytes)?;
                x = x.saturating_add(code as usize * unit_px);
            }
            _ => {
                r.take(unit_bytes)?;
                x = x.saturating_add(usize::from(code.unsigned_abs()) * unit_px);
            }
        }
    }
}

/// The picture being written by one sample.
struct Lines<'p> {
    width: usize,
    format: Format,
    palette: &'p Palette,
    picture: &'p mut [u8],
}

impl Lines<'_> {
    fn decode(&mut self, r: &mut Reader, start: usize, count: usize) -> Result<()> {
        let (unit_px, unit_bytes) = self.format.unit();
        let row_bytes = self.width * 4;
        for row in start..start.saturating_add(count) {
            let skip = r.byte()?;
            if skip == 0 {
                break; // end of the frame
            }
            // lines past the bottom are read but not drawn
            let span = row.checked_mul(row_bytes).and_then(|o| Some(o..o.checked_add(row_bytes)?));
            let mut line = Line { px: span.and_then(|s| self.picture.get_mut(s)), x: usize::from(skip - 1) * unit_px, width: self.width };
            loop {
                let code = r.byte()? as i8;
                match code {
                    -1 => break,
                    0 => line.x = line.x.saturating_add(usize::from(r.byte()?.saturating_sub(1)) * unit_px),
                    1.. => {
                        let n = code as usize;
                        let data = r.take(n * unit_bytes)?;
                        line.copy(self.format, self.palette, data);
                    }
                    _ => {
                        let unit = r.take(unit_bytes)?;
                        line.repeat(self.format, self.palette, unit, usize::from(code.unsigned_abs()));
                    }
                }
            }
        }
        Ok(())
    }
}

/// One line of the picture and the position in it (in pixels; may run past the right edge, where
/// pixels are dropped).
struct Line<'a> {
    px: Option<&'a mut [u8]>,
    x: usize,
    width: usize,
}

impl Line<'_> {
    /// The pixels `x .. x + n` that lie inside the line, then advance by `n`.
    fn span(&mut self, n: usize) -> Option<&mut [[u8; 4]]> {
        let x = self.x;
        self.x = x.saturating_add(n);
        let end = self.x.min(self.width);
        if x >= end {
            return None;
        }
        Some(self.px.as_deref_mut()?.get_mut(x * 4..end * 4)?.as_chunks_mut::<4>().0)
    }

    /// Copy units of pixel data.
    fn copy(&mut self, format: Format, palette: &Palette, data: &[u8]) {
        match format {
            Format::Argb32 => {
                let src = data.as_chunks::<4>().0;
                if let Some(dst) = self.span(src.len()) {
                    for (d, s) in dst.iter_mut().zip(src) {
                        *d = u32::from_be_bytes(*s).rotate_left(8).to_be_bytes();
                    }
                }
            }
            Format::Rgb24 => {
                let src = data.as_chunks::<3>().0;
                if let Some(dst) = self.span(src.len()) {
                    for (d, s) in dst.iter_mut().zip(src) {
                        *d = [s[0], s[1], s[2], 255];
                    }
                }
            }
            Format::Rgb555 => {
                let src = data.as_chunks::<2>().0;
                if let Some(dst) = self.span(src.len()) {
                    for (d, s) in dst.iter_mut().zip(src) {
                        *d = rgb555(*s);
                    }
                }
            }
            Format::Indexed(bits) => {
                let per_byte = 8 / usize::from(bits.max(1));
                if let Some(dst) = self.span(data.len() * per_byte) {
                    for (d, i) in dst.iter_mut().zip(indices(data, bits)) {
                        *d = palette[usize::from(i)];
                    }
                }
            }
        }
    }

    /// Repeat one unit `n` times.
    fn repeat(&mut self, format: Format, palette: &Palette, unit: &[u8], n: usize) {
        let mut pattern = [[0u8; 4]; 16];
        let len = match format {
            Format::Argb32 => {
                let Some(&argb) = unit.first_chunk::<4>() else { return };
                pattern[0] = u32::from_be_bytes(argb).rotate_left(8).to_be_bytes();
                1
            }
            Format::Rgb24 => {
                let Some(&[r, g, b]) = unit.first_chunk::<3>() else { return };
                pattern[0] = [r, g, b, 255];
                1
            }
            Format::Rgb555 => {
                let Some(&px) = unit.first_chunk::<2>() else { return };
                pattern[0] = rgb555(px);
                1
            }
            Format::Indexed(bits) => {
                let mut k = 0;
                for (p, i) in pattern.iter_mut().zip(indices(unit, bits)) {
                    *p = palette[usize::from(i)];
                    k += 1;
                }
                k
            }
        };
        let Some(pattern) = pattern.get(..len) else { return };
        let Some(dst) = self.span(n.saturating_mul(len)) else { return };
        if let [px] = pattern {
            dst.fill(*px);
        } else {
            for (d, p) in dst.iter_mut().zip(pattern.iter().cycle()) {
                *d = *p;
            }
        }
    }
}

/// The palette indices in `data`, most significant bits first.
fn indices(data: &[u8], bits: u8) -> impl Iterator<Item = u8> + '_ {
    let bits = bits.clamp(1, 8);
    let per_byte = 8 / bits;
    let mask = ((1u16 << bits) - 1) as u8;
    data.iter().flat_map(move |&b| (0..per_byte).map(move |k| (b >> (8 - bits * (k + 1))) & mask))
}

/// A big-endian `xRRRRRGG GGGBBBBB` pixel as RGBA8 (5-bit components widened by bit replication).
fn rgb555(b: [u8; 2]) -> [u8; 4] {
    let v = u16::from_be_bytes(b);
    let c = |s: u16| {
        let x = ((v >> s) & 0x1f) as u8;
        (x << 3) | (x >> 2)
    };
    [c(10), c(5), c(0), 255]
}

#[cfg(test)]
mod tests;
