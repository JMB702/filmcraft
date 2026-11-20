//! What the review page decides, apart from the DOM: which comments stand, which file to play,
//! which frame is on screen, where a tick or the playhead sits on the scrub bar. Every value from
//! the server or the browser is treated as hostile (NaN times, zero rates, huge frames).

use filmcraft_review::model::{Comment, CommentEvent, EventKind, Rendition, ShareInfo};
use filmcraft_review::{text, timing, token};
use filmcraft_time::FrameRate;

/// `localStorage` keys. The name and device secret are per device, for every review link.
pub const NAME_KEY: &str = "filmcraft-review-name";
pub const DEVICE_KEY: &str = "filmcraft-review-device";
pub const QUALITY_KEY: &str = "filmcraft-review-quality";
/// Comment ids this device wrote, per link (prefix + the first 16 characters of the token).
pub const OWN_KEY: &str = "filmcraft-review-own-";
/// At most this many own comment ids are remembered per link.
pub const MAX_OWN: usize = 500;

/// The link's base path (`/r/<token>/`) from `location.pathname`, if it is a review link.
pub fn base_path(pathname: &str) -> Option<String> {
    let rest = pathname.strip_prefix("/r/")?;
    let tok = rest.split('/').next()?;
    token::is_token(tok).then(|| format!("/r/{tok}/"))
}

/// The share's timing, checked once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    pub rate: FrameRate,
    pub drop_frame: bool,
    pub tc_base: i64,
    /// Frames in the file (at least 1).
    pub frames: i64,
}

impl Timing {
    /// `None` when the share's frame rate is unusable.
    pub fn of(info: &ShareInfo) -> Option<Timing> {
        Some(Timing {
            rate: timing::rate(info.rate_num, info.rate_den)?,
            drop_frame: info.drop_frame,
            tc_base: info.tc_base,
            frames: info.duration_frames.max(1),
        })
    }

    /// A frame clamped into the file.
    pub fn clamp(&self, frame: i64) -> i64 {
        frame.clamp(0, self.frames - 1)
    }

    /// The frame on screen at the video's `currentTime` (0 for NaN or a negative time).
    pub fn frame_at(&self, seconds: f64) -> i64 {
        match timing::frame_at_seconds(self.rate, seconds) {
            Some(f) => self.clamp(f),
            // a time too large for a frame number is past the end
            None if seconds.is_finite() && seconds > 0.0 => self.frames - 1,
            None => 0,
        }
    }

    /// Where to seek to show `frame` (its middle).
    pub fn seek_to(&self, frame: i64) -> f64 {
        timing::seconds_of_frame(self.rate, self.clamp(frame))
    }

    /// The sequence's own timecode of `frame`.
    pub fn timecode(&self, frame: i64) -> String {
        timing::timecode(self.rate, self.drop_frame, self.tc_base, self.clamp(frame))
    }

    /// `frame` stepped by `delta` frames, kept in the file.
    pub fn step(&self, frame: i64, delta: i64) -> i64 {
        self.clamp(frame.saturating_add(delta))
    }

    /// `frame` moved by `seconds` (the ±10 s buttons), kept in the file. A whole number of
    /// frames, so the jump lands on a frame like the one-frame steps do.
    pub fn jump(&self, frame: i64, seconds: f64) -> i64 {
        let n = timing::frame_at_seconds(self.rate, seconds.abs()).unwrap_or(0);
        self.step(frame, if seconds < 0.0 { n.saturating_neg() } else { n })
    }

    /// Position of `frame` on the scrub bar, 0–100 %. The last frame sits at the right end.
    pub fn percent(&self, frame: i64) -> f64 {
        if self.frames <= 1 {
            return 0.0;
        }
        self.clamp(frame) as f64 * 100.0 / (self.frames - 1) as f64
    }

