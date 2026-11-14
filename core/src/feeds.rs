//! Downloading and parsing RSS, Atom and JSON feeds.

use anyhow::{Context, Result, bail};
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{StatusCode, Url};

use crate::db::{FetchInfo, now_ms};
use crate::docs::Item;

/// Longer item content is cut off, so a single item can't crowd out a feed's sync cache.
pub const MAX_CONTENT_BYTES: usize = 40_000;
/// Longer titles and author names are cut off too, and no more than
/// [`MAX_ITEMS`] items are taken from one download, so a broken or hostile
/// feed can't bloat the database or bog down the article list.
pub const MAX_TITLE_BYTES: usize = 1_000;
pub const MAX_AUTHOR_BYTES: usize = 300;
/// Longer links are left out: half a link leads nowhere.
pub const MAX_LINK_BYTES: usize = 4_000;
/// Longer item IDs are replaced by their hash.
const MAX_GUID_BYTES: usize = 1_000;
pub const MAX_ITEMS: usize = 5_000;
const DAY_SECS: i64 = 24 * 3600;
/// Larger downloads are refused, so a broken or hostile server can't fill memory.
pub const MAX_FEED_BYTES: usize = 20 << 20;

pub struct Feed {
    pub title: String,
    pub site: String,
    /// (guid, item)
    pub items: Vec<(String, Item)>,
}

pub enum Fetched {
    NotModified,
    Feed(Feed, FetchInfo),
}

#[cfg(not(target_arch = "wasm32"))]
pub fn http_client() -> reqwest::Client {
    // HTTPS here and Nostr relays (nostr-sdk) share rustls with its ring
    // backend, which has to be chosen before the first connection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("too many redirects")
        } else if attempt.previous().last().is_some_and(|from| !may_lead_to(from, attempt.url())) {
            attempt.error("redirected to an address on this device or its network")
        } else {
            attempt.follow()
        }
    });
    let builder = reqwest::Client::builder()
        .user_agent(concat!("Feedstr/", env!("CARGO_PKG_VERSION")))
        .redirect(redirects)
        .timeout(std::time::Duration::from_secs(30));
    #[cfg(target_os = "android")]
    let builder = builder.tls_backend_preconfigured(android_tls());
    builder.build().expect("valid HTTP client configuration")
}

/// In the browser, requests go through its fetch(), which handles TLS and timeouts.
#[cfg(target_arch = "wasm32")]
pub fn http_client() -> reqwest::Client {
    reqwest::Client::new()
}

/// TLS checked against Mozilla's root certificates (see `core/Cargo.toml` for why).
#[cfg(target_os = "android")]
fn android_tls() -> rustls::ClientConfig {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth()
}

/// Whether a web or relay address is on this device or its local network
/// (`localhost`, `192.168.…`, `10.…`, a name without a dot, and the like),
/// going by the address as written: names aren't looked up.
pub fn is_local_address(url: &Url) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    let local_v4 = |ip: Ipv4Addr| {
        let carrier_nat = ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64;
        ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast()
            || carrier_nat
    };
    let Some(host) = url.host_str() else { return false };
    match host.trim_start_matches('[').trim_end_matches(']').parse() {
        Ok(IpAddr::V4(ip)) => local_v4(ip),
        Ok(IpAddr::V6(ip)) => match ip.to_ipv4_mapped() {
            Some(ip) => local_v4(ip),
            // Loopback, unspecified, unique local (fc00::/7) and link-local (fe80::/10).
            None => {
                let first = ip.segments()[0];
                ip.is_loopback() || ip.is_unspecified() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
            }
        },
        Err(_) => {
            let name = host.trim_end_matches('.').to_ascii_lowercase();
            let local_names = [".localhost", ".local", ".lan", ".internal", ".home.arpa"];
            name == "localhost" || !name.contains('.') || local_names.iter().any(|end| name.ends_with(end))
        }
    }
}

