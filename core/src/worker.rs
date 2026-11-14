//! Background thread doing everything slow: signing in, fetching feeds, syncing.
//!
//! A sync cycle pulls the latest changes from relays, fetches every feed, then
//! publishes what changed. It runs at startup, on demand, and optionally on a
//! schedule. Closing the app publishes any remaining changes. With syncing set
//! to Manual Only ([`MANUAL_SYNC`]), only [`Cmd::SyncNow`], signing in and
//! signing out talk to relays.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use nostr_sdk::prelude::{Keys, ToBech32};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::watch;

use crate::clock::{self, now_ms};
use crate::db::FetchInfo;
use crate::db::{self, Conn, Db, Store};
use crate::feeds::{self, Fetched};
use crate::import_export;
use crate::longform;
pub use crate::nostr::DEFAULT_RELAYS;
use crate::nostr::{NostrTransport, Signer};
use crate::sync::{self, Options, Transport};
use crate::youtube;

/// Feeds downloaded at once, and at most this many from the same site.
const FEED_FETCHES_IN_FLIGHT: usize = 32;
const FEED_FETCHES_PER_SITE: usize = 4;

/// In the browser, changes are uploaded this soon after they're made (the
/// tab can be closed at any moment), or this long after a failed upload.
const UPLOAD_DELAY: Duration = Duration::from_secs(4);
const UPLOAD_RETRY: Duration = Duration::from_secs(60);

/// Further tries at signing in again with what was saved (a second apart).
const SAVED_LOGIN_RETRIES: usize = 5;

/// Kept in a Nostr feed's (otherwise unused) ETag once this device has read it in full.
const NOSTR_FETCHED: &str = "fetched";

/// Whether this platform can fetch the feed: browsers can't read most
/// sites, so the web version only fetches Nostr feeds itself.
fn fetchable(url: &str) -> bool {
    !cfg!(target_arch = "wasm32") || longform::is_nostr(url)
}

/// Fires every [`UPLOAD_DELAY`] in the browser, and never natively (where
/// changes are uploaded on a schedule and on closing).
async fn upload_tick() {
    if cfg!(target_arch = "wasm32") { clock::sleep(UPLOAD_DELAY).await } else { std::future::pending().await }
}

/// Coming back after this long in the background counts as starting again (and syncs).
const RESUME_SYNC_AFTER: Duration = Duration::from_secs(15 * 60);

/// Automatic syncs (at start, on coming back, on a schedule) wait until this
/// long after the last complete sync, so opening the app again and again (say,
/// coming back from each article read in the browser) doesn't sync each time.
/// Syncs the user asks for don't wait.
const AUTO_SYNC_GAP: Duration = Duration::from_secs(10 * 60);

/// How often the web version syncs while open, unless told otherwise.
pub const WEB_SYNC_MINUTES: u32 = 10;

/// Never syncs on its own: only the Sync button (or R) syncs. Stored as the
/// number of minutes, so an older version reading it just never syncs on a
/// schedule.
pub const MANUAL_SYNC: u32 = u32::MAX;

/// Choices for how often to sync automatically, in minutes (0 = only at start
/// and exit, [`MANUAL_SYNC`] = never on its own).
pub const SYNC_SCHEDULES: &[u32] = &[MANUAL_SYNC, 0, 15, 30, 60, 180];

#[derive(Clone, Copy)]
pub enum FileKind {
    /// Subscriptions as an OPML file.
    Opml,
    /// Everything, as a Feedstr backup.
    Backup,
}

impl FileKind {
    /// What the file is called in file pickers.
    pub fn label(self) -> &'static str {
        match self {
            FileKind::Opml => "OPML",
            FileKind::Backup => "Feedstr backup",
        }
    }

    /// File name extensions, the usual one first.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            FileKind::Opml => &["opml", "xml"],
            FileKind::Backup => &["json"],
        }
    }

    /// Media type, for platforms that pick files by type.
    pub fn mime(self) -> &'static str {
        match self {
            FileKind::Opml => "text/x-opml",
            FileKind::Backup => "application/json",
        }
    }
}

/// What [`Platform`] methods return: natively a future that may move
/// between threads; in the (single-threaded) browser, any future.
#[cfg(not(target_arch = "wasm32"))]
pub type PlatformFuture<T> = futures::future::BoxFuture<'static, Result<T>>;
#[cfg(target_arch = "wasm32")]
pub type PlatformFuture<T> = futures::future::LocalBoxFuture<'static, Result<T>>;

/// What each platform does its own way: asking the user for a file to read
/// or a place to save one, handing Nostr links to other apps, and sharing links.
pub trait Platform: Send + Sync {
    /// The contents of a file the user picks, or `None` if they cancel.
    fn open_file(&self, kind: FileKind) -> PlatformFuture<Option<Vec<u8>>>;
    /// Saves `content` where the user picks, suggesting `name`. Returns a
    /// description of where it went, or `None` if they cancel.
    fn save_file(&self, kind: FileKind, name: String, content: Vec<u8>) -> PlatformFuture<Option<String>>;
    /// Whether `nostr:` links can be opened in the user's Nostr app.
    fn opens_nostr_links(&self) -> bool {
        false
    }
    /// Opens a `nostr:` link (NIP-21) in the user's Nostr app.
    fn open_nostr_link(&self, _link: &str) -> Result<()> {
        bail!("Nostr links can't be opened in other apps here")
    }
    /// Whether links can be handed to the system's share sheet; otherwise
    /// articles offer to copy their address instead.
    fn shares_links(&self) -> bool {
        false
    }
    /// Shares a web link (an article's, titled `title`) through the system's share sheet.
    fn share_link(&self, _title: &str, _link: &str) -> Result<()> {
        bail!("links can't be shared here")
    }
    /// A signer app installed on the device, to sign in with (`None` if there is none).
    fn signer_app(&self) -> Option<Arc<dyn SignerApp>> {
        None
    }
    /// Forgets which signer app and account were used (when signing out),
    /// even if the app has been uninstalled since.
    fn forget_signer_app(&self) {}
    /// Called with `true` when syncing starts and `false` once the worker has
    /// nothing left to do, so the platform can keep the app running in the
    /// background meanwhile (Android: a foreground service with a notification).
    fn keep_running(&self, _on: bool) {}
}

/// A signer app on the device that holds the user's key (on Android: NIP-55,
/// like Amber), provided by the platform.
///
/// It deals in the app's own text (keys in hex or npub, events as JSON); the
/// answers are checked before use.
pub trait SignerApp: Send + Sync {
    /// The user's public key; the first time, the app asks the user to allow Feedstr.
    fn public_key(&self) -> PlatformFuture<String>;
    /// Signs `event` (unsigned, as JSON) for `pubkey` (hex). The answer is the
    /// signed event as JSON, or just its signature.
    fn sign_event(&self, event: String, pubkey: String) -> PlatformFuture<String>;
    fn nip44_encrypt(&self, text: String, to: String) -> PlatformFuture<String>;
    fn nip44_decrypt(&self, payload: String, from: String) -> PlatformFuture<String>;
}

