//! Websites on Nostr (nsites, NIP-5A) as feeds.
//!
//! An nsite is a static website whose list of files is a Nostr event: a
//! *site manifest* with a `path` tag (`["path", "/index.html", "<sha256>"]`)
//! per file. Someone has one root site (kind 15128) and any number of named
//! ones (kind 35128, named by their `d` tag). Older sites have an event per
//! file instead (kind 34128: the path in `d`, the hash in `x`), which is read
//! too for root sites. The files themselves are on Blossom servers, by their
//! SHA-256 hash (`GET https://server/<sha256>`): those the manifest lists in
//! `server` tags, then those of the owner's server list (kind 10063).
//!
//! As a feed (`nsite:npub1…` for a root site, `nsite:npub1…/name` for a named
//! one), a site shows its updates the best way it can:
//!
//! 1. If it has a feed of its own (linked from its home page, or at a usual
//!    place like `/feed.xml`), that feed, as with any website.
//! 2. Otherwise its pages, each version of a page an article of its own, so a
//!    new or changed page shows up as a new article. A page's date is when the
//!    site (or, on older sites, the page) was published.
//!
//! Every file is checked against its hash, so a server can't change a page.
//! Links point at a gateway that shows nsites on the web (set on each device).
//! Relays and Blossom servers let browser pages read them, so the web version
//! fetches nsites itself.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::future::Either;
use nostr_sdk::prelude::*;
use reqwest::Url;

use crate::clock;
use crate::db::Db;
use crate::docs::{self, Item};
use crate::feeds::{self, Feed};
use crate::html;
use crate::longform;
use crate::proxy::Route;

const SCHEME: &str = "nsite:";
/// Site manifests: someone's root site, and their named sites.
const ROOT_SITE: u16 = 15128;
const NAMED_SITE: u16 = 35128;
/// An older root site's files, one event each.
const LEGACY_FILE: u16 = 34128;
/// Someone's Blossom servers (BUD-03).
const SERVER_LIST: u16 = 10063;

pub const DEFAULT_GATEWAY: &str = "nsite.lol";
const GATEWAY_KEY: &str = "nsite_gateway";

/// Pages read at most in one fetch (the newest ones), so a big site isn't
/// downloaded whole when it is first added; the others are read by the next fetches.
const PAGES_PER_FETCH: usize = 20;
/// No more pages are started after reading pages for this long in one fetch.
const PAGES_TIME: Duration = Duration::from_secs(60);
/// One download from one server, at most (the browser has no timeout of its own).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// A whole fetch of a site, at most, so a slow site can't hold up a sync.
const FETCH_TIMEOUT: Duration = Duration::from_secs(180);
/// Larger pages aren't read (their text is cut down to
/// [`feeds::MAX_CONTENT_BYTES`] anyway); feeds may be as large as other feeds.
const MAX_PAGE_BYTES: usize = 2 << 20;
/// Blossom servers asked for a file at most.
const MAX_SERVERS: usize = 8;
/// Files a site may list; more are ignored.
const MAX_PATHS: usize = 10_000;
/// Where sites usually keep their feed, if their home page doesn't link to it.
const FEED_PATHS: &[&str] = &[
    "/feed.xml",
    "/rss.xml",
    "/atom.xml",
    "/index.xml",
    "/feed.json",
    "/feed/index.xml",
    "/rss/index.xml",
    "/blog/feed.xml",
    "/blog/rss.xml",
    "/blog/atom.xml",
    "/blog/index.xml",
    "/posts/index.xml",
];
/// Files shown as pages.
const PAGE_ENDINGS: &[&str] = &[".html", ".htm", ".md", ".markdown"];

pub fn is_nsite(url: &str) -> bool {
    url.starts_with(SCHEME)
}

/// One site: someone's root site, or one of their named sites.
#[derive(Clone, Debug, PartialEq)]
pub struct Site {
    pub author: PublicKey,
    pub name: Option<String>,
}