    /// The frame under a pointer at `x` on a track `width` wide (both CSS pixels).
    pub fn frame_at_x(&self, x: f64, width: f64) -> i64 {
        if !x.is_finite() || !width.is_finite() || width <= 0.0 {
            return 0;
        }
        let f = (x / width).clamp(0.0, 1.0) * (self.frames - 1) as f64;
        self.clamp(f.round() as i64)
    }
}

/// What the browser says about the connection and screen.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Device {
    /// CSS pixels.
    pub viewport_width: f64,
    pub save_data: bool,
    /// `navigator.connection.type == "cellular"` (Android Chrome only).
    pub cellular: bool,
    /// `navigator.connection.effectiveType` is `slow-2g`, `2g` or `3g`.
    pub slow: bool,
}

/// The file to play: the reviewer's choice when it is ready, else small on phones, cellular, slow
/// links or Save-Data, else the largest. Renditions are smallest first.
pub fn pick_rendition(ready: &[Rendition], choice: Option<&str>, d: Device) -> Option<usize> {
    if ready.is_empty() {
        return None;
    }
    if let Some(i) = choice.and_then(|c| ready.iter().position(|r| r.name == c)) {
        return Some(i);
    }
    let phone = d.viewport_width.is_finite() && d.viewport_width > 0.0 && d.viewport_width < 700.0;
    if d.save_data || d.cellular || d.slow || phone { Some(0) } else { Some(ready.len() - 1) }
}

/// The quality chip's label for a rendition.
pub fn quality_label(r: &Rendition) -> &'static str {
    if r.height >= 720 || r.width >= 1280 { "HD" } else { "SD" }
}

/// Apply a live event to the comment list (kept in frame, then time order). Edits and deletes
/// reach the page only from the device that owns the comment, so they are applied as they come.
/// Returns whether anything changed.
pub fn apply(comments: &mut Vec<Comment>, ev: &CommentEvent) -> bool {
    match ev.kind {
        EventKind::Add => {
            if comments.iter().any(|c| c.id == ev.comment) {
                return false;
            }
            comments.push(Comment {
                id: ev.comment.clone(),
                author: ev.author.clone(),
                text: ev.text.clone(),
                frame: ev.frame,
                at_unix: ev.at_unix,
                edited: false,
            });
            sort(comments);
            true
        }
        EventKind::Edit => match comments.iter_mut().find(|c| c.id == ev.comment) {
            Some(c) if c.text != ev.text => {
                c.text = ev.text.clone();
                c.edited = true;
                true
            }
            _ => false,
        },
        EventKind::Delete => {
            let n = comments.len();
            comments.retain(|c| c.id != ev.comment);
            comments.len() != n
        }
    }
}

/// Add the comment the server acknowledged (the live event may come first or after).
pub fn add_posted(comments: &mut Vec<Comment>, c: Comment) -> bool {
    if comments.iter().any(|x| x.id == c.id) {
        return false;
    }
    comments.push(c);
    sort(comments);
    true
}

fn sort(comments: &mut [Comment]) {
    comments.sort_by(|a, b| a.frame.cmp(&b.frame).then(a.at_unix.cmp(&b.at_unix)));
}

/// "No comments yet", "1 comment", "12 comments".
pub fn count_label(n: usize) -> String {
    match n {
        0 => "No comments yet".into(),
        1 => "1 comment".into(),
        n => format!("{n} comments"),
    }
}

/// A reviewer's name as typed, cleaned like the server does (`None`: empty).
pub fn clean_name(s: &str) -> Option<String> {
    text::clean(s, text::MAX_NAME, true)
}

/// Comment text as typed, cleaned like the server does (`None`: empty).
pub fn clean_comment(s: &str) -> Option<String> {
    text::clean(s, text::MAX_COMMENT, false)
}

/// A stored device secret, if it has the right shape.
pub fn valid_device(s: &str) -> bool {
    text::is_hex(s, token::DEVICE_HEX)
}

