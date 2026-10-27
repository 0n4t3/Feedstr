//! Downloading and parsing RSS, Atom and JSON feeds.

use anyhow::{Context, Result, bail};
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{StatusCode, Url};

use crate::db::{FetchInfo, now_ms};
use crate::docs::Item;

/// Longer item content is cut off, so a single item can't crowd out a feed's sync cache.
pub const MAX_CONTENT_BYTES: usize = 40_000;
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
    let builder = reqwest::Client::builder()
        .user_agent(concat!("Feedstr/", env!("CARGO_PKG_VERSION")))
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
    match find_feed_link(&html) {
        Some(href) => Ok(final_url.join(&href)?.to_string()),
        None => bail!("no feed found at {url}"),
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
    let feed = feed_rs::parser::Builder::new().base_uri(Some(url)).build().parse(body)?;
    let now = now_ms() / 1000;
    let items = feed
        .entries
        .into_iter()
        .map(|entry| {
            // Unknown (0) when missing or implausibly far in the future; the item
            // then keeps the time it was first seen (see `Db::store_fetched`).
            let published =
                entry.published.or(entry.updated).map(|d| d.timestamp()).filter(|&t| t <= now + DAY_SECS).unwrap_or(0);
            let link = entry.links.iter().find(|l| l.rel.as_deref() != Some("enclosure")).or(entry.links.first());
            let content = entry.content.and_then(|c| c.body).or(entry.summary.map(|s| s.content)).unwrap_or_default();
            let item = Item {
                title: entry.title.map(|t| t.content).unwrap_or_default(),
                link: link.map(|l| l.href.clone()).unwrap_or_default(),
                author: entry.authors.first().map(|a| a.name.clone().unwrap_or_default()).unwrap_or_default(),
                published,
                content: truncate(content, MAX_CONTENT_BYTES),
                t: published * 1000,
            };
            (entry.id, item)
        })
        .collect();
    let site = feed.links.iter().find(|l| l.rel.as_deref() != Some("self")).map(|l| l.href.clone()).unwrap_or_default();
    Ok(Feed { title: feed.title.map(|t| t.content).unwrap_or_default(), site, items })
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