pub enum Login {
    SyncKey(String),
    Bunker(String),
    /// A remote signer scanning a QR code (`nostrconnect://`, NIP-46); not offered on phones.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    QrCode,
    /// A Nostr browser extension (NIP-07), on the web.
    #[cfg(target_arch = "wasm32")]
    Extension,
    /// A signer app on the device (NIP-55 on Android).
    SignerApp,
    /// Sign in again with what was saved last time.
    Saved,
}

pub enum Cmd {
    Login(Login),
    /// Signs out after a final sync. Unless forced, refuses if that sync fails.
    Logout {
        force: bool,
    },
    /// Runs a sync cycle now.
    SyncNow,
    AddFeed {
        input: String,
        category: String,
    },
    /// Finds the YouTube channel behind a channel's web address (see
    /// [`youtube::channel_url`]); answers with [`Msg::YoutubeChannel`].
    LookUpYoutube(String),
    /// Subscribes to a YouTube channel found by [`Cmd::LookUpYoutube`];
    /// answers with [`Msg::FeedAdded`].
    AddYoutube {
        channel: youtube::Channel,
        /// Leave out Shorts and livestreams.
        videos_only: bool,
        category: String,
    },
    RelaysChanged,
    /// Fetches one feed now (say, one just switched on).
    FetchFeed(String),
    ScheduleChanged,
    /// Asks for a file and imports it.
    Import(FileKind),
    /// Asks where to save, and exports.
    Export(FileKind),
    /// Deletes all Feedstr data from relays and this device, and signs out.
    EraseAll,
    /// The app went to the background, where phones may close it without
    /// notice: saves changes to relays. `kept_running`: the platform was asked
    /// to keep the app running meanwhile (see [`Remote::paused`]).
    Paused { kept_running: bool },
    /// The app is back from the background, after `away_ms` (0 if it wasn't
    /// known to be away): syncs if that was a while.
    Resumed { away_ms: i64 },
    /// Publishes remaining changes, answers with [`Msg::ExitReady`] and stops.
    Exit,
}

pub enum Msg {
    LoggedIn {
        npub: String,
    },
    /// The `nostrconnect://` link to show as a QR code while signing in.
    ConnectLink(String),
    LoginFailed(String),
    LoggedOut,
    /// Signing out was refused because changes couldn't be saved to relays.
    LogoutBlocked,
    Status(String),
    /// Like [`Msg::Status`], for something under way: how far along it is, from 0 to 1.
    Progress(String, f32),
    FeedAdded(Result<String, String>),
    YoutubeChannel(Result<youtube::Channel, String>),
    /// The database changed; reload what's on screen.
    Changed,
    /// An import or export finished: what happened, or `None` if the user cancelled.
    FileDone(Option<Result<String, String>>),
    /// Deleting everything finished (and signed out), or failed and changed nothing here.
    Erased(Result<String, String>),
    ExitReady,
}

/// The relays this device uses: the user's own list once they've changed it
/// (on any of their devices, see [`crate::docs::RelayList`]), otherwise this
/// version's defaults (so new defaults in an update apply).
pub fn relays(db: &Db) -> Vec<String> {
    match db.relay_list() {
        Ok(list) if list.is_custom() => list.listed(),
        _ => DEFAULT_RELAYS.iter().map(|r| r.to_string()).collect(),
    }
}

/// Whether the user has their own relay list instead of the defaults.
pub fn custom_relays(db: &Db) -> bool {
    db.relay_list().is_ok_and(|list| list.is_custom())
}

/// Saves the user's relay list, to be synced to their other devices. If it's
/// just the defaults, the devices go back to following the defaults, so later
/// versions can change them.
pub fn set_relays(db: &Db, relays: &[String]) -> Result<()> {
    let mut list: Vec<String> = Vec::new();
    for relay in relays.iter().filter_map(|r| parse_relay(r)) {
        if !list.contains(&relay) {
            list.push(relay);
        }
    }
    let set = |list: &[String]| list.iter().cloned().collect::<std::collections::BTreeSet<_>>();
    let defaults: Vec<String> = DEFAULT_RELAYS.iter().filter_map(|r| parse_relay(r)).collect();
    db.set_relay_list(&list, set(&list) != set(&defaults))
}

/// A relay address as typed, cleaned up (`wss://relay.example`), or `None` if
/// it isn't one. Relays from older versions are stored the same way.
pub fn parse_relay(input: &str) -> Option<String> {
    let url = reqwest::Url::parse(input.trim()).ok()?;
    let valid = matches!(url.scheme(), "ws" | "wss") && url.has_host() && url.query().is_none();
    valid.then(|| url.as_str().trim_end_matches('/').to_string())
}

/// Minutes between automatic sync cycles; 0 means only at start and exit,
/// [`MANUAL_SYNC`] only when asked to.
pub fn sync_minutes(db: &Db) -> u32 {
    db.meta("sync_minutes").ok().flatten().and_then(|m| m.parse().ok()).unwrap_or(0)
}

/// Whether syncing is Manual Only: nothing is downloaded or uploaded unless
/// the Sync button (or R) is used, not at start, in the background or on closing.
pub fn manual_sync(db: &Db) -> bool {
    sync_minutes(db) == MANUAL_SYNC
}

/// Whether anything changed since the relays last took everything (so
/// going to the background right after a sync doesn't sync again).
fn has_changes(db: &Db) -> bool {
    let uploaded = db.meta("uploaded_rev").ok().flatten().and_then(|r| r.parse::<i64>().ok());
    uploaded.is_none_or(|uploaded| db.revision().ok() != Some(uploaded))
}

/// The worker's side of the app: commands go in, messages come out. Sending
/// `true` on `closing` cuts short what is running (like fetching feeds), so
/// [`Cmd::Exit`] gets to save changes sooner.
pub struct Handle {
    pub tx: UnboundedSender<Cmd>,
    pub rx: mpsc::Receiver<Msg>,
    pub remote: Remote,
    pub closing: watch::Sender<bool>,
    /// Stops waiting for a QR code sign-in.
    pub cancel_connect: Arc<tokio::sync::Notify>,
}

/// Tells the worker about the app's life cycle from any thread (say, Android's).
#[derive(Clone)]
pub struct Remote {
    tx: UnboundedSender<Cmd>,
    life: Arc<Lifecycle>,
    /// To tell whether going to the background leaves changes to upload.
    store: Store,
}

/// Whether the app is in the background, and since when. Kept apart from the
/// commands, which wait while the worker is busy: when it gets to them, it can
/// tell whether the app is still in the background, and how long it really was.
#[derive(Default)]
struct Lifecycle {
    hidden: AtomicBool,
    hidden_at: AtomicI64,
    /// Whether the worker asked to keep the app running (see [`Platform::keep_running`]).
    busy: AtomicBool,
}

