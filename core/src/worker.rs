//! Background thread doing everything slow: signing in, fetching feeds, syncing.
//!
//! A sync cycle pulls the latest changes from relays, fetches every feed, then
//! publishes what changed. It runs at startup, on demand, and optionally on a
//! schedule. Closing the app publishes any remaining changes.

use std::collections::HashMap;
use std::sync::Arc;
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
use crate::nostr::{DEFAULT_RELAYS, NostrTransport, Signer};
use crate::sync::{self, Options, Transport};

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

/// How often the web version syncs while open, unless told otherwise.
pub const WEB_SYNC_MINUTES: u32 = 10;

/// Choices for how often to sync automatically, in minutes (0 = only at start and exit).
pub const SYNC_SCHEDULES: &[u32] = &[0, 15, 30, 60, 180];

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
/// or a place to save one, and handing Nostr links to other apps.
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
    /// A signer app installed on the device, to sign in with (`None` if there is none).
    fn signer_app(&self) -> Option<Arc<dyn SignerApp>> {
        None
    }
    /// Forgets which signer app and account were used (when signing out),
    /// even if the app has been uninstalled since.
    fn forget_signer_app(&self) {}
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
    /// notice: saves changes to relays.
    Paused,
    /// The app is back from the background: syncs if it was away a while.
    Resumed,
    /// Publishes remaining changes, answers with [`Msg::ExitReady`] and stops.
    Exit,
}

pub enum Msg {
    LoggedIn {
        npub: String,
    },
    LoginFailed(String),
    LoggedOut,
    /// Signing out was refused because changes couldn't be saved to relays.
    LogoutBlocked,
    Status(String),
    FeedAdded(Result<String, String>),
    /// The database changed; reload what's on screen.
    Changed,
    /// An import or export finished: what happened, or `None` if the user cancelled.
    FileDone(Option<Result<String, String>>),
    /// Deleting everything finished (and signed out), or failed and changed nothing here.
    Erased(Result<String, String>),
    ExitReady,
}

pub fn relays(db: &Db) -> Vec<String> {
    let mut relays: Vec<String> = match db.meta("relays").ok().flatten() {
        Some(list) => list.lines().map(String::from).collect(),
        None => DEFAULT_RELAYS.iter().map(|r| r.to_string()).collect(),
    };
    // Older versions could store one twice.
    let mut seen = std::collections::HashSet::new();
    relays.retain(|relay| seen.insert(parse_relay(relay).unwrap_or_else(|| relay.clone())));
    relays
}

/// A relay address as typed, cleaned up (`wss://relay.example`), or `None` if
/// it isn't one. Relays from older versions are stored the same way.
pub fn parse_relay(input: &str) -> Option<String> {
    let url = reqwest::Url::parse(input.trim()).ok()?;
    let valid = matches!(url.scheme(), "ws" | "wss") && url.has_host() && url.query().is_none();
    valid.then(|| url.as_str().trim_end_matches('/').to_string())
}

/// Minutes between automatic sync cycles; 0 means only at start and exit.
pub fn sync_minutes(db: &Db) -> u32 {
    db.meta("sync_minutes").ok().flatten().and_then(|m| m.parse().ok()).unwrap_or(0)
}

/// The worker's side of the app: commands go in, messages come out. Sending
/// `true` on `closing` cuts short what is running (like fetching feeds), so
/// [`Cmd::Exit`] gets to save changes sooner.
pub struct Handle {
    pub tx: UnboundedSender<Cmd>,
    pub rx: mpsc::Receiver<Msg>,
    pub closing: watch::Sender<bool>,
}

/// Tells the worker about the app's life cycle from any thread (say, Android's).
#[derive(Clone)]
pub struct Remote(pub(crate) UnboundedSender<Cmd>);

impl Remote {
    pub fn paused(&self) {
        let _ = self.0.send(Cmd::Paused);
    }

    pub fn resumed(&self) {
        let _ = self.0.send(Cmd::Resumed);
    }

    /// Uploads what's left and stops syncing (say, when another browser tab takes over).
    pub fn exit(&self) {
        let _ = self.0.send(Cmd::Exit);
    }
}

