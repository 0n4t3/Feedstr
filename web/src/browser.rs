//! Starting the app in the page, keeping its database between visits, and
//! the browser's way of picking and saving files.

use std::future::Future;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eframe::egui;
use feedstr_core::{App, Db, FileKind, Platform, PlatformFuture, Remote, Store};
use js_sys::{Promise, Uint8Array};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    BroadcastChannel, HtmlAnchorElement, HtmlCanvasElement, HtmlInputElement, IdbDatabase, IdbRequest,
    IdbTransactionMode, MessageEvent,
};

/// The browser's clock, for nostr (through universal-time).
struct Clock;

impl universal_time::WallClock for Clock {
    fn system_time(&self) -> universal_time::SystemTime {
        universal_time::SystemTime::from_unix_duration(Duration::from_millis(js_sys::Date::now() as u64))
    }
}

impl universal_time::MonotonicClock for Clock {
    fn instant(&self) -> universal_time::Instant {
        universal_time::Instant::from_ticks(Duration::from_millis(js_sys::Date::now() as u64))
    }
}

universal_time::define_time_provider!(Clock);

/// A snapshot is saved this long after changes (and right away when the tab is hidden).
const SAVE_DELAY: Duration = Duration::from_secs(5);
/// How the tabs of Feedstr in this browser talk to each other.
const TABS: &str = "feedstr-tabs";
/// Set once another tab took over: this one no longer saves or syncs.
static STOPPED: AtomicBool = AtomicBool::new(false);

pub fn start() {
    let _ = eframe::WebLogger::init(log::LevelFilter::Info);
    // Inside another site's page, clicks could be steered into revealing the Sync Key.
    if framed() {
        show_error("Feedstr can't be shown inside another website. Open it in a tab of its own.");
        return;
    }
    spawn_local(async {
        if let Err(e) = run().await {
            log::error!("Feedstr couldn't start: {e:?}");
            let reason = e.as_string().unwrap_or_else(|| format!("{e:?}"));
            let hint = if reason.contains("WebGL") {
                " Feedstr draws with WebGL: turn on hardware acceleration in your browser's settings."
            } else {
                ""
            };
            show_error(&format!("Feedstr couldn't start: {reason}.{hint}"));
        }
    });
}

/// Replaces the page with a message (as text, never as HTML).
fn show_error(message: &str) {
    if let Some(body) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.body()) {
        body.set_text_content(Some(message));
        let _ = body.set_attribute("style", "color: #888; font: 16px sans-serif; padding: 2em; overflow: auto");
    }
}

fn framed() -> bool {
    let Some(window) = web_sys::window() else { return false };
    match window.top() {
        Ok(Some(top)) => !js_sys::Object::is(&top, &window),
        _ => true,
    }
}

async fn run() -> Result<(), JsValue> {
    // Only one tab works at a time: each has its own copy of the database,
    // and saving would overwrite the other's. A tab that was open saves its
    // copy and steps aside before this one loads it.
    let tabs = BroadcastChannel::new(TABS)?;
    take_over(&tabs).await?;
    let saved = match snapshot::load().await {
        Ok(saved) => saved,
        Err(e) => {
            log::warn!("couldn't read the saved copy: {e:?}");
            None
        }
    };
    let db = match saved.map(|bytes| Db::from_snapshot(&bytes)) {
        Some(Ok(db)) => db,
        Some(Err(e)) => {
            // Everything that matters is on relays: start fresh, and sync.
            log::warn!("the saved copy is damaged ({e:#}); starting fresh");
            Db::open_memory().map_err(js_error)?
        }
        None => Db::open_memory().map_err(js_error)?,
    };
    let store: Store = Rc::new(db);
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("feedstr"))
        .ok_or("no canvas")?
        .dyn_into::<HtmlCanvasElement>()?;
    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(move |cc| {
                let app = App::new(cc, store.clone(), Arc::new(Browser))?;
                watch_visibility(store.clone(), app.remote());
                step_aside_for_newer_tabs(tabs, store.clone(), app.remote());
                let saved = store.changes_made();
                Ok(Box::new(Web { app, store, saved, last_save: 0 }))
            }),
        )
        .await
}

/// The shared app, plus keeping its database in the browser.
struct Web {
    app: App,
    store: Store,
    /// [`Db::changes_made`] when the snapshot was last saved.
    saved: u64,
    /// When it was (ms).
    last_save: i64,
}

