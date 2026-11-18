//! DOM glue for the review page (wasm32). Every reviewer string reaches the page through
//! `textContent`, never as HTML. Until setup succeeds the page stays the server's no-script page,
//! so a failure here never leaves a reviewer without a way to comment.

use std::cell::RefCell;
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
    Device { viewport_width: width, save_data, cellular, slow }
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
        doc,
    };
    let token = base.get(3..3 + filmcraft_review::token::TOKEN_HEX).unwrap_or_default();
    let own_key = format!("{}{}", logic::OWN_KEY, token.get(..16).unwrap_or_default());
    let own = load(&win, &own_key).map(|s| logic::parse_own(&s)).unwrap_or_default();
    let choice = load(&win, logic::QUALITY_KEY);
    let current = logic::pick_rendition(&info.renditions, choice.as_deref(), device_info(&win)).ok_or("no file is ready")?;
    let hash = win.location().hash().unwrap_or_default();
    let focus = hash.strip_prefix("#c-").filter(|id| filmcraft_review::text::is_hex(id, filmcraft_review::token::ID_HEX)).map(str::to_string);
    let device = device_secret(&win);
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
    }));
    with(&app, |a| {
        a.reveal();
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
    wire_keyboard(&app, &win);
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
        // the script checks these; the browser's own check would block a hidden field
        let _ = self.els.name.remove_attribute("required");
        let _ = self.els.text.remove_attribute("required");
        self.els.quality.set_hidden(self.renditions.len() < 2);
    }

    fn frame(&self) -> i64 {
        self.timing.frame_at(self.els.video.current_time())
    }

    /// Point the video at the current rendition, keeping the time (and playing) when switching.
    fn load_file(&mut self, keep: bool) {
        let Some(r) = self.renditions.get(self.current) else { return };
        let src = format!("v/{}.mp4", r.name);
        self.els.quality.set_text_content(Some(logic::quality_label(r)));
        let _ = self.els.quality.set_attribute("aria-label", &format!("Video quality: {} ({}p). Tap to switch.", logic::quality_label(r), r.height));
        if self.els.video.src().ends_with(&src) || self.els.video.src().contains(&format!("{src}#")) {
            return;
        }
        if keep {
            self.restore = Some((self.els.video.current_time(), !self.els.video.paused()));
            self.els.video.set_src(&src);
        } else {
            // `#t=0.001`: Safari shows the first frame instead of black before playback
            self.els.video.set_src(&format!("{src}#t=0.001"));
        }
    }

    fn seek(&mut self, frame: i64) {
        let _ = self.els.video.pause();
        self.els.video.set_current_time(self.timing.seek_to(frame));
        self.shown = -1;
        self.draw();
    }

    /// Timecode, playhead and the composer's time, when the frame changed.
    fn draw(&mut self) {
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

    fn set_status(&self, msg: &str, error: bool) {
        self.els.status.set_text_content(Some(msg));
        let _ = self.els.status.class_list().toggle_with_force("err", error);
    }

    /// The comment list, the count and the scrub ticks, from `self.comments`.
    fn render_comments(&self) {
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
        self.els.count.set_text_content(Some(&logic::count_label(self.comments.len())));
        self.els.empty.set_hidden(!self.comments.is_empty());
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

fn wire_video(app: &Shared) {
    let Some(video) = with(app, |a| a.els.video.clone()) else { return };
    let a2 = app.clone();
    listen(&video, "loadedmetadata", move |_: Event| {
        with(&a2, |a| {
            if let Some((t, play)) = a.restore.take() {
                a.els.video.set_current_time(t);
                if play {
                    let _ = a.els.video.play();
                }
            } else if let Some(id) = a.focus.take() {
                if let Some(f) = a.comments.iter().find(|c| c.id == id).map(|c| c.frame) {
                    a.seek(f);
                    a.highlight(&id);
                }
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
}

fn wire_steps(app: &Shared) {
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
                a.load_file(true);
            }
        });
    });
    // arrow keys step frames when not typing (Shift: ten frames)
    let Some(doc) = with(app, |a| a.els.doc.clone()) else { return };
    let a2 = app.clone();
    listen(&doc, "keydown", move |e: KeyboardEvent| {
        let typing = e.target().and_then(|t| t.dyn_into::<Element>().ok()).is_some_and(|t| matches!(t.tag_name().as_str(), "INPUT" | "TEXTAREA"));
        let delta = match e.key().as_str() {
            "ArrowLeft" => -1,
            "ArrowRight" => 1,
            _ => return,
        };
        if typing {
            return;
        }
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
    let at_pointer = |a: &mut App, e: &PointerEvent| {
        let r = a.els.track.get_bounding_client_rect();
        let f = a.timing.frame_at_x(f64::from(e.client_x()) - r.left(), r.width());
        a.seek(f);
    };
    let a2 = app.clone();
    listen(&track, "pointerdown", move |e: PointerEvent| {
        e.prevent_default();
        with(&a2, |a| {
            a.scrubbing = true;
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
    for name in ["pointerup", "pointercancel"] {
        let a2 = app.clone();
        listen(&track, name, move |_: PointerEvent| {
            with(&a2, |a| a.scrubbing = false);
        });
    }
}

fn wire_list(app: &Shared) {
    let Some(list) = with(app, |a| a.els.list.clone()) else { return };
    let a2 = app.clone();
    listen(&list, "click", move |e: Event| {
        let Some(t) = e.target().and_then(|t| t.dyn_into::<Element>().ok()) else { return };
        let Some(li) = t.closest("li.c").ok().flatten() else { return };
        let id = li.id().strip_prefix("c-").unwrap_or_default().to_string();
        let act = t.closest("[data-act]").ok().flatten().and_then(|b| b.get_attribute("data-act"));
        e.prevent_default();
        match act.as_deref() {
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
}

/// Edit or delete one of this device's comments.
fn change_comment(app: &Shared, id: String, act: String) {
    let Some((win, base, device, old)) =
        with(app, |a| (a.win.clone(), a.base.clone(), a.device.clone(), a.comments.iter().find(|c| c.id == id).map(|c| c.text.clone())))
    else {
        return;
    };
    let Some(old) = old else { return };
    let body = if act == "edit" {
        let Ok(Some(new)) = win.prompt_with_message_and_default("Edit your comment", &old) else { return };
        let Some(new) = logic::clean_comment(&new) else { return };
        if new == old {
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
                let kind = if act == "edit" { filmcraft_review::EventKind::Edit } else { filmcraft_review::EventKind::Delete };
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
                if logic::apply(&mut a.comments, &ev) {
                    a.render_comments();
                }
                a.set_status(if act == "edit" { "Comment changed." } else { "Comment deleted." }, false);
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
        with(&a2, |a| {
            let _ = a.els.video.pause();
        });
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
                    a.set_status(&msg, false);
                }
                _ => a.set_status(&logic::error_text(status, &text), true),
            }
        });
    });
}

/// Keep the composer above the on-screen keyboard (`--kb`, see review.css).
fn wire_keyboard(app: &Shared, win: &Window) {
    let Some(vv) = win.visual_viewport() else { return };
    let update = {
        let app = app.clone();
        move |_: Event| {
            with(&app, |a| {
                let inner = a.win.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
                let kb = a.win.visual_viewport().map_or(0.0, |v| logic::keyboard_offset(inner, v.height(), v.offset_top()));
                if let Some(root) = a.els.doc.document_element().and_then(|r| r.dyn_into::<HtmlElement>().ok()) {
                    let _ = root.style().set_property("--kb", &format!("{kb}px"));
                }
            });
        }
    };
    listen(&vv, "resize", update.clone());
    listen(&vv, "scroll", update);
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

/// Redraw the timecode and playhead every animation frame while anything changes.
fn animate(app: Shared) {
    let cb: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let again = cb.clone();
    let tick = Closure::<dyn FnMut()>::new(move || {
        let win = with(&app, |a| {
            a.draw();
            a.win.clone()
        });
        if let (Some(win), Ok(c)) = (win, again.try_borrow()) {
            if let Some(c) = c.as_ref() {
                let _ = win.request_animation_frame(c.as_ref().unchecked_ref());
            }
        }
    });
    if let Some(win) = web_sys::window() {
        let _ = win.request_animation_frame(tick.as_ref().unchecked_ref());
    }
    if let Ok(mut slot) = cb.try_borrow_mut() {
        *slot = Some(tick);
    }
}
