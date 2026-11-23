//! Background thread doing everything slow: signing in, fetching feeds, syncing.
//!
//! A sync cycle pulls the latest changes from relays, fetches every feed, then
//! publishes what changed. It runs at startup, on demand, and optionally on a
//! schedule. Closing the app publishes any remaining changes. With syncing set
//! to Manual Only ([`MANUAL_SYNC`]), only [`Cmd::SyncNow`], signing in and
//! signing out talk to relays.
//!
//! Syncing is optional: without it ([`Login::Local`], see [`local_only`]),
//! there is no account and no signer, and a cycle only fetches the feeds.

use std::collections::{HashMap, VecDeque};
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
use crate::gemini;
use crate::import_export;
use crate::longform;
pub use crate::nostr::DEFAULT_RELAYS;
use crate::nostr::{NostrTransport, Signer};
use crate::nsite;
use crate::proxy::{self, Route};
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
/// sites, so the web version only fetches Nostr feeds and nsites itself
/// (relays, and Blossom servers, which let browser pages read them).
pub(crate) fn fetchable(url: &str) -> bool {
    !cfg!(target_arch = "wasm32") || longform::is_nostr(url) || nsite::is_nsite(url)
}

/// Says which proxy couldn't be reached, when that's why a fetch through one failed.
fn proxy_hint(e: anyhow::Error, route: Route) -> anyhow::Error {
    let refused = e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
            // What reqwest's SOCKS connector says (and all it says) when it
            // can't connect to the proxy itself.
            || cause.to_string().ends_with("failed to create underlying connection")
    });
    match route {
        Route::Direct => e,
        _ if refused => e.context(format!("couldn't reach the {} proxy (is it running?)", route.badge())),
        _ => e.context(format!("fetching through {} failed", route.badge())),
    }
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
    /// Opens a `gemini://` link in the user's Gemini browser.
    fn open_gemini_link(&self, _link: &str) -> Result<()> {
        bail!("Gemini links can't be opened in other apps here: copy the address instead")
    }
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
    /// No sync and no account: a feed reader on this device only (see [`local_only`]).
    #[cfg(not(target_arch = "wasm32"))]
    Local,
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
    /// Stops the sync under way, if there is one (see [`Worker::adding_meanwhile`]).
    CancelSync,
    AddFeed {
        input: String,
        category: String,
        /// How to fetch it (web and Gemini feeds only; `.onion` and `.i2p`
        /// sites go through their network whatever this says).
        route: Route,
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
    /// Trusts the new certificate of the capsule that kept this Gemini feed
    /// from being fetched (see [`gemini::changed_key`]), and fetches the feed.
    TrustCapsule(String),
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
    Paused {
        kept_running: bool,
    },
    /// The app is back from the background, after `away_ms` (0 if it wasn't
    /// known to be away): syncs if that was a while.
    Resumed {
        away_ms: i64,
    },
    /// Publishes remaining changes, answers with [`Msg::ExitReady`] and stops.
    Exit,
}