impl eframe::App for Web {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if STOPPED.load(Ordering::Relaxed) {
            return;
        }
        self.app.ui(ui, frame);
        open_links_safely(ui.ctx());
        let changes = self.store.changes_made();
        if changes == self.saved {
            return;
        }
        let wait = self.last_save + SAVE_DELAY.as_millis() as i64 - js_sys::Date::now() as i64;
        if wait > 0 {
            ui.ctx().request_repaint_after(Duration::from_millis(wait as u64));
            return;
        }
        self.saved = changes;
        self.last_save = js_sys::Date::now() as i64;
        keep(&self.store);
    }
}

/// Opens the links the app asked for in a new tab that can't reach back to
/// this one (eframe would let it, and a page from a feed could then replace
/// Feedstr with a fake sign-in asking for the Sync Key), and without telling
/// the site where the visitor came from.
fn open_links_safely(ctx: &egui::Context) {
    let mut links = Vec::new();
    ctx.output_mut(|o| {
        o.commands.retain(|command| match command {
            egui::OutputCommand::OpenUrl(open) => {
                links.push(open.url.clone());
                false
            }
            _ => true,
        })
    });
    let Some(window) = web_sys::window() else { return };
    for url in links {
        let _ = window.open_with_url_and_target_and_features(&url, "_blank", "noopener,noreferrer");
    }
}

/// Saves a snapshot of the database if the user is signed in and asked to be
/// remembered, and otherwise makes sure none is left in the browser.
fn keep(db: &Db) {
    if !STOPPED.load(Ordering::Relaxed) {
        spawn_local(saving(db));
    }
}

/// [`keep`], as a future that finishes once the browser has stored it.
fn saving(db: &Db) -> impl Future<Output = ()> + 'static {
    let signed_in = db.meta("login").ok().flatten().is_some();
    let remember = db.meta("remember").ok().flatten().as_deref() != Some("0");
    let bytes = if signed_in && remember { db.snapshot().map_err(|e| log::error!("{e:#}")).ok() } else { None };
    async move {
        let result = match bytes {
            Some(bytes) => snapshot::save(&bytes).await,
            None => snapshot::clear().await,
        };
        if let Err(e) = result {
            log::error!("couldn't save to the browser's storage: {e:?}");
        }
    }
}

/// Asks a Feedstr tab that is already open to save its copy and step aside,
/// and waits for it (briefly, if there is none).
async fn take_over(tabs: &BroadcastChannel) -> Result<(), JsValue> {
    let (mut stepping_aside, mut saved) = (None, None);
    let stepping_aside_promise = Promise::new(&mut |resolve, _| stepping_aside = Some(resolve));
    let saved_promise = Promise::new(&mut |resolve, _| saved = Some(resolve));
    let (stepping_aside, saved) = (stepping_aside.expect("set"), saved.expect("set"));
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let reply = match event.data().as_string().as_deref() {
            Some("stepping aside") => &stepping_aside,
            Some("saved") => &saved,
            _ => return,
        };
        let _ = reply.call1(&JsValue::NULL, &JsValue::TRUE);
    });
    tabs.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    tabs.post_message(&"opening".into())?;
    if within(stepping_aside_promise, 300).await {
        within(saved_promise, 10_000).await;
    }
    tabs.set_onmessage(None);
    drop(on_message);
    Ok(())
}

/// Whether `promise` resolves to true within `timeout_ms`.
async fn within(promise: Promise, timeout_ms: i32) -> bool {
    let timeout = Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_1(&resolve, timeout_ms, &JsValue::FALSE);
        }
    });
    JsFuture::from(Promise::race(&js_sys::Array::of2(&promise, &timeout))).await.is_ok_and(|v| v.is_truthy())
}

/// When a newer tab opens, this one saves its copy, uploads what's left, and
/// stops, offering to take over again.
fn step_aside_for_newer_tabs(tabs: BroadcastChannel, store: Store, remote: Remote) {
    let channel = tabs.clone();
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        if event.data().as_string().as_deref() != Some("opening") || STOPPED.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = channel.post_message(&"stepping aside".into());
        remote.exit();
        let save = saving(&store);
        let channel = channel.clone();
        spawn_local(async move {
            save.await;
            let _ = channel.post_message(&"saved".into());
            show_paused();
        });
    });
    tabs.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
}