/// Own comment ids from storage (comma-separated), dropping anything that is not an id.
pub fn parse_own(s: &str) -> Vec<String> {
    s.split(',').filter(|id| text::is_hex(id, token::ID_HEX)).take(MAX_OWN).map(str::to_string).collect()
}

/// Own comment ids for storage, newest kept when over the cap.
pub fn store_own(ids: &[String]) -> String {
    let skip = ids.len().saturating_sub(MAX_OWN);
    ids[skip..].join(",")
}

/// The scripted page's box (review.css `html.wasm body`): it is sized to the part of the screen
/// the reviewer can see, so the video stays at the top, only the comment list scrolls and the
/// composer sits right above the on-screen keyboard. iOS Safari keeps the layout viewport when the
/// keyboard opens and shrinks (and may shift) the visual viewport, which this follows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shell {
    /// Height in CSS px.
    pub height: f64,
    /// Offset from the layout viewport's top in CSS px.
    pub top: f64,
    /// The on-screen keyboard is up (a text field has focus and the view shrank): the page drops
    /// its header and transport so the video, the list and the composer still fit.
    pub keyboard: bool,
}

/// Keyboard detection: the view has to shrink by more than this while a field has focus.
const KEYBOARD_MIN: f64 = 120.0;

/// The page's size with no keyboard up. On iOS the keyboard can shrink the layout viewport itself
/// (newer Safari, third-party keyboards), so a portrait phone with the keyboard up looks wider
/// than tall. Layout and keyboard decisions use this size instead of the live one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rest {
    /// Width in CSS px.
    pub width: f64,
    /// Height in CSS px.
    pub height: f64,
}

impl Rest {
    /// The resting size after a viewport change. While a text field has focus the size measured
    /// before typing stands; a change of width (the phone turned) takes the new size.
    pub fn next(prev: Option<Rest>, width: f64, height: f64, typing: bool) -> Option<Rest> {
        let usable = |v: f64| v.is_finite() && v > 0.0;
        if !usable(width) || !usable(height) {
            return prev;
        }
        match prev {
            Some(p) if typing && (p.width - width).abs() < 1.0 => Some(Rest { height: p.height.max(height), ..p }),
            _ => Some(Rest { width, height }),
        }
    }

    /// A phone on its side: the header steps aside and the comments go beside the picture.
    pub fn low(self) -> bool {
        self.width > self.height && self.height <= 500.0
    }

    /// The comments beside the picture: wide screens and phones on their side.
    pub fn side_by_side(self) -> bool {
        self.width >= 900.0 || self.low()
    }
}

/// The shell box from `window.innerHeight`, the resting height (see [`Rest`]) and
/// `visualViewport` (height, offsetTop, scale). `None`: leave the page at its normal size
/// (nothing usable, or the reviewer pinch-zoomed and needs to pan around).
pub fn shell(inner_height: f64, rest_height: f64, vv_height: f64, vv_offset_top: f64, vv_scale: f64, typing: bool) -> Option<Shell> {
    let usable = |v: f64| v.is_finite() && v > 0.0;
    if !usable(inner_height) || !usable(vv_height) || !vv_offset_top.is_finite() || !usable(vv_scale) || (vv_scale - 1.0).abs() > 0.01 {
        return None;
    }
    let height = vv_height.min(inner_height).round();
    let top = vv_offset_top.max(0.0).round();
    let full = if usable(rest_height) { rest_height.max(inner_height) } else { inner_height };
    Some(Shell { height, top, keyboard: typing && full - height > KEYBOARD_MIN })
}

