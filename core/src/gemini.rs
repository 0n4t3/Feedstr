//! Feeds on Gemini (`gemini://…`), the small-web protocol.
//!
//! A Gemini feed is either an Atom, RSS or JSON feed served over Gemini, or a
//! page of links that start with a date (`=> gemini://… 2026-05-01 - A title`,
//! the "subscribing to Gemini pages" convention most gemlogs follow). Entries
//! of such feeds rarely carry their text, so the pages of new entries are
//! read too, and shown as their articles.
//!
//! A request is one TLS connection: the address goes out, a line with a status
//! and the kind of content comes back, then the content, then the capsule
//! hangs up. Capsules usually sign their own certificates, so a certificate
//! is trusted the first time it is seen and must stay the same afterwards,
//! unless it is one a certificate authority vouches for.

use anyhow::{Context, Result, ensure};
use reqwest::Url;

use crate::docs::Item;
use crate::feeds::{self, Feed};

/// Whether `url` is a Gemini address.
pub fn is_gemini(url: &str) -> bool {
    url.trim_start().get(..9).is_some_and(|start| start.eq_ignore_ascii_case("gemini://"))
}

/// A Gemini address as typed or found in a page, tidied up for asking for it
/// (so the same capsule is always written the same way).
pub fn typed_url(input: &str) -> Result<Url> {
    let mut url = Url::parse(input.trim()).context("not a valid address")?;
    ensure!(url.scheme() == "gemini", "not a Gemini address");
    ensure!(url.username().is_empty() && url.password().is_none(), "Gemini addresses can't hold a user name");
    let name = url.host_str().and_then(capsule_name).context("not a Gemini address")?;
    url.set_host(Some(&name)).ok().context("not a Gemini address")?;
    url.set_fragment(None);
    if url.path().is_empty() {
        url.set_path("/");
    }
    Ok(url)
}

/// A capsule's name the way it is looked up: in lowercase, with letters
/// outside ASCII in their `xn--` form, and an IP address in its usual form.
/// `host` is as the URL parser has it, which leaves the names of anything
/// but web addresses as they were written.
fn capsule_name(host: &str) -> Option<String> {
    if host.starts_with('[') {
        // An IPv6 address, which the parser did read.
        return Some(host.to_ascii_lowercase());
    }
    // "M%C3%BCller.example" is how the parser keeps "Müller.example".
    let mut bytes = Vec::with_capacity(host.len());
    let mut rest = host.as_bytes();
    while let Some((&byte, after)) = rest.split_first() {
        let escaped = (byte == b'%').then(|| after.get(..2)).flatten();
        match escaped.and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()) {
            Some(decoded) => {
                bytes.push(decoded);
                rest = &after[2..];
            }
            None => {
                bytes.push(byte);
                rest = after;
            }
        }
    }
    let name = String::from_utf8(bytes).ok()?;
    // Nothing that would end the name early, or add to it, when read as a web address.
    let odd = |c: char| "/\\@:?#[]%".contains(c) || c.is_whitespace() || c.is_control();
    if name.is_empty() || name.contains(odd) {
        return None;
    }
    // Gemini names capsules the way the web names sites, and for web addresses the parser knows the rules.
    Url::parse(&format!("https://{name}/")).ok()?.host_str().map(String::from)
}

/// Text for putting into HTML.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Where it is noted that a feed couldn't be fetched because a capsule's
/// certificate changed (the value is the capsule, as `name:port`), so the
/// feed report can offer to trust the new one.
pub fn changed_key(feed_url: &str) -> String {
    format!("gemini_changed:{feed_url}")
}

/// A link line of a Gemini page (`=> address label`): its address and label.
// Pages are only read where they can be fetched: not in the browser.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn link_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("=>")?.trim();
    let (address, label) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!address.is_empty()).then_some((address, label.trim()))
}

/// A page of dated links as a feed, or `None` if it has none: every link
/// whose label starts with a date (`2026-05-01`) is an entry, published at
/// noon (UTC) that day, and the page's first heading names the feed. A date
/// that can't be right (after tomorrow, or before 1970) counts as unknown,
/// like in other feeds: the entry then keeps the time it was first seen.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn gemfeed(text: &str, page: &Url) -> Option<Feed> {
    let mut title = None;
    let mut items: Vec<(String, Item)> = Vec::new();
    let mut preformatted = false;
    let latest = crate::clock::now_ms() / 1000 + 24 * 3600;
    for line in text.lines() {
        if line.starts_with("```") {
            preformatted = !preformatted;
        } else if preformatted {
            continue;
        } else if let Some(heading) = line.strip_prefix('#').filter(|rest| !rest.starts_with('#')) {
            title.get_or_insert_with(|| heading.trim().to_string());
        } else if let Some((address, label)) = link_line(line) {
            let Some(date) = label.get(..10).and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) else {
                continue;
            };
            let Ok(link) = page.join(address) else { continue };
            let name = label[10..].trim_start_matches(|c: char| c.is_whitespace() || "-–—:".contains(c)).trim();
            let published = date.and_hms_opt(12, 0, 0).map_or(0, |noon| noon.and_utc().timestamp());
            let published = if (1..=latest).contains(&published) { published } else { 0 };
            if items.len() < feeds::MAX_ITEMS && !items.iter().any(|(guid, _)| guid == link.as_str()) {
                let item = Item {
                    title: feeds::truncate(name.to_string(), feeds::MAX_TITLE_BYTES),
                    link: link.to_string(),
                    published,
                    t: published * 1000,
                    ..Default::default()
                };
                items.push((link.to_string(), item));
            }
        }
    }
    if items.is_empty() {
        return None;
    }
    let title = title.filter(|t| !t.is_empty()).or_else(|| page.host_str().map(String::from)).unwrap_or_default();
    Some(Feed { title: feeds::truncate(title, feeds::MAX_TITLE_BYTES), site: page.to_string(), items })
}

