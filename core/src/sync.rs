//! Two-way sync of the local database with a set of relays.
//!
//! Each round:
//! 1. Read our documents from every relay separately, asking each relay only
//!    for versions newer than the one we know it has (plus a full read once a
//!    day, to notice relays that lost data). A relay that can't be read
//!    completely is left alone for the rest of the round.
//! 2. Merge every new version into the local database (see [`crate::docs`]).
//! 3. Publish the merged document to each relay whose copy differs from it
//!    (missing, out of date, or overwritten by a concurrent writer).
//!
//! Rounds repeat until no relay needs an update, which also catches another
//! device publishing between our read and our write. Local data is never
//! replaced by remote data, only merged with it, so an offline or stale relay
//! can at worst delay a change, never undo it. A document we fail to decrypt
//! or decode is never overwritten, because we can't know what it contains.
//!
//! To be quick without hammering relays: documents are only rebuilt and
//! compared when their local revision changed, unchanged documents aren't
//! downloaded again, feed caches are republished at most every few hours,
//! requests and publishes run a few at a time, and relays that rate-limit us
//! are given a growing break.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Result, anyhow};
use futures::future::join_all;
use futures::{StreamExt, stream};

use crate::db::{self, Db, KnownDoc, now_ms};
use crate::docs::{self, FeedItems, Lww, StateShard, Subscriptions};

const MAX_ROUNDS: usize = 3;
/// Most events published per sync; the rest waits for the next one.
const MAX_PUBLISHES: usize = 500;
/// Documents asked for per request (one filter each, so result limits can't cut answers short).
const FILTERS_PER_REQ: usize = 10;
/// Requests waiting for an answer at once, per relay.
const REQS_IN_FLIGHT: usize = 4;
/// Events being decrypted at once (with a remote signer, each one is a round trip).
const DECRYPTS_IN_FLIGHT: usize = 8;
/// Events being encrypted and signed at once (with a remote signer, each one is a round trip).
const PREPARES_IN_FLIGHT: usize = 8;
/// Events sent at once to a relay we know nothing about yet. The number
/// doubles while the relay accepts everything, up to the maximum, and drops to
/// one (remembered) once it asks us to slow down.
const FIRST_SEND_WINDOW: usize = 4;
const MAX_SEND_WINDOW: usize = 16;
/// A feed's cache is republished at most this often (it only matters to clients that can't fetch feeds).
const ITEMS_PUBLISH_INTERVAL_SECS: u64 = 3 * 3600;
const FULL_FETCH_INTERVAL_MS: i64 = 24 * 3600 * 1000;
const MIN_BACKOFF_MS: i64 = 60 * 1000;
const MAX_BACKOFF_MS: i64 = 3600 * 1000;

/// One of our events as received from a relay, still encrypted.
pub struct RawEvent {
    pub id: String,
    pub d: String,
    pub created_at: u64,
    pub payload: String,
}

/// Network side of the sync: reading and writing our NIP-78 events.
pub trait Transport {
    /// An encrypted, signed event, ready to send.
    type Event;
    fn relays(&self) -> Vec<String>;
    /// One request: our events with the given d-tags (at most [`FILTERS_PER_REQ`])
    /// on `relay`, each only if created at or after its `since`. Must fail unless
    /// the relay confirmed (EOSE) that it sent everything it has.
    async fn fetch(&self, relay: &str, wanted: &[(String, Option<u64>)]) -> Result<Vec<RawEvent>>;
    async fn decrypt(&self, payload: &str) -> Result<String>;
    /// Encrypts and signs an event (once, for every relay). Returns its id too.
    async fn prepare(&self, d: &str, created_at: u64, content: &str) -> Result<(String, Self::Event)>;
    /// Signs NIP-09 deletion requests for our documents with these d-tags
    /// (every version up to `created_at`) and for these event ids.
    async fn prepare_deletion(&self, ds: &[String], ids: &[String], created_at: u64) -> Result<Vec<Self::Event>>;
    /// Sends a prepared event to one relay. The error is the relay's reason for rejecting it.
    async fn send(&self, relay: &str, event: &Self::Event) -> Result<(), String>;
}

#[derive(Clone, Copy)]
pub struct Options {
    /// Publish local changes; otherwise only download and merge.
    pub publish: bool,
    /// Republish a feed's cache at most every few hours.
    pub throttle_feed_caches: bool,
}

