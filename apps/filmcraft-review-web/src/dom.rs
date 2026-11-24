//! DOM glue for the review page (wasm32). Every reviewer string reaches the page through
//! `textContent`, never as HTML. Until setup succeeds the page stays the server's no-script page,
//! so a failure here never leaves a reviewer without a way to comment.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use filmcraft_review::model::{Comment, CommentEvent, Rendition, ShareInfo};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Document, Element, Event, EventSource, EventTarget, HtmlButtonElement, HtmlDialogElement, HtmlElement, HtmlFormElement, HtmlInputElement,
    HtmlTextAreaElement, HtmlVideoElement, KeyboardEvent, MessageEvent, PointerEvent, Request, RequestInit, Response, ScrollIntoViewOptions,
    ScrollLogicalPosition, Storage, Window,
};

use crate::logic::{self, Device, Timing};

type Shared = Rc<RefCell<App>>;

struct Els {
    doc: Document,
    video: HtmlVideoElement,
    play: HtmlElement,
    tc: Element,
    at: HtmlElement,
    track: HtmlElement,
    fill: HtmlElement,
    head: HtmlElement,
    ticks: Element,
    list: Element,
    count: Element,
    empty: HtmlElement,
    composer: HtmlFormElement,
    text: HtmlTextAreaElement,
    name: HtmlInputElement,
    namerow: Element,
    status: Element,
    send: HtmlButtonElement,
    quality: HtmlElement,
    who: Option<HtmlDialogElement>,
    whoname: Option<HtmlInputElement>,
    stage: HtmlElement,
    /// The buffered stretch on the scrub bar and the playback notice over the picture (made here).
    buf: Option<HtmlElement>,
    note: Option<HtmlElement>,
}

struct App {
    win: Window,
    els: Els,
    base: String,
    timing: Timing,
    renditions: Vec<Rendition>,
    current: usize,
    comments: Vec<Comment>,
    own: Vec<String>,
    own_key: String,
    device: String,
    /// The frame last drawn (-1: redraw).
    shown: i64,
    scrubbing: bool,
    posting: bool,
    /// After a file switch: the time to restore and whether to play.
    restore: Option<(f64, bool)>,
    /// The comment to show once the video can seek (from `#c-<id>`).
    focus: Option<String>,
    /// The play button's state as last drawn (`None`: draw it).
    playing_shown: Option<bool>,
    /// The page size before the keyboard came up (see `logic::Rest`).
    rest: Cell<Option<logic::Rest>>,
    /// The comment being edited in place. While it is open the list is not rebuilt (that would
    /// drop the text box and close the keyboard); `stale` marks a rebuild owed when it closes.
    editing: Option<String>,
    stale: bool,
    /// `?debug` on the link: a readout of the viewport numbers, for diagnosing a phone's layout.
    diag: Option<Element>,
    /// The frame last asked for. It stands in for the video's own time while that cannot be
    /// trusted: before the file's metadata, during a seek, a file switch or a drag on the bar.
    target: Option<i64>,
    /// A drag on the scrub bar: the last frame sought, and whether to play on when it ends.
    scrub_sought: i64,
    scrub_resume: bool,
    /// Stall watch and recovery (see `watch_playback`).
    watch: logic::Watch,
    retries: u32,
    retry_at: Option<f64>,
    /// Buffered seconds when the last stall was judged, to tell "slow" from "nothing arriving".
    stall_buffered: f64,
    busy_shown: bool,
    note_until: f64,
    note_sticky: bool,
    last_toggle: f64,
    /// When the buffered stretch was last drawn.
    buf_drawn: f64,
    /// Comments written without a connection, sent when it returns.
    outbox: Vec<logic::Queued>,
    outbox_key: String,
    flush_at: Option<f64>,
    flush_tries: u32,
    flushing: bool,
}

#[wasm_bindgen(start)]
pub fn start() {
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = run().await {
            web_sys::console::warn_1(&JsValue::from_str(&format!("review page: {e}; the basic page still works")));
        }
    });
}

fn by_id<T: JsCast>(doc: &Document, id: &str) -> Result<T, String> {
    doc.get_element_by_id(id).ok_or_else(|| format!("#{id} is missing"))?.dyn_into::<T>().map_err(|_| format!("#{id} has the wrong type"))
}

/// Call `obj.name()` if the browser has it; `None` when it is missing or throws.
fn call_method(obj: &JsValue, name: &str) -> Option<JsValue> {
    let f = js_sys::Reflect::get(obj, &JsValue::from_str(name)).ok()?.dyn_into::<js_sys::Function>().ok()?;
    f.call0(obj).ok()
}

fn js_err(e: JsValue) -> String {
    e.as_string().or_else(|| js_sys::JSON::stringify(&e).ok().and_then(|s| s.as_string())).unwrap_or_else(|| "script error".into())
}

fn storage(win: &Window) -> Option<Storage> {
    win.local_storage().ok().flatten()
}

fn load(win: &Window, key: &str) -> Option<String> {
    storage(win)?.get_item(key).ok().flatten()
}

fn save(win: &Window, key: &str, value: &str) {
    if let Some(s) = storage(win) {
        let _ = s.set_item(key, value);
    }
}

/// `(status, body)` of a request; status 0 when the network failed.
async fn fetch(win: &Window, url: &str, method: &str, body: Option<&str>) -> (u16, String) {
    let init = RequestInit::new();
    init.set_method(method);
    if let Some(b) = body {
        init.set_body(&JsValue::from_str(b));
    }
    let Ok(req) = Request::new_with_str_and_init(url, &init) else { return (0, String::new()) };
    if body.is_some() {
        let _ = req.headers().set("Content-Type", "application/json");
    }
    let Ok(resp) = JsFuture::from(win.fetch_with_request(&req)).await else { return (0, String::new()) };
    let Ok(resp) = resp.dyn_into::<Response>() else { return (0, String::new()) };
    let status = resp.status();
    let text = match resp.text() {
        Ok(p) => JsFuture::from(p).await.ok().and_then(|t| t.as_string()).unwrap_or_default(),
        Err(_) => String::new(),
    };
    (status, text)
}

fn device_secret(win: &Window) -> String {
    if let Some(d) = load(win, logic::DEVICE_KEY).filter(|d| logic::valid_device(d)) {
        return d;
    }
    let mut b = [0u8; 16];
    let ok = win.crypto().ok().is_some_and(|c| c.get_random_values_with_u8_array(&mut b).is_ok());
    if !ok {
        // no secret: this device can post but not edit or delete later
        return String::new();
    }
    let d: String = b.iter().map(|x| format!("{x:02x}")).collect();
    save(win, logic::DEVICE_KEY, &d);
    d
}

fn device_info(win: &Window) -> Device {
    let width = win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
    let conn = js_sys::Reflect::get(&win.navigator(), &JsValue::from_str("connection")).ok().filter(|c| c.is_object());
    let field = |name: &str| conn.as_ref().and_then(|c| js_sys::Reflect::get(c, &JsValue::from_str(name)).ok());
    let save_data = field("saveData").and_then(|v| v.as_bool()).unwrap_or(false);
    let cellular = field("type").and_then(|v| v.as_string()).is_some_and(|t| t == "cellular");
    let slow = field("effectiveType").and_then(|v| v.as_string()).is_some_and(|t| matches!(t.as_str(), "slow-2g" | "2g" | "3g"));
    Device { viewport_width: width, save_data, cellular, slow, measured_kbps: measured_kbps(win) }
}