impl Remote {
    /// Returns whether the worker has something to finish in the background
    /// (a sync under way, or changes to upload) and got the news: the app should
    /// then be kept running, and the worker tells the platform when it can stop.
    pub fn paused(&self) -> bool {
        self.life.hidden_at.store(now_ms(), Ordering::Relaxed);
        self.life.hidden.store(true, Ordering::Relaxed);
        let work = self.life.busy.load(Ordering::Relaxed)
            || db::connect(&self.store).is_ok_and(|db| {
                db.meta("login").ok().flatten().is_some() && !manual_sync(&db) && has_changes(&db)
            });
        self.tx.send(Cmd::Paused { kept_running: work }).is_ok() && work
    }

    pub fn resumed(&self) {
        let was_hidden = self.life.hidden.swap(false, Ordering::Relaxed);
        let away_ms = if was_hidden { now_ms() - self.life.hidden_at.load(Ordering::Relaxed) } else { 0 };
        let _ = self.tx.send(Cmd::Resumed { away_ms });
    }

    /// Uploads what's left and stops syncing (say, when another browser tab takes over).
    pub fn exit(&self) {
        let _ = self.tx.send(Cmd::Exit);
    }
}

/// Starts the worker: on a thread of its own natively, as a task of the page in the browser.
pub fn spawn(store: &Store, ctx: eframe::egui::Context, platform: Arc<dyn Platform>) -> Result<Handle> {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (closing_tx, closing) = watch::channel(false);
    let cancel_connect = Arc::new(tokio::sync::Notify::new());
    let mut worker = Worker::new(db::connect(store)?, msg_tx, ctx, closing, platform);
    worker.cancel_connect = cancel_connect.clone();
    let remote = Remote { tx: cmd_tx.clone(), life: worker.life.clone(), store: store.clone() };
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(worker.run(cmd_rx));
    });
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(worker.run(cmd_rx));
    Ok(Handle { tx: cmd_tx, rx: msg_rx, remote, closing: closing_tx, cancel_connect })
}

struct Worker {
    db: Conn,
    http: reqwest::Client,
    nostr: Option<NostrTransport>,
    /// Reads long-form posts; made when first needed.
    reader: tokio::sync::Mutex<Option<Arc<longform::Reader>>>,
    msg_tx: mpsc::Sender<Msg>,
    ctx: eframe::egui::Context,
    /// Becomes true when the app is closing.
    closing: watch::Receiver<bool>,
    platform: Arc<dyn Platform>,
    cancel_connect: Arc<tokio::sync::Notify>,
    /// Shared with [`Remote`].
    life: Arc<Lifecycle>,
    /// Whether the platform was asked to keep the app running (see [`Platform::keep_running`]).
    kept_running: std::cell::Cell<bool>,
}

impl Worker {
    fn new(
        db: Conn,
        msg_tx: mpsc::Sender<Msg>,
        ctx: eframe::egui::Context,
        closing: watch::Receiver<bool>,
        platform: Arc<dyn Platform>,
    ) -> Self {
        let reader = tokio::sync::Mutex::default();
        let cancel_connect = Arc::default();
        Worker {
            db,
            http: feeds::http_client(),
            nostr: None,
            reader,
            msg_tx,
            ctx,
            closing,
            platform,
            cancel_connect,
            life: Arc::default(),
            kept_running: std::cell::Cell::new(false),
        }
    }

    fn send(&self, msg: Msg) {
        let _ = self.msg_tx.send(msg);
        self.ctx.request_repaint();
    }

    fn status(&self, text: impl Into<String>) {
        self.send(Msg::Status(text.into()));
    }