#[derive(Default, Debug)]
pub struct Report {
    pub relays_ok: usize,
    pub relays_total: usize,
    pub published: usize,
    /// Changes are waiting because this sync hit its publishing limit.
    pub more_pending: bool,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Target {
    Subscriptions,
    /// One shard of the feed list (a subscriptions document with only feeds).
    Feeds(u8),
    State(u8),
    Items(String),
}

impl Target {
    fn rev_key(&self) -> String {
        match self {
            // Every change to the subscriptions bumps one revision; the documents
            // that didn't actually change are found equal and left alone.
            Target::Subscriptions | Target::Feeds(_) => db::SUBSCRIPTIONS_REV.to_string(),
            Target::State(shard) => db::state_rev(*shard),
            Target::Items(url) => db::items_rev(url),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Doc {
    Subscriptions(Subscriptions),
    State(StateShard),
    Items(FeedItems),
}

impl Doc {
    fn decode(target: &Target, text: &str, now: i64) -> Result<Doc> {
        Ok(match target {
            Target::Subscriptions | Target::Feeds(_) => Doc::Subscriptions(docs::decode(text)?),
            Target::State(_) => {
                let mut doc: StateShard = docs::decode(text)?;
                doc.prune(now);
                Doc::State(doc)
            }
            Target::Items(_) => Doc::Items(docs::decode(text)?),
        })
    }

    fn local(db: &Db, target: &Target, now: i64) -> Result<Doc> {
        Ok(match target {
            Target::Subscriptions => Doc::Subscriptions(db.subscriptions_doc_without_feeds()?),
            Target::Feeds(shard) => Doc::Subscriptions(db.feeds_doc(*shard)?),
            Target::State(shard) => {
                let mut doc = db.state_doc(*shard)?;
                doc.prune(now);
                shrink_to_fit(&mut doc);
                Doc::State(doc)
            }
            Target::Items(url) => Doc::Items(db.feed_items_doc(url)?),
        })
    }

    fn merge_into(&self, db: &Db, target: &Target) -> Result<()> {
        match (self, target) {
            (Doc::Subscriptions(doc), _) => db.merge_subscriptions(doc).map(|_| ()),
            (Doc::State(doc), _) => db.merge_state(doc),
            (Doc::Items(doc), Target::Items(url)) => db.merge_feed_items(url, doc),
            (Doc::Items(_), _) => unreachable!("items documents always belong to a feed"),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Doc::Subscriptions(doc) => doc.feeds.is_empty() && doc.categories.is_empty(),
            Doc::State(doc) => doc.items.is_empty(),
            Doc::Items(doc) => doc.items.is_empty() && doc.meta_t == 0,
        }
    }

    fn encode(&self) -> String {
        match self {
            Doc::Subscriptions(doc) => docs::encode(doc),
            Doc::State(doc) => docs::encode(doc),
            Doc::Items(doc) => docs::encode(doc),
        }
    }
}

/// Only reached with tens of thousands of recent read items in one shard:
/// drops the oldest non-favorite entries (other devices may then show them unread).
fn shrink_to_fit(doc: &mut StateShard) {
    while docs::encode(doc).len() > docs::MAX_ENCODED {
        let mut by_age: Vec<(i64, String)> = doc
            .items
            .iter()
            .filter(|(_, s)| !s.f.as_ref().is_some_and(|f| f.0))
            .map(|(k, s)| (s.r.as_ref().map_or(0, |r| r.1), k.clone()))
            .collect();
        if by_age.is_empty() {
            return;
        }
        by_age.sort();
        log::warn!("state shard too large, dropping {} old entries", by_age.len().div_ceil(10));
        for (_, key) in by_age.iter().take(by_age.len().div_ceil(10)) {
            doc.items.remove(key);
        }
    }
}

/// The version of a document a relay holds. It has been merged locally.
struct Version {
    created_at: u64,
    content: String,
    synced_rev: Option<i64>,
}

/// Per relay, the versions it holds; `None` if it couldn't be read.
type View = BTreeMap<String, Option<HashMap<String, Version>>>;

/// State shared by the steps of one sync.
struct Run<'a, T> {
    db: &'a Db,
    t: &'a T,
    options: Options,
    now: i64,
    relays: Vec<String>,
    /// Relays due for a full read.
    full: BTreeSet<String>,
    /// Relays that asked us to slow down.
    backed_off: BTreeSet<String>,
    /// Documents we couldn't read, and so must not overwrite.
    blocked: BTreeSet<String>,
    /// Events planned for publishing in this sync.
    planned: usize,
    /// Events published in this sync.
    publishes: usize,
    report: Report,
}

/// An event to publish.
struct Job {
    d: String,
    rev: i64,
    relays: Vec<String>,
    created_at: u64,
    content: String,
}

/// Reads from one relay, a few requests at a time.
async fn fetch_chunked<T: Transport>(t: &T, relay: &str, wanted: &[(String, Option<u64>)]) -> Result<Vec<RawEvent>> {
    let chunks = stream::iter(wanted.chunks(FILTERS_PER_REQ)).map(|chunk| t.fetch(relay, chunk));
    let results: Vec<Result<Vec<RawEvent>>> = chunks.buffer_unordered(REQS_IN_FLIGHT).collect().await;
    Ok(results.into_iter().collect::<Result<Vec<_>>>()?.into_iter().flatten().collect())
}

pub async fn sync<T: Transport>(db: &Db, t: &T, options: Options) -> Result<Report> {
    sync_at(db, t, options, now_ms()).await
}

async fn sync_at<T: Transport>(db: &Db, t: &T, options: Options, now: i64) -> Result<Report> {
    let relays = t.relays();
    let mut full = BTreeSet::new();
    let mut backed_off = BTreeSet::new();
    for relay in &relays {
        let state = db.relay_state(relay)?;
        if now - state.full_fetch_at > FULL_FETCH_INTERVAL_MS {
            full.insert(relay.clone());
        }
        if state.backoff_until > now {
            backed_off.insert(relay.clone());
        }
    }
    let report = Report { relays_total: relays.len(), ..Default::default() };
    let blocked = BTreeSet::new();
    let mut run = Run { db, t, options, now, relays, full, backed_off, blocked, planned: 0, publishes: 0, report };
    let mut all_ds = BTreeSet::new();
    for _ in 0..MAX_ROUNDS {
        run.blocked.clear();

        // The subscriptions document comes first: it holds the salt that names
        // the other documents.
        let subs = BTreeMap::from([(docs::SUBSCRIPTIONS_D.to_string(), Target::Subscriptions)]);
        let mut view = run.read(&subs).await?;
        if run.blocked.contains(docs::SUBSCRIPTIONS_D) && db.salt()?.is_none() {
            // Without the salt we'd invent a new one and orphan everything.
            anyhow::bail!("{}", run.report.problems.join("; "));
        }
        let salt = db.ensure_salt()?;
        // Then the feed list, which says which feeds' caches there are.
        let feeds: BTreeMap<String, Target> =
            (0..docs::FEED_SHARDS).map(|shard| (docs::feeds_d(&salt, shard), Target::Feeds(shard))).collect();
        join(&mut view, run.read(&feeds).await?);
        // Read state before feed caches, so it is published first if there is a lot to publish.
        let mut rest = Vec::new();
        for shard in 0..docs::STATE_SHARDS {
            rest.push((docs::state_d(&salt, shard), Target::State(shard)));
        }
        for url in db.synced_feeds()? {
            rest.push((docs::items_d(&salt, &url), Target::Items(url)));
        }
        join(&mut view, run.read(&rest.iter().cloned().collect()).await?);
        let targets: Vec<(String, Target)> = subs.into_iter().chain(feeds).chain(rest).collect();
        for (relay, versions) in &view {
            if versions.is_some() && run.full.remove(relay) {
                let state = db::RelayState { full_fetch_at: now, ..db.relay_state(relay)? };
                db.set_relay_state(relay, &state)?;
            }
        }
        run.report.relays_ok = view.values().filter(|v| v.is_some()).count();
        all_ds = targets.iter().map(|(d, _)| d.clone()).collect();

        let before = run.publishes;
        let mut jobs = Vec::new();
        for (d, target) in &targets {
            jobs.extend(run.plan(d, target, &view)?);
        }
        run.publish(jobs).await?;
        if run.publishes == before || run.planned >= MAX_PUBLISHES {
            break;
        }
    }
    db.prune_sync_cache(&run.relays, &all_ds)?;
    run.report.published = run.publishes;
    let mut seen = BTreeSet::new();
    run.report.problems.retain(|p| seen.insert(p.clone()));
    Ok(run.report)
}

/// Adds what a second read found to a view. A relay only counts as readable
/// if every read succeeded.
fn join(view: &mut View, mut more: View) {
    for (relay, versions) in view.iter_mut() {
        match (versions.as_mut(), more.remove(relay)) {
            (Some(versions), Some(Some(more))) => versions.extend(more),
            _ => *versions = None,
        }
    }
}

/// Deletes all of the account's Feedstr data from every relay: deletion
/// requests for every document, then a subscriptions document that says
/// "erased", so other devices erase their copies instead of uploading them
/// again, and old copies on relays that missed the deletion are ignored.
/// Returns how many relays took both. The local data is left to the caller.
pub async fn erase<T: Transport>(db: &Db, t: &T) -> Result<usize> {
    let relays = t.relays();
    let mut ds = BTreeSet::from([docs::SUBSCRIPTIONS_D.to_string()]);
    if let Some(Lww(salt, _)) = db.salt()? {
        ds.extend((0..docs::FEED_SHARDS).map(|shard| docs::feeds_d(&salt, shard)));
        ds.extend((0..docs::STATE_SHARDS).map(|shard| docs::state_d(&salt, shard)));
        ds.extend(db.all_feeds()?.iter().map(|feed| docs::items_d(&salt, &feed.url)));
    }
    let (mut ids, mut newest) = (BTreeSet::new(), 0);
    for relay in &relays {
        for (d, known) in db.known_docs(relay)? {
            newest = newest.max(known.created_at);
            ds.insert(d);
            ids.insert(known.event_id);
        }
    }
    // Later than every timestamp in the data, so all of it counts as erased.
    let local = db.subscriptions_doc()?;
    let stamps = (local.feeds.values().map(|v| v.1))
        .chain(local.categories.values().map(|v| v.1))
        .chain(local.salt.map(|s| s.1))
        .chain(local.cache.map(|c| c.1));
    let erased_at = (stamps.max().unwrap_or(0) + 1).max(now_ms());
    let marker = docs::encode(&Subscriptions { erased_at: Some(erased_at), ..Default::default() });

    // Deletion requests cover every version up to their own time, so the
    // marker that replaces the subscriptions document must come after it.
    let deleted_at = ((now_ms() / 1000) as u64).max(newest);
    let (ds, ids): (Vec<String>, Vec<String>) = (ds.into_iter().collect(), ids.into_iter().collect());
    let deletions = t.prepare_deletion(&ds, &ids, deleted_at).await?;
    let (_, marker) = t.prepare(docs::SUBSCRIPTIONS_D, deleted_at + 1, &marker).await?;
    let (deletions, marker) = (&deletions, &marker);
    let erased = join_all(relays.iter().map(|relay| async move {
        for deletion in deletions {
            if let Err(reason) = t.send(relay, deletion).await {
                log::warn!("relay {relay} refused a deletion: {reason}");
                return false;
            }
        }
        t.send(relay, marker).await.inspect_err(|reason| log::warn!("relay {relay}: {reason}")).is_ok()
    }));
    Ok(erased.await.into_iter().filter(|ok| *ok).count())
}

impl<T: Transport> Run<'_, T> {
    /// Reads the given documents from every relay and merges new versions.
    async fn read(&mut self, targets: &BTreeMap<String, Target>) -> Result<View> {
        let mut known = BTreeMap::new();
        let mut requests = Vec::new();
        for relay in &self.relays {
            let docs = self.db.known_docs(relay)?;
            let full = self.full.contains(relay);
            let wanted: Vec<(String, Option<u64>)> =
                targets.keys().map(|d| (d.clone(), docs.get(d).filter(|_| !full).map(|k| k.created_at + 1))).collect();
            known.insert(relay.clone(), docs);
            requests.push(wanted);
        }
        let t = self.t;
        let results =
            join_all(self.relays.iter().zip(&requests).map(|(relay, wanted)| fetch_chunked(t, relay, wanted)));
        let results = results.await;
        let decrypted = self.decrypt_new(&results).await?;

        let mut view = View::new();
        for (relay, result) in self.relays.clone().iter().zip(results) {
            let mut events = match result {
                Ok(events) => events,
                Err(e) => {
                    log::warn!("relay {relay}: {e:#}");
                    view.insert(relay.clone(), None);
                    continue;
                }
            };
            let full = self.full.contains(relay);
            let mut versions = HashMap::new();
            let mut readable = true;
            for (d, doc) in known.remove(relay).unwrap_or_default() {
                if !targets.contains_key(&d) {
                    continue;
                }
                match self.db.decrypted(&doc.event_id)? {
                    _ if full => self.db.forget_known_doc(relay, &d)?,
                    Some(content) => {
                        versions.insert(d, Version { created_at: doc.created_at, content, synced_rev: doc.synced_rev });
                    }
                    None => {
                        // Our copy of what the relay holds is gone: read it all again next time.
                        self.db.forget_known_doc(relay, &d)?;
                        readable = false;
                    }
                }
            }
            events.sort_by_key(|e| e.created_at);
            for event in events {
                let Some(target) = targets.get(&event.d) else { continue };
                if versions.get(&event.d).is_some_and(|v: &Version| v.created_at >= event.created_at) {
                    continue;
                }
                match self.receive(&event, target, &decrypted) {
                    Ok(content) => {
                        let doc =
                            KnownDoc { created_at: event.created_at, event_id: event.id.clone(), synced_rev: None };
                        self.db.set_known_doc(relay, &event.d, &doc)?;
                        versions.insert(event.d, Version { created_at: event.created_at, content, synced_rev: None });
                    }
                    Err(e) => {
                        if self.blocked.insert(event.d.clone()) {
                            self.report.problems.push(format!("can't read {}: {e:#}", event.d));
                        }
                    }
                }
            }
            view.insert(relay.clone(), readable.then_some(versions));
        }
        Ok(view)
    }

    /// Decrypts every event not decrypted before (once per event, ever), all at once.
    async fn decrypt_new(&self, results: &[Result<Vec<RawEvent>>]) -> Result<HashMap<String, Result<String, String>>> {
        let mut todo = BTreeMap::new();
        for event in results.iter().flatten().flatten() {
            if !todo.contains_key(&event.id) && self.db.decrypted(&event.id)?.is_none() {
                todo.insert(event.id.clone(), event.payload.as_str());
            }
        }
        let t = self.t;
        let decrypts = stream::iter(todo).map(|(id, payload)| async move {
            let content = t.decrypt(payload).await.map_err(|e| format!("{e:#}"));
            (id, content)
        });
        Ok(decrypts.buffer_unordered(DECRYPTS_IN_FLIGHT).collect().await)
    }

    /// Merges a version we haven't seen on this relay.
    fn receive(
        &self,
        event: &RawEvent,
        target: &Target,
        decrypted: &HashMap<String, Result<String, String>>,
    ) -> Result<String> {
        let content = match self.db.decrypted(&event.id)? {
            Some(content) => content,
            None => decrypted
                .get(&event.id)
                .cloned()
                .unwrap_or_else(|| Err("not decrypted".into()))
                .map_err(|e| anyhow!(e))?,
        };
        Doc::decode(target, &content, self.now)?.merge_into(self.db, target)?;
        self.db.set_decrypted(&event.id, &content)?;
        Ok(content)
    }

    /// Decides whether the local document needs publishing, and to which readable relays.
    fn plan(&mut self, d: &str, target: &Target, view: &View) -> Result<Option<Job>> {
        if !self.options.publish || self.blocked.contains(d) {
            return Ok(None);
        }
        let rev = self.db.rev(&target.rev_key())?;
        let online: Vec<(&String, Option<&Version>)> =
            view.iter().filter_map(|(relay, versions)| versions.as_ref().map(|v| (relay, v.get(d)))).collect();
        let unsynced: Vec<_> = online.iter().filter(|(_, v)| v.is_none_or(|v| v.synced_rev != Some(rev))).collect();
        if unsynced.is_empty() {
            return Ok(None);
        }
        let newest = online.iter().filter_map(|(_, v)| v.map(|v| v.created_at)).max().unwrap_or(0);
        let now_secs = (self.now / 1000) as u64;
        let recently_published = newest + ITEMS_PUBLISH_INTERVAL_SECS > now_secs;
        let throttled = self.options.throttle_feed_caches && matches!(target, Target::Items(_));
        if throttled && recently_published && unsynced.iter().all(|(_, v)| v.is_some()) {
            return Ok(None);
        }

        let local = Doc::local(self.db, target, self.now)?;
        // The subscriptions document names the feed list's shards, so it is
        // worth publishing as soon as there are any feeds.
        let empty = match target {
            Target::Subscriptions => self.db.subscriptions_doc()?.feeds.is_empty(),
            _ => local.is_empty(),
        };
        let mut stale = Vec::new();
        for (relay, version) in unsynced {
            match version {
                None if empty => {}
                None => stale.push((*relay).clone()),
                Some(v) => match Doc::decode(target, &v.content, self.now) {
                    Ok(remote) if remote == local => self.db.mark_synced(relay, d, rev)?,
                    Ok(_) => stale.push((*relay).clone()),
                    Err(e) => {
                        self.report.problems.push(format!("can't read {d}: {e:#}"));
                        return Ok(None);
                    }
                },
            }
        }
        stale.retain(|relay| !self.backed_off.contains(relay));
        if stale.is_empty() {
            return Ok(None);
        }
        if self.planned >= MAX_PUBLISHES {
            self.report.more_pending = true;
            return Ok(None);
        }
        let content = local.encode();
        if content.len() > docs::MAX_ENCODED {
            self.report.problems.push(format!("{d} is too large to sync"));
            return Ok(None);
        }
        self.planned += 1;
        // Relays keep only the newest version of a replaceable event, so ours
        // must be newer than anything seen, even if our clock is behind.
        let created_at = now_secs.max(newest + 1);
        Ok(Some(Job { d: d.to_string(), rev, relays: stale, created_at, content }))
    }

    /// Publishes the jobs. Each event is encrypted and signed once (a few at a
    /// time), then every relay gets its events in batches of its own size, so
    /// a slow or strict relay doesn't hold up the others. A relay that asks us
    /// to slow down gets nothing more in this sync, and smaller batches from
    /// then on: hitting a limit repeatedly can get us banned for a while.
    async fn publish(&mut self, jobs: Vec<Job>) -> Result<()> {
        let t = self.t;
        let prepares = stream::iter(&jobs).map(|job| t.prepare(&job.d, job.created_at, &job.content));
        let prepared: Vec<_> = prepares.buffered(PREPARES_IN_FLIGHT).collect().await;
        let mut ready = Vec::new();
        for (job, result) in jobs.iter().zip(prepared) {
            match result {
                Ok((id, event)) => {
                    self.db.set_decrypted(&id, &job.content)?;
                    self.publishes += 1;
                    ready.push((job, id, event));
                }
                Err(e) => self.report.problems.push(format!("publishing failed: {e:#}")),
            }
        }
        let ready = &ready;
        let mut lanes = Vec::new();
        for relay in self.relays.iter().filter(|relay| !self.backed_off.contains(*relay)) {
            let window = match self.db.relay_state(relay)?.send_window {
                0 => FIRST_SEND_WINDOW,
                window => window as usize,
            };
            lanes.push(async move {
                let mut todo = ready.iter().enumerate().filter(|(_, (job, ..))| job.relays.contains(relay)).peekable();
                let (mut window, mut stopped, mut results) = (window, false, Vec::new());
                while !stopped && todo.peek().is_some() {
                    let batch: Vec<_> = todo.by_ref().take(window).collect();
                    let sends = batch.iter().map(|(_, (_, _, event))| t.send(relay, event));
                    for ((i, _), result) in batch.iter().zip(join_all(sends).await) {
                        stopped |= result.as_ref().is_err_and(|reason| asks_to_slow_down(reason));
                        results.push((*i, result));
                    }
                    if !stopped && batch.len() == window {
                        window = (window * 2).min(MAX_SEND_WINDOW);
                    }
                }
                (relay.clone(), results, if stopped { 1 } else { window })
            });
        }
        for (relay, results, window) in join_all(lanes).await {
            for (i, result) in results {
                let (job, id, _) = &ready[i];
                self.record(job, id, &relay, result)?;
            }
            let state = db::RelayState { send_window: window as i64, ..self.db.relay_state(&relay)? };
            self.db.set_relay_state(&relay, &state)?;
        }
        Ok(())
    }

    /// Notes what the relay now holds, and backs off from relays that rate-limit us.
    fn record(&mut self, job: &Job, id: &str, relay: &str, result: Result<(), String>) -> Result<()> {
        let mut state = self.db.relay_state(relay)?;
        match result {
            Ok(()) => {
                let doc = KnownDoc { created_at: job.created_at, event_id: id.to_string(), synced_rev: Some(job.rev) };
                self.db.set_known_doc(relay, &job.d, &doc)?;
                if state.backoff_ms > 0 {
                    state.backoff_ms = 0;
                    self.db.set_relay_state(relay, &state)?;
                }
            }
            Err(reason) => {
                // The break grows once per sync, however many events a batch had in flight.
                if asks_to_slow_down(&reason) && !self.backed_off.contains(relay) {
                    state.backoff_ms = (state.backoff_ms * 2).clamp(MIN_BACKOFF_MS, MAX_BACKOFF_MS);
                    state.backoff_until = self.now + state.backoff_ms;
                    self.db.set_relay_state(relay, &state)?;
                    self.backed_off.insert(relay.to_string());
                }
                self.report.problems.push(format!("{relay} rejected an update: {reason}"));
            }
        }
        Ok(())
    }
}

/// Whether a relay's reason for rejecting an event means "stop sending for now".
fn asks_to_slow_down(reason: &str) -> bool {
    reason.contains("rate-limited") || reason.contains("banned")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::db::View;
    use crate::docs::{Item, Lww};

    const PUSH: Options = Options { publish: true, throttle_feed_caches: true };

    /// In-memory relays with replaceable-event semantics. Content is stored as plaintext.
    #[derive(Default)]
    struct FakeNet {
        relays: RefCell<BTreeMap<String, FakeRelay>>,
        stats: RefCell<Stats>,
    }

    #[derive(Default, Clone)]
    struct FakeRelay {
        online: bool,
        rate_limited: bool,
        events: BTreeMap<String, (u64, String)>,
    }

    #[derive(Default, Debug)]
    struct Stats {
        events_sent: usize,
        decrypts: usize,
        publish_attempts: BTreeMap<String, usize>,
    }

    impl FakeNet {
        fn new(n: usize) -> Self {
            let net = FakeNet::default();
            for i in 0..n {
                net.relays.borrow_mut().insert(format!("r{i}"), FakeRelay { online: true, ..Default::default() });
            }
            net
        }
        fn relay(&self, relay: &str) -> std::cell::RefMut<'_, FakeRelay> {
            std::cell::RefMut::map(self.relays.borrow_mut(), |r| r.get_mut(relay).unwrap())
        }
        fn set_online(&self, relay: &str, online: bool) {
            self.relay(relay).online = online;
        }
        fn put(&self, relay: &str, d: &str, created_at: u64, content: &str) {
            self.relay(relay).events.insert(d.into(), (created_at, content.into()));
        }
        fn get(&self, relay: &str, d: &str) -> Option<(u64, String)> {
            self.relays.borrow()[relay].events.get(d).cloned()
        }
        fn len(&self, relay: &str) -> usize {
            self.relays.borrow()[relay].events.len()
        }
        fn reset_stats(&self) {
            *self.stats.borrow_mut() = Stats::default();
        }
    }