/// The speed this page's own script arrived at (its wasm file is the largest thing it loads):
/// bytes over the time its body took, from the browser's resource timings. `None` from cache.
fn measured_kbps(win: &Window) -> Option<f64> {
    let get = |o: &JsValue, k: &str| js_sys::Reflect::get(o, &JsValue::from_str(k)).ok();
    let perf = get(win, "performance")?;
    let f = get(&perf, "getEntriesByType")?.dyn_into::<js_sys::Function>().ok()?;
    let entries = f.call1(&perf, &JsValue::from_str("resource")).ok()?.dyn_into::<js_sys::Array>().ok()?;
    let num = |o: &JsValue, k: &str| get(o, k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    entries.iter().filter(|e| get(e, "name").and_then(|n| n.as_string()).is_some_and(|n| n.contains(".wasm"))).find_map(|e| {
        let bytes = num(&e, "transferSize").max(num(&e, "encodedBodySize").min(num(&e, "transferSize") + 1.0));
        logic::throughput_kbps(bytes, num(&e, "responseEnd") - num(&e, "responseStart"))
    })
}

fn listen<E: wasm_bindgen::convert::FromWasmAbi + 'static>(target: &EventTarget, name: &str, f: impl FnMut(E) + 'static) {
    let c = Closure::<dyn FnMut(E)>::new(f);
    let _ = target.add_event_listener_with_callback(name, c.as_ref().unchecked_ref());
    // listeners live as long as the page
    c.forget();
}

/// Run `f` on the app unless it is already borrowed (a nested event); skipped then.
fn with<R>(app: &Shared, f: impl FnOnce(&mut App) -> R) -> Option<R> {
    let mut a = app.try_borrow_mut().ok()?;
    Some(f(&mut a))
}

async fn run() -> Result<(), String> {
    let win = web_sys::window().ok_or("no window")?;
    let doc = win.document().ok_or("no document")?;
    let base = logic::base_path(&win.location().pathname().map_err(js_err)?).ok_or("not a review link")?;
    let (status, body) = fetch(&win, &format!("{base}info"), "GET", None).await;
    if status != 200 {
        return Err(format!("info: {status}"));
    }
    let info: ShareInfo = serde_json::from_str(&body).map_err(|e| format!("info: {e}"))?;
    let timing = Timing::of(&info).ok_or("the share has no usable frame rate")?;
    let els = Els {
        video: by_id(&doc, "video")?,
        play: by_id(&doc, "play")?,
        tc: by_id(&doc, "tc")?,
        at: by_id(&doc, "at")?,
        track: by_id(&doc, "track")?,
        fill: by_id(&doc, "fill")?,
        head: by_id(&doc, "head")?,
        ticks: by_id(&doc, "ticks")?,
        list: by_id(&doc, "list")?,
        count: by_id(&doc, "count")?,
        empty: by_id(&doc, "empty")?,
        composer: by_id(&doc, "composer")?,
        text: by_id(&doc, "text")?,
        name: by_id(&doc, "name")?,
        namerow: by_id(&doc, "namerow")?,
        status: by_id(&doc, "status")?,
        send: by_id(&doc, "send")?,
        quality: by_id(&doc, "quality")?,
        who: by_id(&doc, "who").ok(),
        whoname: by_id(&doc, "whoname").ok(),
        stage: by_id(&doc, "stage")?,
        buf: None,
        note: None,
        doc,
    };
    let token = base.get(3..3 + filmcraft_review::token::TOKEN_HEX).unwrap_or_default();
    let own_key = format!("{}{}", logic::OWN_KEY, token.get(..16).unwrap_or_default());
    let own = load(&win, &own_key).map(|s| logic::parse_own(&s)).unwrap_or_default();
    let outbox_key = format!("{}{}", logic::OUTBOX_KEY, token.get(..16).unwrap_or_default());
    let outbox = load(&win, &outbox_key).map(|s| logic::parse_outbox(&s)).unwrap_or_default();
    let choice = load(&win, logic::QUALITY_KEY);
    let current = logic::pick_rendition(&info.renditions, choice.as_deref(), device_info(&win)).ok_or("no file is ready")?;
    let hash = win.location().hash().unwrap_or_default();
    let focus = hash.strip_prefix("#c-").filter(|id| filmcraft_review::text::is_hex(id, filmcraft_review::token::ID_HEX)).map(str::to_string);
    let device = device_secret(&win);
    let diag = diag_box(&els.doc, &win);
    let app = Rc::new(RefCell::new(App {
        win: win.clone(),
        els,
        base: base.clone(),
        timing,
        renditions: info.renditions,
        current,
        comments: info.comments,
        own,
        own_key,
        device,
        shown: -1,
        scrubbing: false,
        posting: false,
        restore: None,
        focus,
        playing_shown: None,
        rest: Cell::new(None),
        editing: None,
        stale: false,
        diag,
        target: None,
        scrub_sought: -1,
        scrub_resume: false,
        watch: logic::Watch::new(js_sys::Date::now()),
        retries: 0,
        retry_at: None,
        stall_buffered: 0.0,
        busy_shown: false,
        note_until: 0.0,
        note_sticky: false,
        last_toggle: 0.0,
        buf_drawn: 0.0,
        flush_at: (!outbox.is_empty()).then(js_sys::Date::now),
        outbox,
        outbox_key,
        flush_tries: 0,
        flushing: false,
    }));
    with(&app, |a| {
        a.reveal();
        a.build_extras();
        a.load_file(false);
        a.render_comments();
        a.setup_name();
        a.draw();
    });
    wire_video(&app);
    wire_steps(&app);
    wire_scrub(&app);
    wire_list(&app);
    wire_composer(&app);
    wire_viewport(&app, &win);
    wire_live(&app, &base);
    animate(app);
    Ok(())
}

impl App {
    /// Switch the page to its scripted form.
    fn reveal(&self) {
        let doc = &self.els.doc;
        if let Some(root) = doc.document_element() {
            let _ = root.class_list().add_1("wasm");
        }
        if let Ok(nodes) = doc.query_selector_all(".js") {
            for i in 0..nodes.length() {
                if let Some(e) = nodes.item(i).and_then(|n| n.dyn_into::<HtmlElement>().ok()) {
                    e.set_hidden(false);
                }
            }
        }
        // our transport sits under the picture; the browser's controls would cover it
        self.els.video.set_controls(false);
        // the script checks these; the browser's own check would block a hidden field
        let _ = self.els.name.remove_attribute("required");
        let _ = self.els.text.remove_attribute("required");
        self.els.quality.set_hidden(self.renditions.len() < 2);
    }

    /// The buffered stretch on the scrub bar and the notice over the picture.
    fn build_extras(&mut self) {
        let doc = &self.els.doc;
        if let Ok(b) = doc.create_element("div").and_then(|e| e.dyn_into::<HtmlElement>().map_err(Into::into)) {
            b.set_class_name("buf");
            let first = self.els.track.first_child();
            if self.els.track.insert_before(&b, first.as_ref()).is_ok() {
                self.els.buf = Some(b);
            }
        }
        if let Ok(n) = doc.create_element("p").and_then(|e| e.dyn_into::<HtmlElement>().map_err(Into::into)) {
            n.set_class_name("pnote");
            let _ = n.set_attribute("role", "status");
            n.set_hidden(true);
            if self.els.stage.append_child(&n).is_ok() {
                self.els.note = Some(n);
            }
        }
    }

    /// The video's time, or the time a pending file switch will restore.
    fn position(&self) -> f64 {
        self.restore.map_or_else(|| self.els.video.current_time(), |r| r.0)
    }

    /// Whether the video's own time can be trusted right now.
    fn settled(&self) -> bool {
        self.restore.is_none() && !self.scrubbing && self.els.video.ready_state() >= 1 && !self.els.video.seeking()
    }

    fn frame(&self) -> i64 {
        match self.target {
            Some(t) if !self.settled() => t,
            _ => self.timing.frame_at(self.position()),
        }
    }

    fn playing(&self) -> bool {
        match self.restore {
            Some((_, play)) => play,
            None => !self.els.video.paused() && !self.els.video.ended(),
        }
    }