/// Whether a page at `from` may send Feedstr on to `to` (by a redirect, or a
/// link to its feed): only to web addresses, and not from out on the internet
/// to this device or its local network, where a request can do things (to a
/// router's settings page, say) that the site couldn't do itself. A feed that
/// is on the local network to begin with is fine.
pub fn may_lead_to(from: &Url, to: &Url) -> bool {
    matches!(to.scheme(), "http" | "https") && (is_local_address(from) || !is_local_address(to))
}

pub async fn fetch(http: &reqwest::Client, url: &str, cache: &FetchInfo) -> Result<Fetched> {
    let mut request = http.get(url);
    if let Some(etag) = &cache.etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    if let Some(modified) = &cache.last_modified {
        request = request.header(IF_MODIFIED_SINCE, modified);
    }
    let response = request.send().await?;
    if response.status() == StatusCode::NOT_MODIFIED {
        return Ok(Fetched::NotModified);
    }
    let response = response.error_for_status()?;
    let header = |name| response.headers().get(name).and_then(|v| v.to_str().ok()).map(String::from);
    let info = FetchInfo { etag: header(ETAG), last_modified: header(LAST_MODIFIED) };
    // Relative links are relative to where the feed ended up after redirects.
    let base = response.url().to_string();
    let body = read_limited(response, MAX_FEED_BYTES).await?;
    Ok(Fetched::Feed(parse(&body, &base)?, info))
}

