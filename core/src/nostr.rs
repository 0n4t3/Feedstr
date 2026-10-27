//! Signing in, and the relay side of [`crate::sync::Transport`].

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use nostr_connect::client::NostrConnect;
use nostr_sdk::prelude::*;

use crate::sync::{RawEvent, Transport};
use crate::worker::SignerApp;

/// A relay slower than this is skipped for the round (it catches up next time).
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const BUNKER_TIMEOUT: Duration = Duration::from_secs(120);

/// Tags per NIP-09 deletion request.
const DELETION_TAGS_PER_EVENT: usize = 100;

pub const DEFAULT_RELAYS: &[&str] = &["wss://relay.ditto.pub", "wss://nos.lol", "wss://nostr.mom"];

pub enum Signer {
    /// A Sync Key (nsec) stored on this device.
    Keys(Keys),
    /// A remote signer (NIP-46 bunker).
    Bunker(Box<NostrConnect>),
    /// A browser extension (NIP-07), on the web.
    #[cfg(target_arch = "wasm32")]
    Extension(nostr_browser_signer::BrowserSigner),
    /// A signer app on the device (NIP-55 on Android, like Amber).
    App(Arc<dyn SignerApp>),
}

impl Signer {
    pub fn bunker(uri: &str, app_key: &Keys) -> Result<Self> {
        let uri = NostrConnectUri::parse(uri.trim()).context("not a valid bunker:// address")?;
        Ok(Signer::Bunker(Box::new(NostrConnect::new(uri, app_key.clone(), BUNKER_TIMEOUT, None)?)))
    }

    /// The browser's Nostr extension (NIP-07).
    #[cfg(target_arch = "wasm32")]
    pub fn extension() -> Result<Self> {
        let signer = nostr_browser_signer::BrowserSigner::new();
        Ok(Signer::Extension(signer.map_err(|_| anyhow!("no Nostr browser extension found"))?))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn extension() -> Result<Self> {
        bail!("browser extensions only work in the web version")
    }

    pub async fn public_key(&self) -> Result<PublicKey> {
        Ok(match self {
            Signer::Keys(keys) => keys.public_key(),
            Signer::Bunker(bunker) => bunker.get_public_key_async().await?,
            #[cfg(target_arch = "wasm32")]
            Signer::Extension(extension) => extension.get_public_key_async().await?,
            Signer::App(app) => PublicKey::parse(app.public_key().await?.trim())?,
        })
    }

    async fn sign(&self, unsigned: UnsignedEvent) -> Result<Event> {
        Ok(match self {
            Signer::Keys(keys) => keys.sign_event_async(unsigned).await?,
            Signer::Bunker(bunker) => bunker.sign_event_async(unsigned).await?,
            #[cfg(target_arch = "wasm32")]
            Signer::Extension(extension) => extension.sign_event_async(unsigned).await?,
            Signer::App(app) => signed_by_app(unsigned, app.as_ref()).await?,
        })
    }

    async fn encrypt(&self, to: &PublicKey, text: &str) -> Result<String> {
        Ok(match self {
            Signer::Keys(keys) => keys.nip44_encrypt_async(to, text).await?,
            Signer::Bunker(bunker) => bunker.nip44_encrypt_async(to, text).await?,
            #[cfg(target_arch = "wasm32")]
            Signer::Extension(extension) => extension.nip44_encrypt_async(to, text).await?,
            Signer::App(app) => app.nip44_encrypt(text.to_string(), to.to_hex()).await?,
        })
    }

    async fn decrypt(&self, from: &PublicKey, payload: &str) -> Result<String> {
        Ok(match self {
            Signer::Keys(keys) => keys.nip44_decrypt_async(from, payload).await?,
            Signer::Bunker(bunker) => bunker.nip44_decrypt_async(from, payload).await?,
            #[cfg(target_arch = "wasm32")]
            Signer::Extension(extension) => extension.nip44_decrypt_async(from, payload).await?,
            Signer::App(app) => app.nip44_decrypt(payload.to_string(), from.to_hex()).await?,
        })
    }
}

/// Has a signer app sign `unsigned`, making sure the answer is that event,
/// properly signed: the app may answer with the whole event or the signature.
async fn signed_by_app(mut unsigned: UnsignedEvent, app: &dyn SignerApp) -> Result<Event> {
    unsigned.ensure_id();
    let answer = app.sign_event(unsigned.as_json(), unsigned.pubkey.to_hex()).await?;
    app_signature(unsigned, answer.trim())
}

fn app_signature(unsigned: UnsignedEvent, answer: &str) -> Result<Event> {
    let event = match Event::from_json(answer) {
        Ok(event) => event,
        Err(_) => unsigned
            .clone()
            .add_signature(Signature::from_hex(answer).context("the signer app's answer isn't a signature")?)?,
    };
    event.verify().context("the signer app's signature doesn't check out")?;
    let same = Some(event.id) == unsigned.id && event.pubkey == unsigned.pubkey;
    anyhow::ensure!(same, "the signer app signed something else");
    Ok(event)
}

pub struct NostrTransport {
    client: Client,
    signer: Signer,
    pubkey: PublicKey,
    relays: Vec<String>,
}

impl NostrTransport {
    pub async fn new(signer: Signer, relays: Vec<String>) -> Result<Self> {
        let pubkey = signer.public_key().await?;
        let client = Client::default();
        let mut added = Vec::new();
        for url in relays {
            match client.add_relay(&url).await {
                Ok(_) => added.push(url),
                Err(e) => log::warn!("skipping relay {url}: {e}"),
            }
        }
        client.connect().await;
        Ok(Self { client, signer, pubkey, relays: added })
    }