    fn progress(&self, text: impl Into<String>, done: usize, total: usize) {
        self.send(Msg::Progress(text.into(), done as f32 / total.max(1) as f32));
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Cmd>) {
        let mut next_cycle = self.next_cycle();
        // When to try again after a failed upload in the browser.
        let mut retry_at = 0;
        loop {
            // Nothing left to do (for now): the app needn't be kept running.
            if self.kept_running.get() && rx.is_empty() {
                self.platform.keep_running(false);
                self.set_kept_running(false);
            }
            // A sync can bring a relay list changed on another device.
            if self.nostr.as_ref().is_some_and(|nostr| nostr.relays() != relays(&self.db)) {
                self.reconnect().await;
            }
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Cmd::Login(login) => match (matches!(login, Login::Saved), self.login_with_retries(login).await) {
                            (saved, Ok(npub)) => {
                                self.send(Msg::LoggedIn { npub });
                                if !saved {
                                    self.follows_by_default().await;
                                }
                                // Signing in on purpose always syncs; starting up only when it's
                                // automatic, and not again right after the last time.
                                if !saved || (!manual_sync(&self.db) && self.auto_sync_due()) {
                                    self.cycle(false).await;
                                } else if manual_sync(&self.db) {
                                    // Replaces "Connecting to relays…", which would stay up otherwise.
                                    self.status("Manual only: Sync to sync now");
                                } else {
                                    self.status("Synced less than 10 minutes ago: Sync to sync again now");
                                }
                                next_cycle = self.next_cycle();
                            }
                            (_, Err(e)) => self.send(Msg::LoginFailed(format!("{e:#}"))),
                        },
                        Cmd::Logout { force } => self.logout(force).await,
                        Cmd::SyncNow => {
                            self.cycle(false).await;
                            next_cycle = self.next_cycle();
                        }
                        // Adding and fetching feeds go on if the app is left meanwhile.
                        Cmd::AddFeed { input, category } => {
                            self.keep_running();
                            let result = self.add_feed(&input, &category).await.map_err(|e| format!("{e:#}"));
                            self.send(Msg::FeedAdded(result));
                            self.send(Msg::Changed);
                        }
                        Cmd::LookUpYoutube(input) => {
                            self.keep_running();
                            self.status("Looking up the channel…");
                            let result = match youtube::channel_url(&input) {
                                Some(url) => youtube::look_up(&self.http, &url).await.map_err(|e| format!("{e:#}")),
                                None => Err("that is not a YouTube channel's address".to_string()),
                            };
                            self.status(if result.is_ok() { "" } else { "Looking up the channel failed" });
                            self.send(Msg::YoutubeChannel(result));
                        }
                        Cmd::AddYoutube { channel, videos_only, category } => {
                            self.keep_running();
                            let url = channel.feed_url(videos_only);
                            let result = self.subscribe(&url, &category, channel.name.as_deref()).await;
                            self.send(Msg::FeedAdded(result.map(|()| url).map_err(|e| format!("{e:#}"))));
                            self.send(Msg::Changed);
                        }
                        Cmd::RelaysChanged => self.reconnect().await,
                        Cmd::FetchFeed(url) => {
                            self.keep_running();
                            self.status("Fetching…");
                            let status = match self.fetch_feed(&url).await {
                                Ok(()) => "Fetched".to_string(),
                                Err(e) => format!("Fetching failed: {e:#}"),
                            };
                            self.status(status);
                            self.send(Msg::Changed);
                        }
                        Cmd::ScheduleChanged => next_cycle = self.next_cycle(),
                        Cmd::Paused { kept_running } => {
                            // The platform may have started keeping the app running when it
                            // went to the background (Android does); stopped once idle.
                            if kept_running {
                                self.set_kept_running(true);
                            }
                            // Back already (this waited for a sync to finish): nothing to save
                            // in a hurry, and the sync just uploaded anyway.
                            let hidden = self.life.hidden.load(Ordering::Relaxed);
                            if hidden && self.nostr.is_some() && !manual_sync(&self.db) && self.has_changes() {
                                self.push(true).await;
                            }
                        }
                        Cmd::Resumed { away_ms } => {
                            let long = away_ms > RESUME_SYNC_AFTER.as_millis() as i64;
                            if long && !manual_sync(&self.db) && self.auto_sync_due() {
                                self.cycle(false).await;
                                next_cycle = self.next_cycle();
                            }
                        }
                        Cmd::Import(kind) => {
                            let result = self.import(kind).await.map_err(|e| format!("Import failed: {e:#}"));
                            self.send(Msg::FileDone(result.transpose()));
                            self.send(Msg::Changed);
                        }
                        Cmd::Export(kind) => {
                            let result = self.export(kind).await.map_err(|e| format!("Export failed: {e:#}"));
                            self.send(Msg::FileDone(result.transpose()));
                        }
                        Cmd::EraseAll => {
                            let result = self.erase_all().await.map_err(|e| format!("{e:#}"));
                            if result.is_ok() {
                                next_cycle = self.next_cycle();
                            }
                            self.send(Msg::Erased(result));
                        }
                        Cmd::Exit => {
                            if self.nostr.is_some() && !manual_sync(&self.db) && self.has_changes() {
                                self.status("Saving changes to relays…");
                                self.push(true).await;
                            }
                            if self.kept_running.get() {
                                self.platform.keep_running(false);
                            }
                            self.send(Msg::ExitReady);
                            break;
                        }
                    }
                }
                _ = clock::sleep(Duration::from_millis((next_cycle - now_ms()).max(0) as u64)) => {
                    self.cycle(true).await;
                    next_cycle = self.next_cycle();
                }
                _ = upload_tick(), if self.nostr.is_some() && !manual_sync(&self.db) => {
                    if self.has_changes() && now_ms() >= retry_at && !self.push(true).await {
                        retry_at = now_ms() + UPLOAD_RETRY.as_millis() as i64;
                    }
                }
            }
        }
    }

    /// Connects to the relays in the (changed) relay list.
    async fn reconnect(&mut self) {
        *self.reader.lock().await = None;
        if self.nostr.is_some()
            && let Err(e) = self.login_with_retries(Login::Saved).await
        {
            self.send(Msg::LoginFailed(format!("{e:#}")));
        }
    }

    /// When the next scheduled sync is due (ms).
    fn next_cycle(&self) -> i64 {
        const NEVER: i64 = 365 * 24 * 3600 * 1000;
        match sync_minutes(&self.db) {
            _ if self.nostr.is_none() => now_ms() + NEVER,
            MANUAL_SYNC => now_ms() + NEVER,
            0 if cfg!(target_arch = "wasm32") => self.not_before_gap(now_ms() + WEB_SYNC_MINUTES as i64 * 60 * 1000),
            0 => now_ms() + NEVER,
            minutes => self.not_before_gap(now_ms() + minutes as i64 * 60 * 1000),
        }
    }

    /// `at`, or later if that is too soon after the last complete sync (see [`AUTO_SYNC_GAP`]).
    fn not_before_gap(&self, at: i64) -> i64 {
        at.max(self.synced_at() + AUTO_SYNC_GAP.as_millis() as i64)
    }

    /// When the last complete sync (download, feeds and upload all done) finished (ms).
    fn synced_at(&self) -> i64 {
        self.db.meta("synced_at").ok().flatten().and_then(|t| t.parse().ok()).unwrap_or(0)
    }

    /// Whether an automatic sync may run now (see [`AUTO_SYNC_GAP`]).
    fn auto_sync_due(&self) -> bool {
        now_ms() - self.synced_at() >= AUTO_SYNC_GAP.as_millis() as i64
    }

    /// Asks the platform to keep the app running until the worker is idle again.
    fn keep_running(&self) {
        if !self.kept_running.get() {
            self.platform.keep_running(true);
            self.set_kept_running(true);
        }
    }

    /// Pulls from relays, fetches every feed, then publishes what changed.
    async fn cycle(&self, throttle_feed_caches: bool) {
        self.keep_running();
        let read = self.sync(Options { publish: false, throttle_feed_caches }).await.saved;
        self.refresh().await;
        let saved = self.push(throttle_feed_caches).await;
        // Fetching feeds stops early when the app is closing, so that doesn't count as complete.
        if read && saved && !*self.closing.borrow() {
            let _ = self.db.set_meta("synced_at", &now_ms().to_string());
        }
    }

    /// Publishes everything waiting. Returns whether it was all saved.
    async fn push(&self, throttle_feed_caches: bool) -> bool {
        self.keep_running();
        let saved = self.sync(Options { publish: true, throttle_feed_caches }).await.saved;
        // What the sync merged was published too, so this is the revision after it.
        // Kept in the database, so a restart doesn't upload again with nothing changed.
        if saved && let Ok(revision) = self.db.revision() {
            let _ = self.db.set_meta("uploaded_rev", &revision.to_string());
        }
        saved
    }

    fn has_changes(&self) -> bool {
        has_changes(&self.db)
    }

    /// Records whether the platform is keeping the app running, for [`Remote::paused`] too.
    fn set_kept_running(&self, on: bool) {
        self.kept_running.set(on);
        self.life.busy.store(on, Ordering::Relaxed);
    }

    /// Signing in again with what was saved is tried a few times: a browser
    /// extension or a signer may not be ready yet when the app starts.
    async fn login_with_retries(&mut self, login: Login) -> Result<String> {
        let saved = matches!(login, Login::Saved);
        let mut result = self.login(login).await;
        for _ in 0..SAVED_LOGIN_RETRIES {
            if !saved || result.is_ok() {
                break;
            }
            clock::sleep(Duration::from_secs(1)).await;
            result = self.login(Login::Saved).await;
        }
        result
    }

    async fn login(&mut self, login: Login) -> Result<String> {
        let signer = match login {
            Login::SyncKey(nsec) => {
                let keys = Keys::parse(nsec.trim()).context("that is not a valid Sync Key")?;
                self.db.set_meta("login", "sync_key")?;
                self.db.set_meta("sync_key", &keys.secret_key().to_bech32()?)?;
                Signer::Keys(keys)
            }
            Login::Bunker(uri) => {
                self.status("Waiting for the remote signer to approve…");
                let signer = Signer::bunker(&uri, &self.bunker_app_key()?)?;
                signer.public_key().await.context("the remote signer did not respond")?;
                self.db.set_meta("login", "bunker")?;
                self.db.set_meta("bunker_uri", uri.trim())?;
                signer
            }
            Login::QrCode => {
                let (link, signer) = Signer::nostr_connect(&self.bunker_app_key()?, &relays(&self.db))?;
                self.send(Msg::ConnectLink(link));
                self.status("Waiting for your signer app…");
                tokio::select! {
                    key = signer.public_key() => {
                        key.context("the signer app didn't connect")?;
                    }
                    () = self.cancel_connect.notified() => bail!("cancelled"),
                }
                // From now on it's a remote signer like any other.
                let uri = signer.bunker_uri().await.context("the signer app didn't say how to reach it")?;
                self.db.set_meta("login", "bunker")?;
                self.db.set_meta("bunker_uri", &uri)?;
                signer
            }
            #[cfg(target_arch = "wasm32")]
            Login::Extension => {
                self.status("Waiting for the browser extension…");
                let signer = Signer::extension()?;
                self.db.set_meta("login", "extension")?;
                signer
            }
            Login::SignerApp => {
                self.status("Waiting for the signer app…");
                let signer = Signer::App(self.platform.signer_app().context("no signer app is installed")?);
                signer.public_key().await.context("the signer app didn't give Feedstr your key")?;
                self.db.set_meta("login", "signer_app")?;
                signer
            }
            Login::Saved => match self.db.meta("login")?.as_deref() {
                Some("signer_app") => {
                    Signer::App(self.platform.signer_app().context("the signer app isn't installed any more")?)
                }
                Some("sync_key") => Signer::Keys(Keys::parse(&self.db.meta("sync_key")?.unwrap_or_default())?),
                Some("extension") => Signer::extension()?,
                Some("bunker") => {
                    self.status("Connecting to the remote signer…");
                    Signer::bunker(&self.db.meta("bunker_uri")?.unwrap_or_default(), &self.bunker_app_key()?)?
                }
                _ => bail!("not signed in"),
            },
        };
        if let Some(old) = self.nostr.take() {
            old.shutdown().await;
        }
        self.status("Connecting to relays…");
        let nostr = NostrTransport::new(signer, relays(&self.db)).await?;
        let pubkey = nostr.pubkey().to_hex();
        // Never mix two accounts' data in one database.
        if self.db.meta("pubkey")?.is_some_and(|old| old != pubkey) {
            self.db.wipe_account(false)?;
        }
        self.db.set_meta("pubkey", &pubkey)?;
        let npub = nostr.pubkey().to_bech32()?;
        self.nostr = Some(nostr);
        Ok(npub)
    }

    /// The key identifying this app to the remote signer; kept so approval survives restarts.
    fn bunker_app_key(&self) -> Result<Keys> {
        if let Some(secret) = self.db.meta("bunker_app_key")? {
            return Ok(Keys::parse(&secret)?);
        }
        let keys = Keys::generate();
        self.db.set_meta("bunker_app_key", &keys.secret_key().to_secret_hex())?;
        Ok(keys)
    }

    async fn logout(&mut self, force: bool) {
        self.status("Saving changes to relays before signing out…");
        let saved = self.push(true).await;
        if !saved && !force {
            self.send(Msg::LogoutBlocked);
            return;
        }
        if let Some(nostr) = self.nostr.take() {
            nostr.shutdown().await;
        }
        self.platform.forget_signer_app();
        if let Err(e) = self.db.wipe_account(true) {
            self.status(format!("Signing out failed: {e:#}"));
        }
        self.send(Msg::LoggedOut);
    }

    async fn erase_all(&mut self) -> Result<String> {
        let Some(nostr) = &self.nostr else { bail!("not connected to relays, so nothing was deleted") };
        // Learn about feeds added on other devices, so their data goes too.
        self.status("Checking for data from your other devices…");
        self.sync(Options { publish: false, throttle_feed_caches: true }).await;
        self.status("Deleting your data from relays…");
        let total = nostr.relays().len();
        let erased = sync::erase(&self.db, nostr).await?;
        if erased == 0 {
            bail!("no relay accepted the deletion, so nothing was deleted. Check your connection and try again");
        }
        if let Some(nostr) = self.nostr.take() {
            nostr.shutdown().await;
        }
        self.platform.forget_signer_app();
        self.db.wipe_everything()?;
        let mut done = format!("Your Feedstr data was deleted from {erased} of {total} relays and from this device.");
        if erased < total {
            done +=
                " Relays that couldn't be reached are cleaned up when one of your other devices next syncs with them.";
        }
        Ok(done)
    }

    async fn add_feed(&self, input: &str, category: &str) -> Result<String> {
        let url = if let Some(author) = longform::parse_author(input) {
            longform::author_url(&author)
        } else if longform::is_nip05(input) {
            longform::author_url(&longform::resolve_nip05(&self.http, input).await?)
        } else {
            match feeds::discover(&self.http, input).await {
                Ok(url) => url,
                // Most sites don't let browser pages read them; the other
                // devices fetch the feed (and report it if it isn't one).
                Err(_) if !fetchable(input) => feeds::typed_url(input)?.to_string(),
                Err(e) => return Err(e),
            }
        };
        self.subscribe(&url, category, None).await?;
        Ok(url)
    }

    /// Subscribes to a feed and fetches it. `title` names it until the first
    /// fetch does (which may only happen on another device).
    async fn subscribe(&self, url: &str, category: &str, title: Option<&str>) -> Result<()> {
        self.db.set_subscribed(url, true)?;
        if !category.trim().is_empty() {
            self.db.set_category(url, category)?;
        }
        if let Some(title) = title {
            self.db.set_title_if_empty(url, title)?;
        }
        if fetchable(url) {
            self.fetch_feed(url).await?;
        } else {
            self.status("Added. Your other devices fetch this feed; its articles show up here after they sync.");
        }
        Ok(())
    }

    async fn fetch_feed(&self, url: &str) -> Result<()> {
        if longform::is_nostr(url) {
            // Articles from other devices' caches don't count: each device
            // reads the full history once, then only what's new.
            let fetched_before = self.db.fetch_info(url)?.etag.as_deref() == Some(NOSTR_FETCHED);
            return match self.fetch_longform(url, fetched_before).await {
                Ok(feed) => {
                    self.db.store_fetched(url, &feed.title, &feed.site, &feed.items)?;
                    // Finding nothing may only mean the relays were slow to answer:
                    // the history isn't counted as read until posts come back.
                    let fetched = fetched_before || !feed.items.is_empty();
                    let done = FetchInfo { etag: fetched.then(|| NOSTR_FETCHED.into()), last_modified: None };
                    self.db.set_fetch_result(url, &done, None)
                }
                Err(e) => {
                    self.db.set_fetch_error(url, &format!("{e:#}"))?;
                    Err(e)
                }
            };
        }
        let cache = self.db.fetch_info(url)?;
        match feeds::fetch(&self.http, url, &cache).await {
            Ok(Fetched::NotModified) => self.db.set_fetch_result(url, &cache, None),
            Ok(Fetched::Feed(feed, info)) => {
                self.db.store_fetched(url, &feed.title, &feed.site, &feed.items)?;
                self.db.set_fetch_result(url, &info, None)
            }
            Err(e) => {
                self.db.set_fetch_error(url, &format!("{e:#}"))?;
                Err(e)
            }
        }
    }

    /// A Nostr feed: one person's long-form posts, or everyone the user follows.
    /// `fetched_before`: this device read the feed in full already, so only newer posts are asked for.
    async fn fetch_longform(&self, url: &str, fetched_before: bool) -> Result<feeds::Feed> {
        let reader = self.reader().await;
        let newest = if fetched_before { self.db.newest_published(url)? } else { None };
        if url == longform::FOLLOWS_URL {
            let user =
                self.nostr.as_ref().map(|n| n.pubkey()).context("sign in to see posts from people you follow")?;
            reader.fetch_follows(user, newest).await
        } else {
            let author = longform::parse_author(url).context("not a Nostr address")?;
            reader.fetch_author(author, newest).await
        }
    }

    /// The reader of public Nostr posts, made when first needed.
    async fn reader(&self) -> Arc<longform::Reader> {
        let mut reader = self.reader.lock().await;
        reader.get_or_insert_with(|| Arc::new(longform::Reader::new(relays(&self.db)))).clone()
    }

    /// At a fresh sign-in: turns on the follows feed if the account follows
    /// anyone on Nostr, unless one of the user's devices already turned it on
    /// or off. That's only known after reading the relays, so nothing happens
    /// if none of them answers.
    async fn follows_by_default(&self) {
        let downloaded = self.sync(Options { publish: false, throttle_feed_caches: false }).await.saved;
        let untouched = self.db.feed_known(longform::FOLLOWS_URL).is_ok_and(|known| !known);
        let Some(user) = self.nostr.as_ref().map(|n| n.pubkey()) else { return };
        if !downloaded || !untouched || !self.reader().await.follows_anyone(user).await {
            return;
        }
        if self.db.set_subscribed(longform::FOLLOWS_URL, true).is_ok() {
            let _ = self.db.set_title_if_empty(longform::FOLLOWS_URL, longform::FOLLOWS_TITLE);
            self.send(Msg::Changed);
        }
    }

    async fn refresh(&self) {
        self.fetch_feeds(&self.db.subscribed_feeds().unwrap_or_default()).await;
        if let Err(e) = self.db.prune() {
            log::warn!("pruning old items failed: {e:#}");
        }
        self.send(Msg::Changed);
    }

    async fn fetch_feeds(&self, urls: &[String]) {
        let urls: Vec<&String> = urls.iter().filter(|url| fetchable(url)).collect();
        let total = urls.len();
        let mut done = 0;
        // Many feeds from one site (say, YouTube channels) shouldn't get us blocked there.
        let mut sites: HashMap<String, Arc<Semaphore>> = HashMap::new();
        let fetches = urls.into_iter().map(|url| {
            // Nostr feeds read from many relays, not one site, so each gets its own limit.
            let site = match reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)) {
                Some(host) if !longform::is_nostr(url) => host,
                _ => url.to_string(),
            };
            let limit = sites.entry(site).or_insert_with(|| Arc::new(Semaphore::new(FEED_FETCHES_PER_SITE))).clone();
            async move {
                let _permit = limit.acquire().await;
                self.fetch_feed(url).await
            }
        });
        let mut fetches = futures::stream::iter(fetches.collect::<Vec<_>>()).buffer_unordered(FEED_FETCHES_IN_FLIGHT);
        let mut closing = self.closing.clone();
        loop {
            let result = tokio::select! {
                result = fetches.next() => result,
                // Feeds are fetched again next time; saving changes before closing matters more.
                _ = closing.wait_for(|closing| *closing) => None,
            };
            let Some(result) = result else { break };
            done += 1;
            if let Err(e) = result {
                log::warn!("fetching feed failed: {e:#}");
            }
            self.progress(format!("Fetching feeds… {done}/{total}"), done, total);
        }
    }

    /// Returns what was imported, or `None` if no file was picked.
    async fn import(&self, kind: FileKind) -> Result<Option<String>> {
        let Some(bytes) = self.platform.open_file(kind).await? else { return Ok(None) };
        let text = import_export::imported_text(bytes)?;
        Ok(Some(match kind {
            FileKind::Opml => {
                let import = import_export::import_opml(&self.db, &text)?;
                self.fetch_feeds(&import.added).await;
                let mut done = format!("Added {}", count(import.added.len(), "feed"));
                self.status(&done);
                match import.already_subscribed {
                    0 => {}
                    1 => done += " (1 was already subscribed)",
                    n => done += &format!(" ({n} were already subscribed)"),
                }
                done
            }
            FileKind::Backup => {
                let import = import_export::import_backup(&self.db, &text)?;
                format!("Imported {} and {}", count(import.feeds, "feed"), count(import.items, "item"))
            }
        }))
    }

    /// Returns where the file was saved, or `None` if the user cancelled.
    async fn export(&self, kind: FileKind) -> Result<Option<String>> {
        let date = chrono::Local::now().format("%Y-%m-%d");
        let (name, content) = match kind {
            FileKind::Opml => (format!("feedstr-feeds-{date}.opml"), import_export::export_opml(&self.db)?),
            FileKind::Backup => (format!("feedstr-backup-{date}.json"), import_export::export_backup(&self.db)?),
        };
        Ok(self.platform.save_file(kind, name, content.into_bytes()).await?.map(|place| format!("Saved to {place}")))
    }

    async fn sync(&self, options: Options) -> SyncOutcome {
        let mut outcome = SyncOutcome { saved: false };
        let Some(nostr) = &self.nostr else { return outcome };
        self.status(if options.publish { "Uploading changes…" } else { "Downloading changes…" });
        let erased_before = self.db.erased_at().unwrap_or(0);
        // Every step counts up from zero; the bar shows the step under way.
        // Repaint only when what's shown changes: an upload can send thousands of events.
        let shown = std::cell::RefCell::new(String::new());
        let progress = |p: sync::Progress| {
            let percent = p.done * 100 / p.total.max(1);
            let text = match p.step {
                sync::Step::Reading => format!("Reading from relays… {percent}%"),
                sync::Step::Decrypting => format!("Decrypting changes… {}/{}", p.done, p.total),
                sync::Step::Encrypting => format!("Encrypting changes… {}/{}", p.done, p.total),
                sync::Step::Uploading => format!("Uploading changes… {percent}%"),
            };
            if *shown.borrow() != text {
                self.progress(&text, p.done, p.total);
                *shown.borrow_mut() = text;
            }
        };
        let status = match sync::sync_reporting(&self.db, nostr, options, &progress).await {
            Ok(report) => {
                outcome = SyncOutcome { saved: report.relays_ok > 0 && report.problems.is_empty() };
                let time = chrono::Local::now().format("%H:%M");
                let mut status = format!("Synced at {time} · {}/{} relays", report.relays_ok, report.relays_total);
                if report.relays_ok == 0 {
                    status = "Sync failed: no relay reachable".into();
                }
                if !report.problems.is_empty() {
                    status += &format!(" · {}", report.problems.join("; "));
                }
                status
            }
            Err(e) => format!("Sync failed: {e:#}"),
        };
        let status = if self.db.erased_at().unwrap_or(0) > erased_before {
            "Your Feedstr data was deleted on another device, so this device's copy was erased too.".to_string()
        } else {
            status
        };
        self.status(status);
        self.send(Msg::Changed);
        outcome
    }
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

