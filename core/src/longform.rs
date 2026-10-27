//! Nostr long-form posts (NIP-23) as feeds: one person's posts
//! (`nostr:npub1…`), or those of everyone the user follows (`nostr:follows`).
//!
//! Posts are read where their authors publish them: each author's relay list
//! (NIP-65) is looked up on the user's relays and an index relay, and those
//! relays are asked for the posts. Nobody has to be followed for this.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nostr_sdk::prelude::*;

use crate::db::now_ms;
use crate::docs::Item;
use crate::feeds::{self, Feed};
use crate::html;

pub const FOLLOWS_URL: &str = "nostr:follows";
pub const FOLLOWS_TITLE: &str = "Nostr Follows";
/// Where "Open in Ditto" shows Nostr posts and profiles.
pub(crate) const WEB_VIEWER: &str = "https://ditto.pub";
/// Collects relay lists and profiles, so it knows where most people publish.
const INDEX_RELAY: &str = "wss://purplepag.es";
const TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Write relays per author looked at for the follows feed.
const RELAYS_PER_AUTHOR: usize = 3;
/// Relays asked for posts at most, chosen to cover as many authors as possible.
const MAX_CONTENT_RELAYS: usize = 25;
/// Authors per request.
const AUTHORS_PER_FILTER: usize = 250;
/// How far back the follows feed looks when it has no posts yet.
const FOLLOWS_HISTORY_SECS: i64 = 60 * 24 * 3600;
/// Updates look this far before the newest post, for posts that reached relays late.
const OVERLAP_SECS: i64 = 7 * 24 * 3600;
/// Posts asked for at once when reading someone's whole history.
const PAGE: usize = 100;
/// Pages read per relay at most.
const MAX_PAGES: usize = 50;
/// A writer's own relays read for their history at most (relay lists can be long).
const MAX_HISTORY_RELAYS: usize = 10;

pub fn is_nostr(url: &str) -> bool {
    url.starts_with("nostr:")
}

pub fn author_url(author: &PublicKey) -> String {
    format!("nostr:{}", author.to_bech32().expect("bech32 encoding can't fail"))
}

/// The person an npub, nprofile or `nostr:` address refers to.
pub fn parse_author(input: &str) -> Option<PublicKey> {
    let input = input.trim().trim_start_matches("nostr:");
    let bech32 = input.starts_with("npub1") || input.starts_with("nprofile1");
    bech32.then(|| PublicKey::parse(input).ok()).flatten()
}

/// The `nostr:` link (NIP-21) for the post or profile an item links to on the
/// web viewer, for opening it in a Nostr app instead.
pub fn nostr_link(item_link: &str) -> Option<String> {
    let id = item_link.strip_prefix(WEB_VIEWER)?.strip_prefix('/')?;
    let kinds = ["naddr1", "nevent1", "note1", "npub1", "nprofile1"];
    // Only bech32 characters, so the link can't carry anything else to the app opening it.
    let bech32 = id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    (bech32 && kinds.iter().any(|kind| id.starts_with(kind))).then(|| format!("nostr:{id}"))
}

/// Whether the input looks like a NIP-05 address (name@domain).
pub fn is_nip05(input: &str) -> bool {
    let input = input.trim();
    !input.contains("://")
        && !input.contains(char::is_whitespace)
        && input.split_once('@').is_some_and(|(_, domain)| domain.contains('.'))
}

/// Looks up the person behind a NIP-05 address.
pub async fn resolve_nip05(http: &reqwest::Client, address: &str) -> Result<PublicKey> {
    let (name, domain) = address.trim().split_once('@').context("not a NIP-05 address")?;
    let name = if name.is_empty() { "_".to_string() } else { name.to_lowercase() };
    let url = reqwest::Url::parse_with_params(&format!("https://{domain}/.well-known/nostr.json"), [("name", &name)])?;
    let response = http.get(url).send().await?.error_for_status()?;
    let body = feeds::read_limited(response, 1 << 20).await?;
    let json: serde_json::Value = serde_json::from_slice(&body).context("not a NIP-05 server")?;
    let hex = json["names"][&name].as_str().with_context(|| format!("{domain} doesn't know {name}"))?;
    Ok(PublicKey::from_hex(hex)?)
}

