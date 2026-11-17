//! The documents Feedstr keeps on Nostr relays, and how they are encoded.
//!
//! Every document is a NIP-78 event (kind 30078) whose content is
//! `nip44_encrypt(self, base64(deflate(json {"v": 1, "data": <doc>})))`.
//!
//! | d-tag                               | document         |
//! |-------------------------------------|------------------|
//! | `feedstr:subscriptions`             | [`Subscriptions`] without feeds |
//! | `feedstr:<h("feeds:<0..16>")>`      | [`Subscriptions`] with only the feeds (and their categories and routes) in that shard |
//! | `feedstr:<h("state:<0..32>")>`      | [`StateShard`]    |
//! | `feedstr:<h("items:<feed url>")>`   | [`FeedItems`]     |
//!
//! where `h(x)` is the first 32 hex chars of `sha256(salt + ":" + x)` and the
//! salt lives inside the (encrypted) subscriptions document, so relays can't
//! tell which feeds someone follows.
//!
//! Every value that can change carries its own timestamp and documents are
//! merged field by field (last writer wins, ties go to the larger value).
//! Merging is commutative and idempotent, so it doesn't matter in which
//! order, or how many times, versions from different relays are applied.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FORMAT_VERSION: u32 = 1;
pub const SUBSCRIPTIONS_D: &str = "feedstr:subscriptions";
/// Encoded size limit, so the encrypted event stays below the 64 KiB many relays enforce.
pub const MAX_ENCODED: usize = 40_000;
/// Read/unread state older than this (and not a favorite) is no longer synced.
/// Items older than this without any state count as read.
pub const RETENTION_MS: i64 = 60 * 24 * 3600 * 1000;
pub const STATE_SHARDS: u8 = 32;
/// The feed list is spread over this many documents, so it can hold
/// thousands of feeds (each document holds roughly 800).
pub const FEED_SHARDS: u8 = 16;

/// Last-writer-wins register: `[value, timestamp in ms]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lww<T>(pub T, pub i64);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Subscriptions {
    /// Random value used to name the other documents. The *first* writer wins.
    pub salt: Option<Lww<String>>,
    /// Feed URL -> subscribed. Unsubscribing keeps the entry as a tombstone.
    pub feeds: BTreeMap<String, Lww<bool>>,
    /// Feed URL -> category name ("" for none).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub categories: BTreeMap<String, Lww<String>>,
    /// Feed URL -> how it is fetched: "" directly, "tor" or "i2p" through
    /// that network's proxy on each device (see `proxy::Route`). Synced with
    /// the feed list, in the same shards.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub routes: BTreeMap<String, Lww<String>>,
    /// How much of each feed is cached on relays; `None` means the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<Lww<CacheLimits>>,
    /// Word filters by id. Removing one keeps it as a tombstone.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filters: BTreeMap<String, Lww<WordFilter>>,
    /// When the user deleted all their Feedstr data (ms). Devices erase their
    /// copies on seeing a newer value, and every older entry is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erased_at: Option<i64>,
    /// The relays to sync with.
    #[serde(default, skip_serializing_if = "RelayList::is_empty")]
    pub relays: RelayList,
}

/// The relays to sync with, shared so that a relay added or removed on one
/// device is used by the others too.
///
/// Until the user changes the list, every device uses the defaults of the
/// version of Feedstr it runs (`custom` is unset or false), so an update can
/// change them. "Use the default relays" sets `custom` to false again and
/// leaves the entries alone; the next change starts from the defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RelayList {
    /// Whether the relays below are used instead of the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<Lww<bool>>,
    /// Relay address -> in the list. Removing one keeps it as a tombstone, so
    /// each relay is merged on its own: two devices adding or removing
    /// different relays both get their way.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relays: BTreeMap<String, Lww<bool>>,
}

impl RelayList {
    pub fn is_empty(&self) -> bool {
        self.custom.is_none() && self.relays.is_empty()
    }

    pub fn is_custom(&self) -> bool {
        self.custom.as_ref().is_some_and(|custom| custom.0)
    }

    /// The relays in the list, in the order they were added.
    pub fn listed(&self) -> Vec<String> {
        let mut listed: Vec<(i64, &String)> =
            self.relays.iter().filter(|(_, listed)| listed.0).map(|(url, listed)| (listed.1, url)).collect();
        listed.sort();
        listed.into_iter().map(|(_, url)| url.clone()).collect()
    }

    /// The newest timestamp in the list (0 if there is none).
    pub fn newest(&self) -> i64 {
        self.relays.values().map(|v| v.1).chain(self.custom.as_ref().map(|c| c.1)).max().unwrap_or(0)
    }

    /// Makes `relays` the list (`custom`), or goes back to the defaults.
    /// Relays that are added get timestamps from `t` on, in order; `t` must be
    /// newer than every timestamp in the list.
    pub fn set(&mut self, relays: &[String], custom: bool, t: i64) {
        self.custom = Some(Lww(custom, t));
        if !custom {
            return;
        }
        for (url, listed) in &mut self.relays {
            if listed.0 && !relays.contains(url) {
                *listed = Lww(false, t);
            }
        }
        let mut next = t;
        for url in relays {
            // A relay kept from before signing in to this account (timestamp 0) is shared now.
            if self.relays.get(url).is_none_or(|listed| !listed.0 || listed.1 == 0) {
                self.relays.insert(url.clone(), Lww(true, next));
                next += 1;
            }
        }
    }