/// Replaces the page with a note that another tab took over, and a way back.
fn show_paused() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let Some(body) = document.body() else { return };
    body.set_text_content(Some("Feedstr was opened in another tab, so this one stepped aside. "));
    let _ = body.set_attribute("style", "color: #888; font: 16px sans-serif; padding: 2em");
    if let Ok(button) = document.create_element("button") {
        button.set_text_content(Some("Use it here"));
        let reload = Closure::<dyn FnMut()>::new(|| {
            let _ = web_sys::window().map(|w| w.location().reload());
        });
        let _ = button.add_event_listener_with_callback("click", reload.as_ref().unchecked_ref());
        reload.forget();
        let _ = body.append_child(&button);
    }
}

/// A hidden tab may be closed without warning: save everything (and upload
/// changes) when it is hidden, and sync when it comes back after a while.
fn watch_visibility(store: Store, remote: Remote) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    let page = document.clone();
    let on_change = Closure::<dyn FnMut()>::new(move || {
        if STOPPED.load(Ordering::Relaxed) {
            return;
        }
        if page.visibility_state() == web_sys::VisibilityState::Hidden {
            keep(&store);
            remote.paused();
        } else {
            remote.resumed();
        }
    });
    let _ = document.add_event_listener_with_callback("visibilitychange", on_change.as_ref().unchecked_ref());
    on_change.forget();
}

fn js_error(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

fn anyhow_error(e: JsValue) -> anyhow::Error {
    anyhow::anyhow!("{}", e.as_string().unwrap_or_else(|| format!("{e:?}")))
}

/// The database snapshot in IndexedDB, compressed. (localStorage would be
/// simpler, but holds only about 5 MB.)
mod snapshot {
    use super::*;

    const DATABASE: &str = "feedstr";
    const STORE: &str = "snapshot";
    const KEY: &str = "database";

    pub async fn load() -> Result<Option<Vec<u8>>, JsValue> {
        let db = open().await?;
        let store = db.transaction_with_str(STORE)?.object_store(STORE)?;
        let value = finished(&store.get(&KEY.into())?).await?;
        if value.is_undefined() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        let compressed = Uint8Array::new(&value).to_vec();
        flate2::read::DeflateDecoder::new(&compressed[..]).read_to_end(&mut bytes).map_err(js_error)?;
        Ok(Some(bytes))
    }

    pub async fn save(bytes: &[u8]) -> Result<(), JsValue> {
        let mut deflate = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        deflate.write_all(bytes).map_err(js_error)?;
        let compressed = Uint8Array::from(&deflate.finish().map_err(js_error)?[..]);
        let db = open().await?;
        let store = db.transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite)?.object_store(STORE)?;
        finished(&store.put_with_key(&compressed, &KEY.into())?).await?;
        Ok(())
    }

    pub async fn clear() -> Result<(), JsValue> {
        let db = open().await?;
        let store = db.transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite)?.object_store(STORE)?;
        finished(&store.delete(&KEY.into())?).await?;
        Ok(())
    }

    async fn open() -> Result<IdbDatabase, JsValue> {
        let factory = web_sys::window().ok_or("no window")?.indexed_db()?.ok_or("no IndexedDB")?;
        let request = factory.open_with_u32(DATABASE, 1)?;
        let create_store = Closure::once(move |event: web_sys::Event| {
            if let Some(Ok(db)) = event.target().map(|t| t.unchecked_into::<IdbRequest>().result()) {
                let _ = db.unchecked_into::<IdbDatabase>().create_object_store(STORE);
            }
        });
        request.set_onupgradeneeded(Some(create_store.as_ref().unchecked_ref()));
        Ok(finished(&request).await?.unchecked_into())
    }

    /// Waits for an IndexedDB request, and returns its result.
    async fn finished(request: &IdbRequest) -> Result<JsValue, JsValue> {
        let done = Promise::new(&mut |resolve, reject| {
            request.set_onsuccess(Some(&resolve));
            request.set_onerror(Some(&reject));
        });
        JsFuture::from(done).await?;
        request.result()
    }
}

/// Files are picked with the browser's file chooser and saved as downloads.
struct Browser;

impl Platform for Browser {
    fn open_file(&self, kind: FileKind) -> PlatformFuture<Option<Vec<u8>>> {
        Box::pin(async move { pick_file(kind).await.map_err(anyhow_error) })
    }