    pub fn pubkey(&self) -> PublicKey {
        self.pubkey
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }

    /// One REQ with a filter per document.
    async fn request(&self, relay: &Relay, wanted: &[(String, Option<u64>)]) -> Result<Vec<Event>> {
        let base = Filter::new().kind(Kind::ApplicationSpecificData).author(self.pubkey);
        let filters: Vec<Filter> = wanted
            .iter()
            .map(|(d, since)| {
                let filter = base.clone().identifier(d);
                match since {
                    Some(since) => filter.since(Timestamp::from_secs(*since)),
                    None => filter,
                }
            })
            .collect();
        let started = crate::clock::now_ms();
        let events = relay.fetch_events(filters).timeout(FETCH_TIMEOUT).await?;
        // The stream ends without an error on timeout or disconnect too, but
        // only an EOSE proves we have seen everything the relay holds.
        let elapsed = Duration::from_millis((crate::clock::now_ms() - started).max(0) as u64);
        if elapsed >= FETCH_TIMEOUT || !relay.status().is_connected() {
            bail!("no complete answer");
        }
        Ok(events.into_iter().collect())
    }
}

impl Transport for NostrTransport {
    type Event = Event;

    fn relays(&self) -> Vec<String> {
        self.relays.clone()
    }

    async fn fetch(&self, url: &str, wanted: &[(String, Option<u64>)]) -> Result<Vec<RawEvent>> {
        let relay = self.client.relay(url).await?.ok_or_else(|| anyhow!("unknown relay"))?;
        if !relay.status().is_connected() {
            relay.try_connect().timeout(CONNECT_TIMEOUT).await.context("offline")?;
        }
        let mut raw = Vec::new();
        for event in self.request(&relay, wanted).await? {
            let d = event.tags.identifier().unwrap_or_default();
            let ours = event.pubkey == self.pubkey && event.kind == Kind::ApplicationSpecificData;
            if !ours || !wanted.iter().any(|(w, _)| *w == d) || event.verify().is_err() {
                continue;
            }
            let created_at = event.created_at.as_secs();
            raw.push(RawEvent { id: event.id.to_hex(), d, created_at, payload: event.content });
        }
        Ok(raw)
    }

    async fn decrypt(&self, payload: &str) -> Result<String> {
        self.signer.decrypt(&self.pubkey, payload).await
    }

    async fn prepare(&self, d: &str, created_at: u64, content: &str) -> Result<(String, Event)> {
        let encrypted = self.signer.encrypt(&self.pubkey, content).await?;
        let unsigned = EventBuilder::new(Kind::ApplicationSpecificData, encrypted)
            .tag(Tag::identifier(d))
            .custom_created_at(Timestamp::from_secs(created_at))
            .finalize_unsigned(self.pubkey);
        let event = self.signer.sign(unsigned).await?;
        Ok((event.id.to_hex(), event))
    }

    async fn prepare_deletion(&self, ds: &[String], ids: &[String], created_at: u64) -> Result<Vec<Event>> {
        let coordinates = ds.iter().map(|d| Coordinate::new(Kind::ApplicationSpecificData, self.pubkey).identifier(d));
        let ids = ids.iter().filter_map(|id| EventId::from_hex(id).ok());
        let mut tags: Vec<Tag> = coordinates.map(|c| Tag::coordinate(c, None)).chain(ids.map(Tag::event)).collect();
        let mut events = Vec::new();
        // Several smaller requests rather than one event too large for relays.
        while !tags.is_empty() {
            let chunk: Vec<Tag> = tags.drain(..tags.len().min(DELETION_TAGS_PER_EVENT)).collect();
            let unsigned = EventBuilder::new(Kind::EventDeletion, "Feedstr data deleted by its owner")
                .tags(chunk)
                .custom_created_at(Timestamp::from_secs(created_at))
                .finalize_unsigned(self.pubkey);
            events.push(self.signer.sign(unsigned).await?);
        }
        Ok(events)
    }