pub enum Msg {
    /// Signed in, as `npub`, or set up without sync (`None`, see [`local_only`]).
    LoggedIn {
        npub: Option<String>,
    },
    /// The `nostrconnect://` link to show as a QR code while signing in.
    ConnectLink(String),
    LoginFailed(String),
    LoggedOut,
    /// Signing out was refused because changes couldn't be saved to relays.
    LogoutBlocked,
    Status(String),
    /// A sync started (`true`) or ended, so it can be cancelled meanwhile.
    Syncing(bool),
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
/// Shown on starting with "Manual only" (and cleared on leaving it).
pub const MANUAL_STATUS: &str = "Manual only: Sync to sync now";

/// [`MANUAL_STATUS`], without sync (see [`local_only`]).
pub const MANUAL_LOCAL_STATUS: &str = "Manual only: Refresh to fetch feeds now";

pub fn manual_sync(db: &Db) -> bool {
    sync_minutes(db) == MANUAL_SYNC
}

/// Kept as the sign-in (`login` in the database) when the user chose not to sync.
const LOCAL_LOGIN: &str = "local";

/// Whether this device doesn't sync (the user chose not to): no account, no
/// signer and nothing on relays, just a feed reader. A cycle only fetches the
/// feeds. Relays are still used to read Nostr feeds and nsites. Signing in
/// later adds what's here to the account. Never on the web, where browsers
/// can't fetch most feeds themselves.
pub fn local_only(db: &Db) -> bool {
    !cfg!(target_arch = "wasm32") && db.meta("login").ok().flatten().as_deref() == Some(LOCAL_LOGIN)
}

/// Whether this device is signed in to sync (and not [`local_only`]).
pub fn syncs(db: &Db) -> bool {
    db.meta("login").ok().flatten().is_some_and(|login| login != LOCAL_LOGIN)
}

/// Whether anything changed since the relays last took everything (so
/// going to the background right after a sync doesn't sync again).
fn has_changes(db: &Db) -> bool {
    let uploaded = db.meta("uploaded_rev").ok().flatten().and_then(|r| r.parse::<i64>().ok());
    uploaded.is_none_or(|uploaded| db.revision().ok() != Some(uploaded))
}

/// The next command for the worker: those put off during a sync first.
async fn next_command(later: &mut VecDeque<Cmd>, rx: &mut UnboundedReceiver<Cmd>) -> Option<Cmd> {
    match later.pop_front() {
        Some(cmd) => Some(cmd),
        None => rx.recv().await,
    }
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
    /// For [`Remote::finish`], as [`Handle::closing`].
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    closing: watch::Sender<bool>,
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
    /// Counts changes to how this device connects (everything through Tor
    /// turned on or off, Tor's address changed), and wakes a sync under way
    /// (see [`Remote::network_changed`]).
    network: std::sync::atomic::AtomicU64,
    network_changed: tokio::sync::Notify,
    /// Set once the worker has handled [`Cmd::Exit`].
    exited: AtomicBool,
}

impl Remote {
    /// Returns whether the worker has something to finish in the background
    /// (a sync under way, or changes to upload) and got the news: the app should
    /// then be kept running, and the worker tells the platform when it can stop.
    pub fn paused(&self) -> bool {
        self.life.hidden_at.store(now_ms(), Ordering::Relaxed);
        self.life.hidden.store(true, Ordering::Relaxed);
        let work = self.life.busy.load(Ordering::Relaxed)
            || db::connect(&self.store).is_ok_and(|db| syncs(&db) && !manual_sync(&db) && has_changes(&db));
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

    /// Closes the app without its window (Android, when the activity is gone
    /// and the process is about to end): uploads what's left as closing does,
    /// and waits for that for up to `limit`. Returns whether it finished.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn finish(&self, limit: Duration) -> bool {
        let _ = self.closing.send(true);
        if self.tx.send(Cmd::Exit).is_err() {
            // The worker has stopped already.
            return true;
        }
        let until = std::time::Instant::now() + limit;
        while !self.life.exited.load(Ordering::Relaxed) {
            if std::time::Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    /// How this device connects changed (see [`Lifecycle::network`]): a sync
    /// under way stops at once, rather than finish over its old connections
    /// (directly, when everything was just sent through Tor), and relays and
    /// remote signers connect again the new way.
    pub fn network_changed(&self) {
        self.life.network.fetch_add(1, Ordering::Relaxed);
        self.life.network_changed.notify_waiters();
        let _ = self.tx.send(Cmd::RelaysChanged);
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
    let remote =
        Remote { tx: cmd_tx.clone(), life: worker.life.clone(), store: store.clone(), closing: closing_tx.clone() };
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
    /// HTTP clients for feeds fetched through Tor or I2P.
    #[cfg(not(target_arch = "wasm32"))]
    proxied: proxy::Clients,
    nostr: Option<NostrTransport>,
    /// Reads long-form posts; made when first needed.
    /// With the proxy it connects through, so a change to that (turning on
    /// "everything through Tor" during a fetch) makes a new one at once.
    reader: tokio::sync::Mutex<Option<(Arc<longform::Reader>, crate::nostr::RelayProxy)>>,
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
    /// The [`Lifecycle::network`] count the relay connections were made with.
    network_seen: std::cell::Cell<u64>,
    /// Feeds being added, and how many have been (see [`Worker::adding`]):
    /// to tell when one was added while changes were being uploaded.
    adds_running: std::cell::Cell<u32>,
    adds_done: std::cell::Cell<u64>,
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
            #[cfg(not(target_arch = "wasm32"))]
            proxied: proxy::Clients::default(),
            nostr: None,
            reader,
            msg_tx,
            ctx,
            closing,
            platform,
            cancel_connect,
            life: Arc::default(),
            kept_running: std::cell::Cell::new(false),
            network_seen: std::cell::Cell::new(0),
            adds_running: std::cell::Cell::new(0),
            adds_done: std::cell::Cell::new(0),
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
        // Commands that came in during a sync and wait for it (see [`Worker::adding_meanwhile`]).
        let mut later = VecDeque::new();
        loop {
            // Nothing left to do (for now): the app needn't be kept running.
            if self.kept_running.get() && rx.is_empty() && later.is_empty() {
                self.platform.keep_running(false);
                self.set_kept_running(false);
            }
            // A sync can bring a relay list changed on another device.
            if self.nostr.as_ref().is_some_and(|nostr| nostr.relays_asked_for() != relays(&self.db)) {
                self.reconnect().await;
            }
            tokio::select! {
                cmd = next_command(&mut later, &mut rx) => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Cmd::Login(login) => match (matches!(login, Login::Saved), self.login_with_retries(login).await) {
                            (saved, Ok(npub)) => {
                                let local = npub.is_none();
                                self.send(Msg::LoggedIn { npub });
                                // Signing in on purpose always syncs; starting up only when it's
                                // automatic, and not again right after the last time. Choosing
                                // not to sync is like starting up: there's nothing to sync.
                                let on_purpose = !saved && !local;
                                if on_purpose {
                                    self.follows_by_default().await;
                                }
                                if on_purpose || (!manual_sync(&self.db) && self.auto_sync_due()) {
                                    self.adding_meanwhile(&mut rx, &mut later, self.cycle(false)).await;
                                } else if manual_sync(&self.db) {
                                    // Replaces "Connecting to relays…", which would stay up otherwise.
                                    self.status(if local { MANUAL_LOCAL_STATUS } else { MANUAL_STATUS });
                                } else if local && self.db.subscribed_feeds().is_ok_and(|feeds| feeds.is_empty()) {
                                    // Nothing to fetch: nothing to say.
                                    self.status("");
                                } else if local {
                                    self.status("Updated less than 10 minutes ago: Refresh to fetch feeds again now");
                                } else {
                                    self.status("Synced less than 10 minutes ago: Sync to sync again now");
                                }
                                next_cycle = self.next_cycle();
                            }
                            (_, Err(e)) => self.send(Msg::LoginFailed(format!("{e:#}"))),
                        },
                        Cmd::Logout { force } => self.logout(force).await,
                        // Nothing is syncing (a sync takes it in `adding_meanwhile`).
                        Cmd::CancelSync => {}
                        Cmd::SyncNow => {
                            self.adding_meanwhile(&mut rx, &mut later, self.cycle(false)).await;
                            next_cycle = self.next_cycle();
                        }
                        cmd @ (Cmd::AddFeed { .. } | Cmd::LookUpYoutube(_) | Cmd::AddYoutube { .. }) => {
                            self.adding(cmd).await;
                        }
                        Cmd::RelaysChanged => self.reconnect().await,
                        Cmd::TrustCapsule(url) => {
                            self.trust_capsule(&url);
                            self.fetch_now(&url).await;
                        }
                        Cmd::FetchFeed(url) => self.fetch_now(&url).await,
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
                                self.adding_meanwhile(&mut rx, &mut later, self.cycle(false)).await;
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
                            self.life.exited.store(true, Ordering::Relaxed);
                            self.send(Msg::ExitReady);
                            break;
                        }
                    }
                }
                _ = clock::sleep(Duration::from_millis((next_cycle - now_ms()).max(0) as u64)) => {
                    self.adding_meanwhile(&mut rx, &mut later, self.cycle(true)).await;
                    next_cycle = self.next_cycle();
                }
                _ = upload_tick(), if self.nostr.is_some() && !manual_sync(&self.db) => {
                    if self.has_changes() && now_ms() >= retry_at && !self.adding_meanwhile(&mut rx, &mut later, self.push(true)).await.unwrap_or(false) {
                        retry_at = now_ms() + UPLOAD_RETRY.as_millis() as i64;
                    }
                }
            }
        }
    }