/// A Gemini page ("gemtext") as HTML for the reader: headings, links (each on
/// a line of its own, as Gemini has them), lists, quotes and preformatted text.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn gemtext_to_html(text: &str, page: &Url) -> String {
    let mut out = String::new();
    // The kind of lines being collected ("ul", "blockquote" or "pre"), if any.
    let mut open: Option<&str> = None;
    let mut switch = |out: &mut String, to: Option<&'static str>| {
        if open != to {
            if let Some(tag) = open {
                *out += &format!("</{tag}>");
            }
            if let Some(tag) = to {
                *out += &format!("<{tag}>");
            }
            open = to;
        }
    };
    let mut preformatted = false;
    for line in text.lines() {
        if line.starts_with("```") {
            preformatted = !preformatted;
            switch(&mut out, preformatted.then_some("pre"));
        } else if preformatted {
            out += &escape(line);
            out.push('\n');
        } else if let Some(item) = line.strip_prefix("* ") {
            switch(&mut out, Some("ul"));
            out += &format!("<li>{}</li>", escape(item.trim()));
        } else if let Some(quote) = line.strip_prefix('>') {
            switch(&mut out, Some("blockquote"));
            out += &format!("<p>{}</p>", escape(quote.trim()));
        } else {
            switch(&mut out, None);
            let level = line.bytes().take_while(|&b| b == b'#').count();
            if let Some((address, label)) = link_line(line) {
                let label = if label.is_empty() { address } else { label };
                match page.join(address) {
                    Ok(link) => out += &format!("<p><a href=\"{}\">{}</a></p>", escape(link.as_str()), escape(label)),
                    Err(_) => out += &format!("<p>{}</p>", escape(label)),
                }
            } else if (1..=3).contains(&level) {
                out += &format!("<h{level}>{}</h{level}>", escape(line[level..].trim()));
            } else if !line.trim().is_empty() {
                out += &format!("<p>{}</p>", escape(line.trim()));
            }
        }
    }
    switch(&mut out, None);
    out
}

#[cfg(not(target_arch = "wasm32"))]
pub use client::{CertChanged, Client};

/// Asking capsules for pages. Not in the browser, which can't open connections of its own.
#[cfg(not(target_arch = "wasm32"))]
mod client {
    use std::cell::Cell;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    use anyhow::bail;

    use super::*;
    use crate::db::Db;
    use crate::proxy::Route;
    use crate::{docs, html};

    const PORT: u16 = 1965;
    /// For one page, redirects included: from looking up the capsule to the end of its answer.
    const TIMEOUT: Duration = Duration::from_secs(30);
    /// For a whole fetch, the feed and its entries' pages, so a slow (or
    /// unfriendly) capsule can't hold up a sync for long.
    const BUDGET: Duration = Duration::from_secs(120);
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    /// The same through Tor, which is slower.
    const TOR_TIMEOUT: Duration = Duration::from_secs(60);
    const TOR_BUDGET: Duration = Duration::from_secs(300);
    /// Circuits through Tor take a while to build, more so to `.onion` capsules.
    const TOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
    const MAX_REDIRECTS: usize = 5;
    /// An address may be 1024 bytes, and so may what comes after the status.
    const MAX_ADDRESS_BYTES: usize = 1024;
    const MAX_HEADER_BYTES: u64 = 2048;
    /// Entries' pages are cut down to [`feeds::MAX_CONTENT_BYTES`] anyway.
    const MAX_PAGE_BYTES: usize = 2 << 20;
    /// The most entries' pages read in one fetch (the newest ones): a gemlog's
    /// whole history isn't downloaded when it is first added.
    const PAGES_PER_FETCH: usize = 20;

    /// A capsule's certificate isn't the one seen before (and no certificate
    /// authority vouches for the new one): its `host:port`.
    #[derive(Debug)]
    pub struct CertChanged(pub String);

