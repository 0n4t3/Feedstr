//! Fetching feeds through Tor or I2P.
//!
//! Each feed has a route, synced with the feed list (see
//! [`crate::docs::Subscriptions::routes`]): directly, through Tor, or through
//! I2P. The proxies themselves are this device's own (a Tor client or Orbot,
//! an I2P router), so their addresses are kept on each device.
//!
//! - **Tor**: a SOCKS5 proxy (`127.0.0.1:9050` by default: tor and Orbot).
//!   Names are looked up by Tor (`socks5h`), which `.onion` addresses need
//!   and which keeps lookups of other sites from leaving outside Tor.
//! - **I2P**: the router's HTTP proxy (`127.0.0.1:4444` by default: I2P and i2pd).
//!
//! Each site reached through Tor gets circuits of its own: its connections
//! carry their own SOCKS user name ([`isolation_tag`]), which Tor keeps
//! apart, so the sites a user follows can't be linked by a shared exit.
//!
//! Addresses ending in `.onion` always go through Tor and those ending in
//! `.i2p` through I2P, whatever the feed's setting says. A feed set to go
//! through a proxy never falls back to a direct connection: if the proxy
//! can't be reached, fetching it fails.
//!
//! **Everything through Tor** ([`tor_everything`], a setting of each
//! device): every connection Feedstr makes goes through Tor, except to
//! `.i2p` sites (through I2P): feeds, whatever their own route, nsites,
//! lookups (NIP-05, YouTube), and Nostr relays (syncing, reading posts,
//! remote signers). If Tor can't be reached, nothing gets through.
//!
//! The browser can't use proxies (nor fetch most feeds): there, these feeds'
//! articles come from the other devices, like those of other feeds.

use anyhow::{Context, Result};
use reqwest::Url;

use crate::db::Db;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    Direct,
    Tor,
    I2p,
}

impl Route {
    pub const ALL: [Route; 3] = [Route::Direct, Route::Tor, Route::I2p];

    /// As stored and synced.
    pub fn key(self) -> &'static str {
        match self {
            Route::Direct => "",
            Route::Tor => "tor",
            Route::I2p => "i2p",
        }
    }

    /// A stored route; `None` for one this version doesn't know (set by a newer one).
    pub fn parse(stored: &str) -> Option<Route> {
        Route::ALL.into_iter().find(|route| route.key() == stored)
    }

    pub fn label(self) -> &'static str {
        match self {
            Route::Direct => "Directly",
            Route::Tor => "Through Tor",
            Route::I2p => "Through I2P",
        }
    }

    /// Short, for the sidebar: "" when direct.
    pub fn badge(self) -> &'static str {
        match self {
            Route::Direct => "",
            Route::Tor => "Tor",
            Route::I2p => "I2P",
        }
    }

    /// The route a feed's own address requires: `.onion` sites are only
    /// reachable through Tor, `.i2p` sites only through I2P.
    pub fn required_by(url: &str) -> Option<Route> {
        let host = Url::parse(url.trim()).ok()?.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        if host.ends_with(".onion") {
            Some(Route::Tor)
        } else if host.ends_with(".i2p") {
            Some(Route::I2p)
        } else {
            None
        }
    }

    /// Where this device's proxy for the route is kept (`None` for direct).
    fn meta_key(self) -> Option<&'static str> {
        match self {
            Route::Direct => None,
            Route::Tor => Some("tor_proxy"),
            Route::I2p => Some("i2p_proxy"),
        }
    }

    /// The usual address of the route's proxy.
    pub fn default_proxy(self) -> &'static str {
        match self {
            Route::Direct => "",
            Route::Tor => "127.0.0.1:9050",
            Route::I2p => "127.0.0.1:4444",
        }
    }
}

/// How the feed at `url` is set to be fetched: through the network its
/// address requires, or as set for it (`stored`, see [`Route::key`]).
pub fn route_for(url: &str, stored: &str) -> Result<Route> {
    effective_route(url, stored, false)
}

/// How `url` is actually reached: as [`route_for`] says, unless everything
/// goes through Tor (`tor_everything`), when anything but a `.i2p` site does.
pub fn effective_route(url: &str, stored: &str, tor_everything: bool) -> Result<Route> {
    match Route::required_by(url) {
        Some(route) => Ok(route),
        None if tor_everything => Ok(Route::Tor),
        None => {
            Route::parse(stored).context("this feed is set to be fetched in a way this version of Feedstr doesn't know")
        }
    }
}

const TOR_EVERYTHING_KEY: &str = "tor_everything";

/// Whether every connection goes through Tor (except to `.i2p` sites):
/// this device's own setting. Never in the browser, which can't use Tor.
pub fn tor_everything(db: &Db) -> bool {
    !cfg!(target_arch = "wasm32") && db.meta(TOR_EVERYTHING_KEY).ok().flatten().as_deref() == Some("1")
}