impl Site {
    /// The site a feed address (`nsite:npub1…` or `nsite:npub1…/name`) refers to.
    pub fn parse_url(url: &str) -> Option<Site> {
        let rest = url.strip_prefix(SCHEME)?;
        let (npub, name) = match rest.split_once('/') {
            Some((npub, name)) => (npub, Some(name)),
            None => (rest, None),
        };
        if !npub.starts_with("npub1") || name.is_some_and(|name| !valid_name(name)) {
            return None;
        }
        Some(Site { author: PublicKey::parse(npub).ok()?, name: name.map(String::from) })
    }

    /// The site typed or pasted: its feed address (`nsite:` with an npub or
    /// nprofile), or its address on a gateway (`https://npub1….gateway/`, or
    /// the encoded form gateways use for named sites).
    pub fn parse_input(input: &str) -> Option<Site> {
        let input = input.trim();
        if let Some(rest) = input.strip_prefix(SCHEME) {
            let (who, name) = match rest.split_once('/') {
                Some((who, name)) => (who, Some(name.trim_end_matches('/'))),
                None => (rest, None),
            };
            let author = longform::parse_author(who)?;
            let name = name.filter(|n| !n.is_empty());
            return name.is_none_or(valid_name).then(|| Site { author, name: name.map(String::from) });
        }
        let url = Url::parse(input).ok().filter(|u| matches!(u.scheme(), "http" | "https"))?;
        let label = url.host_str()?.split('.').next()?.to_ascii_lowercase();
        // A gateway's address has the site in front of the gateway's own (with a dot in it).
        if url.host_str()?.matches('.').count() < 2 {
            return None;
        }
        if label.starts_with("npub1") {
            return Some(Site { author: PublicKey::parse(&label).ok()?, name: None });
        }
        if label.len() > 50 && label.len() <= 63 {
            let (key, name) = label.split_at(50);
            let author = PublicKey::from_slice(&from_base36(key)?).ok()?;
            return valid_name(name).then(|| Site { author, name: Some(name.to_string()) });
        }
        None
    }

    /// Its feed address.
    pub fn feed_url(&self) -> String {
        let npub = self.author.to_bech32().expect("bech32 encoding can't fail");
        match &self.name {
            Some(name) => format!("{SCHEME}{npub}/{name}"),
            None => format!("{SCHEME}{npub}"),
        }
    }

    /// Its address on `gateway`, without a slash at the end.
    pub fn web_url(&self, gateway: &str) -> String {
        match &self.name {
            Some(name) => format!("https://{}{name}.{gateway}", base36(&self.author.to_bytes())),
            None => format!("https://{}.{gateway}", self.author.to_bech32().expect("bech32 encoding can't fail")),
        }
    }
}

/// Whether a named site's name (its `d` tag) is one NIP-5A allows: 1 to 13
/// lowercase letters, digits and dashes, not ending in a dash (it has to fit
/// in a DNS label next to the encoded key).
fn valid_name(name: &str) -> bool {
    (1..=13).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.ends_with('-')
}

const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// A 32-byte key in base 36 (lowercase), always 50 characters, as gateways
/// name named sites.
fn base36(bytes: &[u8; 32]) -> String {
    let mut number = bytes.to_vec();
    let mut digits = Vec::with_capacity(50);
    for _ in 0..50 {
        let mut remainder = 0u32;
        for byte in number.iter_mut() {
            let current = remainder * 256 + u32::from(*byte);
            *byte = (current / 36) as u8;
            remainder = current % 36;
        }
        digits.push(BASE36[remainder as usize]);
    }
    digits.reverse();
    String::from_utf8(digits).expect("base 36 digits are ASCII")
}

/// The 32-byte key 50 base-36 characters stand for, if they do.
fn from_base36(text: &str) -> Option<[u8; 32]> {
    if text.len() != 50 {
        return None;
    }
    let mut number = [0u8; 32];
    for c in text.bytes() {
        let mut carry = BASE36.iter().position(|&d| d == c)? as u32;
        for byte in number.iter_mut().rev() {
            let current = u32::from(*byte) * 36 + carry;
            *byte = (current & 0xff) as u8;
            carry = current >> 8;
        }
        if carry != 0 {
            return None;
        }
    }
    Some(number)
}