    /// The d-tag of a fake deletion request; its content lists the deleted d-tags.
    const DELETE: &str = "\u{0}delete";

    fn fake_id(d: &str, created_at: u64, content: &str) -> String {
        docs::sha256_hex(&format!("{d}{created_at}{content}"))
    }

    impl Transport for FakeNet {
        /// (d, created_at, content)
        type Event = (String, u64, String);

        fn relays(&self) -> Vec<String> {
            self.relays.borrow().keys().cloned().collect()
        }
        async fn fetch(&self, relay: &str, wanted: &[(String, Option<u64>)]) -> Result<Vec<RawEvent>> {
            let relays = self.relays.borrow();
            let relay = &relays[relay];
            anyhow::ensure!(relay.online, "offline");
            let events: Vec<RawEvent> = wanted
                .iter()
                .filter_map(|(d, since)| {
                    let (created_at, content) = relay.events.get(d)?;
                    since.is_none_or(|since| *created_at >= since).then(|| RawEvent {
                        id: fake_id(d, *created_at, content),
                        d: d.clone(),
                        created_at: *created_at,
                        payload: content.clone(),
                    })
                })
                .collect();
            self.stats.borrow_mut().events_sent += events.len();
            Ok(events)
        }
        async fn decrypt(&self, payload: &str) -> Result<String> {
            self.stats.borrow_mut().decrypts += 1;
            anyhow::ensure!(!payload.starts_with("undecryptable"), "decryption failed");
            Ok(payload.to_string())
        }
        async fn prepare_deletion(&self, ds: &[String], _: &[String], created_at: u64) -> Result<Vec<Self::Event>> {
            Ok(vec![(DELETE.to_string(), created_at, ds.join("\n"))])
        }
        async fn prepare(&self, d: &str, created_at: u64, content: &str) -> Result<(String, Self::Event)> {
            Ok((fake_id(d, created_at, content), (d.to_string(), created_at, content.to_string())))
        }
        async fn send(&self, name: &str, (d, created_at, content): &Self::Event) -> Result<(), String> {
            *self.stats.borrow_mut().publish_attempts.entry(name.to_string()).or_default() += 1;
            let mut relay = self.relay(name);
            if !relay.online {
                return Err("connection failed".to_string());
            }
            if relay.rate_limited {
                return Err("rate-limited: slow down".to_string());
            }
            if d == DELETE {
                for d in content.lines() {
                    if relay.events.get(d).is_some_and(|(c, _)| c <= created_at) {
                        relay.events.remove(d);
                    }
                }
                return Ok(());
            }
            if relay.events.get(d).is_none_or(|(c, _)| created_at > c) {
                relay.events.insert(d.clone(), (*created_at, content.clone()));
            }
            Ok(())
        }
    }

