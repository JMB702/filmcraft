//! How a timeline's times are shown: a sequence's Start Time and drop-frame setting, or a media
//! clip's own timecode. Every readout, ruler and typed timecode field goes through [`TimeClock`]
//! so the offset is added (and taken off again when parsing) in exactly one place.

use filmcraft_time::{FrameRate, ParseError, Tick, TimeDisplay, fields_to_frames, format_time, frames_to_fields, parse_time_display};

use crate::{MediaClip, Sequence};

/// Time display for one timeline: times are `start`-relative ticks (0 = the first frame), shown as
/// `t + start` in `display` at `rate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeClock {
    pub rate: FrameRate,
    pub drop_frame: bool,
    /// Displayed time of tick 0 (Sequence ▸ Start Time, or a clip's media start timecode).
    pub start: Tick,
    pub display: TimeDisplay,
    /// Audio Samples display only.
    pub sample_rate: i64,
}

impl Default for TimeClock {
    fn default() -> Self {
        Self::plain(FrameRate::default(), false)
    }
}

impl TimeClock {
    /// Zero-based timecode at `rate` (durations, clip-relative times).
    pub fn plain(rate: FrameRate, drop_frame: bool) -> Self {
        Self { rate, drop_frame: drop_frame && rate.supports_drop_frame(), start: Tick::ZERO, display: TimeDisplay::Timecode, sample_rate: 48_000 }
    }

    /// The same clock in another display format.
    pub fn with_display(self, display: TimeDisplay) -> Self {
        Self { display, ..self }
    }

    /// The same clock without its start offset (durations).
    pub fn zero_based(self) -> Self {
        Self { start: Tick::ZERO, ..self }
    }

    /// The displayed time of timeline time `t` (saturating).
    pub fn shown(&self, t: Tick) -> Tick {
        t + self.start
    }

    /// The displayed frame number of timeline time `t`.
    pub fn shown_frame(&self, t: Tick) -> i64 {
        self.rate.frame_at(self.shown(t))
    }

    /// Timeline time `t` in this clock's display format.
    pub fn format(&self, t: Tick) -> String {
        format_time(self.shown(t), self.rate, self.drop_frame, self.display, self.sample_rate)
    }

    /// Timeline time `t` as SMPTE timecode, whatever the display format.
    pub fn timecode(&self, t: Tick) -> String {
        format_time(self.shown(t), self.rate, self.drop_frame, TimeDisplay::Timecode, self.sample_rate)
    }

    /// A duration (no start offset) in this clock's display format.
    pub fn duration(&self, d: Tick) -> String {
        self.zero_based().format(d)
    }

    /// Typed text in the display format, as timeline time: the start offset is taken off again.
    /// Relative input (`+15`, `-1.00`) counts from `current` (timeline time).
    pub fn parse(&self, input: &str, current: Tick) -> Result<Tick, ParseError> {
        self.parse_as(input, self.display, current)
    }

    /// Typed SMPTE timecode, as timeline time (see [`parse`](Self::parse)).
    pub fn parse_timecode(&self, input: &str, current: Tick) -> Result<Tick, ParseError> {
        self.parse_as(input, TimeDisplay::Timecode, current)
    }

    fn parse_as(&self, input: &str, display: TimeDisplay, current: Tick) -> Result<Tick, ParseError> {
        let f = parse_time_display(input, self.rate, self.drop_frame, display, self.shown_frame(current))?;
        Ok(self.rate.tick_of(f) - self.start)
    }

    /// Ruler marks from timeline time `from` to `to`: one every `step` timecode labels, on round
    /// displayed timecode, as `(timeline time, label number)` (`label number % n == 0` picks the
    /// marks that are also on an `n`-label grid). Drop-frame timecode labels count by their nominal
    /// number, so marks fall on `16;49;18;00`, not on multiples of the frame count; a mark on a
    /// dropped label (`;00`, `;01` at most minute starts) moves to the minute's first label. Other
    /// display formats count frames. At most `max`.
    pub fn ruler_marks(&self, from: Tick, to: Tick, step: i64, max: usize) -> Vec<(Tick, i64)> {
        let step = step.max(1);
        let (n0, n1) = (self.label_number(self.shown_frame(from)), self.label_number(self.shown_frame(to)));
        let mut n = n0.div_euclid(step).saturating_mul(step);
        if n < n0 {
            n = n.saturating_add(step);
        }
        let mut out: Vec<(Tick, i64)> = Vec::new();
        while n <= n1 && out.len() < max {
            let at = self.rate.tick_of(self.frame_of_label(n)) - self.start;
            // two dropped labels can move to the same frame
            if out.last().is_none_or(|(t, _)| *t != at) {
                out.push((at, n));
            }
            let next = n.saturating_add(step);
            if next == n {
                break;
            }
            n = next;
        }
        out
    }

