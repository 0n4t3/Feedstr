//! Local SQLite storage. This is each device's source of truth: remote
//! documents are only ever *merged* into it, never copied over it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::docs::{self, CacheLimits, FeedItems, Item, ItemState, Lww, StateShard, Subscriptions, WordFilter};
use crate::html;
use crate::longform;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS feeds (
    url TEXT PRIMARY KEY,
    subscribed INTEGER NOT NULL DEFAULT 0,
    sub_t INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL DEFAULT '',
    site TEXT NOT NULL DEFAULT '',
    meta_t INTEGER NOT NULL DEFAULT 0,
    etag TEXT,
    last_modified TEXT,
    error TEXT
);
CREATE TABLE IF NOT EXISTS items (
    key TEXT PRIMARY KEY,
    feed_url TEXT NOT NULL,
    title TEXT NOT NULL,
    link TEXT NOT NULL,
    author TEXT NOT NULL,
    published INTEGER NOT NULL,
    content TEXT NOT NULL,
    t INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS items_by_feed ON items (feed_url, published);
CREATE INDEX IF NOT EXISTS items_by_date ON items (published);
CREATE TABLE IF NOT EXISTS state (
    key TEXT PRIMARY KEY,
    read INTEGER NOT NULL DEFAULT 0,
    read_t INTEGER NOT NULL DEFAULT 0,
    fav INTEGER NOT NULL DEFAULT 0,
    fav_t INTEGER NOT NULL DEFAULT 0
);
-- Bumped whenever a synced document's local content may have changed.
CREATE TABLE IF NOT EXISTS revs (target TEXT PRIMARY KEY, rev INTEGER NOT NULL);
-- The newest version of each document known to be on each relay, and the
-- local revision it was last found identical to.
CREATE TABLE IF NOT EXISTS relay_docs (
    relay TEXT NOT NULL,
    d TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    event_id TEXT NOT NULL,
    synced_rev INTEGER,
    PRIMARY KEY (relay, d)
);
CREATE TABLE IF NOT EXISTS decrypted (event_id TEXT PRIMARY KEY, content TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS relays (
    url TEXT PRIMARY KEY,
    full_fetch_at INTEGER NOT NULL DEFAULT 0,
    backoff_until INTEGER NOT NULL DEFAULT 0,
    backoff_ms INTEGER NOT NULL DEFAULT 0
);
-- Categories this device keeps to itself (not synced).
CREATE TABLE IF NOT EXISTS local_categories (name TEXT PRIMARY KEY);
-- For feeds in those categories: what the synced subscriptions document says
-- about them, passed on as is. The feeds table keeps this device's own view.
CREATE TABLE IF NOT EXISTS sync_shadow (
    url TEXT PRIMARY KEY,
    subscribed INTEGER NOT NULL DEFAULT 0,
    sub_t INTEGER NOT NULL DEFAULT 0,
    category TEXT NOT NULL DEFAULT '',
    category_t INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS filters (
    id TEXT PRIMARY KEY,
    phrase TEXT NOT NULL,
    scope TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    t INTEGER NOT NULL
);
";

/// Schema changes after the first release, applied in order (see `PRAGMA user_version`).
const MIGRATIONS: &[&str] = &[
    // Feed categories. Also forget cached ETags once, so every feed is
    // downloaded again and its relative links get resolved.
    "ALTER TABLE feeds ADD COLUMN category TEXT NOT NULL DEFAULT '';
     ALTER TABLE feeds ADD COLUMN category_t INTEGER NOT NULL DEFAULT 0;
     UPDATE feeds SET etag = NULL, last_modified = NULL;",
    // Whether an item was in its feed the last time it was fetched; those are never pruned.
    "ALTER TABLE items ADD COLUMN in_feed INTEGER NOT NULL DEFAULT 0;
     UPDATE feeds SET etag = NULL, last_modified = NULL;",
    // How many events a relay is sent at once (0 = not learned yet).
    "ALTER TABLE relays ADD COLUMN send_window INTEGER NOT NULL DEFAULT 0;",
    // Whether a word filter hides the item (kept up to date by `refresh_hidden`).
    "ALTER TABLE items ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;",
    // When the feed was last subscribed to (ms; synced sub_t at that moment).
    // Until RETENTION_MS after it, all of its items count as unread.
    "ALTER TABLE feeds ADD COLUMN since_t INTEGER NOT NULL DEFAULT 0;",
];

/// SQL expression: is the item `i` (left-joined with state `s`) shown? Word
/// filters never hide favorites.
const VISIBLE: &str = "(i.hidden = 0 OR coalesce(s.fav, 0) = 1)";

/// SQL expression: is the item `i` (left-joined with state `s`) unread?
/// Items without read state are unread when published recently, or when
/// their feed was subscribed to recently.
const UNREAD: &str = "(CASE WHEN s.read_t > 0 THEN s.read = 0 ELSE max(i.published * 1000, f.since_t) > ?1 END)";

/// Items matching the (lowercase) search text in ?3; all if it is empty.
const SEARCH: &str =
    "(?3 = '' OR instr(lower(i.title), ?3) OR instr(lower(i.author), ?3) OR instr(lower(i.content), ?3))";

/// Days items are kept on this device unless the user chooses otherwise (0 = forever).
pub const DEFAULT_KEEP_DAYS: u32 = 0;
/// What "delete items after" starts at when the user turns off keeping everything.
pub const SUGGESTED_KEEP_DAYS: u32 = 90;
const DAY_SECS: i64 = 24 * 3600;

/// Revision keys of the synced documents.
pub const SUBSCRIPTIONS_REV: &str = "subs";

pub fn state_rev(shard: u8) -> String {
    format!("state:{shard}")
}

pub fn items_rev(feed_url: &str) -> String {
    format!("items:{feed_url}")
}

fn bump(conn: &Connection, target: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO revs (target, rev) VALUES (?1, 1) ON CONFLICT (target) DO UPDATE SET rev = rev + 1",
        [target],
    )?;
    Ok(())
}

/// Every feed's synced cache depends on the cache limits.
fn bump_all_feeds(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO revs (target, rev) SELECT 'items:' || url, 1 FROM feeds WHERE true
         ON CONFLICT (target) DO UPDATE SET rev = rev + 1",
        [],
    )?;
    Ok(())
}

/// Favorites come first in a feed's synced cache, so changing one changes that document too.
fn bump_feed_of(conn: &Connection, item_key: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO revs (target, rev) SELECT 'items:' || feed_url, 1 FROM items WHERE key = ?1
         ON CONFLICT (target) DO UPDATE SET rev = rev + 1",
        [item_key],
    )?;
    Ok(())
}

pub use crate::clock::now_ms;

/// Where the app keeps its data. Natively a database file, which the
/// interface and the background worker each open; in the browser one
/// in-memory database (saved as snapshots elsewhere) that they share.
#[cfg(not(target_arch = "wasm32"))]
pub type Store = std::path::PathBuf;
#[cfg(target_arch = "wasm32")]
pub type Store = std::rc::Rc<Db>;

/// A connection to the [`Store`]: its own natively, the shared one in the browser.
#[cfg(not(target_arch = "wasm32"))]
pub type Conn = Db;
#[cfg(target_arch = "wasm32")]
pub type Conn = std::rc::Rc<Db>;

#[cfg(not(target_arch = "wasm32"))]
pub fn connect(store: &Store) -> Result<Conn> {
    Db::open(store)
}

#[cfg(target_arch = "wasm32")]
pub fn connect(store: &Store) -> Result<Conn> {
    Ok(store.clone())
}

#[derive(Clone, Debug, PartialEq)]
pub enum View {
    All,
    Favorites,
    Feed(String),
    Category(String),
    /// One person's posts in the follows feed.
    Person(String),
}

pub struct FeedRow {
    pub url: String,
    pub title: String,
    /// Empty if the feed has none.
    pub category: String,
    pub unread: i64,
    pub error: Option<String>,
    /// Kept on this device only (in a Local Only group).
    pub local: bool,
}

/// A word filter, as shown in settings.
pub struct FilterRow {
    pub id: String,
    pub phrase: String,
    pub scope: String,
}

/// A feed that may need attention.
pub struct ReportRow {
    pub url: String,
    pub title: String,
    pub error: Option<String>,
    /// When its newest item was published (seconds), if it has any.
    pub latest: Option<i64>,
}

/// A feed as stored, for exports.
pub struct FeedInfo {
    pub url: String,
    pub title: String,
    pub site: String,
    pub category: String,
    pub subscribed: bool,
}

#[derive(Clone)]
pub struct ItemRow {
    pub key: String,
    pub feed_title: String,
    pub title: String,
    pub link: String,
    pub author: String,
    pub published: i64,
    pub unread: bool,
    pub fav: bool,
    /// The start of the text, for previews.
    pub snippet: String,
}

/// A document version on a relay, as far as we know.
pub struct KnownDoc {
    pub created_at: u64,
    pub event_id: String,
    pub synced_rev: Option<i64>,
}