    /// Point the video at the current rendition, keeping the time (and playing) when switching.
    fn load_file(&mut self, keep: bool) {
        let Some(r) = self.renditions.get(self.current) else { return };
        let src = format!("v/{}.mp4", r.name);
        self.els.quality.set_text_content(Some(logic::quality_label(r)));
        let _ = self.els.quality.set_attribute("aria-label", &format!("Video quality: {} ({}p). Tap to switch.", logic::quality_label(r), r.height));
        let now = self.els.video.src();
        if keep {
            // a second switch before the first has loaded keeps the first one's time
            self.restore = Some((self.position(), self.playing()));
            self.els.video.set_src(&src);
            self.els.video.load();
            self.watch.reset(js_sys::Date::now());
        } else if !(now.ends_with(&src) || now.contains(&format!("{src}#"))) {
            // `#t=0.001`: Safari shows the first frame instead of black before playback
            self.els.video.set_src(&format!("{src}#t=0.001"));
        }
    }

    /// Show `frame`. Before the file's metadata, or during a file switch, the frame is kept and
    /// shown as soon as the video can seek.
    fn go(&mut self, frame: i64) {
        let f = self.timing.clamp(frame);
        self.target = Some(f);
        let t = self.timing.seek_to(f);
        match self.restore.as_mut() {
            Some(r) => r.0 = t,
            None if self.els.video.ready_state() >= 1 => self.els.video.set_current_time(t),
            None => {}
        }
        self.shown = -1;
        self.draw();
    }

    /// Jump by `seconds` (the ±10 s buttons, J and L), playing on if it was playing.
    fn jump(&mut self, seconds: f64) {
        let f = self.timing.jump(self.frame(), seconds);
        self.go(f);
    }

    /// Show `frame` and stay on it (frame steps, comment times).
    fn seek(&mut self, frame: i64) {
        self.pause();
        self.go(frame);
    }

    fn pause(&mut self) {
        if let Some(r) = self.restore.as_mut() {
            r.1 = false;
        }
        let _ = self.els.video.pause();
    }