/// Starts the worker: on a thread of its own natively, as a task of the page in the browser.
pub fn spawn(store: &Store, ctx: eframe::egui::Context, platform: Arc<dyn Platform>) -> Result<Handle> {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (closing_tx, closing) = watch::channel(false);
    let worker = Worker::new(db::connect(store)?, msg_tx, ctx, closing, platform);
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(worker.run(cmd_rx));
    });
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(worker.run(cmd_rx));
    Ok(Handle { tx: cmd_tx, rx: msg_rx, closing: closing_tx })
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
        Worker { db, http: feeds::http_client(), nostr: None, reader, msg_tx, ctx, closing, platform }
    }

    fn send(&self, msg: Msg) {
        let _ = self.msg_tx.send(msg);
        self.ctx.request_repaint();
    }

    fn status(&self, text: impl Into<String>) {
        self.send(Msg::Status(text.into()));
    }

    async fn run(mut self, mut rx: UnboundedReceiver<Cmd>) {
        let mut next_cycle = self.next_cycle();
        let mut paused_at: Option<i64> = None;
        // What was last uploaded, for uploading changes soon in the browser.
        let (mut uploaded, mut retry_at) = (self.db.revision().unwrap_or(0), 0);
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Cmd::Login(login) => match self.login_with_retries(login).await {
                            Ok(npub) => {
                                self.send(Msg::LoggedIn { npub });
                                self.cycle(false).await;
                                next_cycle = self.next_cycle();
                            }
                            Err(e) => self.send(Msg::LoginFailed(format!("{e:#}"))),
                        },
                        Cmd::Logout { force } => self.logout(force).await,
                        Cmd::SyncNow => {
                            self.cycle(false).await;
                            next_cycle = self.next_cycle();
                        }
                        Cmd::AddFeed { input, category } => {
                            let result = self.add_feed(&input, &category).await.map_err(|e| format!("{e:#}"));
                            self.send(Msg::FeedAdded(result));
                            self.send(Msg::Changed);
                        }
                        Cmd::RelaysChanged => {
                            *self.reader.lock().await = None;
                            if self.nostr.is_some() && let Err(e) = self.login_with_retries(Login::Saved).await {
                                self.send(Msg::LoginFailed(format!("{e:#}")));
                            }
                        }
                        Cmd::FetchFeed(url) => {
                            self.status("Fetching…");
                            let status = match self.fetch_feed(&url).await {
                                Ok(()) => "Fetched".to_string(),
                                Err(e) => format!("Fetching failed: {e:#}"),
                            };
                            self.status(status);
                            self.send(Msg::Changed);
                        }
                        Cmd::ScheduleChanged => next_cycle = self.next_cycle(),
                        Cmd::Paused => {
                            paused_at = Some(now_ms());
                            if self.nostr.is_some() {
                                self.push(true).await;
                            }
                        }
                        Cmd::Resumed => {
                            let away = paused_at.take().map(|at| now_ms() - at);
                            if away.is_some_and(|away| away > RESUME_SYNC_AFTER.as_millis() as i64) {
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
                            if self.nostr.is_some() {
                                self.status("Saving changes to relays…");
                                self.push(true).await;
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
                _ = upload_tick(), if self.nostr.is_some() => {
                    let revision = self.db.revision().unwrap_or(0);
                    if revision != uploaded && now_ms() >= retry_at {
                        if self.push(true).await {
                            uploaded = revision;
                        } else {
                            retry_at = now_ms() + UPLOAD_RETRY.as_millis() as i64;
                        }
                    }
                }
            }
        }
    }

    /// When the next scheduled sync is due (ms).
    fn next_cycle(&self) -> i64 {
        const NEVER: i64 = 365 * 24 * 3600 * 1000;
        match sync_minutes(&self.db) {
            _ if self.nostr.is_none() => now_ms() + NEVER,
            0 if cfg!(target_arch = "wasm32") => now_ms() + WEB_SYNC_MINUTES as i64 * 60 * 1000,
            0 => now_ms() + NEVER,
            minutes => now_ms() + minutes as i64 * 60 * 1000,
        }
    }

    /// Pulls from relays, fetches every feed, then publishes what changed.
    async fn cycle(&self, throttle_feed_caches: bool) {
        self.sync(Options { publish: false, throttle_feed_caches }).await;
        self.refresh().await;
        self.push(throttle_feed_caches).await;
    }

    /// Publishes until nothing is waiting. Returns whether everything was saved.
    async fn push(&self, throttle_feed_caches: bool) -> bool {
        loop {
            let outcome = self.sync(Options { publish: true, throttle_feed_caches }).await;
            if !outcome.saved || !outcome.more_pending {
                break outcome.saved;
            }
        }
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
        self.db.set_subscribed(&url, true)?;
        if !category.trim().is_empty() {
            self.db.set_category(&url, category)?;
        }
        if fetchable(&url) {
            self.fetch_feed(&url).await?;
        } else {
            self.status("Added. Your other devices fetch this feed; its articles show up here after they sync.");
        }
        Ok(url)
    }

    async fn fetch_feed(&self, url: &str) -> Result<()> {
        if longform::is_nostr(url) {
            // Articles from other devices' caches don't count: each device
            // reads the full history once, then only what's new.
            let fetched_before = self.db.fetch_info(url)?.etag.as_deref() == Some(NOSTR_FETCHED);
            return match self.fetch_longform(url, fetched_before).await {
                Ok(feed) => {
                    self.db.store_fetched(url, &feed.title, &feed.site, &feed.items)?;
                    let done = FetchInfo { etag: Some(NOSTR_FETCHED.into()), last_modified: None };
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
        let reader = {
            let mut reader = self.reader.lock().await;
            if reader.is_none() {
                *reader = Some(Arc::new(longform::Reader::new(relays(&self.db))));
            }
            reader.clone().expect("just made")
        };
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
            self.status(format!("Fetching feeds… {done}/{total}"));
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
        let mut outcome = SyncOutcome { saved: false, more_pending: false };
        let Some(nostr) = &self.nostr else { return outcome };
        self.status(if options.publish { "Uploading changes…" } else { "Downloading changes…" });
        let erased_before = self.db.erased_at().unwrap_or(0);
        let status = match sync::sync(&self.db, nostr, options).await {
            Ok(report) => {
                outcome = SyncOutcome {
                    saved: report.relays_ok > 0 && report.problems.is_empty(),
                    more_pending: report.more_pending,
                };
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
    /// The sync hit its publishing limit; more changes are waiting.
    more_pending: bool,
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
        let db = Db::open_memory().unwrap();
        db.set_meta("relays", "wss://nos.lol\nwss://nos.lol/\nwss://nostr.mom").unwrap();
        assert_eq!(relays(&db), ["wss://nos.lol", "wss://nostr.mom"]);
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
}