    /// The nominal label number of displayed frame `frame` (`HH:MM:SS:FF` read as a plain count);
    /// the frame itself unless drop-frame.
    fn label_number(&self, frame: i64) -> i64 {
        if !self.counts_labels() || frame < 0 {
            return frame;
        }
        let (_, h, m, s, f) = frames_to_fields(frame, self.rate, true);
        fields_to_frames(h, m, s, f, self.rate, false)
    }

    /// Whether ruler marks count drop-frame timecode labels rather than frames.
    fn counts_labels(&self) -> bool {
        self.drop_frame && self.display == TimeDisplay::Timecode
    }

    /// The displayed frame of nominal label number `n`; a dropped label gives the minute's first.
    fn frame_of_label(&self, n: i64) -> i64 {
        if !self.counts_labels() || n < 0 {
            return n;
        }
        let base = self.rate.timecode_base().max(1);
        let (secs, f) = (n.div_euclid(base), n.rem_euclid(base));
        let (mins, s) = (secs / 60, secs % 60);
        let f = if s == 0 && mins % 10 != 0 { f.max(base / 15) } else { f };
        fields_to_frames(mins / 60, mins % 60, s, f, self.rate, true)
    }
}

impl Sequence {
    /// Sequence ▸ Start Time as a tick (the displayed time of the sequence's first frame).
    pub fn start_tick(&self) -> Tick {
        self.settings.frame_rate.tick_of(self.start_timecode)
    }

    /// How this sequence's times are shown: Start Time, drop-frame and Display Format.
    pub fn clock(&self) -> TimeClock {
        let s = &self.settings;
        TimeClock {
            rate: s.frame_rate,
            drop_frame: s.drop_frame && s.frame_rate.supports_drop_frame(),
            start: self.start_tick(),
            display: s.display,
            sample_rate: i64::from(s.sample_rate),
        }
    }
}

impl MediaClip {
    /// Whether the clip's timecode counts in drop-frame (Modify ▸ Timecode, or the file's own
    /// timecode track). Rates without drop-frame counting never do.
    pub fn timecode_drop_frame(&self) -> bool {
        self.interpret.timecode_drop_frame.unwrap_or(false) && self.frame_rate().supports_drop_frame()
    }