    /// Start playback. A refusal is said over the picture, never swallowed; a broken file is
    /// loaded again; during a file switch playback starts when the new file can play.
    fn play(&mut self) {
        if let Some(r) = self.restore.as_mut() {
            r.1 = true;
            return;
        }
        if self.els.video.error().is_some() || self.els.video.network_state() == 3 {
            self.reload(true);
            return;
        }
        // play on the last frame starts over (it would otherwise run for a blink and stop)
        if self.els.video.ended() || (self.els.video.ready_state() >= 1 && self.frame() >= self.timing.frames - 1) {
            self.els.video.set_current_time(0.0);
            self.target = None;
        }
        let note = self.els.note.clone();
        let say = move |msg: &str| {
            if let Some(n) = &note {
                n.set_text_content(Some(msg));
                n.set_hidden(false);
            }
        };
        match self.els.video.play() {
            Ok(p) => wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = JsFuture::from(p).await {
                    let name = js_sys::Reflect::get(&e, &JsValue::from_str("name")).ok().and_then(|n| n.as_string()).unwrap_or_default();
                    match name.as_str() {
                        // a pause or a new file arrived first: not a failure
                        "AbortError" => {}
                        "NotAllowedError" => say("Tap play to start the video."),
                        _ => say("The video could not start. Tap play to try again."),
                    }
                }
            }),
            Err(_) => say("The video could not start. Tap play to try again."),
        }
        self.note_until = js_sys::Date::now() + 6000.0;
        self.note_sticky = false;
    }

    /// Load the current file again at the same place (a network error, a dead connection).
    fn reload(&mut self, play: bool) {
        let keep = (self.position(), play || self.playing());
        let Some(r) = self.renditions.get(self.current) else { return };
        self.restore = Some(keep);
        self.els.video.set_src(&format!("v/{}.mp4", r.name));
        self.els.video.load();
        self.watch.reset(js_sys::Date::now());
    }

    /// A notice over the picture. `sticky`: until playback moves again; else for `ms`.
    fn note(&mut self, msg: &str, ms: f64, sticky: bool) {
        if let Some(n) = &self.els.note {
            n.set_text_content(Some(msg));
            n.set_hidden(false);
        }
        self.note_until = js_sys::Date::now() + ms;
        self.note_sticky = sticky;
    }

    fn clear_note(&mut self) {
        if let Some(n) = &self.els.note {
            n.set_hidden(true);
        }
        self.note_sticky = false;
    }

    fn online(&self) -> bool {
        self.win.navigator().on_line()
    }

    fn buffered_total(&self) -> f64 {
        let b = self.els.video.buffered();
        (0..b.length()).filter_map(|i| Some(b.end(i).ok()? - b.start(i).ok()?)).filter(|d| d.is_finite() && *d > 0.0).sum()
    }

    /// Every frame: is the video moving when it should? A spinner after a short wait; after a
    /// long one (or repeated ones) a lighter file, and on the lightest a fresh request once
    /// nothing has arrived for a while. Also the retry after a network error.
    fn watch_playback(&mut self) {
        let now = js_sys::Date::now();
        let v = &self.els.video;
        let broken = v.error().is_some();
        let active = !broken && ((!v.paused() && !v.ended()) || v.seeking() || self.restore.is_some());
        let verdict = self.watch.tick(now, active, v.current_time());
        let busy = verdict != logic::Stall::Fine;
        if busy != self.busy_shown {
            self.busy_shown = busy;
            let _ = self.els.stage.class_list().toggle_with_force("busy", busy);
        }
        match verdict {
            logic::Stall::Stuck => self.stuck(now),
            logic::Stall::Fine if active && self.restore.is_none() && !v.seeking() && v.ready_state() >= 3 => {
                // moving again
                self.retries = 0;
                self.retry_at = None;
                if self.note_sticky {
                    self.clear_note();
                }
            }
            _ => {}
        }
        if self.retry_at.is_some_and(|t| now >= t) {
            self.retry_at = None;
            if !self.online() {
                self.note("No connection. The video continues when it's back.", 0.0, true);
            } else if broken || self.buffered_total() <= self.stall_buffered {
                // nothing arrived since: ask again
                self.reload(false);
                self.retry_at = Some(now + logic::backoff_ms(self.retries.saturating_add(1)));
                self.retries = self.retries.saturating_add(1);
            }
        }
        if !self.note_sticky && now > self.note_until && self.els.note.as_ref().is_some_and(|n| !n.hidden()) {
            self.clear_note();
        }
        if (now - self.buf_drawn).abs() >= 400.0 {
            self.buf_drawn = now;
            self.draw_buffered();
        }
    }

    fn stuck(&mut self, now: f64) {
        self.stall_buffered = self.buffered_total();
        if !self.online() {
            self.note("No connection. The video continues when it's back.", 0.0, true);
            return;
        }
        if self.current > 0 {
            // a lighter file, for this visit only (the reviewer's saved choice stays)
            self.current -= 1;
            self.load_file(true);
            self.note("Slow connection: switched to a lighter video.", 6000.0, false);
        } else {
            self.note("Slow connection. Still loading…", 0.0, true);
            if self.retry_at.is_none() {
                self.retries = self.retries.saturating_add(1);
                self.retry_at = Some(now + logic::backoff_ms(self.retries).max(8000.0));
            }
        }
    }

    /// The video element gave up on its file (the connection dropped mid-load, a bad range).
    fn on_error(&mut self) {
        let now = js_sys::Date::now();
        self.retries = self.retries.saturating_add(1);
        self.stall_buffered = f64::MAX;
        if !self.online() {
            self.note("No connection. The video continues when it's back.", 0.0, true);
            self.retry_at = None;
            return;
        }
        // the same file twice, then a lighter one if there is one
        if self.retries > 2 && self.current > 0 {
            self.current -= 1;
            self.retries = 1;
        }
        self.note("Connection problem. Trying again…", 0.0, true);
        self.retry_at = Some(now + logic::backoff_ms(self.retries));
    }

    /// Back online, or back from the background: pick up where things stopped.
    fn wake(&mut self) {
        self.watch.reset(js_sys::Date::now());
        self.playing_shown = None;
        if self.els.video.error().is_some() || self.retry_at.is_some() {
            self.retry_at = Some(0.0);
        }
        if !self.outbox.is_empty() {
            self.flush_at = Some(0.0);
        }
    }

    fn draw_buffered(&self) {
        let Some(buf) = &self.els.buf else { return };
        let v = &self.els.video;
        let b = v.buffered();
        let ranges: Vec<(f64, f64)> = (0..b.length().min(64)).filter_map(|i| Some((b.start(i).ok()?, b.end(i).ok()?))).collect();
        let (left, width) = logic::buffered_span(&ranges, v.current_time(), v.duration()).unwrap_or((0.0, 0.0));
        let _ = buf.style().set_property("left", &format!("{left:.2}%"));
        let _ = buf.style().set_property("width", &format!("{width:.2}%"));
    }

    /// Timecode, playhead and the composer's time, when the frame changed.
    fn draw(&mut self) {
        self.sync_transport();
        if self.target.is_some() && self.settled() {
            self.target = None;
        }
        // a drag sends one seek at a time (a weak connection cannot keep up with every move)
        if self.scrubbing
            && let Some(f) = self.target
            && f != self.scrub_sought
            && self.restore.is_none()
            && self.els.video.ready_state() >= 1
            && !self.els.video.seeking()
        {
            self.scrub_sought = f;
            self.els.video.set_current_time(self.timing.seek_to(f));
        }
        let f = self.frame();
        if f == self.shown {
            return;
        }
        self.shown = f;
        let tc = self.timing.timecode(f);
        self.els.tc.set_text_content(Some(&tc));
        self.els.at.set_text_content(Some(&tc));
        let p = format!("{:.3}%", self.timing.percent(f));
        let _ = self.els.fill.style().set_property("width", &p);
        let _ = self.els.head.style().set_property("left", &p);
    }

    /// Size the page to the visible screen (`--vvh`, `--vvtop`, `--resth` and the `typing`,
    /// `compose`, `side` and `low` classes; see review.css), so the video stays in view and the
    /// composer sits right on the keyboard.
    fn fit_viewport(&self) {
        let Some(root) = self.els.doc.document_element().and_then(|r| r.dyn_into::<HtmlElement>().ok()) else { return };
        let inner = self.win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let typing = self.els.doc.active_element().is_some_and(|e| matches!(e.tag_name().as_str(), "INPUT" | "TEXTAREA"));
        let width = self.win.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rest = logic::Rest::next(self.rest.get(), width, inner, typing);
        self.rest.set(rest);
        let rest_height = rest.map_or(inner, |r| r.height);
        let shell = self.win.visual_viewport().and_then(|v| logic::shell(inner, rest_height, v.height(), v.offset_top(), v.scale(), typing));
        let touch = self.touch_typing();
        let style = root.style();
        let _ = style.set_property("--resth", &format!("{}px", rest_height.round()));
        match shell {
            Some(b) => {
                let _ = style.set_property("--vvh", &format!("{}px", b.height));
                let _ = style.set_property("--vvtop", &format!("{}px", b.top));
            }
            None => {
                let _ = style.remove_property("--vvh");
                let _ = style.remove_property("--vvtop");
            }
        }
        // iOS can leave the page scrolled after the keyboard closes; while typing on a touch
        // screen the scroll is iOS putting the field on the keyboard, so it stays
        if touch.is_none() && self.win.scroll_y().is_ok_and(|y| y > 0.0) {
            self.win.scroll_to_with_x_and_y(0.0, 0.0);
        }
        let classes = root.class_list();
        let _ = classes.toggle_with_force("typing", touch.is_some());
        let _ = classes.toggle_with_force("compose", touch == Some(true));
        let _ = classes.toggle_with_force("side", rest.is_some_and(logic::Rest::side_by_side));
        let _ = classes.toggle_with_force("low", rest.is_some_and(logic::Rest::low));
    }

    /// Typing on a touch screen: `Some(true)` in the comment bar, `Some(false)` in a comment's
    /// editor. The page then leaves the fixed, measured layout for normal flow and iOS itself
    /// scrolls the field onto the keyboard (iOS 26 reports wrong viewport sizes while the
    /// keyboard is up, so a layout sized from them floats above it or jumps around).
    fn touch_typing(&self) -> Option<bool> {
        if self.win.navigator().max_touch_points() <= 0 {
            return None;
        }
        let a = self.els.doc.active_element()?;
        if !matches!(a.tag_name().as_str(), "INPUT" | "TEXTAREA") {
            return None;
        }
        if a.closest(".composer").ok().flatten().is_some() {
            Some(true)
        } else if a.closest(".edit-box").ok().flatten().is_some() {
            Some(false)
        } else {
            None
        }
    }

    /// Every frame: catch a focus change whose event was missed, and refresh the readout.
    fn check_layout(&self) {
        let on = self.els.doc.document_element().is_some_and(|r| r.class_list().contains("typing"));
        if on != self.touch_typing().is_some() {
            self.fit_viewport();
        }
        let Some(d) = &self.diag else { return };
        let vv = self.win.visual_viewport();
        let n = |v: Result<JsValue, JsValue>| v.ok().and_then(|v| v.as_f64()).unwrap_or(-1.0);
        let composer = self.els.composer.get_bounding_client_rect();
        let cls = self.els.doc.document_element().map(|r| r.class_name()).unwrap_or_default();
        let text = format!(
            "inner {:.0}x{:.0}  scrollY {:.0}\nvv {:.0}x{:.0} top {:.0} scale {:.2}\ncomposer {:.0}-{:.0}  rest {:.0}\n{}",
            n(self.win.inner_width()),
            n(self.win.inner_height()),
            self.win.scroll_y().unwrap_or(-1.0),
            vv.as_ref().map_or(-1.0, |v| v.width()),
            vv.as_ref().map_or(-1.0, |v| v.height()),
            vv.as_ref().map_or(-1.0, |v| v.offset_top()),
            vv.as_ref().map_or(-1.0, |v| v.scale()),
            composer.top(),
            composer.bottom(),
            self.rest.get().map_or(-1.0, |r| r.height),
            cls,
        );
        if d.text_content().as_deref() != Some(text.as_str()) {
            d.set_text_content(Some(&text));
        }
    }

    /// Play or pause. A second tap within a quarter second is the same tap (a double tap would
    /// otherwise start and stop at once).
    fn toggle_play(&mut self) {
        let now = js_sys::Date::now();
        if (now - self.last_toggle).abs() < 250.0 {
            return;
        }
        self.last_toggle = now;
        if self.playing() {
            self.pause();
        } else {
            self.play();
        }
        self.playing_shown = None;
        self.sync_transport();
    }

    /// Keep the transport true to the video every frame, not only on its events (WebKit can
    /// fire `play` while our own handler still holds the app, and that event is then skipped):
    /// the play button shows what the video is doing, and the browser's controls stay off the
    /// picture unless the video is full screen.
    fn sync_transport(&mut self) {
        let playing = self.playing();
        if self.playing_shown != Some(playing) {
            self.playing_shown = Some(playing);
            let _ = self.els.play.class_list().toggle_with_force("playing", playing);
            let _ = self.els.play.set_attribute("aria-label", if playing { "Pause" } else { "Play" });
        }
        if self.els.video.controls() && self.els.doc.fullscreen_element().is_none() {
            self.els.video.set_controls(false);
        }
    }

    /// Full screen. iPhone (and Safari): the video's own full-screen player, which brings its own
    /// controls. Elsewhere: the standard call on the video, whose controls `fullscreenchange`
    /// turns on while it is full screen. The page never turns the controls on by itself, so a
    /// refused or unanswered request cannot leave them over the picture.
    fn fullscreen(&mut self) {
        let video = self.els.video.clone();
        // iPhone's player refuses (throws) before it knows the file
        if video.ready_state() < 1 {
            self.note("The video is still loading. Try full screen in a moment.", 4000.0, false);
            return;
        }
        if call_method(&video, "webkitEnterFullscreen").is_some() {
            return;
        }
        let status = self.els.status.clone();
        let refused = move || {
            status.set_text_content(Some("This browser can't play full screen here."));
            let _ = status.class_list().add_1("err");
        };
        match call_method(&video, "requestFullscreen").map(|r| r.dyn_into::<js_sys::Promise>()) {
            // the standard call answers with a promise; a refusal comes later
            Some(Ok(p)) => wasm_bindgen_futures::spawn_local(async move {
                if JsFuture::from(p).await.is_err() {
                    refused();
                }
            }),
            Some(Err(_)) => {}
            None => refused(),
        }
    }

    fn set_status(&self, msg: &str, error: bool) {
        self.els.status.set_text_content(Some(msg));
        let _ = self.els.status.class_list().toggle_with_force("err", error);
    }

    /// The comment list, the count and the scrub ticks, from `self.comments`.
    fn render_comments(&mut self) {
        if self.editing.is_some() {
            self.stale = true;
            self.els.count.set_text_content(Some(&logic::count_label(self.comments.len())));
            return;
        }
        self.stale = false;
        let doc = &self.els.doc;
        self.els.list.set_text_content(None);
        self.els.ticks.set_text_content(None);
        for c in &self.comments {
            let mine = self.own.contains(&c.id);
            if let Ok(li) = comment_item(doc, &self.timing, c, mine) {
                let _ = self.els.list.append_child(&li);
            }
            if let Ok(t) = doc.create_element("div") {
                t.set_class_name(if mine { "tick mine" } else { "tick" });
                if let Ok(t) = t.dyn_into::<HtmlElement>() {
                    let _ = t.style().set_property("left", &format!("{:.3}%", self.timing.percent(c.frame)));
                    let _ = self.els.ticks.append_child(&t);
                }
            }
        }
        // comments still waiting for a connection, after the posted ones
        for (n, q) in self.outbox.iter().enumerate() {
            let c = Comment { id: format!("queued{n}"), author: q.name.clone(), text: q.text.clone(), frame: q.frame, at_unix: 0, edited: false };
            if let Ok(li) = comment_item(doc, &self.timing, &c, false) {
                let _ = li.class_list().add_1("queued");
                if let Ok(w) = doc.create_element("p") {
                    w.set_class_name("sub");
                    w.set_text_content(Some("Waiting for a connection. It sends by itself."));
                    let _ = li.append_child(&w);
                }
                let _ = self.els.list.append_child(&li);
            }
        }
        self.els.count.set_text_content(Some(&logic::count_label(self.comments.len())));
        self.els.empty.set_hidden(!self.comments.is_empty() || !self.outbox.is_empty());
    }

    fn save_outbox(&self) {
        save(&self.win, &self.outbox_key, &logic::store_outbox(&self.outbox));
    }

    fn highlight(&self, id: &str) {
        if let Ok(nodes) = self.els.list.query_selector_all(".c.on") {
            for i in 0..nodes.length() {
                if let Some(e) = nodes.item(i).and_then(|n| n.dyn_into::<Element>().ok()) {
                    let _ = e.class_list().remove_1("on");
                }
            }
        }
        if let Some(li) = self.els.doc.get_element_by_id(&format!("c-{id}")) {
            let _ = li.class_list().add_1("on");
            let o = ScrollIntoViewOptions::new();
            o.set_block(ScrollLogicalPosition::Nearest);
            li.scroll_into_view_with_scroll_into_view_options(&o);
        }
    }

    fn setup_name(&self) {
        match load(&self.win, logic::NAME_KEY).and_then(|n| logic::clean_name(&n)) {
            Some(n) => {
                self.els.name.set_value(&n);
                let _ = self.els.namerow.class_list().add_1("known");
            }
            None => self.ask_name(),
        }
    }

    fn ask_name(&self) {
        if let Some(d) = self.els.who.as_ref().filter(|d| !d.open()) {
            if let Some(w) = &self.els.whoname {
                w.set_value(&self.els.name.value());
            }
            let _ = d.show_modal();
        }
    }

    fn remember_name(&self, name: &str) {
        save(&self.win, logic::NAME_KEY, name);
        self.els.name.set_value(name);
        let _ = self.els.namerow.class_list().add_1("known");
    }

    fn remember_own(&mut self, id: &str) {
        if !self.own.iter().any(|x| x == id) {
            self.own.push(id.to_string());
            save(&self.win, &self.own_key, &logic::store_own(&self.own));
        }
    }

    /// Open a comment for editing in place: its text becomes a box with the whole comment, the
    /// cursor at the end, and Save and Cancel under it.
    fn start_edit(&mut self, id: &str) {
        if self.editing.as_deref() == Some(id) {
            return;
        }
        self.close_edit();
        let Some(old) = self.comments.iter().find(|c| c.id == id).map(|c| c.text.clone()) else { return };
        let doc = &self.els.doc;
        let Some(li) = doc.get_element_by_id(&format!("c-{id}")) else { return };
        let Ok(edit) = edit_box(doc, &old) else { return };
        for sel in [".txt", ".c-a", ".sub"] {
            if let Ok(Some(e)) = li.query_selector(sel)
                && let Ok(e) = e.dyn_into::<HtmlElement>()
            {
                e.set_hidden(true);
            }
        }
        if li.append_child(&edit).is_err() {
            return;
        }
        self.editing = Some(id.to_string());
        self.mark_editing(true);
        if let Ok(Some(t)) = edit.query_selector("textarea")
            && let Ok(t) = t.dyn_into::<HtmlTextAreaElement>()
        {
            grow(&t);
            let _ = t.focus();
            let end = t.value().encode_utf16().count() as u32;
            let _ = t.set_selection_range(end, end);
            let o = ScrollIntoViewOptions::new();
            o.set_block(ScrollLogicalPosition::Nearest);
            edit.scroll_into_view_with_scroll_into_view_options(&o);
        }
    }

    /// While a comment is open for editing the composer steps aside: the list gets its room, so
    /// the editor and its buttons fit above the keyboard.
    fn mark_editing(&self, on: bool) {
        if let Some(root) = self.els.doc.document_element() {
            let _ = root.class_list().toggle_with_force("editing", on);
        }
    }

    /// Close the editor without saving and redraw the list (with anything that arrived meanwhile).
    fn close_edit(&mut self) {
        if self.editing.take().is_some() {
            self.mark_editing(false);
            self.render_comments();
            self.fit_viewport();
        }
    }

    /// The text in the open editor.
    fn edit_text(&self) -> Option<String> {
        let id = self.editing.as_ref()?;
        let li = self.els.doc.get_element_by_id(&format!("c-{id}"))?;
        let t = li.query_selector("textarea.edit-in").ok().flatten()?.dyn_into::<HtmlTextAreaElement>().ok()?;
        Some(t.value())
    }

    fn grow_text(&self) {
        let s = self.els.text.style();
        let _ = s.set_property("height", "auto");
        let _ = s.set_property("height", &format!("{}px", self.els.text.scroll_height().max(44)));
    }
}

