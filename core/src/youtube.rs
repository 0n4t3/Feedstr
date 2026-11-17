//! YouTube channels: recognizing a channel's web address, finding its feed,
//! and the videos-only feed that leaves out Shorts and livestreams.
//!
//! Every channel has an ID like `UC…` and a feed of all its uploads at
//! `feeds/videos.xml?channel_id=UC…`. YouTube also keeps playlists of each
//! kind of upload; the one starting `UULF` instead of `UC` holds only the
//! regular videos, and has a feed of its own.

use anyhow::{Context, Result, bail};
use reqwest::Url;

use crate::feeds;

const FEED_URL: &str = "https://www.youtube.com/feeds/videos.xml";
const VIDEOS_ONLY_PREFIX: &str = "UULF";

/// A channel found from its web address.
#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    /// Like `UCxxxxxxxxxxxxxxxxxxxxxx`.
    pub id: String,
    /// The channel's name, unless its page couldn't be read (say, in the
    /// browser, which isn't allowed to read YouTube's pages).
    pub name: Option<String>,
}

impl Channel {
    /// The channel's feed: every upload, or with `videos_only`, just the
    /// regular videos (no Shorts or livestreams).
    pub fn feed_url(&self, videos_only: bool) -> String {
        if videos_only {
            format!("{FEED_URL}?playlist_id={VIDEOS_ONLY_PREFIX}{}", &self.id[2..])
        } else {
            format!("{FEED_URL}?channel_id={}", self.id)
        }
    }
}

/// What a pasted YouTube channel address tells us.
#[derive(Debug, PartialEq)]
pub enum ChannelUrl {
    /// `youtube.com/channel/UC…`: the ID is in the address.
    Id(String),
    /// `youtube.com/@name`, `/c/name` or `/user/name`: the channel's page has to be read.
    Page(Url),
}

impl ChannelUrl {
    /// The channel's page (without `/videos` and the like).
    fn page(&self) -> Url {
        match self {
            ChannelUrl::Id(id) => Url::parse(&format!("https://www.youtube.com/channel/{id}")).expect("valid URL"),
            ChannelUrl::Page(url) => url.clone(),
        }
    }
}

/// Whether `input` is a YouTube channel's web address (not its feed, a video or a playlist).
pub fn channel_url(input: &str) -> Option<ChannelUrl> {
    let url = feeds::typed_url(input).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(host.as_str(), "youtube.com" | "www.youtube.com" | "m.youtube.com") {
        return None;
    }
    let mut segments = url.path_segments()?.filter(|s| !s.is_empty());
    let first = segments.next()?;
    let page = |path: String| Url::parse(&format!("https://www.youtube.com/{path}")).ok().map(ChannelUrl::Page);
    if first.starts_with('@') && first.len() > 1 {
        page(first.to_string())
    } else if first == "channel" {
        let id = segments.next()?;
        is_channel_id(id).then(|| ChannelUrl::Id(id.to_string()))
    } else if matches!(first, "c" | "user") {
        page(format!("{first}/{}", segments.next()?))
    } else {
        None
    }
}

/// Finds the channel behind a channel address: its ID, and its name from its page.
pub async fn look_up(http: &reqwest::Client, url: &ChannelUrl) -> Result<Channel> {
    match (url, read_page(http, &url.page()).await) {
        (_, Ok(html)) => {
            let id = match url {
                ChannelUrl::Id(id) => id.clone(),
                ChannelUrl::Page(_) => channel_id(&html).context("couldn't find the channel on its page")?,
            };
            Ok(Channel { id, name: channel_name(&html) })
        }
        // The ID is enough to subscribe; the name comes with the first fetch.
        (ChannelUrl::Id(id), Err(_)) => Ok(Channel { id: id.clone(), name: None }),
        (ChannelUrl::Page(_), Err(e)) if cfg!(target_arch = "wasm32") => Err(e).context(
            "browsers can't read YouTube's pages. Paste the channel's RSS feed or its youtube.com/channel/UC… \
             address instead, or add it on another device",
        ),
        (ChannelUrl::Page(_), Err(e)) => Err(e),
    }
}