struct SyncOutcome {
    /// Everything published reached at least one relay without problems.
    saved: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_addresses_are_checked_and_cleaned_up() {
        assert_eq!(parse_relay(" wss://Relay.Example/ ").as_deref(), Some("wss://relay.example"));
        assert_eq!(parse_relay("ws://localhost:7777").as_deref(), Some("ws://localhost:7777"));
        assert_eq!(parse_relay("wss://relay.example/path").as_deref(), Some("wss://relay.example/path"));
        for bad in ["relay.example", "https://relay.example", "wss://", "wss://exa mple.com", ""] {
            assert_eq!(parse_relay(bad), None, "{bad}");
        }
        // The same relay typed differently is one relay.
        assert_eq!(parse_relay("wss://nos.lol/"), parse_relay("wss://NOS.lol"));
        // A list kept by an older version is cleaned up, and shared from now on.
        let db = Db::open_memory().unwrap();
        db.set_meta("relays", "wss://nos.lol\nwss://nos.lol/\nwss://nostr.mom").unwrap();
        assert_eq!(relays(&db), ["wss://nos.lol", "wss://nostr.mom"]);
        assert!(custom_relays(&db));
        assert_eq!(db.meta("relays").unwrap(), None);
        assert_eq!(db.subscriptions_doc().unwrap().relays.listed(), ["wss://nos.lol", "wss://nostr.mom"]);
    }