    fn save_file(&self, kind: FileKind, name: String, content: Vec<u8>) -> PlatformFuture<Option<String>> {
        Box::pin(async move {
            download(&name, kind.mime(), &content).map_err(anyhow_error)?;
            Ok(Some(format!("your downloads ({name})")))
        })
    }

    /// Browsers can't tell whether a Nostr app is installed, so this is
    /// always offered; without one, nothing happens.
    fn opens_nostr_links(&self) -> bool {
        true
    }

    /// Opens the link in a hidden frame, so the browser hands it to the
    /// Nostr app without leaving this page (or showing an error page).
    fn open_nostr_link(&self, link: &str) -> anyhow::Result<()> {
        anyhow::ensure!(link.starts_with("nostr:"), "not a Nostr link");
        let open = || -> Result<(), JsValue> {
            let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
            let frame = match document.get_element_by_id("nostr-links") {
                Some(frame) => frame,
                None => {
                    let frame = document.create_element("iframe")?;
                    frame.set_id("nostr-links");
                    frame.set_attribute("style", "display: none")?;
                    document.body().ok_or("no page")?.append_child(&frame)?;
                    frame
                }
            };
            frame.set_attribute("src", link)
        };
        open().map_err(anyhow_error)
    }
}

/// Asks for a file with a small overlay holding a real file input: browsers
/// (Firefox above all) only open their file chooser when the user clicks
/// such an input themselves, not when the app clicks it for them.
async fn pick_file(kind: FileKind) -> Result<Option<Vec<u8>>, JsValue> {
    let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let element = |tag: &str, style: &str| -> Result<web_sys::Element, JsValue> {
        let element = document.create_element(tag)?;
        element.set_attribute("style", style)?;
        Ok(element)
    };
    let overlay = element(
        "div",
        "position: fixed; inset: 0; display: flex; align-items: center; justify-content: center; \
         background: rgba(0, 0, 0, 0.45); font: 15px sans-serif; z-index: 10",
    )?;
    let panel = element(
        "div",
        "background: Canvas; color: CanvasText; padding: 20px 24px; border-radius: 12px; display: flex; \
         flex-direction: column; gap: 14px; max-width: 90vw",
    )?;
    let title = element("div", "font-weight: bold")?;
    title.set_text_content(Some(&format!("Choose the {} file to import", kind.label())));
    let input: HtmlInputElement = element("input", "")?.dyn_into()?;
    input.set_type("file");
    let accept: Vec<String> = kind.extensions().iter().map(|e| format!(".{e}")).collect();
    input.set_accept(&accept.join(","));
    let cancel = element("button", "align-self: flex-end; padding: 4px 14px")?;
    cancel.set_text_content(Some("Cancel"));
    panel.append_child(&title)?;
    panel.append_child(&input)?;
    panel.append_child(&cancel)?;
    overlay.append_child(&panel)?;
    document.body().ok_or("no page body")?.append_child(&overlay)?;
    let picked = Promise::new(&mut |resolve, _| {
        input.set_onchange(Some(&resolve));
        let _ = cancel.add_event_listener_with_callback("click", &resolve);
    });
    let result = JsFuture::from(picked).await;
    overlay.remove();
    result?;
    let Some(file) = input.files().and_then(|files| files.get(0)) else { return Ok(None) };
    if file.size() > feedstr_core::MAX_IMPORT_BYTES as f64 {
        return Err(format!("That file is too big to import (over {} MB)", feedstr_core::MAX_IMPORT_BYTES >> 20).into());
    }
    let buffer = JsFuture::from(file.array_buffer()).await?;
    Ok(Some(Uint8Array::new(&buffer).to_vec()))
}

fn download(name: &str, mime: &str, content: &[u8]) -> Result<(), JsValue> {
    let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    let parts = js_sys::Array::of1(&Uint8Array::from(content));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let link: HtmlAnchorElement = document.create_element("a")?.dyn_into()?;
    link.set_href(&url);
    link.set_download(name);
    link.click();
    // Some browsers read the file after click() returns.
    spawn_local(async move {
        let wait = Promise::new(&mut |resolve, _| {
            let _ =
                web_sys::window().map(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 60_000));
        });
        let _ = JsFuture::from(wait).await;
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    Ok(())
}