    /// Adding a feed ([`Cmd::AddFeed`], [`Cmd::LookUpYoutube`] and
    /// [`Cmd::AddYoutube`]), which may run while a sync does.
    async fn adding(&self, cmd: Cmd) {
        // Adding and fetching feeds go on if the app is left meanwhile.
        self.keep_running();
        self.adds_running.set(self.adds_running.get() + 1);
        match cmd {
            Cmd::AddFeed { input, category, route } => {
                let result = self.add_feed(&input, &category, route).await.map_err(|e| format!("{e:#}"));
                self.send(Msg::FeedAdded(result));
                self.send(Msg::Changed);
            }
            Cmd::LookUpYoutube(input) => {
                self.status("Looking up the channel…");
                let result = match youtube::channel_url(&input) {
                    Some(url) => self.look_up_youtube(&url).await.map_err(|e| format!("{e:#}")),
                    None => Err("that is not a YouTube channel's address".to_string()),
                };
                self.status(if result.is_ok() { "" } else { "Looking up the channel failed" });
                self.send(Msg::YoutubeChannel(result));
            }
            Cmd::AddYoutube { channel, videos_only, category } => {
                let url = channel.feed_url(videos_only);
                let new = !self.db.subscribed_feeds().is_ok_and(|feeds| feeds.contains(&url));
                let mut result = self.subscribe(&url, &category, channel.name.as_deref(), None).await;
                if videos_only {
                    result = result.context(
                        "couldn't read the channel's regular videos (it may have none): try without \
                         \"Hide livestreams and Shorts\"",
                    );
                }
                // As with other feeds, one that can't be read isn't added (a channel
                // with no regular videos has no `UULF` feed): the dialog says why,
                // and stays open to try again without "Hide livestreams and Shorts".
                if result.is_err() && new {
                    let _ = self.db.set_subscribed(&url, false);
                }
                self.send(Msg::FeedAdded(result.map(|()| url).map_err(|e| format!("{e:#}"))));
                self.send(Msg::Changed);
            }
            _ => {}
        }
        self.adds_running.set(self.adds_running.get() - 1);
        self.adds_done.set(self.adds_done.get() + 1);
    }