pub fn set_tor_everything(db: &Db, on: bool) -> Result<()> {
    if on { db.set_meta(TOR_EVERYTHING_KEY, "1") } else { db.delete_meta(TOR_EVERYTHING_KEY) }
}

/// The proxy for connections to Nostr relays (syncing, reading posts, remote
/// signers): Tor's, if everything goes through Tor, else none. Relays get
/// the proxy's address as numbers (looked up here if it's a name, which only
/// says where the proxy is), and Tor looks up the relays' names.
#[cfg(not(target_arch = "wasm32"))]
pub fn relay_proxy(db: &Db) -> Result<Option<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    if !tor_everything(db) {
        return Ok(None);
    }
    let address = proxy_address(db, Route::Tor);
    let found: Vec<std::net::SocketAddr> = address.to_socket_addrs().map(Iterator::collect).unwrap_or_default();
    // "localhost" may come back as ::1 first, where tor usually doesn't listen.
    let chosen = found.iter().find(|a| a.is_ipv4()).or(found.first()).copied();
    chosen.map(Some).with_context(|| format!("couldn't find the Tor proxy at {address}"))
}

/// The browser can't use proxies.
#[cfg(target_arch = "wasm32")]
pub fn relay_proxy(_db: &Db) -> Result<Option<std::net::SocketAddr>> {
    Ok(None)
}

/// The address (`host:port`) of this device's proxy for `route`; empty for direct.
pub fn proxy_address(db: &Db, route: Route) -> String {
    let Some(key) = route.meta_key() else { return String::new() };
    db.meta(key).ok().flatten().filter(|a| !a.trim().is_empty()).unwrap_or_else(|| route.default_proxy().to_string())
}

/// Sets this device's proxy for `route` (`None` for the usual address).
pub fn set_proxy_address(db: &Db, route: Route, address: Option<&str>) -> Result<()> {
    let Some(key) = route.meta_key() else { return Ok(()) };
    match address {
        Some(address) => db.set_meta(key, address),
        None => db.delete_meta(key),
    }
}

/// A proxy's address as typed, as `host:port` (a `socks5://` or `http://`
/// in front is left out), or `None` if it isn't one.
pub fn parse_proxy_address(input: &str) -> Option<String> {
    let input = input.trim();
    let bare = input.split_once("://").map_or(input, |(_, rest)| rest).trim_end_matches('/');
    let url = Url::parse(&format!("http://{bare}")).ok()?;
    // The port has to be given (`url.port()` leaves out 80, HTTP's own).
    let has_port = bare
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    let valid = url.host_str().is_some_and(|h| !h.is_empty())
        && has_port
        && url.path() == "/"
        && url.username().is_empty()
        && url.query().is_none();
    valid.then(|| bare.to_string())
}

/// The SOCKS user name for a site's connections through Tor (`site` is its
/// host name): a different one per site gives each site circuits of its own.
/// Tor doesn't check user names; this only tells sites apart.
// Only native builds connect through Tor themselves.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn isolation_tag(site: &str) -> String {
    crate::docs::sha256_hex(site.to_ascii_lowercase())[..16].to_string()
}