    async fn send(&self, relay: &str, event: &Event) -> Result<(), String> {
        match self.client.send_event(event).to([relay]).save_into_database(false).await {
            Ok(output) if !output.success.is_empty() => Ok(()),
            Ok(output) => Err(output.failed.into_values().next().unwrap_or_else(|| "no answer".into())),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::local_relay::MockRelay;

    use super::*;
    use crate::db::{Db, View};
    use crate::docs::Item;
    use crate::sync::{Options, sync};

    const PUSH: Options = Options { publish: true, throttle_feed_caches: true };

    #[test]
    fn signer_app_answers_are_checked() {
        let keys = Keys::generate();
        let unsigned = |text: &str| {
            let mut event = EventBuilder::new(Kind::TextNote, text).finalize_unsigned(keys.public_key());
            event.ensure_id();
            event
        };
        let signed = keys.sign_event(unsigned("hi")).unwrap();
        // Either the whole event or just its signature will do.
        assert_eq!(app_signature(unsigned("hi"), &signed.as_json()).unwrap().id, signed.id);
        let sig = signed.sig.to_string();
        assert_eq!(app_signature(unsigned("hi"), &sig).unwrap().id, signed.id);
        // Something else signed, a forged signature, or nonsense, won't.
        assert!(app_signature(unsigned("bye"), &signed.as_json()).is_err());
        assert!(app_signature(unsigned("bye"), &sig).is_err());
        let other = Keys::generate();
        let theirs =
            other.sign_event(EventBuilder::new(Kind::TextNote, "hi").finalize_unsigned(other.public_key())).unwrap();
        assert!(app_signature(unsigned("hi"), &theirs.as_json()).is_err());
        let mut forged = serde_json::to_value(&signed).unwrap();
        forged["content"] = "changed".into();
        assert!(app_signature(unsigned("hi"), &forged.to_string()).is_err());
        assert!(app_signature(unsigned("hi"), "rejected").is_err());
    }

    #[tokio::test]
    async fn syncs_through_real_relays() {
        let (r1, r2) = (MockRelay::run().await.unwrap(), MockRelay::run().await.unwrap());
        let relays = vec![r1.url().await.to_string(), r2.url().await.to_string()];
        let keys = Keys::generate();
        let feed = "https://example.com/feed";

        let a = Db::open_memory().unwrap();
        a.set_subscribed(feed, true).unwrap();
        let item = Item { title: "Hello".into(), published: 1_900_000_000, t: 1, ..Default::default() };
        a.store_fetched(feed, "Example", "", &[("1".into(), item)]).unwrap();
        let key = crate::docs::item_key(feed, "1");
        a.set_fav(&key, true).unwrap();
        let ta = NostrTransport::new(Signer::Keys(keys.clone()), relays.clone()).await.unwrap();
        let report = sync(&a, &ta, PUSH).await.unwrap();
        assert_eq!((report.relays_ok, report.problems.len()), (2, 0), "{report:?}");

        // Another device with only one relay still reachable.
        r1.shutdown();
        let b = Db::open_memory().unwrap();
        let tb = NostrTransport::new(Signer::Keys(keys), relays).await.unwrap();
        let report = sync(&b, &tb, PUSH).await.unwrap();
        assert_eq!(report.relays_ok, 1, "{report:?}");
        let items = b.items(&View::All, false).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].fav && items[0].title == "Hello");

        // Once in sync, nothing is republished.
        assert_eq!(sync(&b, &tb, PUSH).await.unwrap().published, 0);
    }

    #[derive(Debug)]
    struct ApproveAll;

    impl nostr_connect::signer::NostrConnectSignerActions for ApproveAll {
        fn approve(&self, _: &PublicKey, _: &NostrConnectRequest) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn syncs_through_remote_signer() {
        use nostr_connect::signer::{NostrConnectKeys, NostrConnectRemoteSigner};
        let relay = MockRelay::run().await.unwrap();
        let url = relay.url().await.to_string();
        let user = Keys::generate();
        let bunker_keys = NostrConnectKeys::new(Keys::generate(), user.clone());
        let bunker = NostrConnectRemoteSigner::new(bunker_keys, [url.as_str()], None, None).unwrap();
        let uri = bunker.bunker_uri().to_string();
        tokio::spawn(async move { bunker.serve(ApproveAll).await });
        // NIP-46 requests are ephemeral: one sent before the signer has subscribed is lost.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let db = Db::open_memory().unwrap();
        db.set_subscribed("https://example.com/feed", true).unwrap();
        let signer = Signer::bunker(&uri, &Keys::generate()).unwrap();
        let transport = NostrTransport::new(signer, vec![url.clone()]).await.unwrap();
        assert_eq!(transport.pubkey(), user.public_key());
        let report = sync(&db, &transport, PUSH).await.unwrap();
        assert!(report.problems.is_empty() && report.published > 0, "{report:?}");

        // The data is readable with the user's own key, so other devices can sign in either way.
        let other = Db::open_memory().unwrap();
        let direct = NostrTransport::new(Signer::Keys(user), vec![url]).await.unwrap();
        sync(&other, &direct, PUSH).await.unwrap();
        assert_eq!(other.subscribed_feeds().unwrap(), ["https://example.com/feed"]);
    }
}