/// The gateway this device shows nsites' pages on (a host name).
pub fn gateway(db: &Db) -> String {
    db.meta(GATEWAY_KEY).ok().flatten().filter(|g| !g.is_empty()).unwrap_or_else(|| DEFAULT_GATEWAY.to_string())
}

pub fn set_gateway(db: &Db, gateway: &str) -> Result<()> {
    if gateway == DEFAULT_GATEWAY { db.delete_meta(GATEWAY_KEY) } else { db.set_meta(GATEWAY_KEY, gateway) }
}

/// A gateway as typed (`nsite.example`, or an address on it), as a host
/// name; empty means the default.
pub fn parse_gateway(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return Some(DEFAULT_GATEWAY.to_string());
    }
    let url = if input.contains("://") { input.to_string() } else { format!("https://{input}") };
    let url = Url::parse(&url).ok()?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let valid = host.contains('.') && !feeds::is_local_address(&url) && host.parse::<std::net::IpAddr>().is_err();
    valid.then_some(host)
}

/// What reading a site found.
pub struct Fetched {
    /// `None` if the site hasn't changed since the version `seen` (see [`fetch`]).
    pub feed: Option<Feed>,
    /// The site's version, to keep (as the feed's ETag) once the feed is
    /// stored and given to the next fetch as `seen`. `None` while pages are
    /// still to be read, so the next fetch reads them.
    pub version: Option<String>,
}

/// A site's files: path -> (SHA-256 in hex, when published in seconds).
type Files = HashMap<String, (String, i64)>;

/// The events that say what's on a site: its manifest, plus its owner's
/// Blossom server list if `servers` (to read it), or else one of the older
/// events per file (to tell whether it exists: [`read`] reads those itself,
/// page by page).
fn filters(site: &Site, servers: bool) -> Vec<Filter> {
    let mut filters = Vec::new();
    if servers {
        filters.push(Filter::new().kind(Kind::from(SERVER_LIST)).author(site.author));
    }
    filters.push(match &site.name {
        Some(name) => Filter::new().kind(Kind::from(NAMED_SITE)).author(site.author).identifier(name.clone()),
        None => Filter::new().kind(Kind::from(ROOT_SITE)).author(site.author),
    });
    if site.name.is_none() && !servers {
        filters.push(Filter::new().kind(Kind::from(LEGACY_FILE)).author(site.author).limit(1));
    }
    filters
}

/// Whether the site has been published (as far as its owner's relays and the
/// user's say): tells a gateway's address from another site on an npub subdomain.
pub async fn exists(reader: &longform::Reader, site: &Site) -> bool {
    !reader.events_by(site.author, filters(site, false)).await.0.is_empty()
}

/// The site at the feed address `url`, unless it's still the version `seen`
/// (the one this device last stored). Gives up after [`FETCH_TIMEOUT`].
/// Its files are downloaded with `http`, which goes `route`'s way (directly,
/// or through Tor when everything goes through Tor).
#[allow(clippy::too_many_arguments)]
pub async fn fetch(
    reader: &longform::Reader,
    http: &reqwest::Client,
    route: Route,
    db: &Db,
    url: &str,
    gateway: &str,
    seen: Option<&str>,
) -> Result<Fetched> {
    within(FETCH_TIMEOUT, read(reader, http, route, db, url, gateway, seen))
        .await
        .context("reading the site took too long")?
}

/// `work`, unless it takes longer than `limit` (natively and in the browser).
async fn within<T>(limit: Duration, work: impl Future<Output = T>) -> Result<T> {
    let work = std::pin::pin!(work);
    let timer = std::pin::pin!(clock::sleep(limit));
    match futures::future::select(work, timer).await {
        Either::Left((done, _)) => Ok(done),
        Either::Right(_) => Err(anyhow!("it took too long")),
    }
}