/// Building HTTP clients that go through a proxy. Not in the browser.
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use anyhow::{Context, Result};

    use super::Route;

    /// Requests through Tor and I2P take longer than direct ones.
    const PROXIED_TIMEOUT: Duration = Duration::from_secs(90);

    /// HTTP clients for each proxy (and, through Tor, each site), made when
    /// first needed and made again if the proxy's address changes.
    #[derive(Default)]
    pub struct Clients {
        /// (route, proxy address, Tor isolation tag) -> client
        made: Mutex<HashMap<(Route, String, String), reqwest::Client>>,
    }

    impl Clients {
        /// The client for a feed on `site` (its host name) fetched through
        /// `route`'s proxy at `address` (`direct` for those fetched directly).
        pub fn get(
            &self,
            direct: &reqwest::Client,
            route: Route,
            address: &str,
            site: &str,
        ) -> Result<reqwest::Client> {
            if route == Route::Direct {
                return Ok(direct.clone());
            }
            let isolation = if route == Route::Tor { super::isolation_tag(site) } else { String::new() };
            let mut made = self.made.lock().unwrap();
            let key = (route, address.to_string(), isolation.clone());
            if let Some(client) = made.get(&key) {
                return Ok(client.clone());
            }
            // Only the proxy given: none from the system's settings (`proxy`
            // turns those off), and nothing ever goes around it.
            let proxy = match route {
                Route::Direct => unreachable!(),
                // socks5h: Tor looks up names, so .onion works and lookups
                // don't leak. The user name keeps this site's circuits apart.
                Route::Tor => reqwest::Proxy::all(format!("socks5h://{isolation}:x@{address}")),
                Route::I2p => reqwest::Proxy::all(format!("http://{address}")),
            }
            .with_context(|| format!("“{address}” isn't a proxy address"))?;
            let client = crate::feeds::http_client_builder(route)
                .proxy(proxy)
                .timeout(PROXIED_TIMEOUT)
                .build()
                .context("couldn't set up the proxy")?;
            // Clients for an address no longer used go.
            made.retain(|(r, a, _), _| *r != route || a == address);
            made.insert(key, client.clone());
            Ok(client)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::Clients;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onion_and_i2p_addresses_choose_their_network() {
        assert_eq!(route_for("http://abcdef.onion/feed.xml", "").unwrap(), Route::Tor);
        assert_eq!(route_for("http://Site.I2P./rss", "tor").unwrap(), Route::I2p);
        assert_eq!(route_for("gemini://abcdef.onion/", "").unwrap(), Route::Tor);
        assert_eq!(route_for("https://example.com/feed", "").unwrap(), Route::Direct);
        assert_eq!(route_for("https://example.com/feed", "tor").unwrap(), Route::Tor);
        // A route from a newer version isn't taken for a direct one.
        assert!(route_for("https://example.com/feed", "carrier-pigeon").is_err());
        // Only the end of the name counts.
        assert_eq!(route_for("https://onion.example.com/", "").unwrap(), Route::Direct);
    }

    #[test]
    fn everything_through_tor_but_i2p_sites() {
        assert_eq!(effective_route("https://example.com/feed", "", true).unwrap(), Route::Tor);
        assert_eq!(effective_route("https://example.com/feed", "i2p", true).unwrap(), Route::Tor);
        assert_eq!(effective_route("http://site.i2p/feed", "", true).unwrap(), Route::I2p);
        assert_eq!(effective_route("gemini://capsule.example/", "", true).unwrap(), Route::Tor);
        // Even a route from a newer version: Tor is the way for everything.
        assert_eq!(effective_route("https://example.com/feed", "carrier-pigeon", true).unwrap(), Route::Tor);
        assert_eq!(effective_route("https://example.com/feed", "", false).unwrap(), Route::Direct);
        let db = Db::open_memory().unwrap();
        assert!(!tor_everything(&db) && relay_proxy(&db).unwrap().is_none());
        set_tor_everything(&db, true).unwrap();
        assert!(tor_everything(&db));
        assert_eq!(relay_proxy(&db).unwrap(), Some("127.0.0.1:9050".parse().unwrap()));
        // A name is looked up, IPv4 first.
        set_proxy_address(&db, Route::Tor, Some("localhost:9150")).unwrap();
        assert_eq!(relay_proxy(&db).unwrap(), Some("127.0.0.1:9150".parse().unwrap()));
        set_tor_everything(&db, false).unwrap();
        assert!(!tor_everything(&db));
    }

    #[test]
    fn proxy_addresses_as_typed() {
        assert_eq!(parse_proxy_address(" 127.0.0.1:9050 ").as_deref(), Some("127.0.0.1:9050"));
        assert_eq!(parse_proxy_address("socks5://localhost:9150/").as_deref(), Some("localhost:9150"));
        assert_eq!(parse_proxy_address("http://[::1]:4444").as_deref(), Some("[::1]:4444"));
        assert_eq!(parse_proxy_address("localhost:80").as_deref(), Some("localhost:80"));
        assert_eq!(parse_proxy_address("[::1]"), None);
        assert_eq!(parse_proxy_address("127.0.0.1"), None);
        assert_eq!(parse_proxy_address("127.0.0.1:9050/path"), None);
        assert_eq!(parse_proxy_address(""), None);
    }

    #[test]
    fn each_site_gets_its_own_tor_circuits() {
        assert_eq!(isolation_tag("Example.com"), isolation_tag("example.com"));
        assert_ne!(isolation_tag("a.example"), isolation_tag("b.example"));
        let tag = isolation_tag("a.example");
        assert!(tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn proxies_are_kept_per_device_with_defaults() {
        let db = Db::open_memory().unwrap();
        assert_eq!(proxy_address(&db, Route::Tor), "127.0.0.1:9050");
        assert_eq!(proxy_address(&db, Route::I2p), "127.0.0.1:4444");
        set_proxy_address(&db, Route::Tor, Some("127.0.0.1:9150")).unwrap();
        assert_eq!(proxy_address(&db, Route::Tor), "127.0.0.1:9150");
        set_proxy_address(&db, Route::Tor, None).unwrap();
        assert_eq!(proxy_address(&db, Route::Tor), "127.0.0.1:9050");
        assert_eq!(proxy_address(&db, Route::Direct), "");
    }
}