/// The error text of a failed request: the server's `{"error": …}`, else a generic one.
pub fn error_text(status: u16, body: &str) -> String {
    let server = serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
    match server {
        Some(e) if !e.is_empty() => text::clean(&e, 300, true).unwrap_or_else(|| "The comment was not posted.".into()),
        _ if status == 0 => "No connection. Your comment is still here; try again.".into(),
        _ => format!("The comment was not posted (error {status})."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(num: i64, den: i64, frames: i64) -> ShareInfo {
        ShareInfo {
            title: "t".into(),
            rate_num: num,
            rate_den: den,
            drop_frame: false,
            tc_base: 90_000,
            duration_frames: frames,
            width: 1920,
            height: 1080,
            renditions: vec![],
            comments: vec![],
        }
    }

    fn r(name: &str, w: u32, h: u32) -> Rendition {
        Rendition { name: name.into(), width: w, height: h, bitrate_kbps: 1 }
    }

    fn ev(kind: EventKind, comment: &str, text: &str, frame: i64, at: i64) -> CommentEvent {
        CommentEvent { id: format!("e{at}"), kind, comment: comment.into(), author: "A".into(), text: text.into(), frame, device: String::new(), at_unix: at }
    }

    #[test]
    fn the_link_path_must_hold_a_token() {
        let tok = "ab".repeat(32);
        assert_eq!(base_path(&format!("/r/{tok}/")), Some(format!("/r/{tok}/")));
        assert_eq!(base_path(&format!("/r/{tok}")), Some(format!("/r/{tok}/")));
        assert_eq!(base_path("/r/abc/"), None);
        assert_eq!(base_path("/"), None);
        assert_eq!(base_path(&format!("/x/{tok}/")), None);
        assert_eq!(base_path(&format!("/r/{}/", "AB".repeat(32))), None);
    }

    #[test]
    fn frames_step_and_show_the_sequence_timecode() {
        let t = Timing::of(&info(25, 1, 250)).unwrap();
        assert_eq!(t.timecode(30), "01:00:01:05");
        assert_eq!(t.step(0, -1), 0);
        assert_eq!(t.step(249, 1), 249);
        assert_eq!(t.step(10, i64::MAX), 249);
        // ten seconds is 250 frames at 25 and 299 at 29.97; never past either end
        let big = Timing::of(&info(25, 1, 1000)).unwrap();
        assert_eq!((big.jump(0, 10.0), big.jump(400, -10.0), big.jump(100, -10.0), big.jump(900, 10.0)), (250, 150, 0, 999));
        let ntsc = Timing::of(&info(30000, 1001, 9146)).unwrap();
        assert_eq!((ntsc.jump(195, 10.0), ntsc.jump(195, -10.0)), (494, 0));
        assert_eq!((big.jump(5, f64::NAN), big.jump(5, f64::INFINITY), big.jump(5, -1e300)), (5, 5, 5));
        // seeking to a frame shows that frame
        for f in [0, 1, 124, 249] {
            assert_eq!(t.frame_at(t.seek_to(f)), f);
        }
        assert_eq!(t.frame_at(f64::NAN), 0);
        assert_eq!(t.frame_at(1e300), 249);
        assert!(Timing::of(&info(0, 1, 10)).is_none());
        assert!(Timing::of(&info(25, 0, 10)).is_none());
        // a zero-length share still has one frame
        let one = Timing::of(&info(25, 1, 0)).unwrap();
        assert_eq!((one.frames, one.clamp(5), one.percent(5)), (1, 0, 0.0));
        // 29.97 drop-frame
        let mut i = info(30000, 1001, 10_000);
        i.drop_frame = true;
        i.tc_base = 0;
        let df = Timing::of(&i).unwrap();
        assert_eq!(df.timecode(1800), "00;01;00;02");
        assert_eq!(df.frame_at(df.seek_to(1800)), 1800);
    }

    #[test]
    fn scrub_positions_map_both_ways() {
        let t = Timing::of(&info(24, 1, 101)).unwrap();
        assert_eq!(t.percent(0), 0.0);
        assert_eq!(t.percent(50), 50.0);
        assert_eq!(t.percent(100), 100.0);
        assert_eq!(t.percent(-5), 0.0);
        assert_eq!(t.frame_at_x(150.0, 300.0), 50);
        assert_eq!(t.frame_at_x(-20.0, 300.0), 0);
        assert_eq!(t.frame_at_x(900.0, 300.0), 100);
        assert_eq!(t.frame_at_x(10.0, 0.0), 0);
        assert_eq!(t.frame_at_x(f64::NAN, 300.0), 0);
    }

    #[test]
    fn phones_and_slow_links_get_the_small_file() {
        let ready = [r("small", 960, 540), r("hd", 1920, 1080)];
        let desk = Device { viewport_width: 1400.0, ..Default::default() };
        assert_eq!(pick_rendition(&ready, None, desk), Some(1));
        assert_eq!(pick_rendition(&ready, None, Device { viewport_width: 390.0, ..Default::default() }), Some(0));
        assert_eq!(pick_rendition(&ready, None, Device { save_data: true, ..desk }), Some(0));
        assert_eq!(pick_rendition(&ready, None, Device { cellular: true, ..desk }), Some(0));
        assert_eq!(pick_rendition(&ready, None, Device { slow: true, ..desk }), Some(0));
        // the reviewer's choice wins when that file is ready
        assert_eq!(pick_rendition(&ready, Some("hd"), Device { viewport_width: 390.0, ..Default::default() }), Some(1));
        assert_eq!(pick_rendition(&ready[..1], Some("hd"), desk), Some(0));
        assert_eq!(pick_rendition(&[], None, desk), None);
        assert_eq!(pick_rendition(&ready, None, Device { viewport_width: f64::NAN, ..Default::default() }), Some(1));
        assert_eq!((quality_label(&ready[0]), quality_label(&ready[1])), ("SD", "HD"));
    }

    #[test]
    fn live_events_keep_the_list_in_frame_order_without_duplicates() {
        let mut v = Vec::new();
        assert!(apply(&mut v, &ev(EventKind::Add, "b", "late", 50, 1)));
        assert!(apply(&mut v, &ev(EventKind::Add, "a", "early", 10, 2)));
        assert!(!apply(&mut v, &ev(EventKind::Add, "a", "dup", 10, 3)));
        assert_eq!(v.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        // the post's own reply and the live event, in either order
        assert!(!add_posted(&mut v, Comment { id: "a".into(), author: "A".into(), text: "early".into(), frame: 10, at_unix: 2, edited: false }));
        assert!(apply(&mut v, &ev(EventKind::Edit, "a", "changed", 0, 4)));
        assert!(v[0].edited && v[0].text == "changed" && v[0].frame == 10);
        assert!(!apply(&mut v, &ev(EventKind::Edit, "zz", "x", 0, 5)));
        assert!(apply(&mut v, &ev(EventKind::Delete, "a", "", 0, 6)));
        assert!(!apply(&mut v, &ev(EventKind::Delete, "a", "", 0, 7)));
        assert_eq!(v.len(), 1);
        assert_eq!(count_label(0), "No comments yet");
        assert_eq!(count_label(1), "1 comment");
        assert_eq!(count_label(7), "7 comments");
    }

    #[test]
    fn stored_values_are_checked_before_use() {
        assert!(valid_device(&"0f".repeat(16)));
        assert!(!valid_device("short"));
        assert!(!valid_device(&"zz".repeat(16)));
        let own = parse_own("0123456789abcdef,<script>,fedcba9876543210,,");
        assert_eq!(own, ["0123456789abcdef", "fedcba9876543210"]);
        let many: Vec<String> = (0..MAX_OWN + 10).map(|i| format!("{i:016x}")).collect();
        let stored = store_own(&many);
        assert_eq!(parse_own(&stored).len(), MAX_OWN);
        assert!(stored.starts_with(&format!("{:016x}", 10)));
        assert_eq!(clean_name("  Ana \u{202e}  "), Some("Ana".into()));
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_comment("a\u{0}b"), Some("ab".into()));
    }

    #[test]
    fn the_page_fits_the_visible_screen_and_errors_read_well() {
        // keyboard up while typing: the page is the part above it, shifted where iOS scrolled
        assert_eq!(shell(812.0, 812.0, 430.4, 0.0, 1.0, true), Some(Shell { height: 430.0, top: 0.0, keyboard: true }));
        assert_eq!(shell(812.0, 812.0, 430.0, 96.0, 1.0, true), Some(Shell { height: 430.0, top: 96.0, keyboard: true }));
        // the same shrink without a focused field (toolbar, split view) is not a keyboard
        assert_eq!(shell(812.0, 812.0, 430.0, 0.0, 1.0, false), Some(Shell { height: 430.0, top: 0.0, keyboard: false }));
        // a small shrink (Safari's toolbar) is not a keyboard either
        assert_eq!(shell(812.0, 812.0, 760.0, 0.0, 1.0, true), Some(Shell { height: 760.0, top: 0.0, keyboard: false }));
        assert_eq!(shell(812.0, 812.0, 900.0, -5.0, 1.0, false), Some(Shell { height: 812.0, top: 0.0, keyboard: false }));
        // pinch zoom: leave the page alone so the reviewer can pan
        assert_eq!(shell(812.0, 812.0, 406.0, 100.0, 2.0, false), None);
        for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert_eq!(shell(bad, 812.0, 400.0, 0.0, 1.0, true), None);
            assert_eq!(shell(812.0, 812.0, bad, 0.0, 1.0, true), None);
            assert_eq!(shell(812.0, 812.0, 400.0, 0.0, bad, true), None);
        }
        assert_eq!(shell(812.0, 812.0, 400.0, f64::NAN, 1.0, true), None);
        // the keyboard shrank the layout viewport too (iOS 26, third-party keyboards): still a keyboard
        assert_eq!(shell(430.0, 812.0, 430.0, 0.0, 1.0, true), Some(Shell { height: 430.0, top: 0.0, keyboard: true }));
        // no resting size yet: the live one
        assert_eq!(shell(430.0, f64::NAN, 430.0, 0.0, 1.0, true), Some(Shell { height: 430.0, top: 0.0, keyboard: false }));
        assert_eq!(error_text(429, r#"{"error":"Too many comments at once."}"#), "Too many comments at once.");
        assert_eq!(error_text(500, "<html>"), "The comment was not posted (error 500).");
        assert!(error_text(0, "").starts_with("No connection"));
    }

    #[test]
    fn a_portrait_phone_typing_stays_in_the_portrait_layout() {
        let phone = Rest::next(None, 430.0, 932.0, false);
        assert_eq!(phone, Some(Rest { width: 430.0, height: 932.0 }));
        // the keyboard shrinks the page to 430 x 380: still the portrait phone
        let typing = Rest::next(phone, 430.0, 380.0, true);
        assert_eq!(typing, phone);
        assert!(typing.is_some_and(|r| !r.side_by_side() && !r.low()));
        // the same live size without a focused field is a real change (a short window)
        assert_eq!(Rest::next(phone, 430.0, 380.0, false), Some(Rest { width: 430.0, height: 380.0 }));
        // turned while typing: the new width wins
        let turned = Rest::next(phone, 932.0, 400.0, true);
        assert!(turned.is_some_and(|r| r.low() && r.side_by_side()));
        // wide screens put the comments beside the picture; a tall narrow window does not
        assert!(Rest { width: 1280.0, height: 800.0 }.side_by_side());
        assert!(!Rest { width: 1280.0, height: 800.0 }.low());
        assert!(!Rest { width: 600.0, height: 900.0 }.side_by_side());
        // unusable sizes keep what we had
        for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert_eq!(Rest::next(phone, bad, 900.0, false), phone);
            assert_eq!(Rest::next(phone, 430.0, bad, false), phone);
            assert_eq!(Rest::next(None, bad, bad, false), None);
        }
    }
}