    const FEED: &str = "https://example.com/feed.xml";

    fn client_with_items() -> Db {
        let db = Db::open_memory().unwrap();
        db.set_subscribed(FEED, true).unwrap();
        let published = now_ms() / 1000;
        let items: Vec<(String, Item)> = (0..3)
            .map(|i| {
                let item = Item { title: format!("Item {i}"), published, t: published * 1000, ..Default::default() };
                (format!("guid-{i}"), item)
            })
            .collect();
        db.store_fetched(FEED, "Example", "https://example.com", &items).unwrap();
        db
    }

    fn key(i: usize) -> String {
        docs::item_key(FEED, &format!("guid-{i}"))
    }

    fn read(db: &Db, i: usize) -> bool {
        let items = db.items(&View::Feed(FEED.into()), false).unwrap();
        !items.iter().find(|it| it.key == key(i)).expect("item exists").unread
    }

    fn fav(db: &Db, i: usize) -> bool {
        db.items(&View::Feed(FEED.into()), false).unwrap().iter().any(|it| it.key == key(i) && it.fav)
    }

    async fn sync_ok(db: &Db, net: &FakeNet) -> Report {
        let report = sync(db, net, PUSH).await.unwrap();
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        report
    }

    #[tokio::test]
    async fn new_device_gets_everything() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        a.set_read(&[key(0)], true).unwrap();
        a.set_fav(&key(1), true).unwrap();
        sync_ok(&a, &net).await;

        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.subscribed_feeds().unwrap(), [FEED]);
        assert!(read(&b, 0) && !read(&b, 1) && fav(&b, 1));
        assert_eq!(b.feeds().unwrap()[0].title, "Example");