    /// Merges another device's list, ignoring values from `floor` or before
    /// (last writer wins, ties go to the larger value). Returns whether
    /// anything changed.
    pub fn merge(&mut self, other: &RelayList, floor: i64) -> bool {
        let newer = |remote: &Lww<bool>, local: Option<&Lww<bool>>| {
            remote.1 > floor && local.is_none_or(|local| (remote.1, remote.0) > (local.1, local.0))
        };
        let mut changed = false;
        if let Some(remote) = other.custom.as_ref().filter(|remote| newer(remote, self.custom.as_ref())) {
            self.custom = Some(remote.clone());
            // The account has a list of its own: one kept from before signing in gives way.
            self.relays.retain(|_, listed| listed.1 > 0);
            changed = true;
        }
        for (url, remote) in &other.relays {
            if newer(remote, self.relays.get(url)) {
                self.relays.insert(url.clone(), remote.clone());
                changed = true;
            }
        }
        changed
    }

    /// The list as shared: without values that aren't (timestamp 0).
    pub fn shared(&self) -> RelayList {
        RelayList {
            custom: self.custom.clone().filter(|custom| custom.1 > 0),
            relays: self.relays.iter().filter(|(_, v)| v.1 > 0).map(|(url, v)| (url.clone(), v.clone())).collect(),
        }
    }

    /// Keeps the list but stops sharing it (timestamp 0), so any list the
    /// account already has wins over it.
    pub fn unshare(&mut self) {
        for value in self.custom.iter_mut().chain(self.relays.values_mut()) {
            value.1 = 0;
        }
    }
}

/// Hides items containing a word or phrase.
#[derive(Clone, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct WordFilter {
    pub phrase: String,
    /// "" for all feeds, "category:<name>" or "feed:<url>".
    pub scope: String,
    pub deleted: bool,
}

/// How much of each feed is cached on relays. This is synced, so every
/// device builds identical caches instead of overwriting each other's.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct CacheLimits {
    /// Most items per feed, favorites first. 0 caches no items at all.
    pub items: u32,
    /// Items older than this many days are left out, unless they are favorites.
    pub days: u32,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self { items: 50, days: 30 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ItemState {
    /// Read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r: Option<Lww<bool>>,
    /// Favorite
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f: Option<Lww<bool>>,
}

/// Read/favorite state of all items whose key falls into this shard.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StateShard {
    pub items: BTreeMap<String, ItemState>,
}

/// Cached copy of a feed, so clients that can't fetch it (the web) can still show it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedItems {
    pub title: String,
    pub site: String,
    /// Timestamp of `title` and `site`.
    pub meta_t: i64,
    pub items: BTreeMap<String, Item>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub title: String,
    pub link: String,
    pub author: String,
    /// Seconds since the epoch. A freshly parsed item has 0 if its feed gives
    /// no usable date; stored items always have one.
    pub published: i64,
    /// HTML or plain text.
    pub content: String,
    /// Version timestamp: bumped whenever the content changes.
    pub t: i64,
}

impl StateShard {
    pub fn prune(&mut self, now_ms: i64) {
        self.items.retain(|_, s| {
            let fav = s.f.as_ref().is_some_and(|f| f.0);
            let t = s.r.as_ref().map_or(0, |r| r.1).max(s.f.as_ref().map_or(0, |f| f.1));
            fav || t >= now_ms - RETENTION_MS
        });
    }
}

#[derive(Serialize)]
struct EnvelopeOut<'a, T> {
    v: u32,
    data: &'a T,
}

#[derive(Deserialize)]
struct EnvelopeIn<T> {
    v: u32,
    data: T,
}

#[derive(Deserialize)]
struct VersionOnly {
    v: u32,
}

pub fn encode<T: Serialize>(doc: &T) -> String {
    let json = serde_json::to_vec(&EnvelopeOut { v: FORMAT_VERSION, data: doc }).expect("documents serialize");
    let mut deflate = DeflateEncoder::new(Vec::new(), Compression::best());
    deflate.write_all(&json).expect("writing to a Vec can't fail");
    BASE64.encode(deflate.finish().expect("writing to a Vec can't fail"))
}

pub fn decode<T: DeserializeOwned>(text: &str) -> Result<T> {
    let compressed = BASE64.decode(text.trim()).context("not base64")?;
    let mut json = Vec::new();
    // Cap the inflated size so a malicious document can't exhaust memory.
    DeflateDecoder::new(&compressed[..]).take(32 << 20).read_to_end(&mut json).context("not deflate")?;
    let version: VersionOnly = serde_json::from_slice(&json).context("no format version")?;
    if version.v > FORMAT_VERSION {
        bail!("written by a newer version of Feedstr (format {})", version.v);
    }
    let envelope: EnvelopeIn<T> = serde_json::from_slice(&json).context("malformed document")?;
    debug_assert!(envelope.v <= FORMAT_VERSION);
    Ok(envelope.data)
}

pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    Sha256::digest(data.as_ref()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Stable key of an item, the same on every device.
pub fn item_key(feed_url: &str, guid: &str) -> String {
    sha256_hex(format!("{feed_url}\n{guid}"))[..16].to_string()
}

pub fn shard_of(item_key: &str) -> u8 {
    u8::from_str_radix(item_key.get(..2).unwrap_or("0"), 16).unwrap_or(0) % STATE_SHARDS
}

/// Which feed list document a feed is in.
pub fn feed_shard_of(feed_url: &str) -> u8 {
    u8::from_str_radix(&sha256_hex(feed_url)[..2], 16).unwrap_or(0) % FEED_SHARDS
}

pub fn feeds_d(salt: &str, shard: u8) -> String {
    format!("feedstr:{}", &sha256_hex(format!("{salt}:feeds:{shard}"))[..32])
}

pub fn state_d(salt: &str, shard: u8) -> String {
    format!("feedstr:{}", &sha256_hex(format!("{salt}:state:{shard}"))[..32])
}

pub fn items_d(salt: &str, feed_url: &str) -> String {
    format!("feedstr:{}", &sha256_hex(format!("{salt}:items:{feed_url}"))[..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut doc = Subscriptions::default();
        doc.feeds.insert("https://example.com/feed".into(), Lww(true, 5));
        doc.salt = Some(Lww("abc".into(), 1));
        assert_eq!(decode::<Subscriptions>(&encode(&doc)).unwrap(), doc);
    }

    #[test]
    fn rejects_newer_format() {
        let json = br#"{"v":99,"data":{}}"#;
        let mut deflate = DeflateEncoder::new(Vec::new(), Compression::best());
        deflate.write_all(json).unwrap();
        let text = BASE64.encode(deflate.finish().unwrap());
        let err = decode::<Subscriptions>(&text).unwrap_err();
        assert!(err.to_string().contains("newer version"));
    }

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|url| url.to_string()).collect()
    }

    #[test]
    fn relay_lists_merge_relay_by_relay() {
        let mut a = RelayList::default();
        assert!(!a.is_custom());
        a.set(&urls(&["wss://one", "wss://two"]), true, 10);
        assert_eq!(a.listed(), ["wss://one", "wss://two"]);
        let mut b = a.clone();
        // Concurrently, one device adds a relay and the other removes one.
        a.set(&urls(&["wss://one", "wss://two", "wss://three"]), true, 20);
        b.set(&urls(&["wss://two"]), true, 21);
        let (mut ab, mut ba) = (a.clone(), b.clone());
        assert!(ab.merge(&b, 0) && ba.merge(&a, 0));
        assert_eq!(ab, ba);
        assert_eq!(ab.listed(), ["wss://two", "wss://three"]);
        // Merging again changes nothing.
        assert!(!ab.merge(&ba, 0));
        // Going back to the defaults is the newest change to the mode, so it wins.
        b.set(&[], false, 30);
        assert!(ab.merge(&b, 0));
        assert!(!ab.is_custom());
        // The next change starts from the defaults, not from the old list.
        ab.set(&urls(&["wss://default", "wss://four"]), true, 40);
        assert_eq!(ab.listed(), ["wss://default", "wss://four"]);
        // Values from before an erase are ignored.
        assert!(!RelayList::default().merge(&ab, ab.newest()));
    }

    #[test]
    fn unshared_relay_lists_give_way() {
        let mut device = RelayList::default();
        device.set(&urls(&["wss://mine"]), true, 10);
        device.unshare();
        assert_eq!(device.shared(), RelayList::default());
        assert_eq!(device.listed(), ["wss://mine"]);
        // The account's own list wins...
        let mut account = RelayList::default();
        account.set(&urls(&["wss://theirs"]), true, 5);
        let mut merged = device.clone();
        assert!(merged.merge(&account, 0));
        assert_eq!(merged.listed(), ["wss://theirs"]);
        // ...and a change on this device shares all of its list.
        device.set(&urls(&["wss://mine", "wss://new"]), true, 20);
        assert_eq!(device.shared().listed(), ["wss://mine", "wss://new"]);
    }

    #[test]
    fn prune_keeps_favorites_and_recent() {
        let now = RETENTION_MS * 10;
        let mut shard = StateShard::default();
        let old = now - RETENTION_MS - 1;
        shard.items.insert("old-read".into(), ItemState { r: Some(Lww(true, old)), f: None });
        shard.items.insert("old-fav".into(), ItemState { r: Some(Lww(true, old)), f: Some(Lww(true, old)) });
        shard.items.insert("recent".into(), ItemState { r: Some(Lww(true, now)), f: None });
        shard.prune(now);
        assert_eq!(shard.items.keys().collect::<Vec<_>>(), ["old-fav", "recent"]);
    }
}
