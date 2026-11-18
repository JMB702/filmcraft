//! The review page, rendered on the server. It works on its own (watch, read, post with a typed
//! timecode); the wasm module (`filmcraft-review-web`) adds frame stepping, the comment scrub bar,
//! live comments and the keyboard-following composer. Every piece of reviewer or project text goes
//! through [`html_escape`].

use crate::model::{Comment, ShareRecord};
use crate::text::html_escape;
use crate::timing;

/// The stylesheet (`/assets/review.css`).
pub const CSS: &str = include_str!("review.css");

/// A short version tag for cache busting (FNV-1a of the bytes).
pub fn version_tag(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", h >> 32)
}

/// What the page needs besides the share.
pub struct PageInput<'a> {
    pub share: &'a ShareRecord,
    pub comments: &'a [Comment],
    /// Ready renditions (names), smallest first.
    pub ready: &'a [String],
    /// The `<script type="module">` source when the wasm assets are built in (`None`: no script).
    pub boot: Option<&'a str>,
}

/// The page HTML.
pub fn render(p: &PageInput) -> String {
    let s = p.share;
    let rate = timing::rate(s.rate_num, s.rate_den);
    let tc = |frame: i64| match rate {
        Some(r) => timing::timecode(r, s.drop_frame, timing::tc_base(r, s.start_timecode, filmcraft_time::Tick(s.range_start)), frame),
        None => frame.to_string(),
    };
    let title = html_escape(&s.title);
    let n = p.comments.len();
    let count = match n {
        0 => "No comments yet".to_string(),
        1 => "1 comment".to_string(),
        n => format!("{n} comments"),
    };
    let css_v = version_tag(CSS.as_bytes());
    let mut h = String::with_capacity(8192 + n * 256);
    h.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content\">\n");
    h.push_str("<meta name=\"color-scheme\" content=\"dark\">\n<meta name=\"theme-color\" content=\"#101113\">\n");
    h.push_str("<meta name=\"referrer\" content=\"no-referrer\">\n<meta name=\"robots\" content=\"noindex, nofollow\">\n<meta name=\"format-detection\" content=\"telephone=no\">\n");
    if p.ready.is_empty() {
        h.push_str("<meta http-equiv=\"refresh\" content=\"8\">\n");
    }
    h.push_str(&format!("<title>{title} · Review</title>\n<link rel=\"stylesheet\" href=\"/assets/review.css?v={css_v}\">\n"));
    if let Some(boot) = p.boot.filter(|_| !p.ready.is_empty()) {
        h.push_str(&format!("<script type=\"module\" src=\"{}\"></script>\n", html_escape(boot)));
    }
    h.push_str("</head>\n<body>\n<header class=\"top\">\n<div class=\"title\">");
    h.push_str(&format!("<h1>{title}</h1><p class=\"sub\" id=\"count\">{count}</p></div>\n"));
    h.push_str("<button id=\"quality\" class=\"chip js\" type=\"button\" hidden aria-label=\"Video quality\">SD</button>\n</header>\n");
    match p.ready.first() {
        Some(first) => {
            let first = html_escape(first);
            h.push_str("<div class=\"stage\" id=\"stage\">\n");
            h.push_str(&format!(
                "<video id=\"video\" playsinline webkit-playsinline controls preload=\"metadata\" width=\"{}\" height=\"{}\" src=\"v/{first}.mp4#t=0.001\"></video>\n",
                s.width.max(1),
                s.height.max(1)
            ));
            h.push_str("</div>\n");
            h.push_str("<div class=\"tools js\" id=\"tools\" hidden>\n");
            h.push_str("<button id=\"back\" class=\"step\" type=\"button\" aria-label=\"Back one frame\"><span aria-hidden=\"true\">‹</span> 1f</button>\n");
            h.push_str("<output id=\"tc\" class=\"tc\" aria-live=\"off\">--:--:--:--</output>\n");
            h.push_str("<button id=\"fwd\" class=\"step\" type=\"button\" aria-label=\"Forward one frame\">1f <span aria-hidden=\"true\">›</span></button>\n</div>\n");
            h.push_str("<div class=\"scrub js\" id=\"scrub\" hidden><div class=\"track\" id=\"track\"><div class=\"fill\" id=\"fill\"></div><div class=\"ticks\" id=\"ticks\"></div><div class=\"head\" id=\"head\"></div></div></div>\n");
        }
        None => h.push_str("<div class=\"wait\" role=\"status\"><div class=\"spinner\" aria-hidden=\"true\"></div><p>Rendering the video…</p><p class=\"sub\">This page refreshes by itself.</p></div>\n"),
    }
    h.push_str("<main class=\"feed\">\n<ol id=\"list\" class=\"list\">\n");
    for c in p.comments {
        let id = html_escape(&c.id);
        h.push_str(&format!(
            "<li class=\"c\" id=\"c-{id}\" data-frame=\"{}\"><div class=\"c-h\"><span class=\"who\">{}</span><a class=\"at\" href=\"#c-{id}\">{}</a></div><p class=\"txt\">{}</p></li>\n",
            c.frame,
            html_escape(&c.author),
            html_escape(&tc(c.frame)),
            html_escape(&c.text)
        ));
    }
    h.push_str("</ol>\n");
    h.push_str(&format!("<p id=\"empty\" class=\"empty\"{}>Pause on a frame and say what you see.</p>\n</main>\n", if n == 0 { "" } else { " hidden" }));
    if !p.ready.is_empty() {
        h.push_str("<form id=\"composer\" class=\"composer\" method=\"post\" action=\"comments\">\n");
        h.push_str("<div class=\"namerow\" id=\"namerow\"><input id=\"name\" name=\"name\" class=\"in\" placeholder=\"Your name\" maxlength=\"60\" autocomplete=\"name\" required enterkeyhint=\"next\"></div>\n");
        h.push_str("<input id=\"tcin\" name=\"tc\" class=\"in nojs\" placeholder=\"Timecode, e.g. 01:00:12:05\" inputmode=\"numeric\" autocomplete=\"off\">\n");
        h.push_str(
            "<div class=\"row\"><button id=\"at\" class=\"at-chip js\" type=\"button\" hidden aria-label=\"Comment time: go there\">--:--:--:--</button>",
        );
        h.push_str("<textarea id=\"text\" name=\"text\" class=\"in txtin\" rows=\"1\" maxlength=\"2000\" placeholder=\"Add a comment…\" required enterkeyhint=\"send\"></textarea>");
        h.push_str("<button id=\"send\" class=\"send\" type=\"submit\">Post</button></div>\n");
        h.push_str("<p id=\"status\" class=\"status\" role=\"status\" aria-live=\"polite\"></p>\n</form>\n");
        h.push_str("<dialog id=\"who\" class=\"sheet\"><form method=\"dialog\" id=\"whoform\"><h2>Who’s watching?</h2><p class=\"sub\">Your name goes with your comments. It is remembered on this device.</p>");
        h.push_str("<input id=\"whoname\" class=\"in\" placeholder=\"Your name\" maxlength=\"60\" autocomplete=\"name\" enterkeyhint=\"done\"><div class=\"sheet-b\"><button id=\"whook\" class=\"send\" type=\"submit\">Continue</button></div></form></dialog>\n");
    }
    h.push_str("</body>\n</html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share() -> ShareRecord {
        ShareRecord {
            format: crate::model::SHARE_FORMAT,
            id: "0123456789abcdef".into(),
            token: "ab".repeat(32),
            title: "<b>Cut</b> \"v3\"".into(),
            project: "/p".into(),
            sequence: 1,
            rate_num: 25,
            rate_den: 1,
            drop_frame: false,
            start_timecode: 90_000,
            range_start: 0,
            duration_frames: 250,
            width: 1920,
            height: 1080,
            renditions: vec![],
            created_unix: 0,
            expires_unix: None,
            revoked: false,
        }
    }

    #[test]
    fn reviewer_text_is_escaped_and_timecode_is_the_sequences() {
        let s = share();
        let c = vec![Comment {
            id: "c1".into(),
            author: "<img src=x onerror=alert(1)>".into(),
            text: "</p><script>x</script>".into(),
            frame: 30,
            at_unix: 0,
            edited: false,
        }];
        let ready = vec!["small".to_string()];
        let html = render(&PageInput { share: &s, comments: &c, ready: &ready, boot: Some("/assets/boot.js?v=1") });
        assert!(!html.contains("<script>x"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("<b>Cut"));
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(html.contains("01:00:01:05"));
        assert!(html.contains("playsinline"));
        assert!(html.contains("src=\"v/small.mp4#t=0.001\""));
        assert!(html.contains("<script type=\"module\" src=\"/assets/boot.js?v=1\"></script>"));
        // no inline script anywhere (the CSP forbids it)
        assert_eq!(html.matches("<script").count(), 1);
    }

    #[test]
    fn a_share_still_rendering_waits_and_refreshes() {
        let s = share();
        let html = render(&PageInput { share: &s, comments: &[], ready: &[], boot: Some("/b.js") });
        assert!(html.contains("http-equiv=\"refresh\""));
        assert!(!html.contains("<video"));
        assert!(!html.contains("<form"));
        assert!(!html.contains("<script"));
    }
}