async fn read(
    reader: &longform::Reader,
    http: &reqwest::Client,
    route: Route,
    db: &Db,
    url: &str,
    gateway: &str,
    seen: Option<&str>,
) -> Result<Fetched> {
    let site = Site::parse_url(url).context("not an nsite address")?;
    let (events, name) = reader.events_by(site.author, filters(&site, true)).await;
    let newest = |kind: u16| events.iter().filter(|e| e.kind == Kind::from(kind)).max_by_key(|e| e.created_at);

    let manifest = newest(if site.name.is_some() { NAMED_SITE } else { ROOT_SITE });
    let (files, mut servers, site_title) = match manifest {
        Some(manifest) => {
            let published = not_ahead(manifest.created_at.as_secs() as i64);
            let files = manifest_files(manifest.tags.iter().map(|t| t.as_slice()), published);
            (files, tag_values(manifest, "server"), tag_values(manifest, "title").into_iter().next())
        }
        // An older site: an event per file, read page by page (there can be
        // more than relays send at once).
        None if site.name.is_none() => {
            let events = reader.every_event_by(site.author, Kind::from(LEGACY_FILE)).await;
            (legacy_files(events.iter()), Vec::new(), None)
        }
        None => (Files::new(), Vec::new(), None),
    };
    let npub = site.author.to_bech32()?;
    anyhow::ensure!(
        !files.is_empty(),
        "no nsite found: {} hasn't published one on their relays",
        longform::short(&npub)
    );
    if let Some(list) = newest(SERVER_LIST) {
        servers.extend(tag_values(list, "server"));
    }
    let servers = blossom_servers(servers, route);

    // Unchanged since last time (the same files, servers and gateway): nothing to read.
    let mut lines: Vec<String> = files.iter().map(|(path, (hash, t))| format!("{hash} {t} {path}\n")).collect();
    lines.sort();
    let version = docs::sha256_hex(format!("{gateway}\n{}\n{}", servers.join(" "), lines.concat()));
    let stored = db.newest_published(url)?.is_some();
    if stored && seen == Some(version.as_str()) {
        return Ok(Fetched { feed: None, version: Some(version) });
    }

    let base = site.web_url(gateway);
    let author = name.clone().unwrap_or_else(|| longform::short(&npub));
    let title = site_title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| match (&name, &site.name) {
        (Some(who), Some(site)) => format!("{who}: {site}"),
        (Some(who), None) => who.clone(),
        (None, Some(site)) => format!("{}: {site}", longform::short(&npub)),
        (None, None) => longform::short(&npub),
    });
    let title = feeds::truncate(title, feeds::MAX_TITLE_BYTES);
    // Through Tor, names are looked up by Tor (and a Tor exit can't reach
    // this network): looking them up here would only give them away.
    let servers = if route == Route::Tor { servers } else { outside_local_network(servers).await };
    let blobs = Blobs { http, servers: &servers, failed: Mutex::default() };

    // The site's own feed, if it has one.
    if let Some(path) = own_feed(&blobs, &files, &base).await? {
        let body = blobs.get(&files[&path].0, feeds::MAX_FEED_BYTES).await?;
        let mut feed = feeds::parse(&body, &format!("{base}{path}"))?;
        if feed.title.trim().is_empty() {
            feed.title = title;
        }
        feed.site = format!("{base}/");
        return Ok(Fetched { feed: Some(feed), version: Some(version) });
    }

    // Otherwise its pages, newest first. Every page still on the site is
    // passed on (so it counts as still in the feed), those stored as they are.
    let mut pages: Vec<(&String, &(String, i64))> = files.iter().filter(|(path, _)| is_page(path)).collect();
    pages.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
    let mut items = Vec::new();
    let (mut read, started) = (0, clock::now_ms());
    // Whether every page is stored; if not, the next fetch reads the rest.
    let mut complete = true;
    for (path, (hash, published)) in pages {
        // Each version of a page is an article of its own, so a changed page shows up as new.
        let guid = format!("{path} {hash}");
        if let Some(stored) = db.stored_item(&docs::item_key(url, &guid))? {
            items.push((guid, stored));
            continue;
        }
        let out_of_time = clock::now_ms() - started > PAGES_TIME.as_millis() as i64;
        if read >= PAGES_PER_FETCH || out_of_time {
            complete = false;
            continue;
        }
        read += 1;
        let link = page_link(&base, path);
        let (page_title, content) = match blobs.get(hash, MAX_PAGE_BYTES).await {
            Ok(body) => page(path, &String::from_utf8_lossy(&body), &link),
            // Too large to read, this time or any other: listed with its link.
            Err(e) if e.chain().any(|cause| cause.is::<feeds::TooLarge>()) => (None, String::new()),
            // A page that can't be read now is left out, and tried again next time.
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let item = Item {
            title: feeds::truncate(page_title.unwrap_or_else(|| path.clone()), feeds::MAX_TITLE_BYTES),
            link,
            author: author.clone(),
            published: *published,
            content: feeds::truncate(content, feeds::MAX_CONTENT_BYTES),
            t: published * 1000,
        };
        items.push((guid, item));
    }
    let version = complete.then_some(version);
    Ok(Fetched { feed: Some(Feed { title, site: format!("{base}/"), items }), version })
}