    impl std::fmt::Display for CertChanged {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "{} has a different certificate than before, so this may not be the same capsule", self.0)
        }
    }

    impl std::error::Error for CertChanged {}

    /// Where the certificate last accepted from a capsule is remembered.
    pub fn pin_key(host_and_port: &str) -> String {
        format!("gemini_cert:{host_and_port}")
    }

    fn provider() -> Arc<CryptoProvider> {
        static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
        PROVIDER.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())).clone()
    }

    /// The usual check: a certificate for this name that a certificate authority vouches for.
    fn authorities() -> Result<Arc<WebPkiServerVerifier>> {
        static CHECK: OnceLock<Arc<WebPkiServerVerifier>> = OnceLock::new();
        if let Some(check) = CHECK.get() {
            return Ok(check.clone());
        }
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let check = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider()).build()?;
        Ok(CHECK.get_or_init(|| check).clone())
    }

    /// Trust on first use: accepts a certificate that a certificate authority
    /// vouches for, the one accepted last time (`pinned`, its fingerprint), or
    /// any certificate if none was accepted before. Either way the capsule
    /// must prove the certificate is its own.
    #[derive(Debug)]
    struct Tofu {
        authorities: Arc<WebPkiServerVerifier>,
        pinned: Option<String>,
        /// The fingerprint of the certificate accepted in this connection.
        accepted: Mutex<Option<String>>,
        /// The certificate was refused because it isn't the pinned one.
        changed: AtomicBool,
    }

    impl ServerCertVerifier for Tofu {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            ocsp_response: &[u8],
            now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            let fingerprint: String = Sha256::digest(end_entity.as_ref()).iter().map(|b| format!("{b:02x}")).collect();
            let vouched_for =
                self.authorities.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now).is_ok();
            if !vouched_for && self.pinned.as_ref().is_some_and(|pinned| *pinned != fingerprint) {
                self.changed.store(true, Ordering::Relaxed);
                return Err(rustls::Error::General("the capsule's certificate changed".into()));
            }
            *self.accepted.lock().unwrap() = Some(fingerprint);
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &provider().signature_verification_algorithms)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &provider().signature_verification_algorithms)
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            provider().signature_verification_algorithms.supported_schemes()
        }
    }

    /// A capsule's answer to one request.
    enum Answer {
        /// What it says the content is (like `text/gemini; charset=utf-8`), and the content.
        Content(String, Vec<u8>),
        /// The address to ask instead.
        Redirect(String),
    }

    struct Page {
        /// Where it was found, after redirects.
        url: Url,
        /// The kind of content, like `text/gemini`.
        mime: String,
        /// How its text is encoded, if the capsule says (in lowercase).
        charset: Option<String>,
        body: Vec<u8>,
    }

    impl Page {
        fn new(url: Url, meta: &str, body: Vec<u8>) -> Self {
            // "text/gemini; charset=utf-8; lang=en"
            let mut parts = meta.split(';').map(|part| part.trim().to_ascii_lowercase());
            let mime = parts.next().filter(|mime| !mime.is_empty()).unwrap_or_else(|| "text/gemini".into());
            let charset = parts.find_map(|part| part.strip_prefix("charset=").map(|c| c.trim_matches('"').to_string()));
            Self { url, mime, charset, body }
        }

        /// The content as text: UTF-8 (which Gemini expects), or Latin-1 if the capsule says so.
        fn text(&self) -> String {
            match self.charset.as_deref() {
                Some("iso-8859-1" | "latin1" | "latin-1") => self.body.iter().map(|&byte| byte as char).collect(),
                _ => String::from_utf8_lossy(&self.body).into_owned(),
            }
        }
    }

    /// Reads Gemini feeds. One per fetch of a feed: where the feed may lead
    /// (see `local_ok`) is decided by the feed's own address.
    pub struct Client<'a> {
        db: &'a Db,
        /// Whether capsules on this device or its local network may be asked:
        /// only if the feed itself is on one, so a feed out on the internet
        /// can't have Feedstr send requests into the local network. Set by
        /// the first connection.
        local_ok: Cell<Option<bool>>,
        /// How long one page may take, redirects included.
        timeout: Duration,
        /// How long a whole fetch may take: the feed and its entries' pages.
        budget: Duration,
        /// The Tor proxy (`host:port`) every connection goes through, if any.
        tor: Option<String>,
    }

    impl<'a> Client<'a> {
        pub fn new(db: &'a Db) -> Self {
            Self { db, local_ok: Cell::new(None), timeout: TIMEOUT, budget: BUDGET, tor: None }
        }

        /// Connects through Tor's SOCKS proxy at `proxy`, which also looks up
        /// the capsules' names (so `.onion` capsules can be reached).
        pub fn through_tor(self, proxy: String) -> Self {
            Self { tor: Some(proxy), timeout: TOR_TIMEOUT, budget: TOR_BUDGET, ..self }
        }

        /// The feed at `url`, with the text of its entries: what is stored
        /// already, and the pages of new entries (see [`PAGES_PER_FETCH`]).
        /// Only pages on the feed's own capsule are read, so a feed can't
        /// have Feedstr connect wherever it likes; entries elsewhere are
        /// listed with their link.
        pub async fn fetch(&self, url: &str) -> Result<Feed> {
            let deadline = tokio::time::Instant::now() + self.budget;
            let (mut feed, capsule) = self.read_feed(url).await?;
            let mut newest: Vec<usize> = (0..feed.items.len()).collect();
            newest.sort_by_key(|&i| std::cmp::Reverse(feed.items[i].1.published));
            // Newest first, like `newest`.
            let mut wanted = Vec::new();
            for (rank, i) in newest.into_iter().enumerate() {
                let (guid, item) = &mut feed.items[i];
                if !item.content.is_empty() || !on_capsule(&item.link, &capsule) {
                    continue;
                }
                let key = docs::item_key(url, guid);
                if !self.db.has_item(&key)? {
                    wanted.push(i);
                    continue;
                }
                // A stored entry keeps its text: the feed doesn't have it, which
                // would otherwise look like the capsule had taken it back.
                item.content = self.db.item_content(&key)?;
                // One still without text (its page couldn't be read, or an older
                // version lost it) is tried again while it's among the newest.
                if item.content.is_empty() && rank < PAGES_PER_FETCH {
                    wanted.push(i);
                }
            }
            for i in wanted.into_iter().take(PAGES_PER_FETCH) {
                match tokio::time::timeout_at(deadline, self.article(&feed.items[i].1.link, &capsule)).await {
                    Ok(Ok(content)) => feed.items[i].1.content = feeds::truncate(content, feeds::MAX_CONTENT_BYTES),
                    // An entry whose page can't be read is still listed, with its link.
                    Ok(Err(_)) => {}
                    // Out of time: the rest are listed, and read at a later fetch.
                    Err(_) => break,
                }
            }
            Ok(feed)
        }

        /// The feed at `url`, as its page lists it.
        pub async fn feed(&self, url: &str) -> Result<Feed> {
            Ok(self.read_feed(url).await?.0)
        }

        /// The feed at `url`, and the capsule (`name:port`) it turned out to be on.
        async fn read_feed(&self, url: &str) -> Result<(Feed, String)> {
            let page = self.get(&typed_url(url)?, feeds::MAX_FEED_BYTES, None).await?;
            let capsule = host_and_port(&page.url)?;
            let no_feed = "no feed found there: a Gemini feed is an Atom or RSS feed, or a page of links that \
                           start with a date (like 2026-05-01)";
            let feed = if page.mime == "text/gemini" {
                gemfeed(&page.text(), &page.url).context(no_feed)?
            } else {
                match feeds::parse(&page.body, page.url.as_str()) {
                    Ok(feed) => feed,
                    Err(_) => gemfeed(&page.text(), &page.url).context(no_feed)?,
                }
            };
            Ok((feed, capsule))
        }

        /// An entry's page, on the feed's `capsule`, as HTML for the reader.
        async fn article(&self, link: &str, capsule: &str) -> Result<String> {
            let page = self.get(&typed_url(link)?, MAX_PAGE_BYTES, Some(capsule)).await?;
            let text = page.text();
            Ok(match page.mime.as_str() {
                "text/gemini" => gemtext_to_html(&text, &page.url),
                // Relative links lead where they would on the capsule.
                "text/markdown" => {
                    format!("<base href=\"{}\">{}", escape(page.url.as_str()), html::markdown_to_html(&text))
                }
                "text/html" => format!("<base href=\"{}\">{text}", escape(page.url.as_str())),
                "text/plain" => format!("<pre>{}</pre>", escape(&text)),
                other => bail!("{other} isn't text"),
            })
        }

        /// The page at `url`, following redirects: to Gemini addresses only,
        /// and for an entry's page, only within the capsule (`stay`).
        async fn get(&self, url: &Url, max: usize, stay: Option<&str>) -> Result<Page> {
            match tokio::time::timeout(self.timeout, self.follow(url, max, stay)).await {
                Ok(page) => page,
                Err(_) => bail!("{} took too long to answer", host_and_port(url)?),
            }
        }

        async fn follow(&self, url: &Url, max: usize, stay: Option<&str>) -> Result<Page> {
            let mut url = url.clone();
            for _ in 0..=MAX_REDIRECTS {
                match self.request(&url, max).await? {
                    Answer::Content(meta, body) => return Ok(Page::new(url, &meta, body)),
                    Answer::Redirect(to) => {
                        let to = url.join(&to).ok().and_then(|to| typed_url(to.as_str()).ok());
                        url = to.context("redirected to something that isn't a Gemini address")?;
                        let stays = stay.is_none_or(|capsule| host_and_port(&url).is_ok_and(|to| to == capsule));
                        ensure!(stays, "redirected to another capsule");
                    }
                }
            }
            bail!("too many redirects")
        }

        /// One request: connects, sends the address and reads the answer.
        async fn request(&self, url: &Url, max: usize) -> Result<Answer> {
            ensure!(url.as_str().len() <= MAX_ADDRESS_BYTES, "the address is too long");
            let capsule = host_and_port(url)?;
            let tcp = self.connect(url).await?;
            let pin_key = pin_key(&capsule);
            let pinned = self.db.meta(&pin_key)?;
            let tofu = Arc::new(Tofu {
                authorities: authorities()?,
                pinned: pinned.clone(),
                accepted: Mutex::new(None),
                changed: AtomicBool::new(false),
            });
            let config = rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(tofu.clone())
                .with_no_client_auth();
            let name = ServerName::try_from(host(url)?).context("not a name a capsule can have")?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await;
            let mut tls = match tls {
                Ok(tls) => tls,
                Err(_) if tofu.changed.load(Ordering::Relaxed) => return Err(CertChanged(capsule).into()),
                Err(e) => return Err(e).with_context(|| format!("no secure connection to {capsule}")),
            };
            // Remembered from now on (again, if a certificate authority vouched for a new one).
            let accepted = tofu.accepted.lock().unwrap().clone();
            if accepted.is_some() && accepted != pinned {
                self.db.set_meta(&pin_key, &accepted.unwrap_or_default())?;
            }
            tls.write_all(format!("{url}\r\n").as_bytes()).await?;
            tls.flush().await?;

            let mut reader = BufReader::new(tls);
            let mut header = Vec::new();
            (&mut reader).take(MAX_HEADER_BYTES).read_until(b'\n', &mut header).await?;
            let not_gemini = || format!("{capsule} didn't answer like a Gemini capsule");
            // The whole line, or what follows would be taken for the page.
            ensure!(header.ends_with(b"\n"), not_gemini());
            let header = String::from_utf8_lossy(&header);
            let header = header.trim_end_matches(['\r', '\n']);
            let (status, meta) = header.split_once(' ').unwrap_or((header, ""));
            let meta = meta.trim();
            ensure!(status.len() == 2 && status.bytes().all(|b| b.is_ascii_digit()), not_gemini());
            match status.as_bytes()[0] {
                b'2' => {
                    let mut body = Vec::new();
                    // Many capsules hang up without saying so first, which isn't an error here.
                    match reader.take(max as u64 + 1).read_to_end(&mut body).await {
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
                        Err(e) => return Err(e.into()),
                    }
                    ensure!(body.len() <= max, "the download is too large (over {} KB)", max / 1024);
                    Ok(Answer::Content(meta.to_string(), body))
                }
                b'3' => Ok(Answer::Redirect(meta.to_string())),
                b'1' => bail!("{capsule} asks for something to be typed in, which a feed can't do"),
                b'6' => bail!("{capsule} asks for a client certificate, which Feedstr doesn't have"),
                _ => {
                    let reason: String = meta.chars().take(200).collect();
                    bail!("{capsule} answered with an error ({status} {reason})")
                }
            }
        }

        /// A connection to the capsule at `url`, unless that isn't allowed (see `local_ok`).
        async fn connect(&self, url: &Url) -> Result<TcpStream> {
            let capsule = host_and_port(url)?;
            let name = host(url)?;
            // A .onion capsule only through Tor (and .i2p never): otherwise its
            // name would only go to the DNS server before failing. Links and
            // redirects can lead to one from a capsule fetched another way.
            let route = if self.tor.is_some() { Route::Tor } else { Route::Direct };
            ensure!(
                feeds::reachable(route, url),
                "{capsule} can only be reached through {}, which this feed isn't fetched through",
                Route::required_by(url.as_str()).map_or("its network", |r| r.badge())
            );
            if let Some(proxy) = &self.tor {
                return self.connect_through_tor(url, proxy).await;
            }
            let lookup = tokio::net::lookup_host((name.as_str(), url.port().unwrap_or(PORT))).await;
            let addresses: Vec<SocketAddr> = lookup.with_context(|| format!("couldn't find {capsule}"))?.collect();
            // Going by where the name leads, whatever it looks like.
            let local = addresses.iter().any(|address| feeds::is_local_ip(address.ip()));
            ensure!(
                self.local_ok.get().unwrap_or(true) || !local,
                "{capsule} is on this device or its local network, where a capsule out on the internet can't send Feedstr"
            );
            let mut last = None;
            for address in addresses {
                match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
                    Ok(Ok(tcp)) => {
                        // The feed's own capsule decides, by the address that answered: a name
                        // leading both out to the internet and into the local network isn't local.
                        if self.local_ok.get().is_none() {
                            self.local_ok.set(Some(feeds::is_local_ip(address.ip())));
                        }
                        return Ok(tcp);
                    }
                    Ok(Err(e)) => last = Some(e.to_string()),
                    Err(_) => last = Some("it took too long".into()),
                }
            }
            bail!("couldn't connect to {capsule}: {}", last.unwrap_or_else(|| "no address".into()))
        }

        /// A connection to the capsule at `url` through Tor. Names are looked
        /// up by Tor, never here, so whether a capsule is on the local network
        /// can only be told from its address as written.
        async fn connect_through_tor(&self, url: &Url, proxy: &str) -> Result<TcpStream> {
            let capsule = host_and_port(url)?;
            let local = feeds::is_local_address(url);
            let local_ok = self.local_ok.get().unwrap_or(true);
            self.local_ok.set(Some(self.local_ok.get().unwrap_or(local)));
            ensure!(
                local_ok || !local,
                "{capsule} is on this device or its local network, where a capsule out on the internet can't send Feedstr"
            );
            let name = host(url)?;
            let target = (name.as_str(), url.port().unwrap_or(PORT));
            // Each capsule gets circuits of its own (see `proxy::isolation_tag`).
            let user = crate::proxy::isolation_tag(&name);
            let connect = tokio_socks::tcp::Socks5Stream::connect_with_password(proxy, target, &user, "x");
            match tokio::time::timeout(TOR_CONNECT_TIMEOUT, connect).await {
                Ok(Ok(stream)) => Ok(stream.into_inner()),
                // (Refused connections to the proxy come back as "unreachable".)
                Ok(Err(tokio_socks::Error::ProxyServerUnreachable)) => {
                    bail!("couldn't reach the Tor proxy at {proxy} (is it running?)")
                }
                Ok(Err(tokio_socks::Error::Io(e))) => {
                    bail!("couldn't reach the Tor proxy at {proxy} (is it running?): {e}")
                }
                Ok(Err(e)) => bail!("couldn't connect to {capsule} through Tor: {e}"),
                Err(_) => bail!("couldn't connect to {capsule} through Tor: it took too long"),
            }
        }
    }

    /// Whether a link is to a page on `capsule` (`name:port`).
    fn on_capsule(link: &str, capsule: &str) -> bool {
        typed_url(link).ok().and_then(|url| host_and_port(&url).ok()).is_some_and(|at| at == capsule)
    }

    /// The capsule's name (or IP address), in lowercase.
    fn host(url: &Url) -> Result<String> {
        let host = url.host_str().context("not a Gemini address")?;
        Ok(host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase())
    }

    /// `name:port` of the capsule, as it is named in messages and remembered.
    fn host_and_port(url: &Url) -> Result<String> {
        Ok(format!("{}:{}", host(url)?, url.port().unwrap_or(PORT)))
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::AtomicUsize;

        use rustls::pki_types::PrivateKeyDer;
        use rustls::pki_types::pem::PemObject;

        use super::*;

        /// A certificate for "localhost" signed by itself, as capsules have them, and its key.
        const CERT: &str = "-----BEGIN CERTIFICATE-----\n\
            MIIBmjCCAUGgAwIBAgIUXRw60hKNeN1P9yTBJoOjVvzyGeYwCgYIKoZIzj0EAwIw\n\
            FDESMBAGA1UEAwwJbG9jYWxob3N0MCAXDTI2MTExNjIzMDIwOFoYDzIxMjYxMDIz\n\
            MjMwMjA4WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggqhkjO\n\
            PQMBBwNCAARgGaGMCeaGiD+4YziOrf3Tf4HKOioIh6sntXdMVDP+ucAhZUnQ0CzR\n\
            5oUtoE1+Q7NHEI+I9caGNvmANkT9aA03o28wbTAdBgNVHQ4EFgQUR/CBgI8P4Qpi\n\
            3Vm4bKLngv7Ni4EwHwYDVR0jBBgwFoAUR/CBgI8P4Qpi3Vm4bKLngv7Ni4EwDwYD\n\
            VR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCgYIKoZI\n\
            zj0EAwIDRwAwRAIgTSdyd9HzbifYc3eGrmh/IHSxpdjXH5XvziBaG0DhgSUCIB3N\n\
            9ziAATBDmsB+wx4kc6MkfaTvygDpGz/ScOlOXbI4\n\
            -----END CERTIFICATE-----";
        const KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
            MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgs1hQHagGgGj26DJd\n\
            8ICuxQflsG7bQpOpWb96dAyzYJWhRANCAARgGaGMCeaGiD+4YziOrf3Tf4HKOioI\n\
            h6sntXdMVDP+ucAhZUnQ0CzR5oUtoE1+Q7NHEI+I9caGNvmANkT9aA03\n\
            -----END PRIVATE KEY-----";

        /// An answer that makes the test capsule never answer.
        const HANG: &str = "hang";

        /// A capsule on this computer that answers each address with
        /// `answer(address)` and hangs up (or never answers: [`HANG`]). Returns its port.
        async fn capsule(answer: impl Fn(&str) -> String + Send + Sync + 'static) -> u16 {
            let certs = vec![CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap()];
            let key = PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap();
            let config = rustls::ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let answer = Arc::new(answer);
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    let (acceptor, answer) = (acceptor.clone(), answer.clone());
                    tokio::spawn(async move {
                        let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                        let (mut request, mut byte) = (Vec::new(), [0u8; 1]);
                        while !request.ends_with(b"\r\n") {
                            if tls.read(&mut byte).await.unwrap_or(0) == 0 {
                                return;
                            }
                            request.push(byte[0]);
                        }
                        let address = String::from_utf8_lossy(&request).trim().to_string();
                        let answer = answer(&address);
                        if answer == HANG {
                            std::future::pending::<()>().await;
                        }
                        let _ = tls.write_all(answer.as_bytes()).await;
                        let _ = tls.shutdown().await;
                    });
                }
            });
            port
        }

        /// The path of a Gemini address.
        fn path(address: &str) -> &str {
            address.strip_prefix("gemini://").and_then(|rest| rest.find('/').map(|at| &rest[at..])).unwrap_or("")
        }

        #[tokio::test]
        async fn reads_a_gemlog_and_the_pages_of_its_new_entries() {
            let requests = Arc::new(AtomicUsize::new(0));
            let counted = requests.clone();
            let port = capsule(move |address| {
                counted.fetch_add(1, Ordering::Relaxed);
                match path(address) {
                    "/gemlog/" => {
                        "20 text/gemini\r\n# A gemlog\n=> a.gmi 2026-05-01 - First\n=> b.gmi 2026-05-02 Gone\n"
                    }
                    "/gemlog/a.gmi" => {
                        "20 text/gemini; lang=en\r\n# First\nThe text of the first post.\n=> b.gmi Next\n"
                    }
                    _ => "51 Not found\r\n",
                }
                .to_string()
            })
            .await;
            let db = Db::open_memory().unwrap();
            let url = format!("gemini://localhost:{port}/gemlog/");
            let feed = Client::new(&db).fetch(&url).await.unwrap();
            assert_eq!(feed.title, "A gemlog");
            let titles: Vec<&str> = feed.items.iter().map(|(_, item)| item.title.as_str()).collect();
            assert_eq!(titles, ["First", "Gone"]);
            // The first entry's page is its text, with links made whole; the second's couldn't be read.
            let next = format!("<a href=\"gemini://localhost:{port}/gemlog/b.gmi\">Next</a>");
            assert!(feed.items[0].1.content.contains("<p>The text of the first post.</p>"));
            assert!(feed.items[0].1.content.contains(&next), "{}", feed.items[0].1.content);
            assert_eq!(feed.items[1].1.content, "");
            assert_eq!(requests.load(Ordering::Relaxed), 3);
            // The capsule's certificate is remembered.
            assert!(db.meta(&pin_key(&format!("localhost:{port}"))).unwrap().is_some());
            // Entries stored with their text aren't read again; the one whose
            // page couldn't be read is tried again.
            db.set_subscribed(&url, true).unwrap();
            db.store_fetched(&url, &feed.title, &feed.site, &feed.items).unwrap();
            let again = Client::new(&db).fetch(&url).await.unwrap();
            assert_eq!(again.items.len(), 2);
            assert_eq!(requests.load(Ordering::Relaxed), 5);
        }

        #[tokio::test]
        async fn follows_redirects_to_an_atom_feed() {
            let port = capsule(|address| match path(address) {
                "/" => "31 /feeds/atom.xml\r\n".to_string(),
                "/feeds/atom.xml" => "20 application/atom+xml\r\n<?xml version=\"1.0\"?>\
                        <feed xmlns=\"http://www.w3.org/2005/Atom\"><title>Atom over Gemini</title>\
                        <entry><id>1</id><title>Post</title><link href=\"../posts/1.gmi\"/>\
                        <updated>2026-05-01T00:00:00Z</updated><content>Already here</content></entry></feed>"
                    .to_string(),
                "/elsewhere" => "30 https://web.example/feed\r\n".to_string(),
                "/loop" => "30 /loop\r\n".to_string(),
                "/input" => "10 Your name?\r\n".to_string(),
                "/big" => format!("20 text/gemini\r\n{}", "x".repeat(5000)),
                "/unended" => "20 text/gemini".to_string(),
                "/long" => format!("20 text/gemini; {}\r\n=> a.gmi 2026-05-01 A\n", "x".repeat(3000)),
                _ => "not a gemini answer\r\n".to_string(),
            })
            .await;
            let db = Db::open_memory().unwrap();
            let at = |path: &str| format!("gemini://localhost:{port}{path}");
            let feed = Client::new(&db).fetch(&at("")).await.unwrap();
            assert_eq!(feed.title, "Atom over Gemini");
            // Links are relative to where the feed ended up, and content that came with it is kept.
            assert_eq!(feed.items[0].1.link, at("/posts/1.gmi"));
            assert_eq!(feed.items[0].1.content, "Already here");
            let error = |path: &str| {
                let url = at(path);
                let db = &db;
                async move { format!("{:#}", Client::new(db).feed(&url).await.err().expect("refused")) }
            };
            assert!(error("/elsewhere").await.contains("isn't a Gemini address"));
            assert!(error("/loop").await.contains("too many redirects"));
            assert!(error("/input").await.contains("typed in"));
            assert!(error("/missing").await.contains("didn't answer like a Gemini capsule"));
            // A header has to be a whole line, and not a long one.
            assert!(error("/unended").await.contains("didn't answer like a Gemini capsule"));
            assert!(error("/long").await.contains("didn't answer like a Gemini capsule"));
            // More than was asked for at most is refused.
            let big = Client::new(&db).get(&typed_url(&at("/big")).unwrap(), 4000, None).await;
            assert!(format!("{:#}", big.err().expect("refused")).contains("too large"));
        }

        #[tokio::test]
        async fn stored_entries_keep_their_text_and_unread_ones_are_tried_again() {
            let requests = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = requests.clone();
            let down = Arc::new(AtomicBool::new(true));
            let is_down = down.clone();
            let today = chrono::Utc::now().format("%Y-%m-%d");
            let port = capsule(move |address| {
                seen.lock().unwrap().push(path(address).to_string());
                match path(address) {
                    "/" => format!(
                        "20 text/gemini\r\n=> a.gmi 2026-05-01 Old\n=> b.gmi {today} New\n=> c.gmi 2026-05-02 Lost\n"
                    ),
                    "/a.gmi" => "20 text/gemini\r\nOld text\n".into(),
                    "/b.gmi" if is_down.load(Ordering::Relaxed) => "40 Try later\r\n".into(),
                    "/b.gmi" => "20 text/plain\r\nNew <text>\n".into(),
                    _ => "51 Not found\r\n".into(),
                }
            })
            .await;
            let db = Db::open_memory().unwrap();
            let url = format!("gemini://localhost:{port}/");
            db.set_subscribed(&url, true).unwrap();
            let fetch_and_store = || async {
                let feed = Client::new(&db).fetch(&url).await.unwrap();
                db.store_fetched(&url, &feed.title, &feed.site, &feed.items).unwrap();
                feed
            };
            let feed = fetch_and_store().await;
            assert_eq!(feed.items[0].1.content, "<p>Old text</p>");
            assert_eq!((feed.items[1].1.content.as_str(), feed.items[2].1.content.as_str()), ("", ""));
            // Newest first.
            assert_eq!(*requests.lock().unwrap(), ["/", "/b.gmi", "/c.gmi", "/a.gmi"]);
            requests.lock().unwrap().clear();
            down.store(false, Ordering::Relaxed);
            // The stored text stays (twice over, as storing what was fetched would
            // otherwise empty it), and only entries without text are read again.
            for round in 0..2 {
                let feed = fetch_and_store().await;
                assert_eq!(feed.items[0].1.content, "<p>Old text</p>");
                assert_eq!(feed.items[1].1.content, "<pre>New &lt;text&gt;\n</pre>");
                assert_eq!(feed.items[2].1.content, "");
                let key = docs::item_key(&url, &feed.items[0].0);
                assert_eq!(db.item_content(&key).unwrap(), "<p>Old text</p>");
                let expected: &[&str] = if round == 0 { &["/", "/b.gmi", "/c.gmi"] } else { &["/", "/c.gmi"] };
                assert_eq!(*requests.lock().unwrap(), expected, "round {round}");
                requests.lock().unwrap().clear();
            }
        }

        #[tokio::test]
        async fn only_the_newest_entries_are_tried_again() {
            let requests = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = requests.clone();
            let port = capsule(move |address| {
                seen.lock().unwrap().push(path(address).to_string());
                match path(address) {
                    "/" => {
                        let links: String =
                            (1..=21).map(|day| format!("=> {day}.gmi 2026-01-{day:02} Day {day}\n")).collect();
                        format!("20 text/gemini\r\n{links}")
                    }
                    _ => "51 Not found\r\n".into(),
                }
            })
            .await;
            let db = Db::open_memory().unwrap();
            let url = format!("gemini://localhost:{port}/");
            db.set_subscribed(&url, true).unwrap();
            for _ in 0..2 {
                let feed = Client::new(&db).fetch(&url).await.unwrap();
                db.store_fetched(&url, &feed.title, &feed.site, &feed.items).unwrap();
                // The feed, then the pages of the 20 newest entries, never the oldest one's.
                let asked = std::mem::take(&mut *requests.lock().unwrap());
                assert_eq!(asked.len(), 21);
                assert!(!asked.contains(&"/1.gmi".to_string()) && asked[1] == "/21.gmi", "{asked:?}");
            }
        }

        #[tokio::test]
        async fn only_pages_on_the_feeds_own_capsule_are_read() {
            let requests = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen = requests.clone();
            let port = capsule(move |address| {
                seen.lock().unwrap().push(address.to_string());
                match path(address) {
                    "/" => "20 text/gemini\r\n=> gemini://127.0.0.1:1/x 2026-05-03 Elsewhere\n\
                            => gemini://other.example/y 2026-05-02 Far away\n=> away.gmi 2026-05-01 Moved\n"
                        .into(),
                    "/away.gmi" => "31 gemini://other.example/z\r\n".into(),
                    _ => "51 Not found\r\n".into(),
                }
            })
            .await;
            let db = Db::open_memory().unwrap();
            let url = format!("gemini://localhost:{port}/");
            let feed = Client::new(&db).fetch(&url).await.unwrap();
            // Listed, with their links, but not read; nor is a redirect off the capsule followed.
            assert_eq!(feed.items.len(), 3);
            assert!(feed.items.iter().all(|(_, item)| item.content.is_empty()));
            assert_eq!(*requests.lock().unwrap(), [url.clone(), format!("{url}away.gmi")]);
            let away = format!("gemini://localhost:{port}/away.gmi");
            let error = Client::new(&db).article(&away, &format!("localhost:{port}")).await.expect_err("refused");
            assert!(format!("{error:#}").contains("another capsule"));
        }

        #[tokio::test]
        async fn slow_capsules_cant_hold_up_a_sync() {
            let port = capsule(|address| match path(address) {
                "/" => "20 text/gemini\r\n=> a.gmi 2026-05-03 A\n=> b.gmi 2026-05-02 B\n=> c.gmi 2026-05-01 C\n".into(),
                "/slow" => HANG.into(),
                _ => HANG.into(),
            })
            .await;
            let db = Db::open_memory().unwrap();
            let url = format!("gemini://localhost:{port}/");
            // A page that doesn't come in time.
            let client = Client { timeout: Duration::from_millis(300), ..Client::new(&db) };
            let error = client.feed(&format!("{url}slow")).await.err().expect("refused");
            assert!(format!("{error:#}").contains("took too long"));
            // Entries whose pages don't come before the whole fetch's time is up are listed without them.
            let client = Client { budget: Duration::from_millis(500), ..Client::new(&db) };
            let started = std::time::Instant::now();
            let feed = client.fetch(&url).await.unwrap();
            assert!(started.elapsed() < Duration::from_secs(5));
            assert_eq!(feed.items.len(), 3);
            assert!(feed.items.iter().all(|(_, item)| item.content.is_empty()));
        }

        #[test]
        fn pages_in_latin_1_are_read_as_such() {
            let url = typed_url("gemini://capsule.example/").unwrap();
            let page = Page::new(url.clone(), "text/plain; charset=ISO-8859-1", b"caf\xe9".to_vec());
            assert_eq!((page.mime.as_str(), page.text().as_str()), ("text/plain", "café"));
            let page = Page::new(url, "", "café".as_bytes().to_vec());
            assert_eq!((page.mime.as_str(), page.text().as_str()), ("text/gemini", "café"));
        }

        #[tokio::test]
        async fn a_changed_certificate_is_refused_until_trusted() {
            let port = capsule(|_| "20 text/gemini\r\n=> a.gmi 2026-05-01 A\n".to_string()).await;
            let db = Db::open_memory().unwrap();
            let url = format!("gemini://localhost:{port}/");
            let pin = pin_key(&format!("localhost:{port}"));
            Client::new(&db).feed(&url).await.unwrap();
            let seen = db.meta(&pin).unwrap().unwrap();
            assert_eq!(seen.len(), 64);
            // As if another certificate had been seen there before.
            db.set_meta(&pin, &"0".repeat(64)).unwrap();
            let error = Client::new(&db).feed(&url).await.err().expect("refused");
            let changed: Option<&CertChanged> = error.chain().find_map(|cause| cause.downcast_ref());
            assert_eq!(changed.expect("says which capsule").0, format!("localhost:{port}"));
            // The remembered one stays until the user says otherwise.
            assert_eq!(db.meta(&pin).unwrap().unwrap(), "0".repeat(64));
            db.delete_meta(&pin).unwrap();
            Client::new(&db).feed(&url).await.unwrap();
            assert_eq!(db.meta(&pin).unwrap().unwrap(), seen);
        }

        #[tokio::test]
        async fn a_feed_on_the_internet_cant_lead_into_the_local_network() {
            let port = capsule(|_| "20 text/gemini\r\n=> a.gmi 2026-05-01 A\n".to_string()).await;
            let db = Db::open_memory().unwrap();
            let url = format!("gemini://localhost:{port}/");
            // A feed that is on this computer may be read...
            let client = Client::new(&db);
            client.feed(&url).await.unwrap();
            assert_eq!(client.local_ok.get(), Some(true));
            // ...but not on behalf of a feed that isn't.
            let client = Client::new(&db);
            client.local_ok.set(Some(false));
            let error = format!("{:#}", client.feed(&url).await.err().expect("refused"));
            assert!(error.contains("local network"), "{error}");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use client::pin_key;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn page() -> Url {
        Url::parse("gemini://capsule.example/gemlog/").unwrap()
    }

    #[test]
    fn recognizes_gemini_addresses() {
        assert!(is_gemini("gemini://capsule.example/feed") && is_gemini(" GEMINI://capsule.example"));
        assert!(!is_gemini("https://capsule.example/gemini://") && !is_gemini("gemini:"));
        assert_eq!(typed_url("gemini://capsule.example").unwrap().as_str(), "gemini://capsule.example/");
        assert_eq!(typed_url(" gemini://Capsule.Example/a#b ").unwrap().as_str(), "gemini://capsule.example/a");
        // Names outside ASCII are looked up in their xn-- form, so they're written that way too.
        assert_eq!(
            typed_url("gemini://MÜLLER.example:1965/").unwrap().as_str(),
            "gemini://xn--mller-kva.example:1965/"
        );
        assert_eq!(typed_url("gemini://[::1]:1966/x").unwrap().as_str(), "gemini://[::1]:1966/x");
        for bad in [
            "https://capsule.example/",
            "gemini://",
            "gemini://user@capsule.example/",
            "capsule.example",
            "gemini://evil.example%2Fx/",
            "gemini://a%40b.example/",
        ] {
            assert!(typed_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dates_far_ahead_are_unknown() {
        let feed = gemfeed("=> a.gmi 2999-01-01 Later\n=> b.gmi 2026-05-01 Sooner\n", &page()).unwrap();
        // Unknown (0): stored, it keeps the time it was first seen instead of staying on top.
        assert_eq!(feed.items[0].1.published, 0);
        assert!(feed.items[1].1.published > 0);
    }

    #[test]
    fn dated_links_are_entries() {
        let text = "# My gemlog\n## About stars\n\nSome words.\n\
                    => first.gmi 2026-05-01 - The first post\n\
                    => gemini://other.example/x 2026-05-02: From elsewhere\n\
                    => /about.gmi About me\n\
                    => undated.gmi 2026-13-45 Not a date\n\
                    =>second.gmi\t2026-05-03\n\
                    => first.gmi 2026-05-01 - The first post, again\n\
                    ```\n=> hidden.gmi 2026-05-04 In preformatted text\n```\n";
        let feed = gemfeed(text, &page()).unwrap();
        assert_eq!((feed.title.as_str(), feed.site.as_str()), ("My gemlog", "gemini://capsule.example/gemlog/"));
        let entries: Vec<(&str, &str)> = feed.items.iter().map(|(_, i)| (i.link.as_str(), i.title.as_str())).collect();
        assert_eq!(
            entries,
            [
                ("gemini://capsule.example/gemlog/first.gmi", "The first post"),
                ("gemini://other.example/x", "From elsewhere"),
                ("gemini://capsule.example/gemlog/second.gmi", ""),
            ]
        );
        // Noon (UTC) on the day, and the address is what tells entries apart.
        assert_eq!(feed.items[0].1.published, 1_777_636_800);
        assert_eq!(feed.items[0].0, "gemini://capsule.example/gemlog/first.gmi");
        // A page without dated links isn't a feed; without a heading, the capsule names it.
        assert!(gemfeed("# Home\n=> about.gmi About", &page()).is_none());
        assert_eq!(gemfeed("=> a.gmi 2026-05-01 A", &page()).unwrap().title, "capsule.example");
        // Dates that can't be right are unknown.
        let odd = gemfeed("=> a.gmi 9999-12-31 Future\n=> b.gmi 1900-01-01 Past\n", &page()).unwrap();
        assert!(odd.items.iter().all(|(_, item)| item.published == 0 && item.t == 0));
    }

    #[test]
    fn pages_become_html() {
        let text = "# Title\nA <b> & c\n\n=> next.gmi Next \"post\"\n=> https://web.example/\n* one\n* two\n\
                    > quoted\n```alt\n  <pre> text\n=> not.a.link\n```\n### Small\n#### Not a heading";
        assert_eq!(
            gemtext_to_html(text, &page()),
            "<h1>Title</h1><p>A &lt;b&gt; &amp; c</p>\
             <p><a href=\"gemini://capsule.example/gemlog/next.gmi\">Next &quot;post&quot;</a></p>\
             <p><a href=\"https://web.example/\">https://web.example/</a></p>\
             <ul><li>one</li><li>two</li></ul><blockquote><p>quoted</p></blockquote>\
             <pre>  &lt;pre&gt; text\n=&gt; not.a.link\n</pre><h3>Small</h3><p>#### Not a heading</p>"
        );
        // The reader shows it with its structure.
        let blocks = html::to_blocks(&gemtext_to_html("## Heading\n* item\n=> a.gmi A link", &page()));
        let texts: Vec<String> = blocks.iter().map(|b| b.text()).collect();
        assert_eq!(texts, ["Heading", "item", "A link"]);
    }
}