    /// The clip's media time shown as its source timecode (media start + time).
    pub fn clock(&self) -> TimeClock {
        let rate = self.frame_rate();
        let start = self.info.start_timecode.map(|f| rate.tick_of(f)).unwrap_or(Tick::ZERO);
        TimeClock { start, ..TimeClock::plain(rate, self.timecode_drop_frame()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_time::{ParseError, parse_timecode};

    fn df_clock(start: &str) -> TimeClock {
        let rate = FrameRate::FPS_29_97;
        let f = parse_timecode(start, rate, true, 0).unwrap();
        TimeClock { start: rate.tick_of(f), ..TimeClock::plain(rate, true) }
    }

    #[test]
    fn readout_adds_the_start_time_and_parse_takes_it_off() {
        let c = df_clock("16:49:16;19");
        let rate = c.rate;
        assert_eq!(c.format(Tick::ZERO), "16;49;16;19");
        assert_eq!(c.format(rate.tick_of(30)), "16;49;17;19");
        // across a dropped label: 16;50;59;29 + 1 frame = 16;51;00;02 (16;50 is a tenth minute: none)
        let at = c.parse("16:50:59;29", Tick::ZERO).unwrap();
        assert_eq!(c.format(at + rate.frame_duration()), "16;51;00;02");
        let at = c.parse("16:49:59;29", Tick::ZERO).unwrap();
        assert_eq!(c.format(at + rate.frame_duration()), "16;50;00;00");
        // parse-back: the typed displayed time is the timeline time
        assert_eq!(c.parse("16:49:17;19", Tick::ZERO).unwrap(), rate.tick_of(30));
        assert_eq!(c.parse_timecode("16:49:16;19", Tick::ZERO).unwrap(), Tick::ZERO);
        // relative input counts from the current time
        assert_eq!(c.parse("+15", rate.tick_of(30)).unwrap(), rate.tick_of(45));
        // before the start: a negative timeline time (the caller clamps)
        assert!(c.parse("16:49:16;18", Tick::ZERO).unwrap() < Tick::ZERO);
        // durations never take the offset
        assert_eq!(c.duration(rate.tick_of(30)), "00;00;01;00");
        // other display formats shift too
        assert_eq!(c.with_display(TimeDisplay::Frames).format(rate.tick_of(1)), (c.shown_frame(Tick::ZERO) + 1).to_string());
    }

    #[test]
    fn non_drop_frame_sequence_counts_nominal_frames() {
        let rate = FrameRate::FPS_29_97;
        let f = parse_timecode("16:49:16:19", rate, false, 0).unwrap();
        assert_eq!(f, (((16 * 60 + 49) * 60 + 16) * 30 + 19));
        let c = TimeClock { start: rate.tick_of(f), ..TimeClock::plain(rate, false) };
        assert_eq!(c.format(rate.tick_of(30 * 60)), "16:50:16:19");
        assert_eq!(c.parse("16:50:16:19", Tick::ZERO).unwrap(), rate.tick_of(30 * 60));
    }

    #[test]
    fn hostile_input_is_an_error_and_huge_starts_saturate() {
        let c = df_clock("16:49:16;19");
        for bad in ["", "abc", "1:2:3:4:5", "--3", "12:x"] {
            assert!(matches!(c.parse(bad, Tick::ZERO), Err(ParseError(_))), "{bad}");
        }
        let huge = TimeClock { start: Tick(i64::MAX), ..TimeClock::plain(FrameRate::FPS_29_97, true) };
        let _ = huge.format(Tick(i64::MAX));
        let _ = huge.parse("99999999:99:99:99", Tick(i64::MIN));
        // drop-frame on a rate without it is non-drop
        assert!(!TimeClock::plain(FrameRate::FPS_25, true).drop_frame);
    }

    #[test]
    fn sequence_and_clip_clocks() {
        let mut p = crate::Project::new("x");
        let settings = crate::SequenceSettings { frame_rate: FrameRate::FPS_29_97, drop_frame: true, ..Default::default() };
        let id = p.new_sequence("D", settings, 1, 1, None);
        let q = p.sequence_mut(id).unwrap();
        q.start_timecode = parse_timecode("16:49:16;19", FrameRate::FPS_29_97, true, 0).unwrap();
        assert_eq!(q.clock().format(Tick::ZERO), "16;49;16;19");
        assert_eq!(q.start_tick(), FrameRate::FPS_29_97.tick_of(q.start_timecode));
        // an older project's interpretation has no drop-frame flag: non-drop
        let i: crate::Interpretation = serde_json::from_str(r#"{"frame_rate":null,"par":null,"ignore_alpha":false,"invert_alpha":false}"#).unwrap();
        assert_eq!(i.timecode_drop_frame, None);
        // and a default one writes none
        assert!(!serde_json::to_string(&crate::Interpretation::default()).unwrap().contains("timecode_drop_frame"));
    }

    #[test]
    fn ruler_marks_fall_on_round_displayed_timecode() {
        let c = df_clock("16:49:16;19");
        let rate = c.rate;
        let labels = |from: i64, to: i64, step: i64| -> Vec<String> {
            c.ruler_marks(rate.tick_of(from), rate.tick_of(to), step, 100).iter().map(|(t, _)| c.timecode(*t)).collect()
        };
        // every 2 s over the first 5 s: round seconds, not multiples of the frame count
        assert_eq!(labels(0, 150, 60), ["16;49;18;00", "16;49;20;00"]);
        // every second across a dropped minute start: 16;51;00;00 is no label, the mark is on ;02
        let from = c.parse("16:50:58;00", Tick::ZERO).unwrap();
        let f0 = rate.frame_at(from);
        assert_eq!(labels(f0, f0 + 95, 30), ["16;50;58;00", "16;50;59;00", "16;51;00;02", "16;51;01;00"]);
        // single frames over a dropped label: no duplicate marks
        let marks = c.ruler_marks(rate.tick_of(f0 + 58), rate.tick_of(f0 + 62), 1, 100);
        assert!(marks.windows(2).all(|w| w[0].0 < w[1].0), "{marks:?}");
        // non-drop: plain multiples of the frame count
        let n = TimeClock { start: rate.tick_of(1_816_699), ..TimeClock::plain(rate, false) };
        let marks: Vec<String> = n.ruler_marks(Tick::ZERO, rate.tick_of(150), 60, 100).iter().map(|(t, _)| n.timecode(*t)).collect();
        assert_eq!(marks, ["16:49:18:00", "16:49:20:00"]);
        // damaged numbers stop at `max`
        assert_eq!(c.ruler_marks(Tick(i64::MIN), Tick(i64::MAX), 1, 10).len(), 10);
        assert!(c.ruler_marks(Tick::ZERO, Tick(i64::MAX), i64::MAX, 10).len() <= 10);
    }
}