/// The files a manifest lists, all published at `published`.
fn manifest_files<'a>(tags: impl Iterator<Item = &'a [String]>, published: i64) -> Files {
    let mut files = Files::new();
    for tag in tags {
        if let [kind, path, hash, ..] = tag
            && kind == "path"
            && valid_path(path)
            && valid_hash(hash)
            && files.len() < MAX_PATHS
        {
            files.insert(path.clone(), (hash.to_ascii_lowercase(), published));
        }
    }
    files
}

/// The files of an older site, from its newest event for each.
fn legacy_files<'a>(events: impl Iterator<Item = &'a Event>) -> Files {
    let mut newest: HashMap<String, &Event> = HashMap::new();
    for event in events {
        let Some(path) = event.tags.identifier().filter(|p| valid_path(p)) else { continue };
        if newest.get(&path).is_none_or(|old| old.created_at < event.created_at) {
            newest.insert(path, event);
        }
    }
    let mut files = Files::new();
    for (path, event) in newest.into_iter().take(MAX_PATHS) {
        if let Some(hash) = tag_values(event, "x").into_iter().find(|h| valid_hash(h)) {
            files.insert(path, (hash.to_ascii_lowercase(), not_ahead(event.created_at.as_secs() as i64)));
        }
    }
    files
}

/// A date (seconds) no later than now: a site dated ahead would otherwise
/// stay on top of every list.
fn not_ahead(seconds: i64) -> i64 {
    seconds.min(clock::now_ms() / 1000)
}

