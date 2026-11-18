//! Exact time for review comments. The page and the server speak in frames of the rendered file;
//! the app turns a frame into sequence ticks with the sequence's own rational frame rate, so a
//! comment lands on the frame the reviewer saw, at any rate (23.976, 29.97 drop-frame, 59.94…).

use filmcraft_time::{FrameRate, Tick, format_timecode_frames};

/// The frame rate of a share, if it is a usable one (positive numerator and denominator).
pub fn rate(num: i64, den: i64) -> Option<FrameRate> {
    (num > 0 && den > 0).then(|| FrameRate::new(num, den))
}

/// Sequence time of `frame` of a rendered file that starts at `range_start` and has
/// `duration_frames` frames. `None` for a frame outside the file or a time out of range.
pub fn frame_to_tick(rate: FrameRate, range_start: Tick, duration_frames: i64, frame: i64) -> Option<Tick> {
    if frame < 0 || frame >= duration_frames.max(1) {
        return None;
    }
    let offset = rate.tick_of(frame);
    // tick_of saturates; a saturated value is not this frame
    if offset.0 < 0 || rate.frame_at(offset) != frame || offset.0 == i64::MAX {
        return None;
    }
    range_start.0.checked_add(offset.0).map(Tick)
}

/// Timecode frame number of the file's first frame: the sequence's Start Time plus the frames
/// before the rendered range.
pub fn tc_base(rate: FrameRate, start_timecode: i64, range_start: Tick) -> i64 {
    rate.frame_at(range_start).saturating_add(start_timecode)
}

/// The timecode the sequence shows for `frame` of the rendered file (`tc_base` from [`tc_base`]).
pub fn timecode(rate: FrameRate, drop_frame: bool, tc_base: i64, frame: i64) -> String {
    format_timecode_frames(tc_base.saturating_add(frame), rate, drop_frame)
}

/// The frame shown at `seconds` into the file (the browser's `currentTime`). A frame is shown from
/// its start up to the next one; the small epsilon absorbs the float rounding of a seek to a
/// frame's start. `None` for NaN or a negative time.
pub fn frame_at_seconds(rate: FrameRate, seconds: f64) -> Option<i64> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let f = (seconds * rate.num as f64 / rate.den as f64 + 1e-4).floor();
    (f < i64::MAX as f64).then_some(f as i64)
}

/// Where to seek to show `frame`: the middle of the frame, so neither rounding nor a decoder's
/// own frame timing can land on the neighbour.
pub fn seconds_of_frame(rate: FrameRate, frame: i64) -> f64 {
    (frame.max(0) as f64 + 0.5) * rate.den as f64 / rate.num as f64
}

/// Parse a reviewer-typed timecode (`HH:MM:SS:FF`, `;` for drop-frame, or fewer fields) into a
/// frame of the file. Used by the page's no-script form.
pub fn parse_frame(rate: FrameRate, drop_frame: bool, tc_base: i64, duration_frames: i64, text: &str) -> Option<i64> {
    let text = text.trim();
    // relative entries (`+10`) are an editor feature, not a place in the file
    if text.is_empty() || text.len() > 24 || text.starts_with(['+', '-']) {
        return None;
    }
    let tc_frame = filmcraft_time::parse_timecode(text, rate, drop_frame, 0).ok()?;
    let frame = tc_frame.checked_sub(tc_base)?;
    (0..duration_frames.max(1)).contains(&frame).then_some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_time::TICKS_PER_SECOND;

    #[test]
    fn frames_map_to_exact_sequence_ticks_at_broadcast_rates() {
        for (num, den) in [(24000, 1001), (24, 1), (25, 1), (30000, 1001), (60000, 1001), (50, 1)] {
            let r = rate(num, den).unwrap();
            let start = r.tick_of(100);
            for f in [0, 1, 2, 999, 86_399] {
                let t = frame_to_tick(r, start, 1_000_000, f).unwrap();
                // the tick is exactly that frame's start, and maps back to it
                assert_eq!(t, r.tick_of(100 + f), "{num}/{den} frame {f}");
                assert_eq!(r.frame_at(t), 100 + f);
                // what the browser reports while showing the frame maps back to it, and seeking to
                // the frame's middle shows it
                let mid = seconds_of_frame(r, f);
                assert_eq!(frame_at_seconds(r, mid), Some(f));
                let at_start = f as f64 * den as f64 / num as f64;
                assert_eq!(frame_at_seconds(r, at_start), Some(f), "{num}/{den} start of frame {f}");
            }
        }
        // 23.976: one frame is 1001/24000 s exactly
        let r = rate(24000, 1001).unwrap();
        assert_eq!(frame_to_tick(r, Tick::ZERO, 10, 1).unwrap().0, TICKS_PER_SECOND * 1001 / 24000);
    }

    #[test]
    fn hostile_frames_and_rates_are_refused() {
        let r = rate(25, 1).unwrap();
        assert_eq!(frame_to_tick(r, Tick::ZERO, 10, -1), None);
        assert_eq!(frame_to_tick(r, Tick::ZERO, 10, 10), None);
        assert_eq!(frame_to_tick(r, Tick::ZERO, i64::MAX, i64::MAX - 1), None);
        assert_eq!(frame_to_tick(r, Tick(i64::MAX - 5), 10, 3), None);
        assert!(rate(0, 1).is_none() && rate(25, 0).is_none() && rate(-25, 1).is_none());
        assert_eq!(frame_at_seconds(r, f64::NAN), None);
        assert_eq!(frame_at_seconds(r, -0.5), None);
        assert_eq!(frame_at_seconds(r, f64::INFINITY), None);
        assert_eq!(frame_at_seconds(r, 1e300), None);
        // tc_base and timecode never overflow
        assert_eq!(tc_base(r, i64::MAX, Tick(i64::MAX)), i64::MAX);
        let _ = timecode(r, false, i64::MAX, i64::MAX);
    }

    #[test]
    fn timecode_follows_the_sequence_start_time_and_drop_frame() {
        // 01:00:00:00 start at 25 fps; frame 30 is 01:00:01:05
        let r = rate(25, 1).unwrap();
        let base = tc_base(r, 90_000, Tick::ZERO);
        assert_eq!(timecode(r, false, base, 30), "01:00:01:05");
        // a range that starts 2 s into the sequence
        let base = tc_base(r, 90_000, r.tick_of(50));
        assert_eq!(timecode(r, false, base, 0), "01:00:02:00");
        // 29.97 drop-frame: frame 1800 is 00:01:00;02
        let df = rate(30000, 1001).unwrap();
        assert_eq!(timecode(df, true, 0, 1800), "00;01;00;02");
        assert_eq!(timecode(df, false, 0, 1800), "00:01:00:00");
    }

    #[test]
    fn typed_timecode_parses_to_a_frame_of_the_file() {
        let r = rate(25, 1).unwrap();
        let base = tc_base(r, 90_000, Tick::ZERO);
        assert_eq!(parse_frame(r, false, base, 1000, "01:00:01:05"), Some(30));
        assert_eq!(parse_frame(r, false, base, 1000, "00:00:00:00"), None);
        assert_eq!(parse_frame(r, false, base, 10, "01:00:01:05"), None);
        assert_eq!(parse_frame(r, false, base, 1000, "garbage"), None);
        assert_eq!(parse_frame(r, false, base, 1000, &"9".repeat(500)), None);
    }
}