/// Reads public posts; separate from the sync connection. Each fetch
/// connects to the relays it needs and disconnects when done, so connections
/// to the many relays writers use don't pile up.
pub struct Reader {
    /// The user's relays and the index relay.
    index: Vec<String>,
}

impl Reader {
    pub fn new(relays: Vec<String>) -> Self {
        let mut index = relays;
        index.push(INDEX_RELAY.to_string());
        Self { index: unique(index) }
    }

    /// One person's long-form posts: all that can be found the first time
    /// (`newest` is None), then only those since about the newest one.
    pub async fn fetch_author(&self, author: PublicKey, newest: Option<i64>) -> Result<Feed> {
        let session = Session::new(&self.index).await;
        let feed = session.author(author, newest).await;
        session.client.shutdown().await;
        feed
    }

    /// Long-form posts by everyone `user` follows, newer than about the
    /// newest one stored (or from the last couple of months).
    pub async fn fetch_follows(&self, user: PublicKey, newest: Option<i64>) -> Result<Feed> {
        let session = Session::new(&self.index).await;
        let feed = session.follows(user, newest).await;
        session.client.shutdown().await;
        feed
    }
}

/// The connections of one fetch.
struct Session {
    client: Client,
    index: Vec<String>,
}

impl Session {
    async fn new(index: &[String]) -> Self {
        let client = Client::default();
        for relay in index {
            let _ = client.add_relay(relay).await;
        }
        Self { client, index: index.to_vec() }
    }

    /// Events matching `filter` on these relays. Best effort: relays that
    /// can't be reached are skipped.
    async fn query(&self, relays: &[String], filter: Filter) -> Vec<Event> {
        let mut targets = Vec::new();
        for relay in relays {
            if self.client.add_relay(relay).await.is_ok() || self.client.relay(relay).await.is_ok_and(|r| r.is_some()) {
                targets.push((relay.as_str(), vec![filter.clone()]));
            }
        }
        self.client.try_connect().timeout(CONNECT_TIMEOUT).await;
        match self.client.fetch_events(targets).timeout(TIMEOUT).await {
            Ok(events) => events.into_iter().collect(),
            Err(e) => {
                log::warn!("reading long-form posts failed: {e}");
                Vec::new()
            }
        }
    }