/// The second value of each of the event's tags called `name`.
fn tag_values(event: &Event, name: &str) -> Vec<String> {
    let values = event.tags.iter().map(|t| t.as_slice()).filter(|t| t.first().is_some_and(|k| k == name));
    values.filter_map(|t| t.get(1).cloned()).collect()
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/') && path.len() <= 1024 && !path.contains(['\n', '\r', '\0'])
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The Blossom servers to ask, in order, without repeats: only encrypted
/// connections to servers out on the internet (otherwise a site could point
/// Feedstr at this device or its local network).
fn blossom_servers(listed: Vec<String>, route: Route) -> Vec<String> {
    let mut servers: Vec<String> = Vec::new();
    for server in listed {
        let Ok(url) = Url::parse(server.trim()) else { continue };
        // Not servers only another network reaches (.onion, unless files go
        // through Tor; .i2p): they would only fail, after giving their name
        // to the DNS server.
        let unreachable = !feeds::reachable(route, &url);
        if url.scheme() != "https" || feeds::is_local_address(&url) || unreachable || url.query().is_some() {
            continue;
        }
        let server = url.as_str().trim_end_matches('/').to_string();
        if !servers.contains(&server) && servers.len() < MAX_SERVERS {
            servers.push(server);
        }
    }
    servers
}

fn is_page(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower != "/404.html" && PAGE_ENDINGS.iter().any(|end| lower.ends_with(end))
}

/// Where a page is on the gateway: a folder's `index.html` is the folder.
/// Made by `Url`, so whatever a site's manifest calls a page, the link is a
/// proper one (spaces and quotes escaped) and stays on the site's host.
fn page_link(base: &str, path: &str) -> String {
    let path = path.strip_suffix("index.html").unwrap_or(path);
    let link = Url::parse(&format!("{base}/"))
        .ok()
        .and_then(|site| site.join(&format!(".{path}")).ok().filter(|link| link.host_str() == site.host_str()))
        .map_or_else(|| format!("{base}/"), String::from);
    feeds::truncate(link, feeds::MAX_LINK_BYTES)
}

/// A page's title (if it has one) and its text as HTML for the reader, with
/// its relative links leading where they do on the site (at `link`).
fn page(path: &str, text: &str, link: &str) -> (Option<String>, String) {
    let lower = path.to_ascii_lowercase();
    // A link made by `Url` has no quotes or angle brackets to escape.
    let base = format!("<base href=\"{link}\">");
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        let title = text.lines().find_map(|l| l.strip_prefix("# ")).map(|t| t.trim().to_string());
        return (title.filter(|t| !t.is_empty()), base + &html::markdown_to_html(text));
    }
    (html_title(text), base + text)
}

/// The text of an HTML page's `<title>`.
fn html_title(page: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let start = open + lower[open..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let title = html::decode_entities(page.get(start..end)?.trim());
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

/// Where the site keeps its own feed, if it has one: the feed its home page
/// links to (if that's on the site), or one at a usual place. A home page
/// that can't be read is an error, not a site without a feed: otherwise one
/// failed download would turn the site's feed into its pages until it changes.
async fn own_feed(blobs: &Blobs<'_>, files: &Files, base: &str) -> Result<Option<String>> {
    if let Some((hash, _)) = files.get("/index.html") {
        let home = match blobs.get(hash, MAX_PAGE_BYTES).await {
            Ok(home) => Some(home),
            // Too large to read ever: as if it linked to no feed.
            Err(e) if e.chain().any(|cause| cause.is::<feeds::TooLarge>()) => None,
            Err(e) => return Err(e.context("couldn't read the site's home page")),
        };
        if let Some(home) = home
            && let Some(href) = feeds::find_feed_link(&String::from_utf8_lossy(&home))
            && let Ok(base_url) = Url::parse(&format!("{base}/"))
            && let Ok(feed) = base_url.join(&href)
            && feed.host_str() == base_url.host_str()
            && files.contains_key(feed.path())
        {
            return Ok(Some(feed.path().to_string()));
        }
    }
    Ok(FEED_PATHS.iter().find(|path| files.contains_key(**path)).map(|path| path.to_string()))
}

/// The servers whose names don't lead to this device or its local network
/// (natively: looked up here). A site's server list could otherwise have
/// Feedstr send requests into the local network under a name that looks
/// like any other. (Requests that do no more than the site could do itself,
/// as the files have to match their hashes, but still.) Servers whose name
/// can't be found are left out too.
#[cfg(not(target_arch = "wasm32"))]
async fn outside_local_network(servers: Vec<String>) -> Vec<String> {
    let mut kept = Vec::new();
    for server in servers {
        let Ok(url) = Url::parse(&server) else { continue };
        let Some(host) = url.host_str().map(|h| h.trim_start_matches('[').trim_end_matches(']').to_string()) else {
            continue;
        };
        let lookup =
            within(DOWNLOAD_TIMEOUT, tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443))));
        if let Ok(Ok(addresses)) = lookup.await {
            let addresses: Vec<std::net::SocketAddr> = addresses.collect();
            if !addresses.is_empty() && !addresses.iter().any(|a| feeds::is_local_ip(a.ip())) {
                kept.push(server);
            }
        }
    }
    kept
}