/// One comment as a list item (text set as text, never as HTML).
fn comment_item(doc: &Document, t: &Timing, c: &Comment, mine: bool) -> Result<Element, JsValue> {
    let li = doc.create_element("li")?;
    li.set_class_name("c");
    li.set_id(&format!("c-{}", c.id));
    li.set_attribute("data-frame", &c.frame.to_string())?;
    let h = doc.create_element("div")?;
    h.set_class_name("c-h");
    let who = doc.create_element("span")?;
    who.set_class_name("who");
    who.set_text_content(Some(&c.author));
    let at = doc.create_element("a")?;
    at.set_class_name("at");
    at.set_attribute("href", &format!("#c-{}", c.id))?;
    at.set_text_content(Some(&t.timecode(c.frame)));
    h.append_child(&who)?;
    h.append_child(&at)?;
    let p = doc.create_element("p")?;
    p.set_class_name("txt");
    p.set_text_content(Some(&c.text));
    li.append_child(&h)?;
    li.append_child(&p)?;
    if c.edited {
        let e = doc.create_element("p")?;
        e.set_class_name("sub");
        e.set_text_content(Some("Edited"));
        li.append_child(&e)?;
    }
    if mine {
        let a = doc.create_element("div")?;
        a.set_class_name("c-a");
        for (act, label, class) in [("edit", "Edit", "edit"), ("delete", "Delete", "del")] {
            let b = doc.create_element("button")?;
            b.set_class_name(class);
            b.set_attribute("type", "button")?;
            b.set_attribute("data-act", act)?;
            b.set_text_content(Some(label));
            a.append_child(&b)?;
        }
        li.append_child(&a)?;
    }
    Ok(li)
}