    #[test]
    fn untouched_relays_follow_the_defaults() {
        let db = Db::open_memory().unwrap();
        let defaults: Vec<String> = DEFAULT_RELAYS.iter().map(|r| r.to_string()).collect();
        assert_eq!(relays(&db), defaults);
        assert!(!custom_relays(&db));
        // A list of their own is kept as it is...
        set_relays(&db, &["wss://relay.example".to_string()]).unwrap();
        assert!(custom_relays(&db));
        assert_eq!(relays(&db), ["wss://relay.example"]);
        // ...and going back to the defaults (in any order) follows them again.
        let mut back = defaults.clone();
        back.reverse();
        set_relays(&db, &back).unwrap();
        assert!(!custom_relays(&db));
        assert_eq!(relays(&db), defaults);
    }

    #[test]
    fn relays_stay_after_signing_out_but_give_way_to_the_next_account() {
        let db = Db::open_memory().unwrap();
        set_relays(&db, &["wss://relay.example".to_string()]).unwrap();
        db.wipe_account(true).unwrap();
        assert_eq!(relays(&db), ["wss://relay.example"]);
        assert!(db.subscriptions_doc().unwrap().relays.is_empty());
        // The next account's list (even an older one) is used instead.
        let mut doc = crate::docs::Subscriptions::default();
        doc.relays.set(&["wss://other.example".to_string()], true, 1);
        db.merge_subscriptions(&doc).unwrap();
        assert_eq!(relays(&db), ["wss://other.example"]);
    }

