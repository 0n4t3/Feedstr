//! Feedstr for Android.
//!
//! Runs the shared app (`feedstr-core`) in a NativeActivity. `MainActivity`
//! (Java, in `app/`) handles what egui can't do by itself and talks to this
//! file over JNI: keeping clear of the system bars and keyboard, the back
//! button, going to the background, picking files, the clipboard, opening
//! Nostr links in other apps, and Nostr signer apps.
#![cfg(target_os = "android")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, SafeAreaInsets, epaint::MarginF32};
use feedstr_core::{App, FileKind, Platform, PlatformFuture, Remote, SignerApp};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::refs::Global;
use jni::{EnvUnowned, JValue, Outcome, jni_sig, jni_str};
use winit::platform::android::activity::AndroidApp;

/// The running activity, for calling into Java.
static ANDROID: Mutex<Option<AndroidApp>> = Mutex::new(None);
/// For telling the app about going to the background and back.
static REMOTE: Mutex<Option<Remote>> = Mutex::new(None);
/// To redraw when something below changes.
static EGUI: Mutex<Option<egui::Context>> = Mutex::new(None);
/// Space taken by the system bars, display cutouts and keyboard, in pixels:
/// left, top, right, bottom.
static INSETS: [AtomicI32; 4] = [const { AtomicI32::new(0) }; 4];
static BACK_PRESSED: AtomicBool = AtomicBool::new(false);
/// File picks and signer requests waiting for an answer, by request number.
static PENDING: Mutex<Option<HashMap<i32, oneshot::Sender<Reply>>>> = Mutex::new(None);
static NEXT_REQUEST: AtomicI32 = AtomicI32::new(1);

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_tag("feedstr").with_max_level(log::LevelFilter::Info),
    );
    let Some(dir) = app.internal_data_path() else {
        log::error!("no internal storage");
        return;
    };
    *ANDROID.lock().unwrap() = Some(app.clone());
    let options = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
    let result = eframe::run_native(
        "Feedstr",
        options,
        Box::new(move |cc| {
            *EGUI.lock().unwrap() = Some(cc.egui_ctx.clone());
            let app = App::new(cc, dir.join("feedstr.db"), Arc::new(Android))?;
            *REMOTE.lock().unwrap() = Some(app.remote());
            Ok(Box::new(Droid { app, editing: None, paste: None }))
        }),
    );
    if let Err(e) = result {
        log::error!("Feedstr stopped: {e}");
    }
}

/// The shared app, plus what Android needs around it.
struct Droid {
    app: App,
    /// The text field last being edited, to paste into.
    editing: Option<egui::Id>,
    /// Text to paste in the next frame.
    paste: Option<String>,
}

impl eframe::App for Droid {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if BACK_PRESSED.swap(false, Ordering::Relaxed) && !self.app.go_back() {
            // Nothing left to go back from: leave the app, as other apps do.
            if let Err(e) = with_activity(|env, activity| {
                env.call_method(activity, jni_str!("moveTaskToBack"), jni_sig!("(Z)Z"), &[JValue::Bool(true)])?;
                Ok(())
            }) {
                log::error!("leaving the app failed: {e}");
            }
        }
        // Lay the app out clear of the system bars and keyboard.
        let ctx = ui.ctx().clone();
        ui.scope_builder(egui::UiBuilder::new().max_rect(ctx.content_rect()), |ui| self.app.ui(ui, frame));
        self.clipboard(&ctx);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(text) = self.paste.take() {
            raw_input.events.push(egui::Event::Paste(text));
        }
        let ppp = ctx.pixels_per_point();
        let [left, top, right, bottom] = INSETS.each_ref().map(|i| i.load(Ordering::Relaxed) as f32 / ppp);
        raw_input.safe_area_insets = Some(SafeAreaInsets(MarginF32 { left, right, top, bottom }));
    }
}

