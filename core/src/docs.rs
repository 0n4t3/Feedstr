//! The documents Feedstr keeps on Nostr relays, and how they are encoded.
//!
//! Every document is a NIP-78 event (kind 30078) whose content is
//! `nip44_encrypt(self, base64(deflate(json {"v": 1, "data": <doc>})))`.
//!
//! | d-tag                               | document         |
//! |-------------------------------------|------------------|
//! | `feedstr:subscriptions`             | [`Subscriptions`] without feeds |
//! | `feedstr:<h("feeds:<0..16>")>`      | [`Subscriptions`] with only the feeds (and their categories) in that shard |
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

pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Stable key of an item, the same on every device.
pub fn item_key(feed_url: &str, guid: &str) -> String {
    sha256_hex(&format!("{feed_url}\n{guid}"))[..16].to_string()
}

pub fn shard_of(item_key: &str) -> u8 {
    u8::from_str_radix(item_key.get(..2).unwrap_or("0"), 16).unwrap_or(0) % STATE_SHARDS
}

/// Which feed list document a feed is in.
pub fn feed_shard_of(feed_url: &str) -> u8 {
    u8::from_str_radix(&sha256_hex(feed_url)[..2], 16).unwrap_or(0) % FEED_SHARDS
}

pub fn feeds_d(salt: &str, shard: u8) -> String {
    format!("feedstr:{}", &sha256_hex(&format!("{salt}:feeds:{shard}"))[..32])
}

pub fn state_d(salt: &str, shard: u8) -> String {
    format!("feedstr:{}", &sha256_hex(&format!("{salt}:state:{shard}"))[..32])
}

pub fn items_d(salt: &str, feed_url: &str) -> String {
    format!("feedstr:{}", &sha256_hex(&format!("{salt}:items:{feed_url}"))[..32])
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