    #[tokio::test]
    async fn closing_cuts_feed_fetching_short() {
        // A feed server that never answers: fetching it would take the full HTTP timeout.
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/feed.xml", server.local_addr().unwrap());
        std::thread::spawn(move || {
            let held: Vec<_> = server.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(60));
            drop(held);
        });
        let (msg_tx, _msg_rx) = mpsc::channel();
        let (closing_tx, closing) = watch::channel(false);
        struct NoFiles;
        impl Platform for NoFiles {
            fn open_file(&self, _: FileKind) -> PlatformFuture<Option<Vec<u8>>> {
                Box::pin(async { Ok(None) })
            }
            fn save_file(&self, _: FileKind, _: String, _: Vec<u8>) -> PlatformFuture<Option<String>> {
                Box::pin(async { Ok(None) })
            }
        }
        let ctx = eframe::egui::Context::default();
        let worker = Worker::new(Db::open_memory().unwrap(), msg_tx, ctx, closing, Arc::new(NoFiles));
        worker.db.set_subscribed(&url, true).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            closing_tx.send(true).unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let started = std::time::Instant::now();
        worker.fetch_feeds(&[url]).await;
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A platform that only notes what the worker asks of it.
    #[derive(Default)]
    struct Recorder {
        kept_running: std::sync::Mutex<Vec<bool>>,
    }

    impl Platform for Recorder {
        fn open_file(&self, _: FileKind) -> PlatformFuture<Option<Vec<u8>>> {
            Box::pin(async { Ok(None) })
        }
        fn save_file(&self, _: FileKind, _: String, _: Vec<u8>) -> PlatformFuture<Option<String>> {
            Box::pin(async { Ok(None) })
        }
        fn keep_running(&self, on: bool) {
            self.kept_running.lock().unwrap().push(on);
        }
    }

    /// A signed-in worker that syncs with a relay of its own, with its
    /// database in a file, so it can be looked at (and reused) after the worker stops.
    struct TestApp {
        relay: nostr_sdk::local_relay::MockRelay,
        keys: Keys,
        path: std::path::PathBuf,
    }

    /// What one run of the worker did.
    struct Ran {
        statuses: Vec<String>,
        kept_running: Vec<bool>,
    }

    impl Ran {
        fn synced(&self) -> bool {
            self.statuses.iter().any(|s| s == "Downloading changes…")
        }
        fn uploaded(&self) -> bool {
            self.statuses.iter().any(|s| s == "Uploading changes…")
        }
    }