/// The in-place editor: a box with the comment's whole text, then Cancel and Save.
fn edit_box(doc: &Document, text: &str) -> Result<Element, JsValue> {
    let d = doc.create_element("div")?;
    d.set_class_name("edit-box");
    let t = doc.create_element("textarea")?.dyn_into::<HtmlTextAreaElement>()?;
    t.set_class_name("in txtin edit-in");
    t.set_attribute("maxlength", "2000")?;
    t.set_attribute("enterkeyhint", "done")?;
    t.set_attribute("aria-label", "Edit your comment")?;
    t.set_value(text);
    // the buttons go above the box: iOS scrolls the box itself onto the keyboard, so anything
    // under it would sit behind the keyboard
    let a = doc.create_element("div")?;
    a.set_class_name("c-a");
    for (act, label, class) in [("cancel", "Cancel", "cancel"), ("save", "Save", "save")] {
        let b = doc.create_element("button")?;
        b.set_class_name(class);
        b.set_attribute("type", "button")?;
        b.set_attribute("data-act", act)?;
        b.set_text_content(Some(label));
        a.append_child(&b)?;
    }
    d.append_child(&a)?;
    d.append_child(&t)?;
    Ok(d)
}

/// The `?debug` readout (none without it).
fn diag_box(doc: &Document, win: &Window) -> Option<Element> {
    let search = win.location().search().ok()?;
    if !search.split(['?', '&']).any(|p| p == "debug") {
        return None;
    }
    let d = doc.create_element("pre").ok()?;
    d.set_class_name("diag");
    doc.body()?.append_child(&d).ok()?;
    Some(d)
}

/// Size a text box to its text (the stylesheet caps it).
fn grow(t: &HtmlTextAreaElement) {
    let s = t.style();
    let _ = s.set_property("height", "auto");
    let _ = s.set_property("height", &format!("{}px", t.scroll_height().max(44)));
}

fn wire_video(app: &Shared) {
    let Some(video) = with(app, |a| a.els.video.clone()) else { return };
    let a2 = app.clone();
    listen(&video, "loadedmetadata", move |_: Event| {
        with(&a2, |a| {
            if let Some((t, play)) = a.restore.take() {
                a.els.video.set_current_time(t);
                if play {
                    a.play();
                }
            } else if let Some(f) = a.target {
                // asked for before the video could seek
                a.els.video.set_current_time(a.timing.seek_to(f));
            } else if let Some(id) = a.focus.take()
                && let Some(f) = a.comments.iter().find(|c| c.id == id).map(|c| c.frame)
            {
                a.seek(f);
                a.highlight(&id);
            }
            a.shown = -1;
            a.draw();
        });
    });
    let a2 = app.clone();
    listen(&video, "seeked", move |_: Event| {
        with(&a2, |a| {
            a.shown = -1;
            a.draw();
        });
    });
    // the transport follows the video on its events too (`animate` also checks every frame,
    // which catches an event skipped while a handler held the app)
    for name in ["play", "playing", "pause", "ended", "waiting", "canplay", "webkitbeginfullscreen", "webkitendfullscreen"] {
        let a2 = app.clone();
        listen(&video, name, move |_: Event| {
            with(&a2, |a| a.sync_transport());
        });
    }
    let a2 = app.clone();
    listen(&video, "error", move |_: Event| {
        with(&a2, |a| a.on_error());
    });
    // back online, or back from the background or a locked phone
    let Some(win) = with(app, |a| a.win.clone()) else { return };
    let a2 = app.clone();
    listen(&win, "online", move |_: Event| {
        with(&a2, |a| a.wake());
    });
    let a2 = app.clone();
    listen(&win, "pageshow", move |_: Event| {
        with(&a2, |a| a.wake());
    });
    if let Some(doc) = with(app, |a| a.els.doc.clone()) {
        let a2 = app.clone();
        listen(&doc, "visibilitychange", move |_: Event| {
            with(&a2, |a| {
                if !a.els.doc.hidden() {
                    a.wake();
                }
            });
        });
    }
    // tapping the picture plays and pauses (there are no controls over it)
    let a2 = app.clone();
    listen(&video, "click", move |_: Event| {
        with(&a2, |a| {
            // in full screen the browser's own controls handle the click
            if !a.els.video.controls() {
                a.toggle_play();
            }
        });
    });
    // full screen through the standard API: the browser's controls while it lasts (the page's
    // transport is not in it); `sync_transport` turns them off again afterwards
    let Some(doc) = with(app, |a| a.els.doc.clone()) else { return };
    let a2 = app.clone();
    listen(&doc, "fullscreenchange", move |_: Event| {
        with(&a2, |a| {
            let on = a.els.doc.fullscreen_element().is_some_and(|e| e.is_same_node(Some(AsRef::<web_sys::Node>::as_ref(&a.els.video))));
            a.els.video.set_controls(on);
        });
    });
}

fn wire_steps(app: &Shared) {
    let Some(play) = with(app, |a| a.els.play.clone()) else { return };
    let a2 = app.clone();
    listen(&play, "click", move |_: Event| {
        with(&a2, |a| a.toggle_play());
    });
    if let Some(Ok(fs)) = with(app, |a| by_id::<HtmlElement>(&a.els.doc, "fs")) {
        let a2 = app.clone();
        listen(&fs, "click", move |_: Event| {
            with(&a2, |a| a.fullscreen());
        });
    }
    for (id, delta) in [("back", -1i64), ("fwd", 1)] {
        let Some(Ok(b)) = with(app, |a| by_id::<HtmlElement>(&a.els.doc, id)) else { continue };
        let a2 = app.clone();
        listen(&b, "click", move |_: Event| {
            with(&a2, |a| {
                let f = a.timing.step(a.frame(), delta);
                a.seek(f);
            });
        });
    }
    for (id, seconds) in [("back10", -10.0f64), ("fwd10", 10.0)] {
        let Some(Ok(b)) = with(app, |a| by_id::<HtmlElement>(&a.els.doc, id)) else { continue };
        let a2 = app.clone();
        listen(&b, "click", move |_: Event| {
            with(&a2, |a| a.jump(seconds));
        });
    }
    let Some(at) = with(app, |a| a.els.at.clone()) else { return };
    let a2 = app.clone();
    listen(&at, "click", move |_: Event| {
        with(&a2, |a| {
            let f = a.frame();
            a.seek(f);
        });
    });
    let Some(q) = with(app, |a| a.els.quality.clone()) else { return };
    let a2 = app.clone();
    listen(&q, "click", move |_: Event| {
        with(&a2, |a| {
            if a.renditions.len() > 1 {
                a.current = (a.current + 1) % a.renditions.len();
                if let Some(r) = a.renditions.get(a.current) {
                    save(&a.win, logic::QUALITY_KEY, &r.name);
                }
                a.retries = 0;
                a.retry_at = None;
                a.clear_note();
                a.load_file(true);
            }
        });
    });
    // arrow keys step frames when not typing (Shift: ten frames); Space and K play and pause;
    // J and L jump ten seconds back and forward
    let Some(doc) = with(app, |a| a.els.doc.clone()) else { return };
    let a2 = app.clone();
    listen(&doc, "keydown", move |e: KeyboardEvent| {
        let tag = e.target().and_then(|t| t.dyn_into::<Element>().ok()).map(|t| t.tag_name()).unwrap_or_default();
        if matches!(tag.as_str(), "INPUT" | "TEXTAREA") {
            return;
        }
        let delta = match e.key().as_str() {
            "ArrowLeft" => -1,
            "ArrowRight" => 1,
            // Space on a focused button presses that button instead
            " " | "k" | "K" if tag != "BUTTON" && !e.ctrl_key() && !e.meta_key() => {
                e.prevent_default();
                with(&a2, |a| a.toggle_play());
                return;
            }
            k @ ("j" | "J" | "l" | "L") if !e.ctrl_key() && !e.meta_key() => {
                e.prevent_default();
                let seconds = if k.eq_ignore_ascii_case("j") { -10.0 } else { 10.0 };
                with(&a2, |a| a.jump(seconds));
                return;
            }
            _ => return,
        };
        e.prevent_default();
        let n = if e.shift_key() { 10 } else { 1 };
        with(&a2, |a| {
            let f = a.timing.step(a.frame(), delta * n);
            a.seek(f);
        });
    });
}

