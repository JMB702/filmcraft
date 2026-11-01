# filmcraft-qtrle

Clean-room QuickTime Animation (`rle `) video decoder in pure Rust. Layer L0: depends only on
`thiserror`. Builds for `wasm32-unknown-unknown`; no `unsafe`.

QuickTime Animation is Apple's lossless run-length codec, common for motion-graphics renders with
alpha (After Effects and Premiere previews write it at depth 32).

## Sources

- Mike Melanson, *Description of the Apple QuickTime Animation (RLE) Format*, v0.3 (2004),
  <http://multimedia.cx/qtrle.txt>, and the MultimediaWiki page
  <https://wiki.multimedia.cx/index.php/Apple_QuickTime_RLE> (revision of 16 June 2020) based on it.
- Apple, *QuickTime File Format Specification*: the video sample description (`depth`, color table
  ID, the `ctab` that follows when the ID is 0).
- Black-box behaviour of ffmpeg's decoder on streams we wrote (ffmpeg run as an external program;
  its source was not read). This settled what the documents leave open (see below).

No FFmpeg or other implementation source was read or copied.

## API

```rust
use filmcraft_qtrle::{Decoder, color_table};

// From the sample description: size, depth, and its colour table when the color table ID is 0.
let mut dec = Decoder::new(width, height, depth, color_table(&entry_payload).as_deref())?;
let rgba: Arc<Vec<u8>> = dec.decode(&sample)?;   // straight-alpha RGBA8, width * 4 bytes a row
dec.is_key_frame(&sample);                         // codes every pixel: decoding can start here
dec.reset();                                       // after a seek
```

The decoder keeps the picture between samples (delta frames rewrite only what they code). It hands
out an `Arc` of it; the next sample copies the picture first if the caller still holds it.

## Bitstream

All multi-byte values are big-endian.

| Field | Size | Meaning |
|---|---|---|
| chunk size | 4 | low 30 bits: size of the whole sample. The top two bits are flags (the Premiere preview renders and the title render checked here set bit 30). Size < 8, or a sample shorter than 8 bytes: the picture is unchanged. |
| header | 2 | bit 3 (`0x0008`): a partial update follows; otherwise every line is coded |
| start line, unused, line count, unused | 2 each | only with bit 3; the start line is 0-based |
| lines | | one per updated line |

Each line: a skip byte (0 ends the frame; otherwise `skip − 1` units are left as they were), then
signed codes until −1 (end of line):

- `0`: another skip byte follows (`skip − 1` more units left as they were);
- `n > 0`: `n` units of pixel data follow;
- `n < −1`: one unit follows, repeated `−n` times.

| Depth | Unit | Pixel layout |
|---|---|---|
| 1, 33 | 16 pixels in 2 bytes | 1-bit indices, most significant bit first |
| 2, 34 | 16 pixels in 4 bytes | 2-bit indices, most significant first |
| 4, 36 | 8 pixels in 4 bytes | 4-bit indices, high nibble first |
| 8, 40 | 4 pixels in 4 bytes | 8-bit indices |
| 16 | 1 pixel, 2 bytes | `xRRRRRGG GGGBBBBB` (components widened by bit replication) |
| 24 | 1 pixel, 3 bytes | R, G, B |
| 32 | 1 pixel, 4 bytes | A, R, G, B (straight alpha) |

Indexed depths 1–8 use the sample description's colour table when its ID is 0, otherwise the
default table: 1-bit white/black; 2-bit `ff ac 55 00` greys; the 16-colour table and the 256-colour
table (6×6×6 cube with red slowest, then ten-step red, green, blue and grey ramps, then black).
Depths 33–40 are greys from white (index 0) to black.

Hostile input: reads are bounds-checked (running out of data is `Error::Truncated`; lines decoded
before stay in the picture), runs that pass the right edge are dropped at the edge (they never wrap
into the next line), lines below the picture are read and dropped, and pictures are capped at
16384 a side and 8192×8192 pixels.

## What was verified how

- **ffmpeg-encoded clips** (`tests/it/oracle.rs`): `-c:v qtrle` from `rgb24`, `rgb555be`, `argb`
  (including animated alpha) and `gray` (depth 40), 160×90 and odd sizes (37×23, 36×23), key frames
  every 4–10 frames with ffmpeg's partial-update delta frames and unchanged-picture samples: every
  frame equals ffmpeg's decode exactly.
- **Indexed depths** ffmpeg can't encode (2, 4, 8, 34, 36, 40, plus 4 and 8 with a stored colour
  table): random well-formed streams with key frames, partial updates, skips and repeats, written
  into MOVs by the test: every frame equals ffmpeg's decode exactly. The default tables and the
  units of each depth were read off ffmpeg's output on probe streams.
- **Real files** (`FILMCRAFT_QTRLE_SAMPLE=/path.mov cargo test --release -p filmcraft-qtrle --test it real_file -- --nocapture`;
  never committed): a 3840×2160 depth-32 Premiere preview render and a 1920×1080 depth-24 title
  render match ffmpeg exactly.
- **Robustness** (`tests/it/fuzz.rs`): ~60 000 mutated samples (truncation, bit flips, corrupt
  sizes and headers, random bytes, extreme counts) at every depth, with the picture carried from one
  hostile sample to the next, under `catch_unwind`: no panic.

**Not verified: depth 1 / 33.** The format document's author retracted its 1-bit description, and
ffmpeg's decoder rejects simple well-formed 1-bit lines (it reports pixel positions before the start
of the line), so there is no oracle. This decoder treats 1-bit like 2-bit with 16-pixel, 2-byte
units, which matches ffmpeg on the one case it decodes (one line, 16 pixels wide).

## Performance

`cargo test --release -p filmcraft-qtrle --test it perf:: -- --ignored --nocapture` (Apple M2 Max):
a synthetic 3840×2160 ARGB key frame decodes in about 2.6 ms (33 MB of incompressible pixels) with
the copy of the previous picture included. A real 21–26 MB 3840×2160 Premiere preview frame takes
15–19 ms (short runs): about 55–65 frames a second from one decoder, enough for 2160p at 30 fps.