    impl TestApp {
        async fn new() -> Self {
            let relay = nostr_sdk::local_relay::MockRelay::run().await.unwrap();
            let path = std::env::temp_dir().join(format!("feedstr-test-{}.db", Keys::generate().public_key().to_hex()));
            let app = TestApp { relay, keys: Keys::generate(), path };
            set_relays(&app.db(), &[app.relay.url().await.to_string()]).unwrap();
            // Signed in with a Sync Key, as if earlier.
            app.db().set_meta("login", "sync_key").unwrap();
            app.db().set_meta("sync_key", &app.keys.secret_key().to_bech32().unwrap()).unwrap();
            app
        }

        fn db(&self) -> Db {
            Db::open(&self.path).unwrap()
        }

        /// Starts the worker (as if signed in already), lets `send` send it
        /// commands, and runs it until it has handled them all.
        async fn run(&self, send: impl FnOnce(&Remote)) -> Ran {
            let (msg_tx, msgs) = mpsc::channel();
            let (_closing_tx, closing) = watch::channel(false);
            let platform = Arc::new(Recorder::default());
            let ctx = eframe::egui::Context::default();
            let mut worker = Worker::new(self.db(), msg_tx, ctx, closing, platform.clone());
            let signer = Signer::Keys(self.keys.clone());
            worker.nostr = Some(NostrTransport::new(signer, relays(&worker.db)).await.unwrap());
            let (tx, rx) = unbounded_channel();
            let remote = Remote { tx, life: worker.life.clone(), store: self.path.clone() };
            send(&remote);
            drop(remote);
            worker.run(rx).await;
            let statuses = msgs
                .try_iter()
                .filter_map(|msg| match msg {
                    Msg::Status(text) => Some(text),
                    _ => None,
                })
                .collect();
            let kept_running = platform.kept_running.lock().unwrap().clone();
            Ran { statuses, kept_running }
        }

        fn synced_minutes_ago(&self, minutes: i64) {
            self.db().set_meta("synced_at", &(now_ms() - minutes * 60 * 1000).to_string()).unwrap();
        }

        /// Makes a change that needs uploading.
        fn change(&self) {
            self.db().set_subscribed(&format!("https://example.com/{}.xml", now_ms()), true).unwrap();
        }
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    const LONG_AWAY: i64 = 20 * 60 * 1000;

    #[tokio::test]
    async fn going_to_the_background_uploads_changes() {
        let app = TestApp::new().await;
        app.change();
        let ran = app.run(|remote| assert!(remote.paused())).await;
        assert!(ran.uploaded() && !ran.synced());
        // The platform kept the app running when it left; the worker lets it go once done.
        assert_eq!(ran.kept_running, [false]);
    }

    #[tokio::test]
    async fn going_to_the_background_after_coming_back_already_uploads_nothing() {
        let app = TestApp::new().await;
        app.change();
        // The worker only gets to the command once the app is back (say, after a long sync).
        let ran = app.run(|remote| {
            remote.paused();
            remote.resumed();
        }).await;
        assert!(!ran.uploaded(), "{:?}", ran.statuses);
        assert_eq!(ran.kept_running, [false]);
    }

    #[tokio::test]
    async fn coming_back_syncs_only_if_away_long_and_the_last_sync_is_old() {
        let app = TestApp::new().await;
        app.synced_minutes_ago(30);
        let ran = app.run(|remote| remote.tx.send(Cmd::Resumed { away_ms: 60 * 1000 }).unwrap()).await;
        assert!(!ran.synced(), "away only a minute");

        let ran = app.run(|remote| remote.tx.send(Cmd::Resumed { away_ms: LONG_AWAY }).unwrap()).await;
        assert!(ran.synced() && ran.uploaded());
        assert_eq!(ran.kept_running, [true, false]);
        // That sync was complete, so it counts as the last one.
        let synced_at: i64 = app.db().meta("synced_at").unwrap().unwrap().parse().unwrap();
        assert!(now_ms() - synced_at < 60 * 1000);

        // Back again, soon after it: no sync.
        let ran = app.run(|remote| remote.tx.send(Cmd::Resumed { away_ms: LONG_AWAY }).unwrap()).await;
        assert!(!ran.synced());
        // Asked for, it syncs anyway.
        let ran = app.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        assert!(ran.synced());
    }

    #[test]
    fn coming_back_reports_how_long_the_app_was_away() {
        let (tx, mut rx) = unbounded_channel();
        let store = std::env::temp_dir().join(format!("feedstr-test-{}.db", Keys::generate().public_key().to_hex()));
        let remote = Remote { tx, life: Arc::default(), store };
        remote.resumed();
        assert!(matches!(rx.try_recv(), Ok(Cmd::Resumed { away_ms: 0 })));
        remote.paused();
        remote.life.hidden_at.store(now_ms() - LONG_AWAY, Ordering::Relaxed);
        remote.resumed();
        assert!(matches!(rx.try_recv(), Ok(Cmd::Paused { .. })));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Resumed { away_ms }) if away_ms >= LONG_AWAY));
        // Once the worker has stopped, it can't stop keeping the app running either.
        drop(rx);
        assert!(!remote.paused());
    }

    #[tokio::test]
    async fn manual_only_syncs_only_when_asked() {
        let app = TestApp::new().await;
        app.db().set_meta("sync_minutes", &MANUAL_SYNC.to_string()).unwrap();
        app.change();
        let ran = app
            .run(|remote| {
                remote.paused();
                remote.tx.send(Cmd::Resumed { away_ms: LONG_AWAY }).unwrap();
                remote.exit();
            })
            .await;
        assert!(!ran.synced() && !ran.uploaded(), "{:?}", ran.statuses);
        let ran = app.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        assert!(ran.synced() && ran.uploaded());
    }

    #[tokio::test]
    async fn starting_without_a_sync_says_why() {
        let app = TestApp::new().await;
        let start = |remote: &Remote| remote.tx.send(Cmd::Login(Login::Saved)).unwrap();
        // Not "Connecting to relays…", left up as if it never finished.
        app.synced_minutes_ago(1);
        let ran = app.run(start).await;
        assert!(!ran.synced());
        assert_eq!(ran.statuses.last().unwrap(), "Synced less than 10 minutes ago: Sync to sync again now");
        app.db().set_meta("sync_minutes", &MANUAL_SYNC.to_string()).unwrap();
        app.synced_minutes_ago(60);
        let ran = app.run(start).await;
        assert!(!ran.synced());
        assert_eq!(ran.statuses.last().unwrap(), "Manual only: Sync to sync now");
    }

    #[tokio::test]
    async fn nothing_is_uploaded_again_when_nothing_changed() {
        let app = TestApp::new().await;
        app.change();
        let ran = app.run(|remote| assert!(remote.paused())).await;
        assert!(ran.uploaded());
        // The app is started again (say, after Android closed it), then leaves without changes:
        // nothing to upload, so nothing needs to keep it running either.
        let ran = app.run(|remote| assert!(!remote.paused())).await;
        assert!(!ran.uploaded(), "uploaded again with nothing changed");
        assert!(ran.kept_running.is_empty());
    }
}