fn wire_scrub(app: &Shared) {
    let Some(track) = with(app, |a| a.els.track.clone()) else { return };
    // the playhead follows the finger at once; `draw` sends the seeks one at a time
    let at_pointer = |a: &mut App, e: &PointerEvent| {
        let r = a.els.track.get_bounding_client_rect();
        let f = a.timing.frame_at_x(f64::from(e.client_x()) - r.left(), r.width());
        a.target = Some(f);
        a.shown = -1;
        a.draw();
    };
    let a2 = app.clone();
    listen(&track, "pointerdown", move |e: PointerEvent| {
        e.prevent_default();
        with(&a2, |a| {
            a.scrub_resume = a.playing();
            a.pause();
            a.scrubbing = true;
            a.scrub_sought = -1;
            let _ = a.els.track.class_list().add_1("drag");
            let _ = a.els.track.set_pointer_capture(e.pointer_id());
            at_pointer(a, &e);
        });
    });
    let a2 = app.clone();
    listen(&track, "pointermove", move |e: PointerEvent| {
        with(&a2, |a| {
            if a.scrubbing {
                at_pointer(a, &e);
            }
        });
    });
    for name in ["pointerup", "pointercancel", "lostpointercapture"] {
        let a2 = app.clone();
        listen(&track, name, move |_: PointerEvent| {
            with(&a2, |a| {
                if !a.scrubbing {
                    return;
                }
                a.scrubbing = false;
                let _ = a.els.track.class_list().remove_1("drag");
                // land exactly where the finger left, then carry on playing if it was
                if let Some(f) = a.target {
                    a.go(f);
                }
                if a.scrub_resume {
                    a.scrub_resume = false;
                    a.play();
                }
            });
        });
    }
}

fn wire_list(app: &Shared) {
    let Some(list) = with(app, |a| a.els.list.clone()) else { return };
    let a2 = app.clone();
    listen(&list, "click", move |e: Event| {
        let Some(t) = e.target().and_then(|t| t.dyn_into::<Element>().ok()) else { return };
        let Some(li) = t.closest("li.c").ok().flatten() else { return };
        // taps in the editor place the cursor; they don't seek
        if t.closest(".edit-in").ok().flatten().is_some() {
            return;
        }
        let id = li.id().strip_prefix("c-").unwrap_or_default().to_string();
        let act = t.closest("[data-act]").ok().flatten().and_then(|b| b.get_attribute("data-act"));
        e.prevent_default();
        match act.as_deref() {
            Some("edit") => {
                with(&a2, |a| a.start_edit(&id));
            }
            Some("cancel") => {
                with(&a2, |a| a.close_edit());
            }
            Some(act) => change_comment(&a2, id, act.to_string()),
            None => {
                let frame = li.get_attribute("data-frame").and_then(|f| f.parse::<i64>().ok()).unwrap_or(0);
                with(&a2, |a| {
                    a.seek(frame);
                    a.highlight(&id);
                });
            }
        }
    });
    // the editor grows with its text; Enter saves (Shift+Enter: new line), Escape cancels
    listen(&list, "input", move |e: Event| {
        if let Some(t) = e.target().and_then(|t| t.dyn_into::<HtmlTextAreaElement>().ok()) {
            grow(&t);
        }
    });
    let a2 = app.clone();
    listen(&list, "keydown", move |e: KeyboardEvent| {
        let Some(t) = e.target().and_then(|t| t.dyn_into::<Element>().ok()) else { return };
        if !t.class_list().contains("edit-in") || e.is_composing() {
            return;
        }
        let Some(id) = t.closest("li.c").ok().flatten().map(|li| li.id().strip_prefix("c-").unwrap_or_default().to_string()) else { return };
        if e.key() == "Enter" && !e.shift_key() {
            e.prevent_default();
            change_comment(&a2, id, "save".into());
        } else if e.key() == "Escape" {
            e.prevent_default();
            with(&a2, |a| a.close_edit());
        }
    });
}

/// Edit or delete one of this device's comments.
fn change_comment(app: &Shared, id: String, act: String) {
    let Some((win, base, device, old, typed)) = with(app, |a| {
        let typed = if a.editing.as_deref() == Some(id.as_str()) { a.edit_text() } else { None };
        (a.win.clone(), a.base.clone(), a.device.clone(), a.comments.iter().find(|c| c.id == id).map(|c| c.text.clone()), typed)
    }) else {
        return;
    };
    let Some(old) = old else { return };
    let body = if act == "save" {
        let Some(typed) = typed else { return };
        let Some(new) = logic::clean_comment(&typed) else {
            with(app, |a| a.set_status("A comment can't be empty. Delete it instead, or cancel.", true));
            return;
        };
        if new == old {
            with(app, |a| a.close_edit());
            return;
        }
        serde_json::json!({ "action": "edit", "text": new, "device": device })
    } else {
        if !win.confirm_with_message("Delete this comment?").unwrap_or(false) {
            return;
        }
        serde_json::json!({ "action": "delete", "device": device })
    };
    let app = app.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let (status, text) = fetch(&win, &format!("{base}comments/{id}"), "POST", Some(&body.to_string())).await;
        with(&app, |a| {
            if status == 200 {
                // a delete or a saved edit closes the editor (a failed save keeps it and its text)
                a.editing = None;
                a.mark_editing(false);
                let kind = if act == "save" { filmcraft_review::EventKind::Edit } else { filmcraft_review::EventKind::Delete };
                let new = body.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_string();
                let ev = CommentEvent {
                    id: String::new(),
                    kind,
                    comment: id.clone(),
                    author: String::new(),
                    text: new,
                    frame: 0,
                    device: String::new(),
                    at_unix: 0,
                };
                logic::apply(&mut a.comments, &ev);
                a.render_comments();
                a.fit_viewport();
                a.set_status(if act == "save" { "Comment changed." } else { "Comment deleted." }, false);
            } else {
                a.set_status(&logic::error_text(status, &text), true);
            }
        });
    });
}

fn wire_composer(app: &Shared) {
    let Some((form, text, whoform)) = with(app, |a| (a.els.composer.clone(), a.els.text.clone(), a.els.doc.get_element_by_id("whoform"))) else { return };
    // typing pauses the video, so the comment's frame is the one on screen
    let a2 = app.clone();
    listen(&text, "focus", move |_: Event| {
        with(&a2, |a| a.pause());
    });
    let a2 = app.clone();
    listen(&text, "input", move |_: Event| {
        with(&a2, |a| a.grow_text());
    });
    let a2 = app.clone();
    listen(&text, "keydown", move |e: KeyboardEvent| {
        if e.key() == "Enter" && !e.shift_key() && !e.is_composing() {
            e.prevent_default();
            let form = with(&a2, |a| a.els.composer.clone());
            // outside the borrow: requestSubmit dispatches the submit event at once
            if let Some(f) = form {
                let _ = f.request_submit();
            }
        }
    });
    let a2 = app.clone();
    listen(&form, "submit", move |e: Event| {
        e.prevent_default();
        post_comment(&a2);
    });
    if let Some(wf) = whoform {
        let a2 = app.clone();
        listen(&wf, "submit", move |_: Event| {
            with(&a2, |a| {
                let typed = a.els.whoname.as_ref().map(HtmlInputElement::value).unwrap_or_default();
                if let Some(n) = logic::clean_name(&typed) {
                    a.remember_name(&n);
                }
            });
        });
    }
}