    /// Runs `work` (a sync) while adding the feeds asked for meanwhile, so
    /// the Add feed dialog doesn't wait for the sync to finish. Other
    /// commands wait in `later`, in order, as they would have in `rx`.
    ///
    /// [`Cmd::CancelSync`] stops `work` where it is waiting (on a relay or a
    /// feed), giving `None`. That leaves nothing half done: the database is
    /// only written between those waits, and everything synced is merged
    /// field by field, so what was cut short is done again by the next sync
    /// (as when a sync stops because the network changed).
    async fn adding_meanwhile<T>(
        &self,
        rx: &mut UnboundedReceiver<Cmd>,
        later: &mut VecDeque<Cmd>,
        work: impl Future<Output = T>,
    ) -> Option<T> {
        self.send(Msg::Syncing(true));
        // Boxed, so it can be dropped here (a cancelled sync lets go of what it holds).
        let mut work = Box::pin(work);
        let mut adds = futures::stream::FuturesUnordered::new();
        let mut open = true;
        let done = loop {
            tokio::select! {
                done = &mut work => break Some(done),
                Some(()) = adds.next(), if !adds.is_empty() => {}
                cmd = rx.recv(), if open => match cmd {
                    Some(cmd @ (Cmd::AddFeed { .. } | Cmd::LookUpYoutube(_) | Cmd::AddYoutube { .. })) => {
                        adds.push(self.adding(cmd));
                    }
                    Some(Cmd::CancelSync) => break None,
                    Some(cmd) => later.push_back(cmd),
                    None => open = false,
                },
            }
        };
        // Dropped before the feeds being added finish, so a cancelled sync
        // stops now.
        drop(work);
        if done.is_none() && local_only(&self.db) {
            self.status("Refresh cancelled: feeds not fetched yet are fetched next time");
        } else if done.is_none() {
            self.status("Sync cancelled: what was done so far is kept, the rest is done next time");
            self.send(Msg::Changed);
        }
        self.send(Msg::Syncing(false));
        // Feeds still being added finish here (they borrow the worker).
        while adds.next().await.is_some() {}
        done
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
            // Without sync, a cycle still fetches the feeds.
            _ if self.nostr.is_none() && !local_only(&self.db) => now_ms() + NEVER,
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
    /// Without sync ([`local_only`]), only fetches the feeds.
    async fn cycle(&self, throttle_feed_caches: bool) {
        self.keep_running();
        if local_only(&self.db) {
            let feeds = self.db.subscribed_feeds().map(|feeds| feeds.len()).unwrap_or(0);
            self.refresh().await;
            if !*self.closing.borrow() {
                let _ = self.db.set_meta("synced_at", &now_ms().to_string());
                let time = chrono::Local::now().format("%H:%M");
                self.status(if feeds == 0 { String::new() } else { format!("Updated at {time}") });
            }
            return;
        }
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
        let no_adds = || self.adds_running.get() == 0;
        let (adds, no_adds_before) = (self.adds_done.get(), no_adds());
        let saved = self.sync(Options { publish: true, throttle_feed_caches }).await.saved;
        // What the sync merged was published too, so this is the revision after it.
        // Kept in the database, so a restart doesn't upload again with nothing changed.
        // Not if a feed was being added meanwhile, which may have missed the
        // upload: the next one sends it.
        let alone = no_adds_before && no_adds() && self.adds_done.get() == adds;
        if saved
            && alone
            && let Ok(revision) = self.db.revision()
        {
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
    async fn login_with_retries(&mut self, login: Login) -> Result<Option<String>> {
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

    /// Signs in, returning the account's npub, or `None` without sync ([`Login::Local`]).
    async fn login(&mut self, login: Login) -> Result<Option<String>> {
        let was_local = self.db.meta("login")?.as_deref() == Some(LOCAL_LOGIN);
        // Kept only once signing in has worked: until then, this device stays as it was.
        let mut remember: Vec<(&str, String)> = Vec::new();
        let signer = match login {
            #[cfg(not(target_arch = "wasm32"))]
            Login::Local => {
                // What an unfinished sign-in may have kept (a Sync Key typed, say) goes.
                self.db.set_meta("login", LOCAL_LOGIN)?;
                self.db.delete_meta("sync_key")?;
                self.db.delete_meta("bunker_uri")?;
                return self.go_local().await;
            }
            Login::SyncKey(nsec) => {
                let keys = Keys::parse(nsec.trim()).context("that is not a valid Sync Key")?;
                remember.push(("sync_key", keys.secret_key().to_bech32()?));
                remember.push(("login", "sync_key".into()));
                Signer::Keys(keys)
            }
            Login::Bunker(uri) => {
                self.status("Waiting for the remote signer to approve…");
                let signer = Signer::bunker(&uri, &self.bunker_app_key()?, proxy::relay_proxy(&self.db)?)?;
                signer.public_key().await.context("the remote signer did not respond")?;
                remember.push(("bunker_uri", uri.trim().to_string()));
                remember.push(("login", "bunker".into()));
                signer
            }
            Login::QrCode => {
                let via = proxy::relay_proxy(&self.db)?;
                let (link, signer) = Signer::nostr_connect(&self.bunker_app_key()?, &relays(&self.db), via)?;
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
                remember.push(("bunker_uri", uri));
                remember.push(("login", "bunker".into()));
                signer
            }
            #[cfg(target_arch = "wasm32")]
            Login::Extension => {
                self.status("Waiting for the browser extension…");
                remember.push(("login", "extension".into()));
                Signer::extension()?
            }
            Login::SignerApp => {
                self.status("Waiting for the signer app…");
                let signer = Signer::App(self.platform.signer_app().context("no signer app is installed")?);
                signer.public_key().await.context("the signer app didn't give Feedstr your key")?;
                remember.push(("login", "signer_app".into()));
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
                    let uri = self.db.meta("bunker_uri")?.unwrap_or_default();
                    Signer::bunker(&uri, &self.bunker_app_key()?, proxy::relay_proxy(&self.db)?)?
                }
                #[cfg(not(target_arch = "wasm32"))]
                Some(LOCAL_LOGIN) => return self.go_local().await,
                _ => bail!("not signed in"),
            },
        };
        if let Some(old) = self.nostr.take() {
            old.shutdown().await;
        }
        self.status("Connecting to relays…");
        self.network_seen.set(self.life.network.load(Ordering::Relaxed));
        let nostr = NostrTransport::new(signer, relays(&self.db), proxy::relay_proxy(&self.db)?).await?;
        let pubkey = nostr.pubkey().to_hex();
        // Never mix two accounts' data in one database.
        match self.db.meta("pubkey")? {
            Some(old) if old != pubkey => {
                self.db.wipe_account(false)?;
                self.db.forget_joining()?;
            }
            // What this device has from using it without sync joins the account.
            None if was_local => self.db.joining_account()?,
            _ => {}
        }
        for (key, value) in &remember {
            self.db.set_meta(key, value)?;
        }
        self.db.set_meta("pubkey", &pubkey)?;
        // In case the app was stopped while an erase replaced this device's data.
        if let Err(e) = self.db.restore_kept() {
            log::error!("couldn't restore this device's data: {e:#}");
        }
        let npub = nostr.pubkey().to_bech32()?;
        self.nostr = Some(nostr);
        Ok(Some(npub))
    }

    /// Goes on without sync: no connection to relays for syncing.
    #[cfg(not(target_arch = "wasm32"))]
    async fn go_local(&mut self) -> Result<Option<String>> {
        if let Some(old) = self.nostr.take() {
            old.shutdown().await;
        }
        Ok(None)
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
        // Nothing is on relays without sync: only this device has anything to delete.
        if local_only(&self.db) {
            self.db.wipe_everything()?;
            return Ok("Your Feedstr data was deleted from this device.".into());
        }
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

    /// Fetches one feed, saying how it went.
    async fn fetch_now(&self, url: &str) {
        // In the browser, the other devices fetch most feeds.
        if !fetchable(url) {
            return;
        }
        self.keep_running();
        self.status("Fetching…");
        let status = match self.fetch_feed(url).await {
            Ok(()) => "Fetched".to_string(),
            Err(e) => format!("Fetching failed: {e:#}"),
        };
        self.status(status);
        self.send(Msg::Changed);
    }

    /// Forgets the certificate remembered for the capsule that kept this
    /// Gemini feed from being fetched, so its new one is trusted from the
    /// next fetch on.
    fn trust_capsule(&self, feed_url: &str) {
        let changed = gemini::changed_key(feed_url);
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(Some(capsule)) = self.db.meta(&changed) {
            let _ = self.db.delete_meta(&gemini::pin_key(&capsule));
        }
        let _ = self.db.delete_meta(&changed);
    }

    async fn add_feed(&self, input: &str, category: &str, route: Route) -> Result<String> {
        let mut route = Some(route);
        // An address on an npub subdomain may be an nsite on a gateway, or a
        // site of another kind (some hosts give Nostr users subdomains): only
        // taken for an nsite if one was published.
        let mut site = nsite::Site::parse_input(input);
        if let Some(found) = &site
            && !nsite::is_nsite(input.trim())
            && !nsite::exists(&*self.reader().await?, found).await?
        {
            site = None;
        }
        // Through Tor for everything, if so set (`chosen`, below, is what the
        // feed itself is set to, kept for when that's turned off).
        let tor_everything = proxy::tor_everything(&self.db);
        let url = if let Some(site) = site {
            route = None;
            site.feed_url()
        } else if let Some(author) = longform::parse_author(input) {
            route = None;
            longform::author_url(&author)
        } else if gemini::is_gemini(input) {
            let url = gemini::typed_url(input)?.to_string();
            let chosen = proxy::route_for(&url, route.unwrap_or(Route::Direct).key())?;
            let reached = proxy::effective_route(&url, chosen.key(), tor_everything)?;
            anyhow::ensure!(reached != Route::I2p, "Gemini feeds can't be fetched through I2P");
            // Read first, so a page that isn't a feed isn't subscribed to. (The
            // browser can't: there the other devices fetch it, and report it.)
            #[cfg(not(target_arch = "wasm32"))]
            if let Err(e) = self.gemini_client(reached).feed(&url).await {
                let Some(gemini::CertChanged(capsule)) = e.chain().find_map(|cause| cause.downcast_ref()) else {
                    return Err(e);
                };
                // A capsule followed before (maybe in another feed) that has a new
                // certificate: added anyway. Fetching it notes why it failed, and
                // the feed report offers to trust the new certificate.
                let capsule = capsule.clone();
                let _ = self.subscribe(&url, category, None, Some(chosen)).await;
                anyhow::bail!(
                    "{capsule} isn't using the certificate Feedstr saw there before. The feed was added, but \
                     isn't fetched until you trust the new certificate: see Settings → Feed report"
                );
            }
            route = Some(chosen);
            url
        } else if longform::is_nip05(input) {
            route = None;
            // Asked the way the address's own site would be (its domain may be a .onion).
            let (_, domain) = input.trim().split_once('@').context("not a NIP-05 address")?;
            let site = format!("https://{domain}/");
            let reached = proxy::effective_route(&site, "", tor_everything)?;
            longform::author_url(&longform::resolve_nip05(&self.http_for(reached, &site)?, input).await?)
        } else {
            let typed = feeds::typed_url(input)?.to_string();
            let chosen = proxy::route_for(&typed, route.unwrap_or(Route::Direct).key())?;
            let reached = proxy::effective_route(&typed, chosen.key(), tor_everything)?;
            // The feed found keeps the way it was found: a .onion page may
            // link to a feed elsewhere, which mustn't then be fetched directly.
            route = Some(chosen);
            if reached != Route::Direct && !fetchable(&typed) {
                // The browser can't use the proxy, and asking the site
                // directly is just what going through it should avoid.
                typed
            } else {
                match feeds::discover(&self.http_for(reached, &typed)?, input).await {
                    Ok(url) => url,
                    // Most sites don't let browser pages read them; the other
                    // devices fetch the feed (and report it if it isn't one).
                    Err(_) if !fetchable(input) => typed,
                    // Maybe an nsite on a gateway after all, whose relays didn't answer.
                    Err(e) if nsite::Site::parse_input(input).is_some() => {
                        return Err(e.context(
                            "if this is an nsite, it couldn't be found on its owner's relays: try again, or add \
                             it as nsite:npub1…",
                        ));
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        self.subscribe(&url, category, None, route).await?;
        Ok(url)
    }

    /// Looks up a YouTube channel's page, through Tor if everything goes through Tor.
    async fn look_up_youtube(&self, url: &youtube::ChannelUrl) -> Result<youtube::Channel> {
        const YOUTUBE: &str = "https://www.youtube.com/";
        let route = proxy::effective_route(YOUTUBE, "", proxy::tor_everything(&self.db))?;
        youtube::look_up(&self.http_for(route, YOUTUBE)?, url).await
    }

    /// The HTTP client for the feed at `url`, fetched through `route`.
    #[cfg(not(target_arch = "wasm32"))]
    fn http_for(&self, route: Route, url: &str) -> Result<reqwest::Client> {
        let parsed = reqwest::Url::parse(url).ok();
        let site = parsed.as_ref().and_then(|u| u.host_str()).unwrap_or(url);
        self.proxied.get(&self.http, route, &proxy::proxy_address(&self.db, route), site)
    }

    /// The browser can't use proxies: such feeds are fetched by the other devices.
    #[cfg(target_arch = "wasm32")]
    fn http_for(&self, route: Route, _url: &str) -> Result<reqwest::Client> {
        match route {
            Route::Direct => Ok(self.http.clone()),
            _ => bail!("the browser can't fetch feeds through Tor or I2P"),
        }
    }

    /// A client for Gemini feeds fetched through `route` (directly or through Tor).
    #[cfg(not(target_arch = "wasm32"))]
    fn gemini_client(&self, route: Route) -> gemini::Client<'_> {
        let client = gemini::Client::new(&self.db);
        match route {
            Route::Tor => client.through_tor(proxy::proxy_address(&self.db, Route::Tor)),
            _ => client,
        }
    }

    /// Subscribes to a feed and fetches it. `title` names it until the first
    /// fetch does (which may only happen on another device).
    /// `route`: how it is to be fetched, if that was chosen (see [`Cmd::AddFeed`]).
    async fn subscribe(&self, url: &str, category: &str, title: Option<&str>, route: Option<Route>) -> Result<()> {
        self.db.set_subscribed(url, true)?;
        // The category first: if it is kept on this device only, so is the route.
        if !category.trim().is_empty() {
            self.db.set_category(url, category)?;
        }
        // Only what the address doesn't decide already is stored, and adding
        // a feed again never takes it off Tor or I2P (the choice starts at
        // "directly"): that's done from the feed's menu.
        let current = self.db.route(url)?;
        if let Some(route) = route.filter(|_| Route::required_by(url).is_none())
            && current != route.key()
            && !(route == Route::Direct && !current.is_empty())
        {
            self.db.set_route(url, route.key())?;
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

    /// Fetches a feed. If how this device connects changes meanwhile
    /// (everything was just sent through Tor, say), what's under way stops at
    /// once, rather than go on over its old connections, and the feed is
    /// fetched again the new way. (Nothing is stored until a fetch is done.)
    async fn fetch_feed(&self, url: &str) -> Result<()> {
        loop {
            let seen = self.life.network.load(Ordering::Relaxed);
            tokio::select! {
                result = self.fetch_feed_once(url) => return result,
                () = self.network_change_since(seen) => {}
            }
        }
    }

    /// Returns once how this device connects has changed since `seen` (see
    /// [`Remote::network_changed`]).
    async fn network_change_since(&self, seen: u64) {
        loop {
            // Made before checking, so a change in between still wakes it.
            let wake = self.life.network_changed.notified();
            if self.life.network.load(Ordering::Relaxed) != seen {
                return;
            }
            wake.await;
        }
    }

    async fn fetch_feed_once(&self, url: &str) -> Result<()> {
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
        let tor_everything = proxy::tor_everything(&self.db);
        if nsite::is_nsite(url) {
            let gateway = nsite::gateway(&self.db);
            // The site's version last stored is kept in the feed's (otherwise unused) ETag.
            let seen = self.db.fetch_info(url)?.etag;
            // Its files directly, or through Tor if everything goes through Tor.
            let route = if tor_everything { Route::Tor } else { Route::Direct };
            let fetched = match (self.reader().await, self.http_for(route, url)) {
                (Ok(reader), Ok(http)) => {
                    nsite::fetch(&reader, &http, route, &self.db, url, &gateway, seen.as_deref()).await
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            };
            return match fetched.map_err(|e| proxy_hint(e, route)) {
                Ok(fetched) => {
                    // Nothing new if the site hasn't changed since last time.
                    if let Some(feed) = fetched.feed {
                        self.db.store_fetched(url, &feed.title, &feed.site, &feed.items)?;
                    }
                    self.db.set_fetch_result(url, &FetchInfo { etag: fetched.version, last_modified: None }, None)
                }
                Err(e) => {
                    self.db.set_fetch_error(url, &format!("{e:#}"))?;
                    Err(e)
                }
            };
        }
        // Through Tor or I2P, if the feed's address or its setting says so (or
        // everything goes through Tor); never directly instead.
        let route = match proxy::effective_route(url, &self.db.route(url)?, tor_everything) {
            Ok(route) => route,
            Err(e) => {
                self.db.set_fetch_error(url, &format!("{e:#}"))?;
                return Err(e);
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        if gemini::is_gemini(url) {
            let changed = gemini::changed_key(url);
            if route == Route::I2p {
                let e = anyhow::anyhow!("Gemini feeds can't be fetched through I2P");
                self.db.set_fetch_error(url, &format!("{e:#}"))?;
                return Err(e);
            }
            return match self.gemini_client(route).fetch(url).await {
                Ok(feed) => {
                    self.db.store_fetched(url, &feed.title, &feed.site, &feed.items)?;
                    self.db.delete_meta(&changed)?;
                    self.db.set_fetch_result(url, &FetchInfo { etag: None, last_modified: None }, None)
                }
                Err(e) => {
                    // Noted, so the feed report can offer to trust the capsule's new certificate.
                    if let Some(gemini::CertChanged(capsule)) = e.chain().find_map(|cause| cause.downcast_ref()) {
                        self.db.set_meta(&changed, capsule)?;
                    }
                    self.db.set_fetch_error(url, &format!("{e:#}"))?;
                    Err(e)
                }
            };
        }
        let cache = self.db.fetch_info(url)?;
        let fetched = match self.http_for(route, url) {
            Ok(http) => feeds::fetch(&http, url, &cache).await,
            Err(e) => Err(e),
        };
        match fetched.map_err(|e| proxy_hint(e, route)) {
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
        let reader = self.reader().await?;
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
    /// Fails if everything goes through Tor and Tor's proxy can't be found:
    /// relays are then not asked at all, rather than directly.
    async fn reader(&self) -> Result<Arc<longform::Reader>> {
        let mut reader = self.reader.lock().await;
        let via = proxy::relay_proxy(&self.db)?;
        if let Some((reader, made_via)) = reader.as_ref()
            && *made_via == via
        {
            return Ok(reader.clone());
        }
        let made = Arc::new(longform::Reader::new(relays(&self.db), via));
        Ok(reader.insert((made, via)).0.clone())
    }

    /// At a fresh sign-in: turns on the follows feed if the account follows
    /// anyone on Nostr, unless one of the user's devices already turned it on
    /// or off. That's only known after reading the relays, so nothing happens
    /// if none of them answers.
    async fn follows_by_default(&self) {
        let downloaded = self.sync(Options { publish: false, throttle_feed_caches: false }).await.saved;
        let untouched = self.db.feed_known(longform::FOLLOWS_URL).is_ok_and(|known| !known);
        let Some(user) = self.nostr.as_ref().map(|n| n.pubkey()) else { return };
        let Ok(reader) = self.reader().await else { return };
        if !downloaded || !untouched || !reader.follows_anyone(user).await {
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
        // How this device connects changed since the relays were connected
        // (the command to connect again waits behind this): not over these.
        let seen = self.network_seen.get();
        let changed = || self.life.network.load(Ordering::Relaxed) != seen;
        if changed() {
            self.status("Connecting to relays the new way…");
            return outcome;
        }
        let synced = tokio::select! {
            result = sync::sync_reporting(&self.db, nostr, options, &progress) => Some(result),
            () = self.network_change_since(seen) => None,
        };
        let Some(synced) = synced else {
            // Everything synced is merged field by field: what was cut short
            // is done again by the next sync.
            self.status("Sync stopped: connecting to relays the new way…");
            return outcome;
        };
        let status = match synced {
            Ok(report) => {
                outcome = SyncOutcome { saved: report.relays_ok > 0 && report.problems.is_empty() };
                // The account's data has been read (an erase in it included).
                if outcome.saved {
                    let _ = self.db.joined_account();
                }
                let time = chrono::Local::now().format("%H:%M");
                let mut status = format!("Synced at {time} · {}/{} relays", report.relays_ok, report.relays_total);
                if report.relays_total == 0 && proxy::relay_proxy(&self.db).is_ok_and(|via| via.is_some()) {
                    status = "Sync failed: none of your relays can be used through Tor (unencrypted ws:// ones aren't)"
                        .into();
                } else if report.relays_ok == 0 {
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
        /// For each feed added: how many statuses had come before, and whether it worked.
        added: Vec<(usize, bool)>,
        /// Each sync starting (`true`) and ending.
        syncing: Vec<bool>,
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
            let (closing_tx, closing) = watch::channel(false);
            let platform = Arc::new(Recorder::default());
            let ctx = eframe::egui::Context::default();
            let mut worker = Worker::new(self.db(), msg_tx, ctx, closing, platform.clone());
            if syncs(&worker.db) {
                let signer = Signer::Keys(self.keys.clone());
                worker.nostr = Some(NostrTransport::new(signer, relays(&worker.db), None).await.unwrap());
            }
            let (tx, rx) = unbounded_channel();
            // As in the app, where the Handle keeps one: closing it would read as closing.
            let closing = closing_tx.clone();
            let remote = Remote { tx, life: worker.life.clone(), store: self.path.clone(), closing };
            send(&remote);
            drop(remote);
            worker.run(rx).await;
            drop(closing_tx);
            let (mut statuses, mut added, mut syncing) = (Vec::new(), Vec::new(), Vec::new());
            for msg in msgs.try_iter() {
                match msg {
                    Msg::Status(text) => statuses.push(text),
                    Msg::FeedAdded(result) => added.push((statuses.len(), result.is_ok())),
                    Msg::Syncing(on) => syncing.push(on),
                    _ => {}
                }
            }
            let kept_running = platform.kept_running.lock().unwrap().clone();
            Ran { statuses, kept_running, added, syncing }
        }

        /// The same, set up without sync instead (as if chosen earlier).
        async fn local() -> Self {
            let app = Self::new().await;
            app.db().set_meta("login", LOCAL_LOGIN).unwrap();
            app.db().delete_meta("sync_key").unwrap();
            app
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
        let ran = app
            .run(|remote| {
                remote.paused();
                remote.resumed();
            })
            .await;
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
        let remote = Remote { tx, life: Arc::default(), store, closing: watch::channel(false).0 };
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
    async fn finishing_uploads_what_is_left_and_waits_for_it() {
        let app = TestApp::new().await;
        app.change();
        let mut finishing = None;
        let ran = app
            .run(|remote| {
                let remote = remote.clone();
                finishing = Some(std::thread::spawn(move || remote.finish(Duration::from_secs(30))));
            })
            .await;
        assert!(finishing.unwrap().join().unwrap(), "finish gave up");
        assert!(ran.uploaded(), "{:?}", ran.statuses);
        assert!(!has_changes(&app.db()));
        // With the worker gone, there's nothing to wait for.
        let (tx, rx) = unbounded_channel();
        drop(rx);
        let remote = Remote { tx, life: Arc::default(), store: app.path.clone(), closing: watch::channel(false).0 };
        let started = std::time::Instant::now();
        assert!(remote.finish(Duration::from_secs(30)));
        assert!(started.elapsed() < Duration::from_secs(1));
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
        assert_eq!(ran.statuses.last().unwrap(), MANUAL_STATUS);
    }

    /// A feed server on this device answering every request with `feed`,
    /// after `delay`. Returns the feed's address.
    fn feed_server(delay: Duration, feed: &'static str) -> String {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/feed.xml", server.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in server.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = stream.read(&mut [0; 4096]);
                    std::thread::sleep(delay);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        feed.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(feed.as_bytes());
                });
            }
        });
        url
    }

    const FEED: &str = "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>T</title>\
        <item><title>A</title><link>https://example.com/a</link><guid>a</guid></item></channel></rss>";

    #[tokio::test]
    async fn a_feed_is_added_without_waiting_for_a_sync() {
        let app = TestApp::new().await;
        // A feed that takes a while, so the sync's fetching does too.
        app.db().set_subscribed(&feed_server(Duration::from_secs(3), FEED), true).unwrap();
        let quick = feed_server(Duration::ZERO, FEED);
        let ran = app
            .run(|remote| {
                remote.tx.send(Cmd::SyncNow).unwrap();
                let add = Cmd::AddFeed { input: quick.clone(), category: String::new(), route: Route::Direct };
                remote.tx.send(add).unwrap();
            })
            .await;
        let synced = ran.statuses.iter().rposition(|s| s.starts_with("Synced at")).expect("synced");
        assert_eq!(ran.added.len(), 1, "{:?}", ran.statuses);
        let (statuses_before, ok) = ran.added[0];
        assert!(ok);
        assert!(statuses_before <= synced, "added after the sync: {:?}", ran.statuses);
        assert!(app.db().subscribed_feeds().unwrap().contains(&quick));
    }

    #[tokio::test]
    async fn without_sync_a_refresh_only_fetches_feeds() {
        let app = TestApp::local().await;
        let feed = feed_server(Duration::ZERO, FEED);
        app.db().set_subscribed(&feed, true).unwrap();
        let ran = app.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        assert!(!ran.synced() && !ran.uploaded(), "{:?}", ran.statuses);
        assert!(ran.statuses.last().unwrap().starts_with("Updated at"), "{:?}", ran.statuses);
        assert_eq!(app.db().items(&db::View::Feed(feed), false).unwrap().len(), 1);
        // A complete refresh counts as the last one, so starting again soon doesn't fetch again.
        assert!(app.db().meta("synced_at").unwrap().is_some());
        let ran = app.run(|remote| remote.tx.send(Cmd::Login(Login::Saved)).unwrap()).await;
        assert_eq!(ran.statuses.last().unwrap(), "Updated less than 10 minutes ago: Refresh to fetch feeds again now");
        // Changes have nowhere to go: leaving the app doesn't keep it running for them.
        app.change();
        let ran = app.run(|remote| assert!(!remote.paused())).await;
        assert!(ran.kept_running.is_empty() && !ran.uploaded());
    }

    #[tokio::test]
    async fn without_sync_refreshes_follow_the_schedule() {
        let app = TestApp::local().await;
        app.db().set_meta("sync_minutes", "30").unwrap();
        app.synced_minutes_ago(60);
        let ran = app.run(|_| {}).await;
        let worker = Worker::new(
            app.db(),
            mpsc::channel().0,
            Default::default(),
            watch::channel(false).1,
            Arc::new(Recorder::default()),
        );
        // Due in 30 minutes, though nothing is signed in.
        let due = worker.next_cycle() - now_ms();
        assert!((29 * 60 * 1000..=30 * 60 * 1000).contains(&due), "{due}");
        assert!(ran.statuses.is_empty());
        app.db().set_meta("sync_minutes", &MANUAL_SYNC.to_string()).unwrap();
        let ran = app.run(|remote| remote.tx.send(Cmd::Login(Login::Saved)).unwrap()).await;
        assert_eq!(ran.statuses.last().unwrap(), MANUAL_LOCAL_STATUS);
    }

    #[tokio::test]
    async fn choosing_not_to_sync_forgets_an_unfinished_sign_in() {
        let app = TestApp::new().await;
        let ran = app.run(|remote| remote.tx.send(Cmd::Login(Login::Local)).unwrap()).await;
        assert!(!ran.synced());
        assert!(local_only(&app.db()) && !syncs(&app.db()));
        assert!(app.db().meta("sync_key").unwrap().is_none());
    }

    #[tokio::test]
    async fn turning_on_sync_keeps_and_uploads_what_is_here() {
        let app = TestApp::local().await;
        let feed = feed_server(Duration::ZERO, FEED);
        app.db().set_subscribed(&feed, true).unwrap();
        app.synced_minutes_ago(1);
        let nsec = app.keys.secret_key().to_bech32().unwrap();
        let ran = app.run(|remote| remote.tx.send(Cmd::Login(Login::SyncKey(nsec))).unwrap()).await;
        assert!(ran.synced() && ran.uploaded(), "{:?}", ran.statuses);
        assert!(syncs(&app.db()) && !local_only(&app.db()));
        assert_eq!(app.db().subscribed_feeds().unwrap(), std::slice::from_ref(&feed));
        // Another device signing in to the account gets the feed.
        let mut other = TestApp::new().await;
        set_relays(&other.db(), &relays(&app.db())).unwrap();
        other.keys = app.keys.clone();
        other.db().set_meta("sync_key", &app.keys.secret_key().to_bech32().unwrap()).unwrap();
        other.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        assert!(other.db().subscribed_feeds().unwrap().contains(&feed));
    }

    #[tokio::test]
    async fn turning_on_sync_with_an_erased_account_keeps_what_is_here() {
        // The account's data was deleted from another device.
        let account = TestApp::new().await;
        account.change();
        account.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        let relay = relays(&account.db());
        let ran = account.run(|remote| remote.tx.send(Cmd::EraseAll).unwrap()).await;
        assert!(ran.statuses.iter().any(|s| s.starts_with("Deleting")), "{:?}", ran.statuses);
        // This device, used without sync, signs in to it.
        let app = TestApp::local().await;
        set_relays(&app.db(), &relay).unwrap();
        let feed = feed_server(Duration::ZERO, FEED);
        app.db().set_subscribed(&feed, true).unwrap();
        let nsec = account.keys.secret_key().to_bech32().unwrap();
        let ran = app.run(|remote| remote.tx.send(Cmd::Login(Login::SyncKey(nsec))).unwrap()).await;
        assert!(ran.uploaded(), "{:?}", ran.statuses);
        assert!(app.db().erased_at().unwrap() > 0, "the erase wasn't seen");
        assert_eq!(app.db().subscribed_feeds().unwrap(), std::slice::from_ref(&feed));
        // Another device in the account gets the feed.
        let mut other = TestApp::new().await;
        set_relays(&other.db(), &relay).unwrap();
        other.keys = account.keys.clone();
        other.db().set_meta("sync_key", &account.keys.secret_key().to_bech32().unwrap()).unwrap();
        other.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        assert!(other.db().subscribed_feeds().unwrap().contains(&feed));
    }

    #[tokio::test]
    async fn without_sync_deleting_everything_only_touches_this_device() {
        let app = TestApp::local().await;
        app.change();
        let ran = app.run(|remote| remote.tx.send(Cmd::EraseAll).unwrap()).await;
        assert!(!ran.synced() && !ran.uploaded(), "{:?}", ran.statuses);
        assert!(app.db().subscribed_feeds().unwrap().is_empty());
        assert!(app.db().meta("login").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_failed_sign_in_leaves_the_device_without_sync() {
        let app = TestApp::local().await;
        app.change();
        let nsec = Keys::generate().secret_key().to_bech32().unwrap();
        // Signing in can't work: the Tor proxy can't be found.
        proxy::set_tor_everything(&app.db(), true).unwrap();
        proxy::set_proxy_address(&app.db(), Route::Tor, Some("does-not-exist.invalid:9050")).unwrap();
        app.run(|remote| remote.tx.send(Cmd::Login(Login::SyncKey(nsec))).unwrap()).await;
        assert!(local_only(&app.db()));
        assert!(app.db().meta("sync_key").unwrap().is_none());
        // Starting again later doesn't sign in with that key.
        proxy::set_tor_everything(&app.db(), false).unwrap();
        let ran = app.run(|remote| remote.tx.send(Cmd::Login(Login::Saved)).unwrap()).await;
        assert!(!ran.synced() && !ran.uploaded(), "{:?}", ran.statuses);
        assert!(app.db().meta("pubkey").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_second_erase_reaches_a_device_that_was_closed() {
        let secret = "https://secret.example/feed";
        let same_account = async |app: &TestApp| {
            let mut device = TestApp::new().await;
            set_relays(&device.db(), &relays(&app.db())).unwrap();
            device.keys = app.keys.clone();
            device.db().set_meta("sync_key", &app.keys.secret_key().to_bech32().unwrap()).unwrap();
            device
        };
        // Relays keep one event per second for a document.
        let tick = || tokio::time::sleep(Duration::from_millis(1100));
        let app = TestApp::new().await;
        app.run(|remote| remote.tx.send(Cmd::Login(Login::Saved)).unwrap()).await;
        app.change();
        app.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        tick().await;
        // Everything is deleted from another device, and this one follows...
        same_account(&app).await.run(|remote| remote.tx.send(Cmd::EraseAll).unwrap()).await;
        tick().await;
        app.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        tick().await;
        // ...goes on being used...
        app.db().set_subscribed(secret, true).unwrap();
        app.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        tick().await;
        // ...and is closed when everything is deleted again.
        let other = same_account(&app).await;
        other.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        other.run(|remote| remote.tx.send(Cmd::EraseAll).unwrap()).await;
        tick().await;
        app.synced_minutes_ago(60);
        app.run(|remote| remote.tx.send(Cmd::Login(Login::Saved)).unwrap()).await;
        assert!(app.db().subscribed_feeds().unwrap().is_empty());
        tick().await;
        let fresh = same_account(&app).await;
        fresh.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        assert!(!fresh.db().subscribed_feeds().unwrap().iter().any(|feed| feed == secret));
    }

    #[tokio::test]
    async fn a_sync_can_be_cancelled() {
        let app = TestApp::new().await;
        // A feed that would keep the sync fetching for a long time.
        let slow = feed_server(Duration::from_secs(60), FEED);
        app.db().set_subscribed(&slow, true).unwrap();
        let started = std::time::Instant::now();
        let ran = app
            .run(|remote| {
                remote.tx.send(Cmd::SyncNow).unwrap();
                // Cancelled while the feed is being fetched.
                let tx = remote.tx.clone();
                tokio::spawn(async move {
                    clock::sleep(Duration::from_secs(1)).await;
                    tx.send(Cmd::CancelSync).unwrap();
                });
            })
            .await;
        assert!(started.elapsed() < Duration::from_secs(20), "the sync wasn't stopped");
        assert!(ran.statuses.last().unwrap().starts_with("Sync cancelled"), "{:?}", ran.statuses);
        assert_eq!(ran.syncing, [true, false]);
        // Not a complete sync, so it doesn't count as the last one.
        assert!(app.db().meta("synced_at").unwrap().is_none());
        // The next sync runs as ever.
        app.db().set_subscribed(&slow, false).unwrap();
        let ran = app.run(|remote| remote.tx.send(Cmd::SyncNow).unwrap()).await;
        assert!(ran.statuses.last().unwrap().starts_with("Synced at"), "{:?}", ran.statuses);
        assert!(app.db().meta("synced_at").unwrap().is_some());
    }

    #[tokio::test]
    async fn a_relay_left_out_through_tor_doesnt_reconnect_forever() {
        let app = TestApp::new().await;
        // The test relay is ws://, which isn't used through Tor: none is left.
        app.db().set_meta("tor_everything", "1").unwrap();
        let ran = app
            .run(|remote| {
                remote.tx.send(Cmd::Login(Login::Saved)).unwrap();
                remote.tx.send(Cmd::SyncNow).unwrap();
            })
            .await;
        // Connected once, not again before every command.
        assert_eq!(ran.statuses.iter().filter(|s| *s == "Connecting to relays…").count(), 1, "{:?}", ran.statuses);
        assert_eq!(
            ran.statuses.last().unwrap(),
            "Sync failed: none of your relays can be used through Tor (unencrypted ws:// ones aren't)"
        );
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