    /// Where each author publishes, and their name.
    async fn authors(&self, authors: &[PublicKey]) -> (HashMap<PublicKey, Vec<String>>, HashMap<PublicKey, String>) {
        let mut newest: HashMap<(PublicKey, Kind), Event> = HashMap::new();
        for chunk in authors.chunks(AUTHORS_PER_FILTER) {
            let filter = Filter::new().kinds([Kind::RelayList, Kind::Metadata]).authors(chunk.iter().copied());
            for event in self.query(&self.index, filter).await {
                let key = (event.pubkey, event.kind);
                if newest.get(&key).is_none_or(|old| old.created_at < event.created_at) {
                    newest.insert(key, event);
                }
            }
        }
        let (mut relays, mut names) = (HashMap::new(), HashMap::new());
        for ((author, kind), event) in newest {
            if kind == Kind::RelayList {
                let writes = nip65::extract_relay_list(&event)
                    .filter(|(_, marker)| *marker != Some(RelayMetadata::Read))
                    .map(|(url, _)| url.to_string());
                relays.insert(author, writes.collect());
            } else if let Ok(profile) = Metadata::from_json(&event.content) {
                let name = profile.display_name.filter(|n| !n.trim().is_empty()).or(profile.name);
                if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
                    names.insert(author, name.trim().to_string());
                }
            }
        }
        (relays, names)
    }

    /// The newest version of each long-form post by these authors.
    async fn posts(
        &self,
        authors: &[PublicKey],
        relays: &HashMap<PublicKey, Vec<String>>,
        since: Option<i64>,
    ) -> Vec<Event> {
        // The relays most authors publish to first, plus the user's own.
        let mut coverage: HashMap<&str, usize> = HashMap::new();
        for relay in relays.values().flat_map(|r| r.iter().take(RELAYS_PER_AUTHOR)) {
            *coverage.entry(relay.as_str()).or_default() += 1;
        }
        let mut ranked: Vec<(&str, usize)> = coverage.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut chosen: Vec<String> = ranked.into_iter().take(MAX_CONTENT_RELAYS).map(|(r, _)| r.to_string()).collect();
        chosen.extend(self.index.iter().filter(|r| r.as_str() != INDEX_RELAY).cloned());
        let chosen = unique(chosen);

        let mut events = Vec::new();
        for chunk in authors.chunks(AUTHORS_PER_FILTER) {
            let mut filter = Filter::new().kind(Kind::LongFormTextNote).authors(chunk.iter().copied());
            if let Some(since) = since {
                filter = filter.since(Timestamp::from_secs(since.max(0) as u64));
            }
            events.extend(self.query(&chosen, filter).await);
        }
        newest_versions(events)
    }

    /// Every post `author` has on `relay`, read page by page until the relay
    /// has no older ones. Each relay is read on its own, since relays cap
    /// how many events they return differently.
    async fn history(&self, relay: &str, author: PublicKey) -> Vec<Event> {
        let mut events: Vec<Event> = Vec::new();
        let mut until: Option<Timestamp> = None;
        for _ in 0..MAX_PAGES {
            let mut filter = Filter::new().kind(Kind::LongFormTextNote).author(author).limit(PAGE);
            if let Some(until) = until {
                filter = filter.until(until);
            }
            let page = self.query(&[relay.to_string()], filter).await;
            let Some(oldest) = page.iter().map(|e| e.created_at).min() else { break };
            // `until` includes its own second, so its posts come back again;
            // once there is nothing older, the relay has run out.
            let before = events.len();
            events.extend(page.into_iter().filter(|e| until.is_none_or(|u| e.created_at < u)));
            if events.len() == before {
                break;
            }
            until = Some(oldest);
        }
        events
    }

    async fn author(&self, author: PublicKey, newest: Option<i64>) -> Result<Feed> {
        let (relays, names) = self.authors(&[author]).await;
        let posts = match newest {
            Some(newest) => self.posts(&[author], &relays, Some(update_since(newest))).await,
            None => {
                let sources = history_sources(relays.get(&author).map_or(&[][..], |r| r), &self.index);
                let pages = futures::future::join_all(sources.iter().map(|relay| self.history(relay, author))).await;
                newest_versions(pages.into_iter().flatten().collect())
            }
        };
        let npub = author.to_bech32()?;
        Ok(Feed {
            title: names.get(&author).cloned().unwrap_or_else(|| short(&npub)),
            site: format!("{WEB_VIEWER}/{npub}"),
            items: posts.iter().map(|post| item(post, &names)).collect(),
        })
    }

    async fn follows(&self, user: PublicKey, newest: Option<i64>) -> Result<Feed> {
        let lists = self.query(&self.index, Filter::new().kind(Kind::ContactList).author(user)).await;
        let Some(list) = lists.into_iter().max_by_key(|e| e.created_at) else {
            bail!("no follow list found on your relays");
        };
        let follows: Vec<PublicKey> = list.tags.public_keys().collect::<HashSet<_>>().into_iter().collect();
        let (relays, names) = self.authors(&follows).await;
        let since = newest.map_or(now_ms() / 1000 - FOLLOWS_HISTORY_SECS, update_since);
        let posts = self.posts(&follows, &relays, Some(since)).await;
        let items = posts.iter().map(|post| item(post, &names)).collect();
        Ok(Feed { title: FOLLOWS_TITLE.into(), site: String::new(), items })
    }
}

/// The list without repeats, in its order.
fn unique(mut list: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    list.retain(|item| seen.insert(item.clone()));
    list
}

/// Where to read a writer's whole history: a few of their own relays, and the user's.
fn history_sources(writes: &[String], index: &[String]) -> Vec<String> {
    let own = index.iter().filter(|r| r.as_str() != INDEX_RELAY);
    unique(writes.iter().take(MAX_HISTORY_RELAYS).chain(own).cloned().collect())
}