#[derive(Default)]
pub struct RelayState {
    pub full_fetch_at: i64,
    pub backoff_until: i64,
    pub backoff_ms: i64,
    /// How many events to send this relay at once; 0 if not learned yet.
    pub send_window: i64,
}

pub struct FetchInfo {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    /// The whole database as bytes, to keep a copy elsewhere (like the browser's storage).
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        Ok(self.conn.serialize("main")?.to_vec())
    }

    /// An in-memory database restored from a [`Self::snapshot`].
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        conn.deserialize_read_exact("main", bytes, bytes.len(), false)?;
        Self::init(conn)
    }

    /// How many rows were changed through this connection so far; a new
    /// snapshot is due when this grows.
    pub fn changes_made(&self) -> u64 {
        self.conn.total_changes()
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(10))?;
        // Deleted data (a Sync Key, say) is overwritten, not just marked free.
        conn.pragma_update(None, "secure_delete", "ON")?;
        conn.execute_batch(SCHEMA)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    // ---- Settings ----

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    /// Forgets the account's data (settings like relays stay), and optionally how to sign in.
    pub fn wipe_account(&self, forget_login: bool) -> Result<()> {
        let login = if forget_login { ", 'login', 'sync_key', 'bunker_uri', 'bunker_app_key'" } else { "" };
        self.wipe(&format!(
            "DELETE FROM meta WHERE key IN ('salt', 'salt_t', 'pubkey', 'cache_limits', 'erased_at'{login});"
        ))
    }

    /// Forgets everything, settings and sign-in included.
    pub fn wipe_everything(&self) -> Result<()> {
        self.wipe("DELETE FROM meta;")
    }

    /// Deletes the account's data plus `more`, all or nothing, then makes sure
    /// none of it can be recovered from the database files.
    fn wipe(&self, more: &str) -> Result<()> {
        self.conn.execute_batch(&format!(
            "BEGIN;
             DELETE FROM feeds; DELETE FROM items; DELETE FROM state; DELETE FROM revs; DELETE FROM filters;
             DELETE FROM local_categories; DELETE FROM sync_shadow;
             DELETE FROM relay_docs; DELETE FROM decrypted; DELETE FROM relays; {more}
             COMMIT;"
        ))?;
        // Old versions of pages live on in the write-ahead log, and pages freed
        // before secure_delete was on in the file itself: rewrite both.
        let checkpoint = || self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        checkpoint()?;
        self.conn.execute_batch("VACUUM")?;
        checkpoint()?;
        Ok(())
    }

    /// When the account's data was last erased (ms), or 0.
    pub fn erased_at(&self) -> Result<i64> {
        Ok(self.meta("erased_at")?.and_then(|t| t.parse().ok()).unwrap_or(0))
    }

    /// The time for a new timestamp in the subscriptions document: always after
    /// the last erase, even if this device's clock is behind.
    fn clock(&self) -> Result<i64> {
        Ok(now_ms().max(self.erased_at()? + 1))
    }

    // ---- User actions: a new timestamp always supersedes the value it replaces,
    //      even if another device's clock is ahead of ours. ----

    pub fn set_read(&self, keys: &[String], read: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for key in keys {
            tx.execute(
                "INSERT INTO state (key, read, read_t) VALUES (?1, ?2, ?3)
                 ON CONFLICT (key) DO UPDATE SET read = ?2, read_t = max(?3, read_t + 1)",
                params![key, read, now_ms()],
            )?;
            bump(&tx, &state_rev(docs::shard_of(key)))?;
        }
        Ok(tx.commit()?)
    }

    pub fn set_fav(&self, key: &str, fav: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO state (key, fav, fav_t) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET fav = ?2, fav_t = max(?3, fav_t + 1)",
            params![key, fav, now_ms()],
        )?;
        bump(&tx, &state_rev(docs::shard_of(key)))?;
        bump_feed_of(&tx, key)?;
        Ok(tx.commit()?)
    }

    /// Puts a feed into a category; an empty name removes it from its category.
    pub fn set_category(&self, url: &str, category: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE feeds SET category = ?2, category_t = max(?3, category_t + 1) WHERE url = ?1",
            params![url, category.trim(), self.clock()?],
        )?;
        bump(&tx, SUBSCRIPTIONS_REV)?;
        tx.commit()?;
        // Moving into or out of a category that isn't synced changes what's shared.
        let (was_local, now_local) = (self.is_shadowed(url)?, self.is_local_category(category.trim())?);
        if now_local && !was_local {
            self.keep_local(url)?;
        } else if was_local && !now_local {
            self.share(url)?;
        }
        // Category filters may apply to it now, or no longer.
        self.refresh_hidden(Some(url))
    }

    // ---- Categories kept on this device ----

    pub fn local_categories(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT name FROM local_categories ORDER BY lower(name)")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn is_local_category(&self, name: &str) -> Result<bool> {
        Ok(!name.is_empty()
            && self
                .conn
                .query_row("SELECT count(*) FROM local_categories WHERE name = ?1", [name], |r| r.get::<_, i64>(0))?
                > 0)
    }

    fn is_shadowed(&self, url: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT count(*) FROM sync_shadow WHERE url = ?1", [url], |r| r.get::<_, i64>(0))? > 0)
    }

    /// Keeps a category's feeds on this device only, or shares them again.
    pub fn set_category_synced(&self, name: &str, synced: bool) -> Result<()> {
        if synced {
            self.conn.execute("DELETE FROM local_categories WHERE name = ?1", [name])?;
        } else {
            self.conn.execute("INSERT OR IGNORE INTO local_categories (name) VALUES (?1)", [name])?;
        }
        let urls: Vec<String> = self
            .conn
            .prepare("SELECT url FROM feeds WHERE category = ?1")?
            .query_map([name], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for url in urls {
            if synced { self.share(&url)? } else { self.keep_local(&url)? }
        }
        Ok(())
    }

    /// Stops sharing a feed: the synced document says "unsubscribed" from now
    /// on (so other devices drop it), while this device keeps its own view.
    fn keep_local(&self, url: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_shadow (url, subscribed, sub_t, category, category_t)
             SELECT url, 0, max(?2, sub_t + 1), '', max(?2, category_t + 1) FROM feeds WHERE url = ?1
             ON CONFLICT (url) DO NOTHING",
            params![url, self.clock()?],
        )?;
        bump(&self.conn, SUBSCRIPTIONS_REV)?;
        Ok(())
    }

    /// Shares a feed again: this device's view wins over what was synced meanwhile.
    fn share(&self, url: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE feeds SET
                sub_t = max(?2, sub_t + 1, coalesce((SELECT sub_t FROM sync_shadow WHERE url = ?1), 0) + 1),
                category_t = max(?2, category_t + 1, coalesce((SELECT category_t FROM sync_shadow WHERE url = ?1), 0) + 1)
             WHERE url = ?1",
            params![url, self.clock()?],
        )?;
        self.conn.execute("DELETE FROM sync_shadow WHERE url = ?1", [url])?;
        bump(&self.conn, SUBSCRIPTIONS_REV)?;
        bump(&self.conn, &items_rev(url))?;
        Ok(())
    }

    /// Renames a category (or, with an empty name, removes it) on every feed in it.
    pub fn rename_category(&self, from: &str, to: &str) -> Result<()> {
        // A renamed category that isn't synced stays that way.
        if !to.trim().is_empty() && self.is_local_category(from)? {
            self.conn.execute("INSERT OR IGNORE INTO local_categories (name) VALUES (?1)", [to.trim()])?;
        }
        self.conn.execute("DELETE FROM local_categories WHERE name = ?1 AND ?1 != ?2", [from, to.trim()])?;
        let urls: Vec<String> = self
            .conn
            .prepare("SELECT url FROM feeds WHERE category = ?1")?
            .query_map([from], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for url in urls {
            self.set_category(&url, to)?;
        }
        // Filters move along with a renamed category.
        if !to.trim().is_empty() {
            self.conn.execute(
                "UPDATE filters SET scope = ?2, t = max(?3, t + 1) WHERE scope = ?1 AND deleted = 0",
                params![format!("category:{from}"), format!("category:{}", to.trim()), self.clock()?],
            )?;
            bump(&self.conn, SUBSCRIPTIONS_REV)?;
            self.refresh_hidden(None)?;
        }
        Ok(())
    }

    // ---- Word filters (synced) ----

    pub fn filters(&self) -> Result<Vec<FilterRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, phrase, scope FROM filters WHERE deleted = 0 ORDER BY scope, lower(phrase)")?;
        let rows = stmt.query_map([], |r| Ok(FilterRow { id: r.get(0)?, phrase: r.get(1)?, scope: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Hides items containing `phrase`, in all feeds (`scope` ""), a category
    /// ("category:<name>") or a feed ("feed:<url>").
    pub fn add_filter(&self, phrase: &str, scope: &str) -> Result<()> {
        let phrase = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
        let id = nostr_sdk::prelude::Keys::generate().public_key().to_hex()[..16].to_string();
        let filter = WordFilter { phrase, scope: scope.to_string(), deleted: false };
        self.store_filter(&id, &Lww(filter, self.clock()?))?;
        self.refresh_hidden(None)
    }

    pub fn remove_filter(&self, id: &str) -> Result<()> {
        self.conn
            .execute("UPDATE filters SET deleted = 1, t = max(?2, t + 1) WHERE id = ?1", params![id, self.clock()?])?;
        bump(&self.conn, SUBSCRIPTIONS_REV)?;
        self.refresh_hidden(None)
    }

    /// Stores a filter if it is newer than ours (last writer wins). Returns whether it changed.
    fn store_filter(&self, id: &str, Lww(filter, t): &Lww<WordFilter>) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO filters (id, phrase, scope, deleted, t) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET phrase = excluded.phrase, scope = excluded.scope,
                deleted = excluded.deleted, t = excluded.t
             WHERE (excluded.t, excluded.phrase, excluded.scope, excluded.deleted) > (t, phrase, scope, deleted)",
            params![id, filter.phrase, filter.scope, filter.deleted, t],
        )? > 0;
        if changed {
            bump(&self.conn, SUBSCRIPTIONS_REV)?;
        }
        Ok(changed)
    }

    /// Recomputes which items word filters hide: all of them, or one feed's.
    pub fn refresh_hidden(&self, feed: Option<&str>) -> Result<()> {
        let rules = self.filter_rules()?;
        let mut changes = Vec::new();
        self.hidden_changes(&rules, "(?1 IS NULL OR i.feed_url = ?1)", feed, &mut changes)?;
        self.set_hidden(&changes)
    }

    /// Recomputes whether word filters hide these items (say, new ones).
    fn refresh_hidden_items(&self, keys: &[String]) -> Result<()> {
        let rules = self.filter_rules()?;
        if rules.is_empty() {
            return Ok(());
        }
        let mut changes = Vec::new();
        for key in keys {
            self.hidden_changes(&rules, "i.key = ?1", Some(key), &mut changes)?;
        }
        self.set_hidden(&changes)
    }

    /// (scope, lowercase phrase) of every word filter.
    fn filter_rules(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .filters()?
            .into_iter()
            .map(|f| (f.scope, f.phrase.to_lowercase()))
            .filter(|(_, phrase)| !phrase.is_empty())
            .collect())
    }

    /// Notes which of the items matching `filter` (with `value` as ?1) should
    /// change whether they are hidden. Items are read one at a time and
    /// without holding the write lock, as there can be very many.
    fn hidden_changes(
        &self,
        rules: &[(String, String)],
        filter: &str,
        value: Option<&str>,
        changes: &mut Vec<(String, bool)>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT i.key, i.feed_url, coalesce(f.category, ''), i.title, i.content, i.hidden
             FROM items i LEFT JOIN feeds f ON f.url = i.feed_url
             WHERE {filter} AND (i.hidden = 1 OR ?2)"
        ))?;
        // With no filters, only items hidden so far need a look.
        let mut rows = stmt.query(params![value, !rules.is_empty()])?;
        while let Some(r) = rows.next()? {
            let (feed_url, category): (String, String) = (r.get(1)?, r.get(2)?);
            let applicable: Vec<&str> = rules
                .iter()
                .filter(|(scope, _)| {
                    scope.is_empty()
                        || scope.strip_prefix("feed:") == Some(feed_url.as_str())
                        || (!category.is_empty() && scope.strip_prefix("category:") == Some(category.as_str()))
                })
                .map(|(_, phrase)| phrase.as_str())
                .collect();
            let hidden = !applicable.is_empty() && {
                let (title, content): (String, String) = (r.get(3)?, r.get(4)?);
                let text = format!("{} {}", html::decode_entities(&title), html::plain_text(&content)).to_lowercase();
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                applicable.iter().any(|phrase| contains_words(&text, phrase))
            };
            if hidden != r.get::<_, bool>(5)? {
                changes.push((r.get(0)?, hidden));
            }
        }
        Ok(())
    }

    fn set_hidden(&self, changes: &[(String, bool)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (key, hidden) in changes {
            tx.execute("UPDATE items SET hidden = ?2 WHERE key = ?1", params![key, hidden])?;
        }
        Ok(tx.commit()?)
    }

    // ---- Feed report ----

    /// Subscribed feeds whose last update failed, that have no items, or whose
    /// newest item is older than `days`. Failing feeds first, then the stalest.
    pub fn feed_report(&self, days: u32) -> Result<Vec<ReportRow>> {
        let cutoff = now_ms() / 1000 - days as i64 * DAY_SECS;
        let mut stmt = self.conn.prepare(
            "SELECT url, title, error, latest FROM (
                SELECT f.url, f.title, f.error, (SELECT max(published) FROM items WHERE feed_url = f.url) AS latest
                FROM feeds f WHERE f.subscribed = 1)
             WHERE error IS NOT NULL OR latest IS NULL OR latest < ?1
             ORDER BY error IS NULL, coalesce(latest, 0), lower(title)",
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            let url: String = r.get(0)?;
            let title: String = r.get(1)?;
            Ok(ReportRow {
                title: if title.is_empty() { url.clone() } else { title },
                url,
                error: r.get(2)?,
                latest: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn categories(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT category FROM feeds WHERE subscribed = 1 AND category != '' ORDER BY lower(category)",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_subscribed(&self, url: &str, subscribed: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO feeds (url, subscribed, sub_t, since_t) VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT (url) DO UPDATE SET subscribed = ?2, sub_t = max(?3, sub_t + 1),
                since_t = CASE WHEN subscribed = 0 THEN max(?3, sub_t + 1) ELSE since_t END",
            params![url, subscribed, self.clock()?],
        )?;
        bump(&tx, SUBSCRIPTIONS_REV)?;
        Ok(tx.commit()?)
    }

    // ---- Settings that are synced ----

    pub fn cache_limits(&self) -> Result<Option<Lww<CacheLimits>>> {
        Ok(self.meta("cache_limits")?.map(|json| serde_json::from_str(&json)).transpose()?)
    }

    pub fn set_cache_limits(&self, limits: CacheLimits) -> Result<()> {
        let t = self.cache_limits()?.map_or(0, |old| old.1 + 1).max(self.clock()?);
        self.store_cache_limits(&self.conn, &Lww(limits, t))
    }

    fn store_cache_limits(&self, conn: &Connection, limits: &Lww<CacheLimits>) -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('cache_limits', ?1)",
            [serde_json::to_string(limits)?],
        )?;
        bump(conn, SUBSCRIPTIONS_REV)?;
        bump_all_feeds(conn)?;
        Ok(())
    }

    /// How many days items are kept on this device (0 = forever). Not synced.
    pub fn keep_days(&self) -> Result<u32> {
        Ok(self.meta("keep_days")?.and_then(|d| d.parse().ok()).unwrap_or(DEFAULT_KEEP_DAYS))
    }

    // ---- Queries for the UI ----

    pub fn feeds(&self) -> Result<Vec<FeedRow>> {
        let sql = format!(
            "SELECT f.url, f.title, f.error,
                (SELECT count(*) FROM items i LEFT JOIN state s ON s.key = i.key
                 WHERE i.feed_url = f.url AND {UNREAD} AND {VISIBLE}),
                f.category, f.url IN (SELECT url FROM sync_shadow)
             FROM feeds f WHERE f.subscribed = 1
             ORDER BY f.category = '', lower(f.category), lower(f.title), f.url"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([unread_cutoff()], |r| {
            let url: String = r.get(0)?;
            let title: String = r.get(1)?;
            Ok(FeedRow {
                title: if title.is_empty() { url.clone() } else { title },
                url,
                error: r.get(2)?,
                unread: r.get(3)?,
                category: r.get(4)?,
                local: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The people with posts in the follows feed, with their unread counts.
    pub fn follows_people(&self) -> Result<Vec<(String, i64)>> {
        let sql = format!(
            "SELECT i.author, sum({UNREAD}) FROM items i JOIN feeds f ON f.url = i.feed_url
                LEFT JOIN state s ON s.key = i.key
             WHERE i.feed_url = ?2 AND {VISIBLE} GROUP BY i.author ORDER BY lower(i.author)"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![unread_cutoff(), longform::FOLLOWS_URL], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// When the feed's newest item was published (seconds).
    pub fn newest_published(&self, url: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT max(published) FROM items WHERE feed_url = ?1", [url], |r| r.get(0))?)
    }

    pub fn favorites_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM items i JOIN state s ON s.key = i.key WHERE s.fav = 1",
            [],
            |r| r.get(0),
        )?)
    }

    /// Every item in a view (no paging).
    #[cfg(test)]
    pub fn items(&self, view: &View, unread_only: bool) -> Result<Vec<ItemRow>> {
        self.items_matching(view, unread_only, "", usize::MAX)
    }

    /// Up to `limit` of the newest items in a view, optionally only those whose
    /// title, author or text contains `search`.
    pub fn items_matching(&self, view: &View, unread_only: bool, search: &str, limit: usize) -> Result<Vec<ItemRow>> {
        let (filter, value) = view_filter(view);
        let unread = if unread_only { UNREAD } else { "1" };
        let sql = format!(
            "SELECT i.key, CASE WHEN i.feed_url = ?4 AND i.author != '' THEN i.author ELSE f.title END,
                i.title, i.link, i.author, i.published, {UNREAD}, coalesce(s.fav, 0), f.url,
                substr(i.content, 1, 1500)
             FROM items i JOIN feeds f ON f.url = i.feed_url LEFT JOIN state s ON s.key = i.key
             WHERE {filter} AND {unread} AND {VISIBLE} AND {SEARCH}
             ORDER BY i.published DESC LIMIT ?5"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let map = |r: &Row| {
            let feed_title: String = r.get(1)?;
            Ok(ItemRow {
                key: r.get(0)?,
                feed_title: if feed_title.is_empty() { r.get(8)? } else { feed_title },
                title: r.get(2)?,
                link: r.get(3)?,
                author: r.get(4)?,
                published: r.get(5)?,
                unread: r.get(6)?,
                fav: r.get(7)?,
                snippet: html::snippet(&r.get::<_, String>(9)?, 300),
            })
        };
        let search = search.trim().to_lowercase();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        // In the follows feed, the writer is the source.
        let rows = stmt.query_map(params![unread_cutoff(), value, search, longform::FOLLOWS_URL, limit], map)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every unread item in a view (or those of them matching `search`), for "mark all as read".
    pub fn unread_keys(&self, view: &View, search: &str) -> Result<Vec<String>> {
        let (filter, value) = view_filter(view);
        let sql = format!(
            "SELECT i.key FROM items i JOIN feeds f ON f.url = i.feed_url LEFT JOIN state s ON s.key = i.key
             WHERE {filter} AND {UNREAD} AND {VISIBLE} AND {SEARCH}"
        );
        let search = search.trim().to_lowercase();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![unread_cutoff(), value, search], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn item_content(&self, key: &str) -> Result<String> {
        Ok(self.conn.query_row("SELECT content FROM items WHERE key = ?1", [key], |r| r.get(0))?)
    }

    // ---- Feed fetching ----

    pub fn subscribed_feeds(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT url FROM feeds WHERE subscribed = 1")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Subscribed feeds whose cache is synced (not in a category kept on this device).
    pub fn synced_feeds(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT url FROM feeds WHERE subscribed = 1 AND url NOT IN (SELECT url FROM sync_shadow)")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn fetch_info(&self, url: &str) -> Result<FetchInfo> {
        Ok(self
            .conn
            .query_row("SELECT etag, last_modified FROM feeds WHERE url = ?1", [url], |r| {
                Ok(FetchInfo { etag: r.get(0)?, last_modified: r.get(1)? })
            })
            .optional()?
            .unwrap_or(FetchInfo { etag: None, last_modified: None }))
    }

    pub fn set_fetch_result(&self, url: &str, info: &FetchInfo, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE feeds SET etag = ?2, last_modified = ?3, error = ?4 WHERE url = ?1",
            params![url, info.etag, info.last_modified, error],
        )?;
        Ok(())
    }

    pub fn set_fetch_error(&self, url: &str, error: &str) -> Result<()> {
        self.conn.execute("UPDATE feeds SET error = ?2 WHERE url = ?1", params![url, error])?;
        Ok(())
    }

    /// Stores a freshly fetched copy of a feed. Changed content gets a newer
    /// version timestamp so it wins over older copies on other devices.
    pub fn store_fetched(&self, url: &str, title: &str, site: &str, items: &[(String, Item)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let mut changed = tx.execute(
            "INSERT INTO feeds (url, title, site, meta_t) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (url) DO UPDATE SET title = ?2, site = ?3, meta_t = max(?4, meta_t + 1)
             WHERE title != ?2 OR site != ?3",
            params![url, title, site, now_ms()],
        )? > 0;
        tx.execute("UPDATE items SET in_feed = 0 WHERE feed_url = ?1", [url])?;
        let mut new_or_changed = Vec::new();
        for (guid, item) in items {
            let key = docs::item_key(url, guid);
            tx.execute("UPDATE items SET in_feed = 1 WHERE key = ?1", [&key])?;
            let old: Option<(Item, i64)> = tx
                .query_row("SELECT title, link, author, published, content, t FROM items WHERE key = ?1", [&key], |r| {
                    Ok((read_item(r)?, r.get(5)?))
                })
                .optional()?;
            // An item without a date keeps the time it was first seen.
            let mut item = item.clone();
            if item.published == 0 {
                item.published = old.as_ref().map_or(now_ms() / 1000, |(old, _)| old.published);
                item.t = item.published * 1000;
            }
            let t = match old {
                None => item.t,
                Some((old, _)) if Item { t: item.t, ..old.clone() } == item => continue,
                Some((_, old_t)) => now_ms().max(old_t + 1),
            };
            tx.execute(
                "INSERT OR REPLACE INTO items (key, feed_url, title, link, author, published, content, t, in_feed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
                params![key, url, item.title, item.link, item.author, item.published, item.content, t],
            )?;
            changed = true;
            new_or_changed.push(key);
        }
        if changed {
            bump(&tx, &items_rev(url))?;
        }
        tx.commit()?;
        self.refresh_hidden_items(&new_or_changed)
    }

    /// Deletes what no device needs any more: items of unsubscribed feeds,
    /// items older than [`Self::keep_days`] unless they are still in their feed
    /// or cached on relays,
    /// and state that is no longer synced for items that count as read anyway.
    /// Favorites are always kept. None of this changes a synced document.
    pub fn prune(&self) -> Result<()> {
        let limits = self.cache_limits()?.map(|l| l.0).unwrap_or_default();
        let now = now_ms() / 1000;
        let keep_from = match self.keep_days()? {
            0 => i64::MIN,
            days => now - days as i64 * DAY_SECS,
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM items WHERE key IN (
                SELECT key FROM (
                    SELECT i.key, i.published, i.in_feed, f.subscribed,
                        row_number() OVER (PARTITION BY i.feed_url ORDER BY i.published DESC, i.key) AS position
                    FROM items i LEFT JOIN feeds f ON f.url = i.feed_url LEFT JOIN state s ON s.key = i.key
                    WHERE coalesce(s.fav, 0) = 0)
                WHERE coalesce(subscribed, 0) = 0
                   OR (published < ?3 AND in_feed = 0 AND NOT (position <= ?1 AND published >= ?2)))",
            params![limits.items, now - limits.days as i64 * DAY_SECS, keep_from],
        )?;
        tx.execute(
            "DELETE FROM state WHERE fav = 0 AND max(read_t, fav_t) < ?1
             AND NOT EXISTS (SELECT 1 FROM items i JOIN feeds f ON f.url = i.feed_url
                             WHERE i.key = state.key AND max(i.published * 1000, f.since_t) >= ?1)",
            [unread_cutoff()],
        )?;
        Ok(tx.commit()?)
    }

    // ---- Sync: bookkeeping ----

    /// Grows with every change to any synced document (until data is erased).
    pub fn revision(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT coalesce(sum(rev), 0) FROM revs", [], |r| r.get(0))?)
    }

    pub fn rev(&self, target: &str) -> Result<i64> {
        let rev = self.conn.query_row("SELECT rev FROM revs WHERE target = ?1", [target], |r| r.get(0)).optional()?;
        Ok(rev.unwrap_or(0))
    }

    pub fn known_docs(&self, relay: &str) -> Result<HashMap<String, KnownDoc>> {
        let mut stmt =
            self.conn.prepare("SELECT d, created_at, event_id, synced_rev FROM relay_docs WHERE relay = ?1")?;
        let rows = stmt.query_map([relay], |r| {
            Ok((
                r.get(0)?,
                KnownDoc { created_at: r.get::<_, i64>(1)? as u64, event_id: r.get(2)?, synced_rev: r.get(3)? },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_known_doc(&self, relay: &str, d: &str, doc: &KnownDoc) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO relay_docs (relay, d, created_at, event_id, synced_rev) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![relay, d, doc.created_at as i64, doc.event_id, doc.synced_rev],
        )?;
        Ok(())
    }

    pub fn forget_known_doc(&self, relay: &str, d: &str) -> Result<()> {
        self.conn.execute("DELETE FROM relay_docs WHERE relay = ?1 AND d = ?2", [relay, d])?;
        Ok(())
    }

    pub fn mark_synced(&self, relay: &str, d: &str, rev: i64) -> Result<()> {
        self.conn
            .execute("UPDATE relay_docs SET synced_rev = ?3 WHERE relay = ?1 AND d = ?2", params![relay, d, rev])?;
        Ok(())
    }

    pub fn decrypted(&self, event_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT content FROM decrypted WHERE event_id = ?1", [event_id], |r| r.get(0))
            .optional()?)
    }

    pub fn set_decrypted(&self, event_id: &str, content: &str) -> Result<()> {
        self.conn
            .execute("INSERT OR REPLACE INTO decrypted (event_id, content) VALUES (?1, ?2)", [event_id, content])?;
        Ok(())
    }

    /// Forgets bookkeeping for relays and documents no longer in use.
    pub fn prune_sync_cache(&self, relays: &[String], ds: &BTreeSet<String>) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let rows: Vec<(String, String)> = tx
            .prepare("SELECT relay, d FROM relay_docs")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (relay, d) in rows {
            if !relays.contains(&relay) || !ds.contains(&d) {
                tx.execute("DELETE FROM relay_docs WHERE relay = ?1 AND d = ?2", [&relay, &d])?;
            }
        }
        tx.execute("DELETE FROM decrypted WHERE event_id NOT IN (SELECT event_id FROM relay_docs)", [])?;
        Ok(tx.commit()?)
    }

    pub fn relay_state(&self, relay: &str) -> Result<RelayState> {
        let state = self
            .conn
            .query_row(
                "SELECT full_fetch_at, backoff_until, backoff_ms, send_window FROM relays WHERE url = ?1",
                [relay],
                |r| {
                    Ok(RelayState {
                        full_fetch_at: r.get(0)?,
                        backoff_until: r.get(1)?,
                        backoff_ms: r.get(2)?,
                        send_window: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(state.unwrap_or_default())
    }

    pub fn set_relay_state(&self, relay: &str, state: &RelayState) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO relays (url, full_fetch_at, backoff_until, backoff_ms, send_window)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![relay, state.full_fetch_at, state.backoff_until, state.backoff_ms, state.send_window],
        )?;
        Ok(())
    }

    // ---- Sync: building local documents ----

    pub fn salt(&self) -> Result<Option<Lww<String>>> {
        match (self.meta("salt")?, self.meta("salt_t")?) {
            (Some(salt), Some(t)) => Ok(Some(Lww(salt, t.parse()?))),
            _ => Ok(None),
        }
    }

    pub fn ensure_salt(&self) -> Result<String> {
        if let Some(salt) = self.salt()? {
            return Ok(salt.0);
        }
        let salt = nostr_sdk::prelude::Keys::generate().secret_key().to_secret_hex()[..32].to_string();
        self.set_meta("salt", &salt)?;
        self.set_meta("salt_t", &self.clock()?.to_string())?;
        bump(&self.conn, SUBSCRIPTIONS_REV)?;
        Ok(salt)
    }

    /// The subscriptions document as synced.
    pub fn subscriptions_doc(&self) -> Result<Subscriptions> {
        self.subscriptions(true)
    }

    /// The synced subscriptions document without its feeds, which are synced
    /// in [`Self::feeds_doc`] shards.
    pub fn subscriptions_doc_without_feeds(&self) -> Result<Subscriptions> {
        Ok(Subscriptions { feeds: BTreeMap::new(), categories: BTreeMap::new(), ..self.subscriptions_doc()? })
    }

    /// The synced feeds (and their categories) in one shard of the feed list.
    pub fn feeds_doc(&self, shard: u8) -> Result<Subscriptions> {
        let mut doc = self.subscriptions_doc()?;
        doc.feeds.retain(|url, _| docs::feed_shard_of(url) == shard);
        doc.categories.retain(|url, _| docs::feed_shard_of(url) == shard);
        Ok(Subscriptions { feeds: doc.feeds, categories: doc.categories, ..Default::default() })
    }

    /// This device's own view, including feeds it doesn't share (for backups).
    pub fn local_subscriptions(&self) -> Result<Subscriptions> {
        self.subscriptions(false)
    }

    fn subscriptions(&self, shared: bool) -> Result<Subscriptions> {
        // Feeds this device doesn't share are described by their shadow.
        let (own, shadow) = if shared {
            (
                "url NOT IN (SELECT url FROM sync_shadow)",
                "UNION ALL SELECT url, subscribed, sub_t, category, category_t FROM sync_shadow",
            )
        } else {
            ("1", "")
        };
        let rows = format!("SELECT url, subscribed, sub_t, category, category_t FROM feeds WHERE {own} {shadow}");
        let mut stmt = self.conn.prepare(&format!("SELECT url, subscribed, sub_t FROM ({rows}) WHERE sub_t > 0"))?;
        let feeds = stmt.query_map([], |r| Ok((r.get(0)?, Lww(r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>()?;
        let mut stmt =
            self.conn.prepare(&format!("SELECT url, category, category_t FROM ({rows}) WHERE category_t > 0"))?;
        let categories =
            stmt.query_map([], |r| Ok((r.get(0)?, Lww(r.get(1)?, r.get(2)?))))?.collect::<Result<_, _>>()?;
        let mut stmt = self.conn.prepare("SELECT id, phrase, scope, deleted, t FROM filters")?;
        let filters = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, Lww(WordFilter { phrase: r.get(1)?, scope: r.get(2)?, deleted: r.get(3)? }, r.get(4)?)))
            })?
            .collect::<Result<_, _>>()?;
        let erased_at = Some(self.erased_at()?).filter(|&t| t > 0);
        Ok(Subscriptions { salt: self.salt()?, feeds, categories, filters, cache: self.cache_limits()?, erased_at })
    }

    pub fn state_doc(&self, shard: u8) -> Result<StateShard> {
        // A shard is made of the keys starting with these two hex digits.
        let prefixes: Vec<String> =
            (0..=255u8).filter(|b| b % docs::STATE_SHARDS == shard).map(|b| format!("'{b:02x}'")).collect();
        self.read_state(&format!("substr(key, 1, 2) IN ({})", prefixes.join(",")))
    }

    /// All read/favorite state, for backups.
    pub fn all_state(&self) -> Result<StateShard> {
        self.read_state("1")
    }

    fn read_state(&self, filter: &str) -> Result<StateShard> {
        let sql = format!("SELECT key, read, read_t, fav, fav_t FROM state WHERE {filter}");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut doc = StateShard::default();
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let (read_t, fav_t): (i64, i64) = (r.get(2)?, r.get(4)?);
            let state = ItemState {
                r: (read_t > 0).then(|| r.get(1).map(|v| Lww(v, read_t))).transpose()?,
                f: (fav_t > 0).then(|| r.get(3).map(|v| Lww(v, fav_t))).transpose()?,
            };
            doc.items.insert(key, state);
        }
        Ok(doc)
    }

    /// Every feed this account knows, subscribed or not.
    pub fn all_feeds(&self) -> Result<Vec<FeedInfo>> {
        let mut stmt =
            self.conn.prepare("SELECT url, title, site, category, subscribed FROM feeds ORDER BY lower(title), url")?;
        let rows = stmt.query_map([], |r| {
            Ok(FeedInfo {
                url: r.get(0)?,
                title: r.get(1)?,
                site: r.get(2)?,
                category: r.get(3)?,
                subscribed: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A feed with every item stored locally, for backups.
    pub fn feed_backup(&self, url: &str) -> Result<FeedItems> {
        let mut doc: FeedItems = self
            .conn
            .query_row("SELECT title, site, meta_t FROM feeds WHERE url = ?1", [url], |r| {
                Ok(FeedItems { title: r.get(0)?, site: r.get(1)?, meta_t: r.get(2)?, items: BTreeMap::new() })
            })
            .optional()?
            .unwrap_or_default();
        let mut stmt = self
            .conn
            .prepare("SELECT title, link, author, published, content, t, key FROM items WHERE feed_url = ?1")?;
        let items = stmt.query_map([url], |r| Ok((r.get(6)?, Item { t: r.get(5)?, ..read_item(r)? })))?;
        doc.items = items.collect::<Result<_, _>>()?;
        Ok(doc)
    }

    /// Names a feed until its first fetch does (e.g. from an imported OPML file).
    pub fn set_title_if_empty(&self, url: &str, title: &str) -> Result<()> {
        self.conn.execute("UPDATE feeds SET title = ?2 WHERE url = ?1 AND title = ''", [url, title])?;
        Ok(())
    }

    /// The feed's newest items (favorites first) within the cache limits, as many as fit into one event.
    pub fn feed_items_doc(&self, url: &str) -> Result<FeedItems> {
        let limits = self.cache_limits()?.map(|l| l.0).unwrap_or_default();
        let oldest = now_ms() / 1000 - limits.days as i64 * DAY_SECS;
        let (title, site, meta_t): (String, String, i64) = self
            .conn
            .query_row("SELECT title, site, meta_t FROM feeds WHERE url = ?1", [url], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?
            .unwrap_or_default();
        let mut stmt = self.conn.prepare(
            "SELECT i.title, i.link, i.author, i.published, i.content, i.t, i.key
             FROM items i LEFT JOIN state s ON s.key = i.key
             WHERE i.feed_url = ?1 AND (coalesce(s.fav, 0) = 1 OR i.published >= ?3)
             ORDER BY coalesce(s.fav, 0) DESC, i.published DESC, i.key LIMIT ?2",
        )?;
        let items: Vec<(String, Item)> = stmt
            .query_map(params![url, limits.items, oldest], |r| Ok((r.get(6)?, Item { t: r.get(5)?, ..read_item(r)? })))?
            .collect::<Result<_, _>>()?;
        let doc = |n: usize| FeedItems {
            title: title.clone(),
            site: site.clone(),
            meta_t,
            items: items[..n].iter().cloned().collect::<BTreeMap<_, _>>(),
        };
        // Usually everything fits; otherwise binary search for the largest prefix that does.
        let fits = |n: usize| docs::encode(&doc(n)).len() <= docs::MAX_ENCODED;
        if fits(items.len()) {
            return Ok(doc(items.len()));
        }
        let (mut lo, mut hi) = (0, items.len() - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if fits(mid) { lo = mid } else { hi = mid - 1 }
        }
        Ok(doc(lo))
    }

    // ---- Sync: merging remote documents (last writer wins, ties go to the larger value) ----

    /// Returns whether the document erased this device's data.
    pub fn merge_subscriptions(&self, doc: &Subscriptions) -> Result<bool> {
        // The user deleted everything (on another device): do the same here.
        let erased = doc.erased_at.filter(|&t| t > self.erased_at().unwrap_or(0));
        if let Some(erased_at) = erased {
            self.wipe_account(false)?;
            self.set_meta("erased_at", &erased_at.to_string())?;
            bump(&self.conn, SUBSCRIPTIONS_REV)?;
        }
        // Anything from before the last erase stays deleted.
        let floor = self.erased_at()?;
        let tx = self.conn.unchecked_transaction()?;
        if let Some(Lww(salt, t)) = doc.salt.as_ref().filter(|s| s.1 > floor) {
            // First writer wins, so a salt never changes once everyone has seen it.
            let keep_local = self.salt()?.is_some_and(|local| (local.1, &local.0) <= (*t, salt));
            if !keep_local {
                tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('salt', ?1)", [salt])?;
                tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('salt_t', ?1)", [t.to_string()])?;
                bump(&tx, SUBSCRIPTIONS_REV)?;
            }
        }
        if let Some(remote) = doc.cache.as_ref().filter(|c| c.1 > floor) {
            let newer = self.cache_limits()?.is_none_or(|local| (remote.1, remote.0) > (local.1, local.0));
            if newer {
                self.store_cache_limits(&tx, remote)?;
            }
        }
        let mut changed = 0;
        let mut regroup = false;
        // Changes to feeds this device doesn't share go to their shadow, not its own view.
        let table = |url: &str| -> Result<&str> { Ok(if self.is_shadowed(url)? { "sync_shadow" } else { "feeds" }) };
        for (url, Lww(subscribed, t)) in doc.feeds.iter().filter(|(_, v)| v.1 > floor) {
            let sql = if self.is_shadowed(url)? {
                "INSERT INTO sync_shadow (url, subscribed, sub_t) VALUES (?1, ?2, ?3)
                 ON CONFLICT (url) DO UPDATE SET subscribed = excluded.subscribed, sub_t = excluded.sub_t
                 WHERE (excluded.sub_t, excluded.subscribed) > (sub_t, subscribed)"
            } else {
                // A feed subscribed to elsewhere is new here too.
                "INSERT INTO feeds (url, subscribed, sub_t, since_t) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT (url) DO UPDATE SET subscribed = excluded.subscribed, sub_t = excluded.sub_t,
                    since_t = CASE WHEN subscribed = 0 THEN excluded.sub_t ELSE since_t END
                 WHERE (excluded.sub_t, excluded.subscribed) > (sub_t, subscribed)"
            };
            changed += tx.execute(sql, params![url, subscribed, t])?;
        }
        for (url, Lww(category, t)) in doc.categories.iter().filter(|(_, v)| v.1 > floor) {
            let moved = tx.execute(
                &format!(
                    "INSERT INTO {} (url, category, category_t) VALUES (?1, ?2, ?3)
                     ON CONFLICT (url) DO UPDATE SET category = excluded.category, category_t = excluded.category_t
                     WHERE (excluded.category_t, excluded.category) > (category_t, category)",
                    table(url)?
                ),
                params![url, category, t],
            )?;
            changed += moved;
            regroup |= moved > 0;
        }
        if changed > 0 {
            bump(&tx, SUBSCRIPTIONS_REV)?;
        }
        tx.commit()?;
        for (id, filter) in doc.filters.iter().filter(|(_, f)| f.1 > floor) {
            regroup |= self.store_filter(id, filter)?;
        }
        if regroup || erased.is_some() {
            self.refresh_hidden(None)?;
        }
        Ok(erased.is_some())
    }

    pub fn merge_state(&self, doc: &StateShard) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (key, state) in &doc.items {
            let (read, read_t) = state.r.as_ref().map_or((false, 0), |r| (r.0, r.1));
            let (fav, fav_t) = state.f.as_ref().map_or((false, 0), |f| (f.0, f.1));
            let old_fav: (i64, bool) = tx
                .query_row("SELECT fav_t, fav FROM state WHERE key = ?1", [key], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .unwrap_or_default();
            let changed = tx.execute(
                "INSERT INTO state (key, read, read_t, fav, fav_t) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (key) DO UPDATE SET
                    read   = CASE WHEN (excluded.read_t, excluded.read) > (read_t, read) THEN excluded.read ELSE read END,
                    read_t = CASE WHEN (excluded.read_t, excluded.read) > (read_t, read) THEN excluded.read_t ELSE read_t END,
                    fav    = CASE WHEN (excluded.fav_t, excluded.fav) > (fav_t, fav) THEN excluded.fav ELSE fav END,
                    fav_t  = CASE WHEN (excluded.fav_t, excluded.fav) > (fav_t, fav) THEN excluded.fav_t ELSE fav_t END
                 WHERE (excluded.read_t, excluded.read) > (read_t, read) OR (excluded.fav_t, excluded.fav) > (fav_t, fav)",
                params![key, read, read_t, fav, fav_t],
            )?;
            if changed > 0 {
                bump(&tx, &state_rev(docs::shard_of(key)))?;
                if (fav_t, fav) > old_fav {
                    bump_feed_of(&tx, key)?;
                }
            }
        }
        Ok(tx.commit()?)
    }

    pub fn merge_feed_items(&self, url: &str, doc: &FeedItems) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let mut changed = tx.execute(
            "INSERT INTO feeds (url, title, site, meta_t) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (url) DO UPDATE SET title = excluded.title, site = excluded.site, meta_t = excluded.meta_t
             WHERE (excluded.meta_t, excluded.title, excluded.site) > (meta_t, title, site)",
            params![url, doc.title, doc.site, doc.meta_t],
        )?;
        let mut new_or_changed = Vec::new();
        for (key, item) in &doc.items {
            let updated = tx.execute(
                "INSERT INTO items (key, feed_url, title, link, author, published, content, t)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (key) DO UPDATE SET title = excluded.title, link = excluded.link, author = excluded.author,
                    published = excluded.published, content = excluded.content, t = excluded.t
                 WHERE (excluded.t, excluded.title, excluded.link, excluded.author, excluded.published, excluded.content)
                     > (t, title, link, author, published, content)",
                params![key, url, item.title, item.link, item.author, item.published, item.content, item.t],
            )?;
            if updated > 0 {
                new_or_changed.push(key.clone());
            }
            changed += updated;
        }
        if changed > 0 {
            bump(&tx, &items_rev(url))?;
        }
        tx.commit()?;
        self.refresh_hidden_items(&new_or_changed)
    }
}

/// Whether `text` contains `phrase` as whole words (both already lowercase).
fn contains_words(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// Which items a view shows, as SQL over `items i`, `feeds f` and `state s`,
/// and the value for its `?2` (every variant uses it, so parameters line up).
fn view_filter(view: &View) -> (String, &str) {
    match view {
        View::All => ("f.subscribed = 1 AND ?2 = ''".into(), ""),
        View::Favorites => ("s.fav = 1 AND ?2 = ''".into(), ""),
        View::Feed(url) => ("i.feed_url = ?2".into(), url.as_str()),
        View::Category(name) => ("f.subscribed = 1 AND f.category = ?2".into(), name.as_str()),
        View::Person(name) => (format!("i.feed_url = '{}' AND i.author = ?2", longform::FOLLOWS_URL), name.as_str()),
    }
}

fn read_item(r: &Row) -> rusqlite::Result<Item> {
    Ok(Item { title: r.get(0)?, link: r.get(1)?, author: r.get(2)?, published: r.get(3)?, content: r.get(4)?, t: 0 })
}

fn unread_cutoff() -> i64 {
    now_ms() - docs::RETENTION_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(published: i64) -> Item {
        Item { title: format!("Item {published}"), published, t: published * 1000, ..Default::default() }
    }

    #[test]
    fn prune_keeps_favorites_recent_items_and_the_relay_cache() {
        let db = Db::open_memory().unwrap();
        let feed = "https://example.com/feed";
        db.set_subscribed(feed, true).unwrap();
        // A cache reaching back far enough to hold old items.
        db.set_cache_limits(CacheLimits { items: 200, days: 3650 }).unwrap();
        db.set_meta("keep_days", &SUGGESTED_KEEP_DAYS.to_string()).unwrap();
        let old = now_ms() / 1000 - SUGGESTED_KEEP_DAYS as i64 * DAY_SECS - 1000;
        // 250 old items, plus 10 recent ones.
        let mut items: Vec<(String, Item)> = (0..250).map(|i| (format!("old-{i}"), item(old - i))).collect();
        items.extend((0..10).map(|i| (format!("new-{i}"), item(now_ms() / 1000 - i))));
        db.store_fetched(feed, "Feed", "", &items).unwrap();
        let oldest = docs::item_key(feed, "old-249");
        db.set_fav(&oldest, true).unwrap();
        // Items still in the feed are always kept, so let the old ones drop out of it.
        db.prune().unwrap();
        assert_eq!(db.items(&View::Feed(feed.into()), false).unwrap().len(), 260);
        db.store_fetched(feed, "Feed", "", &items[250..]).unwrap();
        let before = db.feed_items_doc(feed).unwrap();

        db.prune().unwrap();
        let left = db.items(&View::Feed(feed.into()), false).unwrap();
        // The newest 200 non-favorites stay (they may be in the relay cache), plus the favorite.
        assert_eq!(left.len(), 201);
        assert!(left.iter().any(|i| i.key == oldest));
        assert_eq!(db.feed_items_doc(feed).unwrap(), before, "pruning must not change a synced document");

        // With the default cache (30 days), only the recent items and the favorite stay.
        db.set_cache_limits(CacheLimits::default()).unwrap();
        let before = db.feed_items_doc(feed).unwrap();
        db.prune().unwrap();
        assert_eq!(db.items(&View::Feed(feed.into()), false).unwrap().len(), 11);
        assert_eq!(db.feed_items_doc(feed).unwrap(), before);

        // Keeping everything deletes nothing more.
        db.set_meta("keep_days", "0").unwrap();
        db.store_fetched(feed, "Feed", "", &[("ancient".into(), item(1))]).unwrap();
        db.store_fetched(feed, "Feed", "", &[]).unwrap();
        db.prune().unwrap();
        assert_eq!(db.items(&View::Feed(feed.into()), false).unwrap().len(), 12);
    }

    #[test]
    fn cache_limits_select_what_goes_to_relays() {
        let db = Db::open_memory().unwrap();
        let feed = "https://example.com/feed";
        db.set_subscribed(feed, true).unwrap();
        let now = now_ms() / 1000;
        let items: Vec<(String, Item)> = (0..10).map(|i| (format!("{i}"), item(now - i * DAY_SECS + 3600))).collect();
        db.store_fetched(feed, "Feed", "", &items).unwrap();
        db.set_fav(&docs::item_key(feed, "9"), true).unwrap();
        let cached = |db: &Db| db.feed_items_doc(feed).unwrap().items.len();
        db.set_cache_limits(CacheLimits { items: 3, days: 30 }).unwrap();
        assert_eq!(cached(&db), 3);
        // Items 0..=5 are within 5 days, plus the old favorite.
        db.set_cache_limits(CacheLimits { items: 100, days: 5 }).unwrap();
        assert_eq!(cached(&db), 7);
        db.set_cache_limits(CacheLimits { items: 0, days: 5 }).unwrap();
        assert_eq!(cached(&db), 0);
    }

    #[test]
    fn prune_drops_unsubscribed_feeds_but_not_their_favorites() {
        let db = Db::open_memory().unwrap();
        let feed = "https://example.com/feed";
        db.set_subscribed(feed, true).unwrap();
        let now = now_ms() / 1000;
        db.store_fetched(feed, "Feed", "", &[("a".into(), item(now)), ("b".into(), item(now))]).unwrap();
        db.set_fav(&docs::item_key(feed, "a"), true).unwrap();
        db.set_subscribed(feed, false).unwrap();
        db.prune().unwrap();
        let favorites = db.items(&View::Favorites, false).unwrap();
        assert_eq!(favorites.len(), 1);
        assert_eq!(db.items(&View::Feed(feed.into()), false).unwrap().len(), 1);
    }

    #[test]
    fn prune_forgets_old_state_only_where_it_no_longer_matters() {
        let db = Db::open_memory().unwrap();
        let feed = "https://example.com/feed";
        db.set_subscribed(feed, true).unwrap();
        let now = now_ms() / 1000;
        db.store_fetched(feed, "Feed", "", &[("recent".into(), item(now))]).unwrap();
        let long_ago = now_ms() - docs::RETENTION_MS - 1000;
        let mut doc = StateShard::default();
        for key in [docs::item_key(feed, "recent"), "gone".to_string()] {
            doc.items.insert(key, ItemState { r: Some(Lww(true, long_ago)), f: None });
        }
        db.merge_state(&doc).unwrap();
        db.prune().unwrap();
        let keys: Vec<String> = db
            .conn
            .prepare("SELECT key FROM state")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // The recent item would turn unread without its state, so that is kept.
        assert_eq!(keys, [docs::item_key(feed, "recent")]);
    }

    #[test]
    fn new_feeds_start_unread() {
        let db = Db::open_memory().unwrap();
        let old = now_ms() / 1000 - 365 * DAY_SECS;
        let (added, followed_long_ago) = ("https://a.example/feed", "https://b.example/feed");
        db.set_subscribed(added, true).unwrap();
        // Subscribed to on another device a year ago.
        let mut doc = Subscriptions::default();
        doc.feeds.insert(followed_long_ago.into(), Lww(true, old * 1000));
        db.merge_subscriptions(&doc).unwrap();
        for feed in [added, followed_long_ago] {
            db.store_fetched(feed, "Feed", "", &[("old".into(), item(old))]).unwrap();
        }
        let unread = |feed: &str| db.items(&View::Feed(feed.into()), true).unwrap().len();
        assert_eq!((unread(added), unread(followed_long_ago)), (1, 0));
        // Subscribing again elsewhere makes it new here too.
        doc.feeds.insert(followed_long_ago.into(), Lww(false, old * 1000 + 1));
        db.merge_subscriptions(&doc).unwrap();
        doc.feeds.insert(followed_long_ago.into(), Lww(true, now_ms()));
        db.merge_subscriptions(&doc).unwrap();
        assert_eq!(unread(followed_long_ago), 1);
    }

    #[test]
    fn undated_items_keep_their_first_date() {
        let db = Db::open_memory().unwrap();
        let feed = "https://a.example/feed";
        db.set_subscribed(feed, true).unwrap();
        let undated = || [("1".to_string(), Item { title: "Hello".into(), ..Default::default() })];
        db.store_fetched(feed, "Feed", "", &undated()).unwrap();
        let first = db.items(&View::Feed(feed.into()), false).unwrap()[0].published;
        assert!(first > 0);
        db.conn.execute("UPDATE items SET published = published - 100", []).unwrap();
        let rev = db.rev(&items_rev(feed)).unwrap();
        db.store_fetched(feed, "Feed", "", &undated()).unwrap();
        // Not re-dated to now, and not counted as a change.
        assert_eq!(db.items(&View::Feed(feed.into()), false).unwrap()[0].published, first - 100);
        assert_eq!(db.rev(&items_rev(feed)).unwrap(), rev);
    }

    #[test]
    fn views_page_and_mark_all_as_read_reaches_every_item() {
        let db = Db::open_memory().unwrap();
        let feed = "https://a.example/feed";
        db.set_subscribed(feed, true).unwrap();
        let now = now_ms() / 1000;
        let items: Vec<(String, Item)> = (0..1500)
            .map(|i| (format!("g{i}"), Item { title: format!("Post {i}"), published: now - i, ..Default::default() }))
            .collect();
        db.store_fetched(feed, "Feed", "", &items).unwrap();
        let view = View::Feed(feed.into());
        assert_eq!(db.items_matching(&view, false, "", 1000).unwrap().len(), 1000);
        assert_eq!(db.items_matching(&view, false, "", 2000).unwrap().len(), 1500);
        // Marking with a search only marks what matches.
        db.set_read(&db.unread_keys(&view, "post 149").unwrap(), true).unwrap();
        assert_eq!(db.feeds().unwrap()[0].unread, 1500 - 11);
        db.set_read(&db.unread_keys(&view, "").unwrap(), true).unwrap();
        assert_eq!(db.feeds().unwrap()[0].unread, 0);
    }

    #[test]
    fn deleted_data_cant_be_recovered_from_the_files() {
        let dir = std::env::temp_dir().join(format!("feedstr-wipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feedstr.db");
        let db = Db::open(&path).unwrap();
        let secret = "nsec1thisisthesecretmarker";
        db.set_meta("sync_key", secret).unwrap();
        db.set_subscribed("https://a.example/feed", true).unwrap();
        let item = Item { title: "Private article marker".into(), published: 1, ..Default::default() };
        db.store_fetched("https://a.example/feed", "Feed", "", &[("1".into(), item)]).unwrap();
        db.wipe_everything().unwrap();
        for file in std::fs::read_dir(&dir).unwrap() {
            let bytes = std::fs::read(file.unwrap().path()).unwrap();
            for marker in [secret.as_bytes(), b"Private article marker"] {
                assert!(!bytes.windows(marker.len()).any(|w| w == marker));
            }
        }
        drop(db);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn filters_apply_to_newly_fetched_items_only_as_needed() {
        let db = Db::open_memory().unwrap();
        let feed = "https://a.example/feed";
        db.set_subscribed(feed, true).unwrap();
        db.add_filter("spoiler", "").unwrap();
        let item = |title: &str| Item { title: title.into(), published: now_ms() / 1000, ..Default::default() };
        db.store_fetched(feed, "Feed", "", &[("1".into(), item("Big spoiler"))]).unwrap();
        db.store_fetched(feed, "Feed", "", &[("1".into(), item("Big spoiler")), ("2".into(), item("News"))]).unwrap();
        let titles: Vec<String> = db.items(&View::All, false).unwrap().into_iter().map(|i| i.title).collect();
        assert_eq!(titles, ["News"]);
        // An edit that removes the word shows the item again.
        db.store_fetched(feed, "Feed", "", &[("1".into(), item("Big news"))]).unwrap();
        assert_eq!(db.items(&View::All, false).unwrap().len(), 2);
    }

    #[test]
    fn snapshots_restore_everything() {
        let db = Db::open_memory().unwrap();
        db.set_subscribed("https://a.example/feed", true).unwrap();
        db.set_meta("login", "sync_key").unwrap();
        let before = db.changes_made();
        db.set_read(&["k".to_string()], true).unwrap();
        assert!(db.changes_made() > before);
        let copy = Db::from_snapshot(&db.snapshot().unwrap()).unwrap();
        assert_eq!(copy.subscribed_feeds().unwrap(), ["https://a.example/feed"]);
        assert_eq!(copy.meta("login").unwrap().as_deref(), Some("sync_key"));
        assert!(Db::from_snapshot(b"not a database").is_err());
    }

    #[test]
    fn search_and_category_views() {
        let db = Db::open_memory().unwrap();
        let now = now_ms() / 1000;
        for (feed, category) in [("https://a.example/feed", "News"), ("https://b.example/feed", "")] {
            db.set_subscribed(feed, true).unwrap();
            db.set_category(feed, category).unwrap();
            let items = [("1", "Rust 2.0 released", "Big news."), ("2", "Gardening", "Tomatoes and RUST spots")];
            let items: Vec<(String, Item)> = items
                .iter()
                .map(|(guid, title, content)| {
                    let item = Item { title: title.to_string(), content: content.to_string(), ..item(now) };
                    (guid.to_string(), item)
                })
                .collect();
            db.store_fetched(feed, "Feed", "", &items).unwrap();
        }
        assert_eq!(db.items(&View::Category("News".into()), false).unwrap().len(), 2);
        // Case-insensitive, in titles and text.
        assert_eq!(db.items_matching(&View::All, false, "rust", 1000).unwrap().len(), 4);
        assert_eq!(db.items_matching(&View::Category("News".into()), false, " Tomatoes ", 1000).unwrap().len(), 1);
        assert_eq!(db.items_matching(&View::All, false, "nothing like this", 1000).unwrap().len(), 0);
        assert_eq!(db.categories().unwrap(), ["News"]);
    }

    #[test]
    fn migrates_an_existing_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("INSERT INTO feeds (url, subscribed, sub_t, etag) VALUES ('https://a.example/', 1, 5, 'x')", [])
            .unwrap();
        // Pretend this is a database from before categories existed.
        conn.execute_batch("PRAGMA user_version = 0").unwrap();
        let db = Db::init(conn).unwrap();
        assert_eq!(db.feeds().unwrap()[0].category, "");
        assert!(db.fetch_info("https://a.example/").unwrap().etag.is_none());
        db.set_category("https://a.example/", "Tech").unwrap();
        // Opening again doesn't run the migration twice.
        let version: i64 = db.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
    }

    #[test]
    fn keeps_everything_by_default() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.keep_days().unwrap(), 0);
        let feed = "https://example.com/feed";
        db.set_subscribed(feed, true).unwrap();
        db.store_fetched(feed, "Feed", "", &[("ancient".into(), item(1))]).unwrap();
        db.store_fetched(feed, "Feed", "", &[]).unwrap();
        db.prune().unwrap();
        assert_eq!(db.items(&View::Feed(feed.into()), false).unwrap().len(), 1);
    }

    #[test]
    fn word_filters_hide_items_by_scope() {
        let db = Db::open_memory().unwrap();
        let now = now_ms() / 1000;
        let (news, blog) = ("https://news.example/feed", "https://blog.example/feed");
        for (feed, category) in [(news, "News"), (blog, "")] {
            db.set_subscribed(feed, true).unwrap();
            db.set_category(feed, category).unwrap();
            let posts = [
                ("1", "Election results", "<p>Votes counted.</p>"),
                ("2", "Gardening", "<p>Tomatoes &amp; <b>Crypto</b> tips</p>"),
            ];
            let items: Vec<(String, Item)> = posts
                .iter()
                .map(|(guid, title, content)| {
                    (guid.to_string(), Item { title: title.to_string(), content: content.to_string(), ..item(now) })
                })
                .collect();
            db.store_fetched(feed, "Feed", "", &items).unwrap();
        }
        let visible = |db: &Db| db.items(&View::All, false).unwrap().len();
        assert_eq!(visible(&db), 4);

        // Whole words, any case, in titles and text (markup ignored).
        db.add_filter("crypto", "").unwrap();
        assert_eq!(visible(&db), 2);
        db.add_filter("count", "").unwrap(); // "counted" is a different word
        assert_eq!(visible(&db), 2);

        // A category filter only applies to its feeds, and follows a rename.
        db.add_filter("Election", "category:News").unwrap();
        assert_eq!(visible(&db), 1);
        db.rename_category("News", "World").unwrap();
        assert_eq!(visible(&db), 1);
        assert!(db.filters().unwrap().iter().any(|f| f.scope == "category:World"));

        // Favorites always stay; unread counts leave hidden items out.
        db.set_fav(&docs::item_key(blog, "2"), true).unwrap();
        assert_eq!(visible(&db), 2);
        let unread: i64 = db.feeds().unwrap().iter().map(|f| f.unread).sum();
        assert_eq!(unread, 2);

        // Removing filters brings items back.
        for filter in db.filters().unwrap() {
            db.remove_filter(&filter.id).unwrap();
        }
        assert_eq!(visible(&db), 4);
    }

    #[test]
    fn feed_report_lists_failing_and_quiet_feeds() {
        let db = Db::open_memory().unwrap();
        let now = now_ms() / 1000;
        for (feed, published) in [("https://fresh.example/", now), ("https://quiet.example/", now - 400 * DAY_SECS)] {
            db.set_subscribed(feed, true).unwrap();
            db.store_fetched(feed, "", "", &[("1".into(), item(published))]).unwrap();
        }
        db.set_subscribed("https://empty.example/", true).unwrap();
        db.set_subscribed("https://broken.example/", true).unwrap();
        db.set_fetch_error("https://broken.example/", "404 Not Found").unwrap();
        let urls = |days| db.feed_report(days).unwrap().into_iter().map(|r| r.url).collect::<Vec<_>>();
        assert_eq!(urls(365), ["https://broken.example/", "https://empty.example/", "https://quiet.example/"]);
        assert_eq!(urls(500), ["https://broken.example/", "https://empty.example/"]);
    }

    #[test]
    fn follows_feed_groups_by_person() {
        let db = Db::open_memory().unwrap();
        db.set_subscribed(longform::FOLLOWS_URL, true).unwrap();
        let now = now_ms() / 1000;
        let posts: Vec<(String, Item)> = [("a", "Alice"), ("b", "Bob"), ("c", "Alice")]
            .into_iter()
            .map(|(guid, author)| (guid.to_string(), Item { author: author.into(), ..item(now) }))
            .collect();
        db.store_fetched(longform::FOLLOWS_URL, longform::FOLLOWS_TITLE, "", &posts).unwrap();
        db.set_read(&[docs::item_key(longform::FOLLOWS_URL, "a")], true).unwrap();
        assert_eq!(db.follows_people().unwrap(), [("Alice".to_string(), 1), ("Bob".to_string(), 1)]);
        assert_eq!(db.items(&View::Person("Alice".into()), false).unwrap().len(), 2);
    }
}