/// In the browser, which decides where pages may send requests.
#[cfg(target_arch = "wasm32")]
async fn outside_local_network(servers: Vec<String>) -> Vec<String> {
    servers
}

/// Downloads files from a site's Blossom servers.
struct Blobs<'a> {
    http: &'a reqwest::Client,
    servers: &'a [String],
    /// Servers that didn't answer during this fetch: not asked again.
    failed: Mutex<HashSet<String>>,
}

impl Blobs<'_> {
    /// The file with this hash, from the first server that has it (and sends
    /// the file it should: anything else is ignored).
    async fn get(&self, hash: &str, max: usize) -> Result<Vec<u8>> {
        let mut last = None;
        for server in self.servers {
            if self.failed.lock().unwrap().contains(server) {
                continue;
            }
            // The error, and whether the server is broken (rather than just
            // without the file): answering "not found" is fine, not answering isn't.
            let attempt = async {
                let response = self.http.get(format!("{server}/{hash}")).send().await;
                let response = response.map_err(|e| (anyhow::Error::from(e), true))?;
                let response = response.error_for_status().map_err(|e| (anyhow::Error::from(e), false))?;
                // Too large is the file, not the server.
                let body = feeds::read_limited(response, max).await.map_err(|e| {
                    let too_large = e.is::<feeds::TooLarge>();
                    (e, !too_large)
                })?;
                if docs::sha256_hex(&body) != hash {
                    return Err((anyhow!("{server} sent a different file"), false));
                }
                Ok::<_, (anyhow::Error, bool)>(body)
            };
            let result = match within(DOWNLOAD_TIMEOUT, attempt).await {
                Ok(done) => done,
                Err(e) => Err((e, true)),
            };
            match result {
                Ok(body) => return Ok(body),
                // The same file (by its hash) is as large on every server.
                Err((e, _)) if e.is::<feeds::TooLarge>() => return Err(e),
                Err((e, broken)) => {
                    if broken {
                        self.failed.lock().unwrap().insert(server.clone());
                    }
                    last = Some(e.context(format!("from {server}")));
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("the site doesn't say where its files are")))
            .context("couldn't download the site's files")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn base36_keys_round_trip_at_fifty_characters() {
        for bytes in [[0u8; 32], [0xff; 32], core::array::from_fn(|i| i as u8 * 7)] {
            let text = base36(&bytes);
            assert_eq!(text.len(), 50);
            assert_eq!(from_base36(&text), Some(bytes));
        }
        // Larger than 32 bytes can hold, or not base 36.
        assert_eq!(from_base36(&"z".repeat(50)), None);
        assert_eq!(from_base36(&"A".repeat(50)), None);
    }

    #[test]
    fn site_addresses() {
        let keys = Keys::generate();
        let npub = keys.public_key().to_bech32().unwrap();
        let root = Site { author: keys.public_key(), name: None };
        let blog = Site { author: keys.public_key(), name: Some("blog".into()) };
        assert_eq!(Site::parse_url(&root.feed_url()), Some(root.clone()));
        assert_eq!(Site::parse_url(&blog.feed_url()), Some(blog.clone()));
        assert_eq!(Site::parse_input(&format!("nsite:{npub}")), Some(root.clone()));
        assert_eq!(Site::parse_input(&format!(" nsite:nostr:{npub}/blog/ ")), Some(blog.clone()));
        // Their addresses on a gateway lead back to them.
        assert_eq!(Site::parse_input(&format!("{}/posts/", root.web_url("gw.example"))), Some(root.clone()));
        assert_eq!(Site::parse_input(&blog.web_url("gw.example")), Some(blog.clone()));
        // Names NIP-5A doesn't allow, and addresses that aren't sites.
        assert_eq!(Site::parse_input(&format!("nsite:{npub}/Blog")), None);
        assert_eq!(Site::parse_input(&format!("nsite:{npub}/blog-")), None);
        assert_eq!(Site::parse_input(&format!("nsite:{npub}/fourteen-chars")), None);
        assert_eq!(Site::parse_input("https://example.com/feed.xml"), None);
        assert_eq!(Site::parse_input(&format!("https://{npub}.com/")), None);
        assert_eq!(Site::parse_input(&npub), None);
    }

    #[test]
    fn manifests_list_valid_files() {
        let hash = "a".repeat(64);
        let tags = [
            tag(&["path", "/index.html", hash.as_str()]),
            tag(&["path", "no-slash.html", hash.as_str()]),
            tag(&["path", "/short.html", "abc"]),
            tag(&["server", "https://blossom.example"]),
            tag(&["path", "/Post.MD", hash.to_uppercase().as_str()]),
        ];
        let files = manifest_files(tags.iter().map(|t| t.as_slice()), 5);
        assert_eq!(files.len(), 2);
        assert_eq!(files["/Post.MD"], (hash.clone(), 5));
        assert!(is_page("/Post.MD") && is_page("/index.html") && !is_page("/404.html") && !is_page("/style.css"));
    }

    #[test]
    fn only_public_encrypted_blossom_servers() {
        let listed = [
            "https://blossom.example/",
            "https://blossom.example",
            "http://plain.example",
            "https://192.168.1.10",
            "https://localhost:3000",
            "not a url",
            "https://cdn.example/files",
            "https://hidden.onion",
        ];
        let servers = blossom_servers(listed.iter().map(|s| s.to_string()).collect(), Route::Direct);
        assert_eq!(servers, ["https://blossom.example", "https://cdn.example/files"]);
        // Through Tor, a .onion server can be reached.
        let servers = blossom_servers(listed.iter().map(|s| s.to_string()).collect(), Route::Tor);
        assert_eq!(servers, ["https://blossom.example", "https://cdn.example/files", "https://hidden.onion"]);
    }

    #[test]
    fn pages_get_titles_and_links() {
        let html = "<html><head><TITLE>\n  Hello &amp; welcome </title></head><body>Hi</body></html>";
        let link = "https://x.gw/a.html";
        assert_eq!(page("/a.html", html, link).0.as_deref(), Some("Hello & welcome"));
        assert_eq!(page("/a.html", "<p>no title</p>", link).0, None);
        let (title, content) = page("/notes/b.md", "intro\n# First post\n\n[home](/)", "https://x.gw/notes/b.md");
        assert_eq!(title.as_deref(), Some("First post"));
        assert!(content.starts_with("<base href=\"https://x.gw/notes/b.md\">") && content.contains("<h1>"));
        // Relative links lead to the site on the gateway.
        let blocks = html::to_blocks(&content);
        let home = blocks.iter().flat_map(|b| &b.spans).find(|s| s.text == "home").unwrap();
        assert_eq!(home.link.as_deref(), Some("https://x.gw/"));
        assert_eq!(page_link("https://x.gw", "/blog/index.html"), "https://x.gw/blog/");
        assert_eq!(page_link("https://x.gw", "/blog/post.html"), "https://x.gw/blog/post.html");
        // Odd names become proper links on the same site.
        assert_eq!(page_link("https://x.gw", "/a b\"c.html"), "https://x.gw/a%20b%22c.html");
        assert_eq!(page_link("https://x.gw", "//evil.example/x.html"), "https://x.gw//evil.example/x.html");
        assert_eq!(page_link("https://x.gw", "/../../up.html"), "https://x.gw/up.html");
    }

    #[test]
    fn gateways_as_typed() {
        assert_eq!(parse_gateway("").as_deref(), Some(DEFAULT_GATEWAY));
        assert_eq!(parse_gateway("https://Gateway.Example/").as_deref(), Some("gateway.example"));
        assert_eq!(parse_gateway("gateway.example").as_deref(), Some("gateway.example"));
        assert_eq!(parse_gateway("localhost"), None);
        assert_eq!(parse_gateway("192.168.1.2"), None);
    }
}