/// Where updates start, given the newest post stored. A date in the future
/// can't push it past now, or no new post would ever be asked for.
fn update_since(newest: i64) -> i64 {
    newest.min(now_ms() / 1000) - OVERLAP_SECS
}

/// The newest version of each post (edits replace a post by its address).
/// Posts claiming to be from the future are left out.
fn newest_versions(events: Vec<Event>) -> Vec<Event> {
    let latest = Timestamp::from_secs((now_ms() / 1000 + 24 * 3600) as u64);
    let mut newest: HashMap<(PublicKey, String), Event> = HashMap::new();
    for event in events.into_iter().filter(|e| e.created_at <= latest) {
        let key = (event.pubkey, event.tags.identifier().unwrap_or_default());
        if newest.get(&key).is_none_or(|old| old.created_at < event.created_at) {
            newest.insert(key, event);
        }
    }
    newest.into_values().collect()
}

/// "npub1abcd…wxyz"
fn short(npub: &str) -> String {
    format!("{}…{}", &npub[..10], &npub[npub.len() - 4..])
}

/// A post as a feed item, identified by its NIP-23 address so edits replace it.
fn item(post: &Event, names: &HashMap<PublicKey, String>) -> (String, Item) {
    let tag = |name: &str| {
        let tag = post.tags.iter().find(|t| t.as_slice().first().is_some_and(|k| k == name))?;
        tag.as_slice().get(1).cloned()
    };
    let d = post.tags.identifier().unwrap_or_default();
    let coordinate = Coordinate::new(Kind::LongFormTextNote, post.pubkey).identifier(&d);
    let naddr = Nip19Coordinate::new(coordinate.clone(), Vec::<RelayUrl>::new()).to_bech32().unwrap_or_default();
    // Never later than the post itself, so a wrong date can't keep it on top.
    let created = post.created_at.as_secs() as i64;
    let published = tag("published_at").and_then(|t| t.parse().ok()).map_or(created, |p: i64| p.min(created));
    let author =
        names.get(&post.pubkey).cloned().unwrap_or_else(|| short(&post.pubkey.to_bech32().unwrap_or_default()));
    let item = Item {
        title: tag("title").unwrap_or_default(),
        link: format!("{WEB_VIEWER}/{naddr}"),
        author,
        published,
        content: feeds::truncate(html::markdown_to_html(&post.content), feeds::MAX_CONTENT_BYTES),
        t: published * 1000,
    };
    (coordinate.to_string(), item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_people() {
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32().unwrap();
        assert_eq!(parse_author(&npub), Some(keys.public_key()));
        assert_eq!(parse_author(&format!("nostr:{npub}")), Some(keys.public_key()));
        assert_eq!(parse_author(&author_url(&keys.public_key())), Some(keys.public_key()));
        assert_eq!(parse_author("https://example.com/feed"), None);
        assert!(is_nip05("nate@mecca1.net") && is_nip05("_@example.com"));
        assert!(!is_nip05("https://user@example.com/feed") && !is_nip05("example.com"));
    }

    #[tokio::test]
    async fn reads_whole_history_page_by_page() {
        use nostr_sdk::local_relay::{LocalRelayBuilder, RateLimit};
        let relay =
            LocalRelayBuilder::default().rate_limit(RateLimit { max_reqs: 500, notes_per_minute: 10_000 }).build();
        relay.run().await.unwrap();
        let url = relay.url().await.to_string();
        let keys = Keys::generate();
        let publisher = Client::default();
        publisher.add_relay(&url).await.unwrap();
        publisher.connect().await;
        // More posts than fit in a few pages, some sharing a second across a page boundary.
        let total = PAGE * 2 + 30;
        for i in 0..total {
            let created_at = Timestamp::from_secs(1_700_000_000 + (i as u64 / 2) * 60);
            let post = EventBuilder::new(Kind::LongFormTextNote, "text")
                .tag(Tag::identifier(format!("post-{i}")))
                .custom_created_at(created_at)
                .finalize(&keys)
                .unwrap();
            publisher.send_event(&post).await.unwrap();
        }
        publisher.shutdown().await;
        // Through a reader whose only relay is this one.
        let reader = Reader { index: vec![url.clone()] };
        let feed = reader.fetch_author(keys.public_key(), None).await.unwrap();
        assert_eq!(feed.items.len(), total);
        // And it doesn't stay connected afterwards.
        let port: u16 = url.trim_end_matches('/').rsplit(':').next().unwrap().parse().unwrap();
        let connected = || established_connections(port) > 0;
        for _ in 0..30 {
            if !connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!connected());
    }

    /// Open TCP connections to or from a local port (Linux).
    fn established_connections(port: u16) -> usize {
        let port = format!(":{port:04X}");
        ["/proc/net/tcp", "/proc/net/tcp6"]
            .iter()
            .filter_map(|file| std::fs::read_to_string(file).ok())
            .flat_map(|table| table.lines().skip(1).map(String::from).collect::<Vec<_>>())
            .filter(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                fields.len() > 3 && fields[3] == "01" && (fields[1].ends_with(&port) || fields[2].ends_with(&port))
            })
            .count()
    }

    #[test]
    fn history_reads_a_few_of_a_writers_relays() {
        let writes: Vec<String> = (0..500).map(|i| format!("wss://relay{i}.example")).collect();
        let index = vec!["wss://mine.example".to_string(), INDEX_RELAY.to_string(), writes[0].clone()];
        let sources = history_sources(&writes, &index);
        assert_eq!(sources.len(), MAX_HISTORY_RELAYS + 1);
        assert_eq!(sources.last().map(String::as_str), Some("wss://mine.example"));
    }

    #[test]
    fn future_dates_dont_stop_updates() {
        let keys = Keys::generate();
        let now = now_ms() / 1000;
        let post = |created: i64, published: i64| {
            EventBuilder::new(Kind::LongFormTextNote, "text")
                .tags([
                    Tag::identifier(format!("p{created}")),
                    Tag::parse(["published_at", &published.to_string()]).unwrap(),
                ])
                .custom_created_at(Timestamp::from_secs(created as u64))
                .finalize(&keys)
                .unwrap()
        };
        let year_2100 = 4_102_444_800;
        // A wrong published_at is capped at the post's own time.
        assert_eq!(item(&post(now, year_2100), &HashMap::new()).1.published, now);
        // A post dated in the future is ignored.
        assert_eq!(newest_versions(vec![post(now, now), post(year_2100, year_2100)]).len(), 1);
        // And a future date already stored doesn't move updates past now.
        assert!((now - OVERLAP_SECS..=now + 1 - OVERLAP_SECS).contains(&update_since(year_2100)));
    }

    #[test]
    fn posts_become_items() {
        let keys = Keys::generate();
        let post = EventBuilder::new(Kind::LongFormTextNote, "# Hi\n\nHello **there**.")
            .tags([
                Tag::identifier("hello"),
                Tag::parse(["title", "Hello world"]).unwrap(),
                Tag::parse(["published_at", "1700000000"]).unwrap(),
            ])
            .finalize(&keys)
            .unwrap();
        let names = HashMap::from([(keys.public_key(), "Alice".to_string())]);
        let (guid, item) = item(&post, &names);
        assert_eq!(guid, format!("30023:{}:hello", keys.public_key().to_hex()));
        assert_eq!((item.title.as_str(), item.author.as_str(), item.published), ("Hello world", "Alice", 1700000000));
        assert!(item.link.starts_with("https://ditto.pub/naddr1"));
        let link = nostr_link(&item.link).unwrap();
        assert_eq!(link, format!("nostr:{}", &item.link["https://ditto.pub/".len()..]));
        assert_eq!(nostr_link("https://blog.example/naddr1-post"), None);
        assert_eq!(nostr_link("https://ditto.pub/about"), None);
        assert_eq!(nostr_link("https://ditto.pub/note1abc?x=1"), None);
        assert_eq!(nostr_link("https://ditto.pub/note1abc --flag"), None);
        assert_eq!(nostr_link("https://ditto.pub/note1ABC"), None);
        assert_eq!(html::plain_text(&item.content), "Hi Hello there.");
    }
}