fn post_comment(app: &Shared) {
    let Some(ready) = with(app, |a| {
        if a.posting {
            return None;
        }
        let Some(name) = logic::clean_name(&a.els.name.value()) else {
            a.ask_name();
            a.set_status("Add your name first.", true);
            return None;
        };
        let Some(text) = logic::clean_comment(&a.els.text.value()) else {
            a.set_status("Write a comment first.", true);
            return None;
        };
        let frame = a.frame();
        a.posting = true;
        a.els.send.set_disabled(true);
        a.set_status(&format!("Posting at {}…", a.timing.timecode(frame)), false);
        let body = serde_json::json!({ "name": name, "text": text, "frame": frame, "device": a.device });
        Some((a.win.clone(), a.base.clone(), name, body.to_string()))
    }) else {
        return;
    };
    let Some((win, base, name, body)) = ready else { return };
    let app = app.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let (status, text) = fetch(&win, &format!("{base}comments"), "POST", Some(&body)).await;
        with(&app, |a| {
            a.posting = false;
            a.els.send.set_disabled(false);
            match (status, serde_json::from_str::<Comment>(&text)) {
                (201, Ok(c)) => {
                    a.remember_name(&name);
                    a.remember_own(&c.id);
                    let msg = format!("Posted at {}.", a.timing.timecode(c.frame));
                    let id = c.id.clone();
                    logic::add_posted(&mut a.comments, c);
                    a.render_comments();
                    a.highlight(&id);
                    a.els.text.set_value("");
                    a.grow_text();
                    // close the keyboard so the reviewer is back to watching
                    let _ = a.els.text.blur();
                    a.set_status(&msg, false);
                }
                // no connection: keep the comment and send it when the connection is back
                (0, _) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
                        && let (Some(text), Some(frame)) = (v.get("text").and_then(|t| t.as_str()), v.get("frame").and_then(serde_json::Value::as_i64))
                        && a.outbox.len() < logic::MAX_QUEUED
                    {
                        a.outbox.push(logic::Queued { name: name.clone(), text: text.to_string(), frame });
                        a.save_outbox();
                        a.remember_name(&name);
                        a.els.text.set_value("");
                        a.grow_text();
                        let _ = a.els.text.blur();
                        a.render_comments();
                        a.flush_tries = 1;
                        a.flush_at = Some(js_sys::Date::now() + logic::backoff_ms(1));
                        a.set_status("No connection. Your comment is saved here and sends by itself when you're back online.", true);
                    } else {
                        a.set_status(&logic::error_text(status, &text), true);
                    }
                }
                _ => a.set_status(&logic::error_text(status, &text), true),
            }
        });
    });
}

/// Send the waiting comments, oldest first. Stops at the first that cannot be sent yet.
fn flush_outbox(app: &Shared) {
    let Some(Some((win, base, q, body))) = with(app, |a| {
        if a.flushing || a.posting {
            return None;
        }
        let q = a.outbox.first()?.clone();
        a.flushing = true;
        a.flush_at = None;
        let body = serde_json::json!({ "name": q.name, "text": q.text, "frame": q.frame, "device": a.device });
        Some((a.win.clone(), a.base.clone(), q, body.to_string()))
    }) else {
        return;
    };
    let app = app.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let (status, text) = fetch(&win, &format!("{base}comments"), "POST", Some(&body)).await;
        let more = with(&app, |a| {
            a.flushing = false;
            let now = js_sys::Date::now();
            match (status, serde_json::from_str::<Comment>(&text)) {
                (201, Ok(c)) => {
                    a.outbox.retain(|x| *x != q);
                    a.save_outbox();
                    a.remember_own(&c.id);
                    logic::add_posted(&mut a.comments, c);
                    a.render_comments();
                    a.flush_tries = 0;
                    if a.outbox.is_empty() {
                        a.set_status("Back online: your waiting comments are posted.", false);
                    }
                    !a.outbox.is_empty()
                }
                // still no connection, or the server asks to slow down: later
                (0 | 429 | 500..=599, _) => {
                    a.flush_tries = a.flush_tries.saturating_add(1);
                    a.flush_at = Some(now + logic::backoff_ms(a.flush_tries));
                    false
                }
                // refused for good (the link was retired, the comment limit): hand the text back
                _ => {
                    a.outbox.retain(|x| *x != q);
                    a.save_outbox();
                    if a.els.text.value().is_empty() {
                        a.els.text.set_value(&q.text);
                        a.grow_text();
                    }
                    a.render_comments();
                    a.set_status(&logic::error_text(status, &text), true);
                    !a.outbox.is_empty()
                }
            }
        });
        if more == Some(true) {
            flush_outbox(&app);
        }
    });
}

/// Keep the page sized to the visible screen as it changes: the keyboard, rotation, toolbars.
fn wire_viewport(app: &Shared, win: &Window) {
    let Some(vv) = win.visual_viewport() else { return };
    let Some(doc) = with(app, |a| a.els.doc.clone()) else { return };
    let targets: [(&EventTarget, &str); 4] = [(&vv, "resize"), (&vv, "scroll"), (&doc, "focusin"), (&doc, "focusout")];
    for (target, name) in targets {
        let a2 = app.clone();
        listen(target, name, move |_: Event| {
            with(&a2, |a| a.fit_viewport());
        });
    }
    with(app, |a| a.fit_viewport());
}

/// Other reviewers' comments as they are posted.
fn wire_live(app: &Shared, base: &str) {
    let Ok(es) = EventSource::new(&format!("{base}events")) else { return };
    let a2 = app.clone();
    listen(&es, "snapshot", move |e: MessageEvent| {
        let Some(data) = e.data().as_string() else { return };
        let Ok(list) = serde_json::from_str::<Vec<Comment>>(&data) else { return };
        with(&a2, |a| {
            if a.comments != list {
                a.comments = list;
                a.render_comments();
            }
        });
    });
    let a2 = app.clone();
    listen(&es, "ev", move |e: MessageEvent| {
        let Some(data) = e.data().as_string() else { return };
        let Ok(ev) = serde_json::from_str::<CommentEvent>(&data) else { return };
        with(&a2, |a| {
            if logic::apply(&mut a.comments, &ev) {
                a.render_comments();
            }
        });
    });
    // kept open for the page's lifetime
    std::mem::forget(es);
}

/// One round of upkeep: the timecode and playhead, the stall watch, the layout check, and any
/// waiting comments that are due.
fn upkeep(app: &Shared) -> Option<Window> {
    let (win, due) = with(app, |a| {
        a.draw();
        a.watch_playback();
        a.check_layout();
        let due = a.flush_at.is_some_and(|t| js_sys::Date::now() >= t) && !a.outbox.is_empty();
        (a.win.clone(), due)
    })?;
    if due {
        flush_outbox(app);
    }
    Some(win)
}

/// Upkeep every animation frame, and twice a second on a timer as well: frames stop while the
/// page is not being drawn (a background tab still playing sound), and recovery from a dead
/// connection must not wait for them.
fn animate(app: Shared) {
    let cb: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let again = cb.clone();
    let a2 = app.clone();
    let tick = Closure::<dyn FnMut()>::new(move || {
        if let (Some(win), Ok(c)) = (upkeep(&a2), again.try_borrow())
            && let Some(c) = c.as_ref()
        {
            let _ = win.request_animation_frame(c.as_ref().unchecked_ref());
        }
    });
    if let Some(win) = web_sys::window() {
        let _ = win.request_animation_frame(tick.as_ref().unchecked_ref());
        let timer = Closure::<dyn FnMut()>::new(move || {
            let _ = upkeep(&app);
        });
        let _ = win.set_interval_with_callback_and_timeout_and_arguments_0(timer.as_ref().unchecked_ref(), 500);
        // runs for the page's lifetime
        timer.forget();
    }
    if let Ok(mut slot) = cb.try_borrow_mut() {
        *slot = Some(tick);
    }
}