impl Droid {
    /// egui's clipboard doesn't work on Android, and the keyboard can't paste
    /// into a NativeActivity: copied text goes to Android's clipboard here,
    /// and a Paste button shows while a text field is being edited.
    fn clipboard(&mut self, ctx: &egui::Context) {
        let copied: Vec<String> = ctx.output(|o| {
            o.commands
                .iter()
                .filter_map(|c| match c {
                    egui::OutputCommand::CopyText(text) => Some(text.clone()),
                    _ => None,
                })
                .collect()
        });
        for text in copied {
            let copy = with_activity(|env, activity| {
                let text = env.new_string(&text)?;
                let sig = jni_sig!("(Ljava/lang/String;)V");
                env.call_method(activity, jni_str!("setClipboardText"), sig, &[JValue::Object(&text)])?;
                Ok(())
            });
            if let Err(e) = copy {
                log::error!("copying failed: {e:#}");
            }
        }
        // Tapping the button takes the focus from the text field in the same
        // frame as the tap, so it stays one frame longer than the editing.
        let editing = ctx.egui_wants_keyboard_input();
        if editing {
            self.editing = ctx.memory(|m| m.focused());
        } else if self.editing.is_none() {
            return;
        }
        let paste = egui::Area::new(egui::Id::unique("paste"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -12.0))
            // Above dialogs and their backdrop, which closes them when tapped.
            .order(egui::Order::Tooltip)
            .show(ctx, |ui| ui.button("Paste").clicked())
            .inner;
        if paste {
            match clipboard_text() {
                Ok(text) => self.paste = text,
                Err(e) => log::error!("pasting failed: {e:#}"),
            }
            // Pressing the button took the focus from the text field: give it back.
            if let Some(id) = self.editing {
                ctx.memory_mut(|m| m.request_focus(id));
            }
            ctx.request_repaint();
        } else if !editing {
            self.editing = None;
        }
    }
}

fn clipboard_text() -> Result<Option<String>> {
    with_activity(|env, activity| {
        let sig = jni_sig!("()Ljava/lang/String;");
        let text = env.call_method(activity, jni_str!("getClipboardText"), sig, &[])?.l()?;
        if text.is_null() {
            return Ok(None);
        }
        let text = env.cast_local::<JString>(text)?;
        Ok(Some(text.mutf8_chars(env)?.to_string()))
    })
}

fn repaint() {
    if let Some(ctx) = EGUI.lock().unwrap().as_ref() {
        ctx.request_repaint();
    }
}

/// Runs `f` with the activity (`MainActivity`) on this thread.
fn with_activity<T>(f: impl FnOnce(&mut jni::Env, &JObject) -> jni::errors::Result<T>) -> Result<T> {
    let app = ANDROID.lock().unwrap().clone().context("the app isn't running")?;
    // Safety: android-activity hands out valid pointers to the VM and to a
    // global reference to the activity, for as long as the activity lives.
    let vm = unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let activity: jni::sys::jobject = app.activity_as_ptr().cast();
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        let activity = unsafe { env.as_cast_raw::<Global<JObject>>(&activity) }?;
        f(env, &activity)
    })
    .map_err(|e| anyhow!("calling Android failed: {e}"))
}

// ---- Files and links ----

enum Reply {
    Opened(Option<Vec<u8>>),
    Saved(Option<String>),
    /// From a signer app; None if the user said no.
    Signer(Option<String>),
    Failed(String),
}

/// Files are picked with Android's document picker, and Nostr links opened in
/// whatever app handles them, all through `MainActivity`.
struct Android;

impl Android {
    /// Starts a pick or request with `start` and waits for `MainActivity` to report back.
    fn ask(
        start: impl FnOnce(&mut jni::Env, &JObject, i32) -> jni::errors::Result<()>,
    ) -> BoxFuture<'static, Result<Reply>> {
        // Android only keeps the low 16 bits of a request code.
        let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed) & 0xffff;
        let (tx, rx) = oneshot::channel();
        PENDING.lock().unwrap().get_or_insert_default().insert(request, tx);
        let started = with_activity(|env, activity| start(env, activity, request));
        Box::pin(async move {
            started?;
            rx.await.map_err(|_| anyhow!("Android stopped waiting for an answer"))
        })
    }
}