/// A response's body, unless it is larger than `max` bytes.
#[cfg(not(target_arch = "wasm32"))]
pub async fn read_limited(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > max {
            bail!("the download is too large (over {} KB)", max / 1024);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A response's body, unless it is larger than `max` bytes. The browser
/// hands it over whole, so a size it announces is checked first.
#[cfg(target_arch = "wasm32")]
pub async fn read_limited(response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let too_large = || anyhow::anyhow!("the download is too large (over {} KB)", max / 1024);
    if response.content_length().is_some_and(|length| length > max as u64) {
        return Err(too_large());
    }
    let body = response.bytes().await?;
    if body.len() > max {
        return Err(too_large());
    }
    Ok(body.to_vec())
}

/// A web address as typed ("example.com/feed" counts too).
pub fn typed_url(input: &str) -> Result<Url> {
    let input = input.trim();
    let url = if input.contains("://") { input.to_string() } else { format!("https://{input}") };
    let url = Url::parse(&url).context("not a valid address")?;
    anyhow::ensure!(matches!(url.scheme(), "http" | "https"), "not a web address");
    Ok(url)
}

/// Resolves what the user typed to a feed URL: either a feed itself, or a web
/// page that links to one.
pub async fn discover(http: &reqwest::Client, input: &str) -> Result<String> {
    let url = typed_url(input)?;
    let response = http.get(url.clone()).send().await?.error_for_status()?;
    let is_html =
        response.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.contains("html"));
    let final_url = response.url().clone();
    let body = read_limited(response, MAX_FEED_BYTES).await?;
    if !is_html && parse(&body, final_url.as_str()).is_ok() {
        return Ok(final_url.to_string());
    }
    let html = String::from_utf8_lossy(&body);
    match find_feed_link(&html).and_then(|href| final_url.join(&href).ok()) {
        Some(feed) if may_lead_to(&final_url, &feed) => Ok(feed.to_string()),
        _ => bail!("no feed found at {url}"),
    }
}

/// Finds `<link rel="alternate" type="application/rss+xml" href="...">` and friends.
fn find_feed_link(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find("<link") {
        let tag_len = rest[start..].find('>')?;
        let tag = &rest[start..start + tag_len];
        let feed_type = ["application/rss+xml", "application/atom+xml", "application/feed+json"];
        if tag.contains("alternate") && feed_type.iter().any(|t| tag.contains(t)) {
            let offset = html.len() - rest.len() + start;
            return crate::html::attribute(&html[offset..offset + tag_len], "href");
        }
        rest = &rest[start + tag_len..];
    }
    None
}

/// Parses a feed downloaded from `url`; relative links are resolved against it.
pub fn parse(body: &[u8], url: &str) -> Result<Feed> {
    // A parser can't be used again after an error, so each attempt gets its own.
    let parser = || feed_rs::parser::Builder::new().base_uri(Some(url)).build();
    let feed = match parser().parse(body) {
        Ok(feed) => feed,
        // feed-rs rejects a whole feed over one empty Media RSS title or
        // description (YouTube has one for every video without a description),
        // so try again without them.
        Err(
            e @ feed_rs::parser::ParseFeedError::ParseError(feed_rs::parser::ParseErrorKind::MissingContent { .. }),
        ) => match without_empty_media_text(body) {
            Some(cleaned) => parser().parse(cleaned.as_slice())?,
            None => return Err(e.into()),
        },
        Err(e) => return Err(e.into()),
    };
    let now = now_ms() / 1000;
    let items = feed
        .entries
        .into_iter()
        .take(MAX_ITEMS)
        .map(|entry| {
            // Unknown (0) when missing or implausibly far in the future; the item
            // then keeps the time it was first seen (see `Db::store_fetched`).
            let published =
                entry.published.or(entry.updated).map(|d| d.timestamp()).filter(|&t| t <= now + DAY_SECS).unwrap_or(0);
            let link = entry.links.iter().find(|l| l.rel.as_deref() != Some("enclosure")).or(entry.links.first());
            let content = entry.content.and_then(|c| c.body).or(entry.summary.map(|s| s.content)).unwrap_or_default();
            let item = Item {
                title: truncate(entry.title.map(|t| t.content).unwrap_or_default(), MAX_TITLE_BYTES),
                link: link.map(|l| bounded_link(&l.href)).unwrap_or_default(),
                author: truncate(
                    entry.authors.first().map(|a| a.name.clone().unwrap_or_default()).unwrap_or_default(),
                    MAX_AUTHOR_BYTES,
                ),
                published,
                content: truncate(content, MAX_CONTENT_BYTES),
                t: published * 1000,
            };
            let guid = if entry.id.len() > MAX_GUID_BYTES { crate::docs::sha256_hex(&entry.id) } else { entry.id };
            (guid, item)
        })
        .collect();
    let site = feed.links.iter().find(|l| l.rel.as_deref() != Some("self")).map(|l| bounded_link(&l.href));
    let site = site.unwrap_or_default();
    let mut title = feed.title.map(|t| t.content).unwrap_or_default();
    // A channel's videos-only feed is named after a playlist; the channel's
    // name (its author) says more.
    if crate::youtube::is_videos_only_feed(url)
        && let Some(channel) = feed.authors.first().and_then(|a| a.name.clone()).filter(|n| !n.trim().is_empty())
    {
        title = channel;
    }
    Ok(Feed { title: truncate(title, MAX_TITLE_BYTES), site, items })
}

/// The link, unless it is too long to be one (see [`MAX_LINK_BYTES`]).
fn bounded_link(link: &str) -> String {
    if link.len() > MAX_LINK_BYTES { String::new() } else { link.to_string() }
}

/// `body` without empty Media RSS title and description elements
/// (`<media:description></media:description>`, `<media:title/>`, …), which
/// carry nothing but make feed-rs give up. None if it has none (comments and
/// CDATA sections are left as they are).
fn without_empty_media_text(body: &[u8]) -> Option<Vec<u8>> {
    let prefixes = media_prefixes(body);
    if prefixes.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    let mut removed = false;
    while let Some(start) = rest.iter().position(|&b| b == b'<') {
        out.extend_from_slice(&rest[..start]);
        rest = &rest[start..];
        // Copied as they are, up to their end (or the end of the feed).
        let skip = [(b"<!--".as_slice(), b"-->".as_slice()), (b"<![CDATA[", b"]]>")]
            .into_iter()
            .find(|(open, _)| rest.starts_with(open))
            .map(|(open, close)| {
                rest[open.len()..]
                    .windows(close.len())
                    .position(|w| w == close)
                    .map_or(rest.len(), |at| open.len() + at + close.len())
            });
        if let Some(len) = skip {
            out.extend_from_slice(&rest[..len]);
            rest = &rest[len..];
        } else if let Some(len) = empty_media_text_len(rest, &prefixes) {
            rest = &rest[len..];
            removed = true;
        } else {
            out.push(b'<');
            rest = &rest[1..];
        }
    }
    out.extend_from_slice(rest);
    removed.then_some(out)
}

/// The prefixes `xml` declares for the Media RSS namespace (usually just "media").
fn media_prefixes(xml: &[u8]) -> Vec<&[u8]> {
    const MEDIA_RSS: &[u8] = b"http://search.yahoo.com/mrss"; // With or without a final slash.
    let mut prefixes = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.windows(6).position(|w| w == b"xmlns:") {
        rest = &rest[at + 6..];
        let Some(eq) = rest.iter().position(|&b| b == b'=' || b == b'>') else { break };
        let prefix = rest[..eq].trim_ascii();
        let value = rest[eq..].strip_prefix(b"=").map(<[u8]>::trim_ascii_start).unwrap_or_default();
        let Some((&quote, value)) = value.split_first().filter(|(q, _)| matches!(q, b'"' | b'\'')) else { continue };
        let Some(len) = value.iter().position(|&b| b == quote) else { break };
        let uri = &value[..len];
        if uri.strip_suffix(b"/").unwrap_or(uri) == MEDIA_RSS && !prefix.is_empty() {
            prefixes.push(prefix);
        }
    }
    prefixes
}

/// If `xml` starts with an empty element named `prefix:title` or
/// `prefix:description`, for one of `prefixes`, how long it is.
fn empty_media_text_len(xml: &[u8], prefixes: &[&[u8]]) -> Option<usize> {
    let name_len = xml[1..].iter().position(|&b| b.is_ascii_whitespace() || b == b'>' || b == b'/')?;
    let name = &xml[1..1 + name_len];
    let colon = name.iter().position(|&b| b == b':')?;
    if !prefixes.contains(&&name[..colon]) || !matches!(&name[colon + 1..], b"title" | b"description") {
        return None;
    }
    // The tag ends at the first '>' outside quoted attribute values.
    let mut quote = None;
    let mut end = None;
    for (i, &b) in xml.iter().enumerate().skip(1 + name_len) {
        match (quote, b) {
            (Some(q), _) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => {
                end = Some(i);
                break;
            }
            _ => {}
        }
    }
    let end = end?;
    if xml[end - 1] == b'/' {
        return Some(end + 1);
    }
    let after = &xml[end + 1..];
    let blank = after.iter().take_while(|b| b.is_ascii_whitespace()).count();
    let close = [b"</".as_slice(), name, b">"].concat();
    after[blank..].starts_with(&close).then(|| end + 1 + blank + close.len())
}

pub fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_feed_links() {
        let html = r#"<head><link rel="stylesheet" href="/s.css"><LINK rel="alternate" type="application/rss+xml" title="Blog" href="/feed.xml?a=1&amp;b=2"></head>"#;
        assert_eq!(find_feed_link(html).as_deref(), Some("/feed.xml?a=1&b=2"));
        assert_eq!(find_feed_link("<html></html>"), None);
        // Unquoted values used to be cut at the first slash.
        let unquoted = "<link rel=alternate type=application/atom+xml href=/atom.xml>";
        assert_eq!(find_feed_link(unquoted).as_deref(), Some("/atom.xml"));
    }

    #[tokio::test]
    async fn endless_downloads_are_cut_off() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/feed.xml", server.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            // Read the request before answering, as a real server would.
            let (mut request, mut byte) = (Vec::new(), [0u8]);
            while !request.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).is_ok() {
                request.push(byte[0]);
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\n\r\n");
            let chunk = vec![b' '; 64 * 1024];
            while stream.write_all(&chunk).is_ok() {}
        });
        let cache = FetchInfo { etag: None, last_modified: None };
        let error = fetch(&http_client(), &url, &cache).await.err().expect("refused");
        assert!(error.to_string().contains("too large"), "{error}");
    }

    #[test]
    fn parses_rss() {
        let rss = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title><link>https://t.example</link>
            <item><title>Hello</title><link>https://t.example/1</link><guid>1</guid><description>&lt;p&gt;Hi&lt;/p&gt;</description>
            <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate></item></channel></rss>"#;
        let feed = parse(rss, "https://t.example/feed.xml").unwrap();
        assert_eq!(feed.title, "T");
        assert_eq!(feed.items.len(), 1);
        let (_, item) = &feed.items[0];
        assert_eq!((item.title.as_str(), item.content.as_str(), item.published), ("Hello", "<p>Hi</p>", 1704067200));
    }

    #[test]
    fn missing_or_future_dates_are_unknown() {
        let rss = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title>
            <item><title>Undated</title><guid>1</guid></item>
            <item><title>Future</title><guid>2</guid><pubDate>Fri, 01 Jan 2100 00:00:00 GMT</pubDate></item>
            </channel></rss>"#;
        let feed = parse(rss, "https://t.example/feed.xml").unwrap();
        assert!(feed.items.iter().all(|(_, item)| item.published == 0));
    }

    #[test]
    fn empty_media_descriptions_dont_break_a_feed() {
        // As YouTube sends them for videos without a description.
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns:media="http://search.yahoo.com/mrss/" xmlns="http://www.w3.org/2005/Atom">
 <title>Channel</title>
 <entry>
  <id>yt:video:a</id><title>No description</title>
  <link rel="alternate" href="https://www.youtube.com/watch?v=a"/>
  <published>2024-01-01T00:00:00+00:00</published>
  <media:group><media:title>No description</media:title><media:description></media:description></media:group>
 </entry>
 <entry>
  <id>yt:video:b</id><title>Described</title>
  <link rel="alternate" href="https://www.youtube.com/watch?v=b"/>
  <published>2024-01-02T00:00:00+00:00</published>
  <media:group><media:title /><media:description type="plain">Hello &amp; welcome</media:description></media:group>
 </entry>
</feed>"#;
        let feed = parse(xml, "https://www.youtube.com/feeds/videos.xml?channel_id=x").unwrap();
        assert_eq!(feed.title, "Channel");
        let titles: Vec<_> = feed.items.iter().map(|(_, item)| item.title.as_str()).collect();
        assert_eq!(titles, ["No description", "Described"]);
        assert_eq!(feed.items[0].1.link, "https://www.youtube.com/watch?v=a");
    }

    #[test]
    fn videos_only_feeds_are_named_after_their_channel() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
 <title>Videos</title>
 <author><name>Some Channel</name><uri>https://www.youtube.com/channel/UCx</uri></author>