async fn read_page(http: &reqwest::Client, url: &Url) -> Result<String> {
    let request = http.get(url.clone());
    // Visitors from some countries are sent to a cookie consent page first;
    // this cookie answers it (with only the necessary cookies). Browsers set
    // cookies themselves.
    #[cfg(not(target_arch = "wasm32"))]
    let request = request.header(reqwest::header::COOKIE, "SOCS=CAI");
    let response = request.send().await?.error_for_status()?;
    if response.url().host_str().is_some_and(|host| host.starts_with("consent.")) {
        bail!("YouTube asked for cookie consent instead of showing the channel");
    }
    let body = feeds::read_limited(response, feeds::MAX_FEED_BYTES).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Like `UC` followed by 22 letters, digits, `-` or `_`.
fn is_channel_id(id: &str) -> bool {
    id.len() == 24 && id.starts_with("UC") && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The ID of the channel a channel page is about.
fn channel_id(html: &str) -> Option<String> {
    let from_url = |url: String| {
        let url = Url::parse(&url).ok()?;
        let id = match url.query_pairs().find(|(k, _)| k == "channel_id") {
            Some((_, id)) => id.into_owned(),
            None => url.path().strip_prefix("/channel/")?.trim_end_matches('/').to_string(),
        };
        is_channel_id(&id).then_some(id)
    };
    let tagged = [
        ("link", "rel", "canonical", "href"),
        ("link", "type", "application/rss+xml", "href"),
        ("meta", "property", "og:url", "content"),
    ];
    let found =
        tagged.iter().find_map(|&(tag, key, value, attr)| find_tag(html, tag, key, value, attr).and_then(from_url));
    found.or_else(|| {
        // In the page's data, as a last resort.
        let start = html.find("\"externalId\":\"")? + "\"externalId\":\"".len();
        let id = html[start..].split('"').next()?;
        is_channel_id(id).then(|| id.to_string())
    })
}

/// The channel's name, from a channel page.
fn channel_name(html: &str) -> Option<String> {
    let name = find_tag(html, "meta", "property", "og:title", "content")
        .or_else(|| find_tag(html, "meta", "name", "title", "content"))
        .or_else(|| {
            let lower = html.to_ascii_lowercase();
            let start = lower.find("<title")?;
            let start = start + lower[start..].find('>')? + 1;
            let end = start + lower[start..].find("</title")?;
            let title = crate::html::decode_entities(&html[start..end]);
            Some(title.trim().trim_end_matches("- YouTube").to_string())
        })?;
    let name = name.trim();
    (!name.is_empty() && name != "YouTube").then(|| feeds::truncate(name.to_string(), feeds::MAX_TITLE_BYTES))
}

/// The value of `attr` in the first `<tag>` whose `key` attribute is `value`.
fn find_tag(html: &str, tag: &str, key: &str, value: &str, attr: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&open) {
        let start = from + found;
        let len = lower[start..].find('>')?;
        let element = &html[start..start + len];
        from = start + len;
        // A whole tag name: `<link `, not `<linkfoo`.
        if !element[open.len()..].starts_with(char::is_whitespace) {
            continue;
        }
        let matches = crate::html::attribute(element, key).is_some_and(|v| v.eq_ignore_ascii_case(value));
        if matches && let Some(found) = crate::html::attribute(element, attr) {
            return Some(found);
        }
    }
    None
}

/// Whether `url` is a channel's videos-only feed (see the module docs).
pub fn is_videos_only_feed(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| {
        url.host_str().is_some_and(|host| host.ends_with("youtube.com"))
            && url.query_pairs().any(|(k, v)| k == "playlist_id" && v.starts_with(VIDEOS_ONLY_PREFIX))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "UCabcdefghijklmnopqrstuv";

    #[test]
    fn recognizes_channel_addresses() {
        let page = |path: &str| Some(ChannelUrl::Page(Url::parse(&format!("https://www.youtube.com/{path}")).unwrap()));
        assert_eq!(channel_url("https://www.youtube.com/@SomeOne"), page("@SomeOne"));
        assert_eq!(channel_url("youtube.com/@SomeOne/videos"), page("@SomeOne"));
        assert_eq!(channel_url("https://m.youtube.com/@SomeOne?si=x"), page("@SomeOne"));
        assert_eq!(channel_url("https://www.youtube.com/c/Name"), page("c/Name"));
        assert_eq!(channel_url("https://www.youtube.com/user/name/"), page("user/name"));
        assert_eq!(
            channel_url(&format!("https://www.youtube.com/channel/{ID}/videos")),
            Some(ChannelUrl::Id(ID.into()))
        );
        // Not channel pages.
        assert_eq!(channel_url("https://www.youtube.com/channel/not-an-id"), None);
        assert_eq!(channel_url(&format!("https://www.youtube.com/feeds/videos.xml?channel_id={ID}")), None);
        assert_eq!(channel_url("https://www.youtube.com/watch?v=abc"), None);
        assert_eq!(channel_url("https://www.youtube.com/@"), None);
        assert_eq!(channel_url("https://example.com/@SomeOne"), None);
        assert_eq!(channel_url("npub1xyz"), None);
    }

    #[test]
    fn builds_feed_urls() {
        let channel = Channel { id: ID.into(), name: None };
        assert_eq!(channel.feed_url(false), format!("https://www.youtube.com/feeds/videos.xml?channel_id={ID}"));
        let videos_only = channel.feed_url(true);
        assert_eq!(videos_only, "https://www.youtube.com/feeds/videos.xml?playlist_id=UULFabcdefghijklmnopqrstuv");
        assert!(is_videos_only_feed(&videos_only));
        assert!(!is_videos_only_feed(&channel.feed_url(false)));
        assert!(!is_videos_only_feed("https://www.youtube.com/feeds/videos.xml?playlist_id=PLabc"));
    }

    #[test]
    fn reads_channel_pages() {
        let html = format!(
            r#"<html><head><title>Some &amp; One - YouTube</title>
            <meta property="og:title" content="Some &amp; One">
            <link rel="alternate" type="application/rss+xml" title="RSS" href="https://www.youtube.com/feeds/videos.xml?channel_id={ID}">
            <link rel="canonical" href="https://www.youtube.com/channel/{ID}"></head></html>"#
        );
        assert_eq!(channel_id(&html).as_deref(), Some(ID));
        assert_eq!(channel_name(&html).as_deref(), Some("Some & One"));

        // Only the feed link, and the name only in the title.
        let html = format!(
            r#"<title>Other - YouTube</title><link rel="alternate" type="application/rss+xml" href="https://www.youtube.com/feeds/videos.xml?channel_id={ID}">"#
        );
        assert_eq!(channel_id(&html).as_deref(), Some(ID));
        assert_eq!(channel_name(&html).as_deref(), Some("Other"));

        // Only in the page's data.
        let html = format!(r#"<title>YouTube</title><script>var d = {{"externalId":"{ID}"}};</script>"#);
        assert_eq!(channel_id(&html).as_deref(), Some(ID));
        assert_eq!(channel_name(&html), None);

        assert_eq!(channel_id("<html></html>"), None);
    }
}