impl Platform for Android {
    fn open_file(&self, _kind: FileKind) -> BoxFuture<'static, Result<Option<Vec<u8>>>> {
        let reply = Self::ask(|env, activity, request| {
            env.call_method(activity, jni_str!("openFile"), jni_sig!("(I)V"), &[JValue::Int(request)])?;
            Ok(())
        });
        Box::pin(async move {
            match reply.await? {
                Reply::Opened(data) => Ok(data),
                Reply::Failed(e) => Err(anyhow!(e)),
                _ => Err(anyhow!("unexpected answer")),
            }
        })
    }

    fn save_file(&self, kind: FileKind, name: String, content: Vec<u8>) -> BoxFuture<'static, Result<Option<String>>> {
        let reply = Self::ask(move |env, activity, request| {
            let name = env.new_string(&name)?;
            let mime = env.new_string(kind.mime())?;
            let data = env.byte_array_from_slice(&content)?;
            env.call_method(
                activity,
                jni_str!("saveFile"),
                jni_sig!("(ILjava/lang/String;Ljava/lang/String;[B)V"),
                &[JValue::Int(request), JValue::Object(&name), JValue::Object(&mime), JValue::Object(&data)],
            )?;
            Ok(())
        });
        Box::pin(async move {
            match reply.await? {
                Reply::Saved(place) => Ok(place),
                Reply::Failed(e) => Err(anyhow!(e)),
                _ => Err(anyhow!("unexpected answer")),
            }
        })
    }

    fn opens_nostr_links(&self) -> bool {
        true
    }

    fn open_nostr_link(&self, link: &str) -> Result<()> {
        let opened = with_activity(|env, activity| {
            let link = env.new_string(link)?;
            let sig = jni_sig!("(Ljava/lang/String;)Z");
            env.call_method(activity, jni_str!("openUri"), sig, &[JValue::Object(&link)])?.z()
        })?;
        anyhow::ensure!(opened, "No app for Nostr links is installed");
        Ok(())
    }

    fn signer_app(&self) -> Option<Arc<dyn SignerApp>> {
        let installed = with_activity(|env, activity| {
            env.call_method(activity, jni_str!("hasSignerApp"), jni_sig!("()Z"), &[])?.z()
        });
        match installed {
            Ok(true) => Some(Arc::new(AndroidSigner)),
            Ok(false) => None,
            Err(e) => {
                log::error!("looking for signer apps failed: {e:#}");
                None
            }
        }
    }

    fn forget_signer_app(&self) {
        let forget = with_activity(|env, activity| {
            env.call_method(activity, jni_str!("forgetSigner"), jni_sig!("()V"), &[])?;
            Ok(())
        });
        if let Err(e) = forget {
            log::error!("forgetting the signer app failed: {e:#}");
        }
    }
}

// ---- Nostr signer apps (NIP-55), like Amber ----

/// Asked for when signing in, so the app needn't ask the user every time.
const SIGNER_PERMISSIONS: &str = r#"[{"type":"sign_event","kind":30078},{"type":"sign_event","kind":5},{"type":"nip44_encrypt"},{"type":"nip44_decrypt"}]"#;

struct AndroidSigner;

impl AndroidSigner {
    /// Asks the signer app in the background, or through its own screen when
    /// it wants the user to decide.
    fn request(kind: &'static str, content: String, pubkey: String) -> PlatformFuture<String> {
        let quiet = with_activity(|env, activity| {
            let (kind, content, pubkey) = (env.new_string(kind)?, env.new_string(&content)?, env.new_string(&pubkey)?);
            let sig = jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;");
            let args = [JValue::Object(&kind), JValue::Object(&content), JValue::Object(&pubkey)];
            let answer = env.call_method(activity, jni_str!("signerQuery"), sig, &args)?.l()?;
            let answer = env.cast_local::<JString>(answer)?;
            Ok(answer.mutf8_chars(env)?.to_string())
        });
        match quiet {
            Ok(answer) if answer.starts_with("ok:") => Box::pin(async move { Ok(answer[3..].to_string()) }),
            Ok(answer) if answer == "rejected" => Box::pin(async { Err(anyhow!("the signer app refused")) }),
            _ => {
                let reply = Android::ask(move |env, activity, request| {
                    let (kind, content, pubkey) =
                        (env.new_string(kind)?, env.new_string(&content)?, env.new_string(&pubkey)?);
                    let sig = jni_sig!("(ILjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V");
                    let args = [
                        JValue::Int(request),
                        JValue::Object(&kind),
                        JValue::Object(&content),
                        JValue::Object(&pubkey),
                    ];
                    env.call_method(activity, jni_str!("signerAsk"), sig, &args)?;
                    Ok(())
                });
                Box::pin(async move { signer_answer(reply.await?) })
            }
        }
    }
}

fn signer_answer(reply: Reply) -> Result<String> {
    match reply {
        Reply::Signer(Some(answer)) => Ok(answer),
        Reply::Signer(None) => Err(anyhow!("cancelled or refused")),
        Reply::Failed(e) => Err(anyhow!(e)),
        _ => Err(anyhow!("unexpected answer")),
    }
}