        // Nothing left to publish once everyone agrees.
        assert_eq!(sync_ok(&a, &net).await.published, 0);
        assert_eq!(sync_ok(&b, &net).await.published, 0);
    }

    #[tokio::test]
    async fn concurrent_changes_on_two_devices_both_survive() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;

        // Both change different things in the same shard and feed list.
        a.set_read(&[key(0)], true).unwrap();
        a.set_subscribed("https://a.example/feed", true).unwrap();
        b.set_fav(&key(0), true).unwrap();
        b.set_subscribed(FEED, false).unwrap();

        sync_ok(&a, &net).await;
        sync_ok(&b, &net).await;
        sync_ok(&a, &net).await;
        for db in [&a, &b] {
            assert!(read(db, 0) && fav(db, 0));
            assert_eq!(db.subscribed_feeds().unwrap(), ["https://a.example/feed"]);
        }
    }

    #[tokio::test]
    async fn change_overwritten_by_concurrent_writer_is_restored() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let salt = a.salt().unwrap().unwrap().0;
        let d = docs::state_d(&salt, docs::shard_of(&key(0)));

        // A marks item 0 read and publishes.
        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;
        // Device B, which read the relays before A's write, now overwrites
        // the shard with a newer event that lacks A's change.
        let mut b_doc = StateShard::default();
        b_doc.items.insert(key(1), docs::ItemState { r: None, f: Some(Lww(true, now_ms())) });
        for relay in ["r0", "r1"] {
            let (created_at, _) = net.get(relay, &d).unwrap();
            net.put(relay, &d, created_at + 1, &docs::encode(&b_doc));
        }

        // A's next sync merges B's version and republishes both changes.
        sync_ok(&a, &net).await;
        assert!(read(&a, 0) && fav(&a, 1));
        let fresh = Db::open_memory().unwrap();
        sync_ok(&fresh, &net).await;
        assert!(read(&fresh, 0) && fav(&fresh, 1));
    }

    #[tokio::test]
    async fn stale_relay_coming_back_does_not_undo_changes() {
        let net = FakeNet::new(3);
        let a = client_with_items();
        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;

        // r2 goes offline; item 0 is marked unread again and synced to r0, r1 only.
        net.set_online("r2", false);
        a.set_read(&[key(0)], false).unwrap();
        let report = sync(&a, &net, PUSH).await.unwrap();
        assert_eq!((report.relays_ok, report.relays_total), (2, 3));

        // r2 returns, still holding "read". A new device must see "unread".
        net.set_online("r2", true);
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert!(!read(&b, 0));
        // ...and r2 was repaired, so a device that only reaches r2 agrees.
        net.set_online("r0", false);
        net.set_online("r1", false);
        let c = Db::open_memory().unwrap();
        sync(&c, &net, PUSH).await.unwrap();
        assert!(!read(&c, 0));
    }

    #[tokio::test]
    async fn local_change_beats_remote_clock_running_ahead() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        // A device whose clock is an hour ahead marked item 0 read.
        let mut doc = StateShard::default();
        doc.items.insert(key(0), docs::ItemState { r: Some(Lww(true, now_ms() + 3_600_000)), f: None });
        a.merge_state(&doc).unwrap();
        assert!(read(&a, 0));
        // The user marks it unread now; that must win everywhere.
        a.set_read(&[key(0)], false).unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        b.merge_state(&doc).unwrap();
        sync_ok(&b, &net).await;
        assert!(!read(&a, 0) && !read(&b, 0));
    }

    #[tokio::test]
    async fn merge_order_does_not_matter() {
        let docs: Vec<StateShard> = (0..4)
            .map(|i| {
                let mut doc = StateShard::default();
                // Conflicting values, including identical timestamps.
                doc.items.insert(key(0), docs::ItemState { r: Some(Lww(i % 2 == 0, 100 + i / 2)), f: None });
                doc.items.insert(key(1), docs::ItemState { r: None, f: Some(Lww(i == 1, 50)) });
                doc
            })
            .collect();
        let merged = |order: &[usize]| {
            let db = Db::open_memory().unwrap();
            for &i in order {
                db.merge_state(&docs[i]).unwrap();
                db.merge_state(&docs[i]).unwrap();
            }
            db.state_doc(docs::shard_of(&key(0)))
                .unwrap()
                .items
                .get(&key(0))
                .cloned()
                .zip(db.state_doc(docs::shard_of(&key(1))).unwrap().items.get(&key(1)).cloned())
        };
        let expected = merged(&[0, 1, 2, 3]);
        for order in [[3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
            assert_eq!(merged(&order), expected);
        }
    }

    #[tokio::test]
    async fn unreadable_document_is_never_overwritten() {
        let net = FakeNet::new(1);
        net.put("r0", docs::SUBSCRIPTIONS_D, 5, "not a document");
        let a = client_with_items();
        let report = sync(&a, &net, PUSH).await;
        // No salt is known yet, so syncing stops instead of inventing one.
        assert!(report.is_err());
        assert_eq!(net.get("r0", docs::SUBSCRIPTIONS_D).unwrap().1, "not a document");
    }

    #[tokio::test]
    async fn salt_conflict_between_first_devices_resolves() {
        let net = FakeNet::new(1);
        // Both devices created a salt before ever syncing; B's is older.
        let b = client_with_items();
        b.ensure_salt().unwrap();
        b.set_read(&[key(2)], true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let a = client_with_items();
        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;
        sync_ok(&b, &net).await;
        sync_ok(&a, &net).await;
        assert_eq!(a.salt().unwrap(), b.salt().unwrap());
        let c = Db::open_memory().unwrap();
        sync_ok(&c, &net).await;
        assert!(read(&c, 0) && read(&c, 2) && !read(&c, 1));
    }

    #[tokio::test]
    async fn changed_item_content_propagates() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        // A refetches the feed and item 0 has been edited by its author.
        let published = now_ms() / 1000;
        let edited = Item { title: "Item 0 (updated)".into(), published, t: published * 1000, ..Default::default() };
        a.store_fetched(FEED, "Example", "https://example.com", &[("guid-0".into(), edited)]).unwrap();
        let later = now_ms() + 4 * 3600 * 1000;
        sync_at(&a, &net, PUSH, later).await.unwrap();
        sync_ok(&b, &net).await;
        let titles: Vec<String> = b.items(&View::All, false).unwrap().into_iter().map(|i| i.title).collect();
        assert!(titles.contains(&"Item 0 (updated)".to_string()), "{titles:?}");
        assert_eq!(sync_at(&a, &net, PUSH, later).await.unwrap().published, 0);
    }

    #[tokio::test]
    async fn unchanged_documents_are_not_downloaded_or_decrypted_again() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert!(net.stats.borrow().decrypts > 0);

        net.reset_stats();
        for db in [&a, &b] {
            assert_eq!(sync_ok(db, &net).await.published, 0);
        }
        let stats = net.stats.borrow();
        assert_eq!((stats.events_sent, stats.decrypts), (0, 0), "{stats:?}");
        assert!(stats.publish_attempts.is_empty());
    }

    #[tokio::test]
    async fn only_changed_documents_travel() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;
        net.reset_stats();
        sync_ok(&b, &net).await;
        assert!(read(&b, 0));
        // Only the one changed state shard was sent and decrypted.
        assert_eq!((net.stats.borrow().events_sent, net.stats.borrow().decrypts), (1, 1));
    }

    #[tokio::test]
    async fn relay_that_lost_data_is_repaired_by_the_daily_full_read() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;
        net.relay("r0").events.clear();
        // Within a day we trust what we know about the relay...
        sync_ok(&a, &net).await;
        assert_eq!(net.len("r0"), 0);
        // ...then a full read notices the loss and fills the relay again.
        sync_at(&a, &net, PUSH, now_ms() + 25 * 3600 * 1000).await.unwrap();
        assert_eq!(net.len("r0"), net.len("r1"));
    }

    #[tokio::test]
    async fn feed_cache_publishing_is_throttled() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let published = now_ms() / 1000;
        let item = Item { title: "New".into(), published, t: published * 1000, ..Default::default() };
        a.store_fetched(FEED, "Example", "https://example.com", &[("guid-new".into(), item)]).unwrap();
        a.set_read(&[key(0)], true).unwrap();
        // State changes go out right away; the feed cache waits.
        assert_eq!(sync_ok(&a, &net).await.published, 1);
        assert_eq!(sync_at(&a, &net, PUSH, now_ms() + 4 * 3600 * 1000).await.unwrap().published, 1);
    }

    #[tokio::test]
    async fn publishing_is_spread_over_several_syncs() {
        let net = FakeNet::new(1);
        let a = Db::open_memory().unwrap();
        let feeds = MAX_PUBLISHES + 20;
        for i in 0..feeds {
            let url = format!("https://example.com/{i}.xml");
            a.set_subscribed(&url, true).unwrap();
            let published = now_ms() / 1000;
            let item = Item { title: "Hello".into(), published, t: published * 1000, ..Default::default() };
            a.store_fetched(&url, "Feed", "", &[("1".into(), item)]).unwrap();
        }
        let mut syncs = 0;
        loop {
            let report = sync_ok(&a, &net).await;
            assert!(report.published <= MAX_PUBLISHES);
            syncs += 1;
            if !report.more_pending {
                break;
            }
        }
        assert!(syncs > 1);
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.items(&View::All, false).unwrap().len(), feeds);
    }

    #[tokio::test]
    async fn rate_limited_relay_gets_a_break() {
        let net = FakeNet::new(2);
        net.relay("r0").rate_limited = true;
        let a = client_with_items();
        let report = sync(&a, &net, PUSH).await.unwrap();
        assert!(report.problems.iter().any(|p| p.contains("rate-limited")));
        // r0 got nothing after the first batch, and from now on gets one event at a time.
        let attempts = net.stats.borrow().publish_attempts["r0"];
        assert!(attempts <= FIRST_SEND_WINDOW);
        assert_eq!(a.relay_state("r0").unwrap().send_window, 1);
        // A minute's break, however many events that batch had.
        assert_eq!(a.relay_state("r0").unwrap().backoff_ms, MIN_BACKOFF_MS);
        assert!(net.len("r1") > 1);

        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;
        assert_eq!(net.stats.borrow().publish_attempts["r0"], attempts);
        // After the break, it catches up.
        net.relay("r0").rate_limited = false;
        sync_at(&a, &net, PUSH, now_ms() + 2 * 60 * 1000).await.unwrap();
        assert_eq!(net.len("r0"), net.len("r1"));
    }

    #[tokio::test]
    async fn undecryptable_event_is_never_overwritten() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let d = docs::state_d(&a.salt().unwrap().unwrap().0, docs::shard_of(&key(0)));
        net.put("r0", &d, now_ms() as u64 / 1000 + 10, "undecryptable");
        a.set_read(&[key(0)], true).unwrap();
        let report = sync(&a, &net, PUSH).await.unwrap();
        assert!(!report.problems.is_empty());
        assert_eq!(net.get("r0", &d).unwrap().1, "undecryptable");
    }

    #[tokio::test]
    async fn device_repairs_a_stale_relay_by_itself() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;
        net.set_online("r1", false);
        a.set_read(&[key(0)], false).unwrap();
        sync(&a, &net, PUSH).await.unwrap();
        net.set_online("r1", true);
        sync_ok(&a, &net).await;
        net.set_online("r0", false);
        let b = Db::open_memory().unwrap();
        sync(&b, &net, PUSH).await.unwrap();
        assert!(!read(&b, 0));
    }

    #[tokio::test]
    async fn cache_limits_are_shared_so_devices_agree() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        a.set_cache_limits(docs::CacheLimits { items: 1, days: 30 }).unwrap();
        sync_ok(&a, &net).await;
        let later = now_ms() + 4 * 3600 * 1000;
        sync_at(&a, &net, PUSH, later).await.unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.cache_limits().unwrap().unwrap().0.items, 1);
        // Both build the same cache, so neither keeps republishing it.
        for db in [&a, &b] {
            assert_eq!(sync_at(db, &net, PUSH, later).await.unwrap().published, 0);
        }
        let d = docs::items_d(&a.salt().unwrap().unwrap().0, FEED);
        let cache: docs::FeedItems = docs::decode(&net.get("r0", &d).unwrap().1).unwrap();
        assert_eq!(cache.items.len(), 1);
    }

    #[tokio::test]
    async fn pulling_never_publishes() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        a.set_read(&[key(0)], true).unwrap();
        let pull = Options { publish: false, throttle_feed_caches: true };
        assert_eq!(sync(&a, &net, pull).await.unwrap().published, 0);
        assert!(net.stats.borrow().publish_attempts.values().all(|&n| n == net.stats.borrow().publish_attempts["r0"]));
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert!(!read(&b, 0), "the change stays local until a push");
        sync_ok(&a, &net).await;
        sync_ok(&b, &net).await;
        assert!(read(&b, 0));
    }

    #[tokio::test]
    async fn categories_sync_between_devices() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        a.set_category(FEED, "Tech").unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.categories().unwrap(), ["Tech"]);
        b.rename_category("Tech", "Programming").unwrap();
        sync_ok(&b, &net).await;
        sync_ok(&a, &net).await;
        assert_eq!(a.categories().unwrap(), ["Programming"]);
        assert_eq!(a.items(&View::Category("Programming".into()), false).unwrap().len(), 3);
        assert_eq!(sync_ok(&a, &net).await.published, 0);
    }

    #[tokio::test]
    async fn thousands_of_feeds_fit() {
        let net = FakeNet::new(1);
        let a = Db::open_memory().unwrap();
        let urls: Vec<String> = (0..3000).map(|i| format!("https://site{i}.example.com/feed.xml")).collect();
        for (i, url) in urls.iter().enumerate() {
            a.set_subscribed(url, true).unwrap();
            a.set_category(url, &format!("Group {}", i % 7)).unwrap();
        }
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.subscribed_feeds().unwrap().len(), urls.len());
        assert_eq!(b.categories().unwrap().len(), 7);
    }

    #[tokio::test]
    async fn feeds_in_the_old_single_document_still_arrive() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        // An older version kept the feed list inside the subscriptions document.
        let mut old = relay_subscriptions(&net, "r0");
        old.feeds.insert("https://old.example/feed".into(), Lww(true, now_ms()));
        net.put("r0", docs::SUBSCRIPTIONS_D, now_ms() as u64 / 1000 + 5, &docs::encode(&old));
        sync_ok(&a, &net).await;
        assert!(a.subscribed_feeds().unwrap().contains(&"https://old.example/feed".to_string()));
        // It now lives in the feed list, where this version looks for it.
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert!(b.subscribed_feeds().unwrap().contains(&"https://old.example/feed".to_string()));
    }

    fn relay_subscriptions(net: &FakeNet, relay: &str) -> Subscriptions {
        docs::decode(&net.get(relay, docs::SUBSCRIPTIONS_D).unwrap().1).unwrap()
    }

    #[tokio::test]
    async fn erasing_deletes_everything_and_other_devices_follow() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        a.set_read(&[key(0)], true).unwrap();
        a.set_category(FEED, "News").unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.subscribed_feeds().unwrap(), [FEED]);

        assert_eq!(erase(&a, &net).await.unwrap(), 2);
        // Only the "erased" subscriptions document is left on the relays.
        for relay in ["r0", "r1"] {
            assert_eq!(net.len(relay), 1);
            assert!(relay_subscriptions(&net, relay).erased_at.is_some());
        }
        // B erases its copy instead of uploading it again.
        sync_ok(&b, &net).await;
        assert!(b.subscribed_feeds().unwrap().is_empty());
        assert!(b.items(&View::All, false).unwrap().is_empty());
        assert!(b.state_doc(docs::shard_of(&key(0))).unwrap().items.is_empty());
        assert!(relay_subscriptions(&net, "r0").feeds.is_empty());
        let fresh = Db::open_memory().unwrap();
        sync_ok(&fresh, &net).await;
        assert!(fresh.subscribed_feeds().unwrap().is_empty() && fresh.categories().unwrap().is_empty());
    }

    #[tokio::test]
    async fn relay_that_missed_the_erase_cant_bring_data_back() {
        let net = FakeNet::new(2);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        net.set_online("r1", false);
        assert_eq!(erase(&a, &net).await.unwrap(), 1);
        // r1 comes back with all the old documents.
        net.set_online("r1", true);
        sync_ok(&b, &net).await;
        assert!(b.subscribed_feeds().unwrap().is_empty());
        assert!(b.items(&View::All, false).unwrap().is_empty());
        // ...and B passed the erase on to r1.
        assert!(relay_subscriptions(&net, "r1").erased_at.is_some());
        assert!(relay_subscriptions(&net, "r1").feeds.is_empty());
    }

    #[tokio::test]
    async fn new_feeds_after_an_erase_sync_normally() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        erase(&a, &net).await.unwrap();
        sync_ok(&b, &net).await;
        // Even if B's clock were behind, its new edits come after the erase.
        b.set_subscribed("https://new.example/feed", true).unwrap();
        assert!(b.subscriptions_doc().unwrap().feeds.values().all(|v| v.1 > b.erased_at().unwrap()));
        sync_ok(&b, &net).await;
        let c = Db::open_memory().unwrap();
        sync_ok(&c, &net).await;
        assert_eq!(c.subscribed_feeds().unwrap(), ["https://new.example/feed"]);
        assert_eq!(sync_ok(&b, &net).await.published, 0);
    }

    #[tokio::test]
    async fn word_filters_sync_between_devices() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        a.add_filter("Item 1", "").unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.filters().unwrap().len(), 1);
        assert_eq!(b.items(&View::All, false).unwrap().len(), 2);
        let id = b.filters().unwrap()[0].id.clone();
        b.remove_filter(&id).unwrap();
        sync_ok(&b, &net).await;
        sync_ok(&a, &net).await;
        assert!(a.filters().unwrap().is_empty());
        assert_eq!(a.items(&View::All, false).unwrap().len(), 3);
        assert_eq!(sync_ok(&a, &net).await.published, 0);
    }

    #[tokio::test]
    async fn groups_kept_on_this_device_leave_other_devices() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        let other = "https://other.example/feed";
        a.set_subscribed(other, true).unwrap();
        a.set_category(FEED, "Local Only").unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.subscribed_feeds().unwrap().len(), 2);

        a.set_category_synced("Local Only", false).unwrap();
        sync_ok(&a, &net).await;
        sync_ok(&b, &net).await;
        // B drops the feed; A keeps it, in its category, with its items.
        assert_eq!(b.subscribed_feeds().unwrap(), [other]);
        assert!(b.categories().unwrap().is_empty());
        assert_eq!(a.subscribed_feeds().unwrap().len(), 2);
        assert_eq!(a.categories().unwrap(), ["Local Only"]);
        assert_eq!(a.items(&View::Category("Local Only".into()), false).unwrap().len(), 3);
        assert!(!a.synced_feeds().unwrap().contains(&FEED.to_string()));
        // The two devices agree, so nothing goes back and forth.
        for db in [&a, &b, &a, &b] {
            assert_eq!(sync_ok(db, &net).await.published, 0);
        }
    }

    #[tokio::test]
    async fn read_state_of_local_groups_stays_on_the_device() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        a.set_category(FEED, "Mine").unwrap();
        a.set_read(&[key(0)], true).unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        let synced = |db: &Db, i: usize| db.all_state().unwrap().items.get(&key(i)).cloned().unwrap_or_default();
        assert_eq!(synced(&b, 0).r.map(|r| r.0), Some(true));

        // Once the group is Local Only, reading and favoriting there stays on A...
        a.set_category_synced("Mine", false).unwrap();
        a.set_read(&[key(1)], true).unwrap();
        a.set_fav(&key(2), true).unwrap();
        sync_ok(&a, &net).await;
        sync_ok(&b, &net).await;
        assert!(synced(&b, 1).r.is_none() && synced(&b, 2).f.is_none());
        // ...and what B does with those items doesn't change A.
        std::thread::sleep(std::time::Duration::from_millis(5));
        b.set_read(&[key(0)], false).unwrap();
        sync_ok(&b, &net).await;
        sync_ok(&a, &net).await;
        assert!(read(&a, 0) && read(&a, 1) && fav(&a, 2));
        // The devices pass the same things on, so nothing goes back and forth.
        for db in [&a, &b, &a, &b] {
            assert_eq!(sync_ok(db, &net).await.published, 0);
        }

        // Synced again: the newest change on either device wins.
        a.set_category_synced("Mine", true).unwrap();
        sync_at(&a, &net, PUSH, now_ms() + 4 * 3600 * 1000).await.unwrap();
        sync_ok(&b, &net).await;
        assert!(!read(&a, 0), "B's later change applies on A");
        assert_eq!(synced(&b, 1).r.map(|r| r.0), Some(true));
        assert_eq!(synced(&b, 2).f.map(|f| f.0), Some(true));
        for db in [&a, &b, &a, &b] {
            assert_eq!(sync_ok(db, &net).await.published, 0);
        }
    }

    #[tokio::test]
    async fn changes_elsewhere_to_a_local_feed_dont_override_this_device() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        a.set_category(FEED, "Local Only").unwrap();
        a.set_category_synced("Local Only", false).unwrap();
        sync_ok(&a, &net).await;
        // Later, B subscribes to the same feed, in another category.
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        b.set_subscribed(FEED, true).unwrap();
        b.set_category(FEED, "Shared").unwrap();
        sync_ok(&b, &net).await;
        sync_ok(&a, &net).await;
        assert_eq!(a.categories().unwrap(), ["Local Only"]);
        for db in [&a, &b, &a] {
            assert_eq!(sync_ok(db, &net).await.published, 0);
        }
        // A passes B's choice on, so a new device follows B.
        let c = Db::open_memory().unwrap();
        sync_ok(&c, &net).await;
        assert_eq!(c.categories().unwrap(), ["Shared"]);
    }

    #[tokio::test]
    async fn syncing_a_group_again_brings_it_back() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        a.set_category(FEED, "Local Only").unwrap();
        a.set_category_synced("Local Only", false).unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert!(b.subscribed_feeds().unwrap().is_empty());
        a.set_category_synced("Local Only", true).unwrap();
        sync_at(&a, &net, PUSH, now_ms() + 4 * 3600 * 1000).await.unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.subscribed_feeds().unwrap(), [FEED]);
        assert_eq!(b.categories().unwrap(), ["Local Only"]);
        assert_eq!(b.items(&View::All, false).unwrap().len(), 3);
    }

    #[tokio::test]
    async fn moving_a_feed_into_a_local_group_stops_sharing_it() {
        let net = FakeNet::new(1);
        let a = client_with_items();
        let other = "https://other.example/feed";
        a.set_subscribed(other, true).unwrap();
        a.set_category(other, "Local Only").unwrap();
        a.set_category_synced("Local Only", false).unwrap();
        sync_ok(&a, &net).await;
        let b = Db::open_memory().unwrap();
        sync_ok(&b, &net).await;
        assert_eq!(b.subscribed_feeds().unwrap(), [FEED]);
        a.set_category(FEED, "Local Only").unwrap();
        sync_ok(&a, &net).await;
        sync_ok(&b, &net).await;
        assert!(b.subscribed_feeds().unwrap().is_empty());
        // Renaming keeps the group on this device.
        a.rename_category("Local Only", "Mine").unwrap();
        assert_eq!(a.local_categories().unwrap(), ["Mine"]);
        sync_ok(&a, &net).await;
        sync_ok(&b, &net).await;
        assert!(b.subscribed_feeds().unwrap().is_empty() && b.categories().unwrap().is_empty());
        // Backups keep everything this device has.
        let backup = crate::import_export::export_backup(&a).unwrap();
        let restored = Db::open_memory().unwrap();
        crate::import_export::import_backup(&restored, &backup).unwrap();
        assert_eq!(restored.subscribed_feeds().unwrap().len(), 2);
    }
}