</feed>"#;
        let videos_only = parse(xml, "https://www.youtube.com/feeds/videos.xml?playlist_id=UULFx").unwrap();
        assert_eq!(videos_only.title, "Some Channel");
        // Other playlists keep their own name.
        let playlist = parse(xml, "https://www.youtube.com/feeds/videos.xml?playlist_id=PLx").unwrap();
        assert_eq!(playlist.title, "Videos");
    }

    #[test]
    fn sites_cant_send_requests_into_the_local_network() {
        let url = |text: &str| Url::parse(text).unwrap();
        for local in [
            "http://localhost:8080/feed",
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://192.168.1.1/reboot",
            "http://10.0.0.5/",
            "http://172.16.4.4/",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:192.168.1.1]/",
            "http://nas/feed",
            "http://printer.local/",
            "ws://router.lan:7777",
        ] {
            assert!(is_local_address(&url(local)), "{local}");
            // Not from a site on the internet, but fine from the local network itself.
            assert!(!may_lead_to(&url("https://blog.example/feed"), &url(local)) || !local.starts_with("http"));
            assert!(!local.starts_with("http") || may_lead_to(&url("http://192.168.1.20/feed"), &url(local)));
        }
        for public in ["https://blog.example/feed", "http://8.8.8.8/", "http://172.32.0.1/", "http://[2001:db8::1]/"] {
            assert!(!is_local_address(&url(public)), "{public}");
            assert!(may_lead_to(&url("https://other.example/"), &url(public)));
        }
        // Only ever to web addresses.
        assert!(!may_lead_to(&url("https://blog.example/"), &url("file:///etc/passwd")));
        assert!(!may_lead_to(&url("https://blog.example/"), &url("ftp://blog.example/feed")));
    }

    #[test]
    fn oversized_fields_and_feeds_are_cut_down() {
        let long = "x".repeat(MAX_LINK_BYTES + 1);
        let mut rss = format!("<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>{long}</title>");
        // An "é" (two bytes) straddles the title's limit.
        let start = "x".repeat(MAX_TITLE_BYTES - 1);
        rss += &format!("<item><title>{start}é{long}</title><link>https://a.example/{long}</link><guid>{long}</guid>");
        rss += &format!("<author>{long}</author></item>");
        for i in 0..MAX_ITEMS + 10 {
            rss += &format!("<item><title>Post {i}</title><guid>{i}</guid></item>");
        }
        rss += "</channel></rss>";
        let feed = parse(rss.as_bytes(), "https://a.example/feed").unwrap();
        assert_eq!(feed.items.len(), MAX_ITEMS);
        assert_eq!(feed.title.len(), MAX_TITLE_BYTES);
        let (guid, item) = &feed.items[0];
        // Cut at a character boundary, so one byte short.
        assert_eq!(item.title.len(), MAX_TITLE_BYTES - 1);
        assert!(item.author.len() <= MAX_AUTHOR_BYTES);
        assert_eq!(item.link, "");
        assert_eq!(guid.len(), 64);
        // Ordinary items are left alone.
        assert_eq!(feed.items[1].0, "0");
        assert_eq!(feed.items[1].1.title, "Post 0");
    }

    #[test]
    fn only_empty_media_text_is_removed() {
        let xml = b"<a xmlns:media=\"http://search.yahoo.com/mrss/\" xmlns:m='http://search.yahoo.com/mrss'>\
            <media:title a='>'/><m:description>\n </m:description><title></title><x:title></x:title>\
            <media:description>x</media:description><media:titles/></a>";
        let cleaned = String::from_utf8(without_empty_media_text(xml).unwrap()).unwrap();
        assert_eq!(
            cleaned,
            "<a xmlns:media=\"http://search.yahoo.com/mrss/\" xmlns:m='http://search.yahoo.com/mrss'>\
            <title></title><x:title></x:title><media:description>x</media:description><media:titles/></a>"
        );
    }

    #[test]
    fn media_text_cleanup_leaves_other_feeds_comments_and_cdata_alone() {
        // No Media RSS namespace: nothing to remove, so the parse error stands.
        assert_eq!(without_empty_media_text(b"<a><media:title/></a>"), None);
        let xml = b"<a xmlns:media=\"http://search.yahoo.com/mrss/\"><!-- <media:title/> -->\
            <b><![CDATA[<media:title></media:title>]]></b><media:title/></a>";
        let cleaned = String::from_utf8(without_empty_media_text(xml).unwrap()).unwrap();
        assert_eq!(
            cleaned,
            "<a xmlns:media=\"http://search.yahoo.com/mrss/\"><!-- <media:title/> -->\
            <b><![CDATA[<media:title></media:title>]]></b></a>"
        );
    }

    #[test]
    fn resolves_relative_links() {
        let rss = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>Blog</title><link>/</link>
            <item><title>Post</title><link>/posts/hello/</link><guid>/posts/hello/</guid></item></channel></rss>"#;
        let feed = parse(rss, "https://blog.example/index.xml").unwrap();
        assert_eq!(feed.site, "https://blog.example/");
        let (guid, item) = &feed.items[0];
        assert_eq!(item.link, "https://blog.example/posts/hello/");
        // The guid is only an identifier, so it stays as it is (and item keys stay stable).
        assert_eq!(guid, "/posts/hello/");
    }
}