impl SignerApp for AndroidSigner {
    fn public_key(&self) -> PlatformFuture<String> {
        let known = with_activity(|env, activity| {
            let key =
                env.call_method(activity, jni_str!("signerPublicKey"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
            if key.is_null() {
                return Ok(None);
            }
            let key = env.cast_local::<JString>(key)?;
            Ok(Some(key.mutf8_chars(env)?.to_string()))
        });
        if let Ok(Some(key)) = known {
            return Box::pin(async move { Ok(key) });
        }
        let reply = Android::ask(|env, activity, request| {
            let permissions = env.new_string(SIGNER_PERMISSIONS)?;
            let sig = jni_sig!("(ILjava/lang/String;)V");
            env.call_method(
                activity,
                jni_str!("signerGetPublicKey"),
                sig,
                &[JValue::Int(request), JValue::Object(&permissions)],
            )?;
            Ok(())
        });
        Box::pin(async move { signer_answer(reply.await?) })
    }

    fn sign_event(&self, event: String, pubkey: String) -> PlatformFuture<String> {
        Self::request("sign_event", event, pubkey)
    }

    fn nip44_encrypt(&self, text: String, to: String) -> PlatformFuture<String> {
        Self::request("nip44_encrypt", text, to)
    }

    fn nip44_decrypt(&self, payload: String, from: String) -> PlatformFuture<String> {
        Self::request("nip44_decrypt", payload, from)
    }
}

fn outcome<T>(outcome: Outcome<T, jni::errors::Error>) -> Result<T> {
    match outcome {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(e) => Err(anyhow!("{e}")),
        Outcome::Panic(_) => Err(anyhow!("crashed")),
    }
}

fn answer(request: i32, reply: Reply) {
    if let Some(tx) = PENDING.lock().unwrap().get_or_insert_default().remove(&request) {
        let _ = tx.send(reply);
    }
}

// ---- Called by MainActivity ----

#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativeInsets(
    _env: EnvUnowned,
    _class: JClass,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
) {
    for (inset, value) in INSETS.iter().zip([left, top, right, bottom]) {
        inset.store(value, Ordering::Relaxed);
    }
    repaint();
}

#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativeBack(_env: EnvUnowned, _class: JClass) {
    BACK_PRESSED.store(true, Ordering::Relaxed);
    repaint();
}

#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativePaused(_env: EnvUnowned, _class: JClass) {
    if let Some(remote) = REMOTE.lock().unwrap().as_ref() {
        remote.paused();
    }
}

#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativeResumed(_env: EnvUnowned, _class: JClass) {
    if let Some(remote) = REMOTE.lock().unwrap().as_ref() {
        remote.resumed();
    }
}

/// `data` is null if the user cancelled.
#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativeFileOpened<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request: i32,
    data: JByteArray<'caller>,
) {
    let reply = env
        .with_env(|env| -> jni::errors::Result<_> {
            Ok(if data.is_null() { None } else { Some(env.convert_byte_array(&data)?) })
        })
        .into_outcome();
    answer(request, outcome(reply).map_or_else(|e| Reply::Failed(e.to_string()), Reply::Opened));
}

/// `place` is null if the user cancelled.
#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativeFileSaved<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request: i32,
    place: JString<'caller>,
) {
    let reply = env
        .with_env(|env| -> jni::errors::Result<_> {
            Ok(if place.is_null() { None } else { Some(place.mutf8_chars(env)?.to_string()) })
        })
        .into_outcome();
    answer(request, outcome(reply).map_or_else(|e| Reply::Failed(e.to_string()), Reply::Saved));
}

#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativeFileFailed<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request: i32,
    message: JString<'caller>,
) {
    let message = env.with_env(|env| -> jni::errors::Result<_> { Ok(message.mutf8_chars(env)?.to_string()) });
    let message = outcome(message.into_outcome()).unwrap_or_else(|_| "unknown error".into());
    answer(request, Reply::Failed(message));
}

/// `text` is null if the user said no.
#[unsafe(no_mangle)]
extern "system" fn Java_top_dns7_feedstr_MainActivity_nativeSignerAnswer<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    request: i32,
    text: JString<'caller>,
) {
    let reply = env
        .with_env(|env| -> jni::errors::Result<_> {
            Ok(if text.is_null() { None } else { Some(text.mutf8_chars(env)?.to_string()) })
        })
        .into_outcome();
    answer(request, outcome(reply).map_or_else(|e| Reply::Failed(e.to_string()), Reply::Signer));
}
