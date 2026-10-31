//! Importing and exporting files: OPML subscription lists (to move between
//! feed readers), and full backups of everything Feedstr stores.
//!
//! Backups reuse the sync documents, and importing one merges it the same way
//! a sync does: newer changes win and nothing is deleted, so importing an old
//! backup can't undo anything.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use quick_xml::escape::escape;
use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};

use crate::db::{Db, now_ms};
use crate::docs::{FeedItems, StateShard, Subscriptions};

/// The biggest file that's imported, so a wrong pick can't use up the memory
/// (and the most a backup may take, so every backup can be imported again).
/// MainActivity.java has the same number.
pub const MAX_IMPORT_BYTES: usize = 100 << 20;

const BACKUP_FORMAT: &str = "feedstr-backup";
const BACKUP_VERSION: u32 = 1;

/// A feed listed in an OPML file.
#[derive(Debug, Default, PartialEq)]
pub struct OpmlFeed {
    pub url: String,
    pub title: String,
    pub site: String,
    /// The folder the feed is in (the innermost one, if nested); empty if none.
    pub category: String,
}

/// Every feed in an OPML file, without duplicates.
pub fn parse_opml(xml: &str) -> Result<Vec<OpmlFeed>> {
    let mut reader = Reader::from_str(xml);
    let mut feeds: Vec<OpmlFeed> = Vec::new();
    let mut is_opml = false;
    // One entry per open <outline>: its name if it is a folder.
    let mut open: Vec<Option<String>> = Vec::new();
    loop {
        let event = reader.read_event().context("not a valid OPML file")?;
        let has_children = matches!(event, Event::Start(_));
        match event {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "opml" => is_opml = true,
            Event::End(e) if e.local_name().as_ref() == "outline" => {
                open.pop();
            }
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "outline" => {
                let mut feed = OpmlFeed::default();
                let mut title = String::new();
                for attr in e.attributes() {
                    let attr = attr.context("not a valid OPML file")?;
                    let value = attr.normalized_value(XmlVersion::Implicit1_0)?.trim().to_string();
                    match attr.key.local_name().as_ref().to_ascii_lowercase().as_str() {
                        "xmlurl" => feed.url = value,
                        "text" => feed.title = value,
                        "title" => title = value,
                        "htmlurl" => feed.site = value,
                        _ => {}
                    }
                }
                if feed.title.is_empty() {
                    feed.title = title;
                }
                feed.category = open.iter().rev().flatten().next().cloned().unwrap_or_default();
                if has_children {
                    open.push(feed.url.is_empty().then(|| feed.title.clone()));
                }
                if !feed.url.is_empty() && !feeds.iter().any(|f| f.url == feed.url) {
                    feeds.push(feed);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !is_opml {
        bail!("not an OPML file");
    }
    Ok(feeds)
}

/// An OPML file with one folder per category.
pub fn write_opml(feeds: &[OpmlFeed]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<opml version=\"2.0\">\n  <head>\n    <title>Feedstr subscriptions</title>\n  </head>\n  <body>\n",
    );
    let outline = |feed: &OpmlFeed, indent: &str| {
        let (title, url, site) = (escape(&feed.title), escape(&feed.url), escape(&feed.site));
        format!(
            "{indent}<outline type=\"rss\" text=\"{title}\" title=\"{title}\" xmlUrl=\"{url}\" htmlUrl=\"{site}\"/>\n"
        )
    };
    let mut categories: Vec<&str> = feeds.iter().map(|f| f.category.as_str()).filter(|c| !c.is_empty()).collect();
    categories.sort_by_key(|c| c.to_lowercase());
    categories.dedup();
    for category in categories {
        let name = escape(category);
        out += &format!("    <outline text=\"{name}\" title=\"{name}\">\n");
        for feed in feeds.iter().filter(|f| f.category == category) {
            out += &outline(feed, "      ");
        }
        out += "    </outline>\n";
    }
    for feed in feeds.iter().filter(|f| f.category.is_empty()) {
        out += &outline(feed, "    ");
    }
    out + "  </body>\n</opml>\n"
}

/// The subscribed feeds as an OPML file.
pub fn export_opml(db: &Db) -> Result<String> {
    let mut feeds: Vec<OpmlFeed> = db
        .all_feeds()?
        .into_iter()
        // Other feed readers can't follow Nostr feeds.
        .filter(|feed| feed.subscribed && !crate::longform::is_nostr(&feed.url))
        .map(|feed| OpmlFeed {
            title: if feed.title.is_empty() { feed.url.clone() } else { feed.title },
            url: feed.url,
            site: feed.site,
            category: feed.category,
        })
        .collect();
    feeds.sort_by_key(|f| f.title.to_lowercase());
    Ok(write_opml(&feeds))
}

pub struct OpmlImport {
    /// Feeds that weren't subscribed before; they still need fetching.
    pub added: Vec<String>,
    pub already_subscribed: usize,
}

/// Subscribes to every feed in an OPML file. Its folders become categories,
/// except for feeds that already have one.
pub fn import_opml(db: &Db, xml: &str) -> Result<OpmlImport> {
    let known = db.all_feeds()?;
    let mut import = OpmlImport { added: Vec::new(), already_subscribed: 0 };
    for feed in parse_opml(xml)? {
        let existing = known.iter().find(|f| f.url == feed.url);
        if existing.is_some_and(|f| f.subscribed) {
            import.already_subscribed += 1;
        } else {
            db.set_subscribed(&feed.url, true)?;
            db.set_title_if_empty(&feed.url, &feed.title)?;
            import.added.push(feed.url.clone());
        }
        if !feed.category.is_empty() && existing.is_none_or(|f| f.category.is_empty()) {
            db.set_category(&feed.url, &feed.category)?;
        }
    }
    Ok(import)
}

#[derive(Serialize, Deserialize)]
struct Backup {
    format: String,
    version: u32,
    /// Milliseconds since the epoch.
    exported_at: i64,
    subscriptions: Subscriptions,
    /// Every feed with all of its locally stored items.
    feeds: BTreeMap<String, FeedItems>,
    /// Read/favorite state of every item.
    state: StateShard,
}

/// Everything Feedstr stores for this account, except the Sync Key and device settings.
pub fn export_backup(db: &Db) -> Result<String> {
    // Everything this device has, including feeds it doesn't sync.
    let mut subscriptions = db.local_subscriptions()?;
    // The salt names this account's documents on relays, and an erase is about
    // this account's relay data too; neither is part of the content.
    subscriptions.salt = None;
    subscriptions.erased_at = None;
    let mut feeds = BTreeMap::new();
    for feed in db.all_feeds()? {
        feeds.insert(feed.url.clone(), db.feed_backup(&feed.url)?);
    }
    let backup = Backup {
        format: BACKUP_FORMAT.into(),
        version: BACKUP_VERSION,
        exported_at: now_ms(),
        subscriptions,
        feeds,
        state: db.all_state()?,
    };
    let json = serde_json::to_string_pretty(&backup)?;
    if json.len() > MAX_IMPORT_BYTES {
        bail!("the backup would be too big to import again (over {} MB)", MAX_IMPORT_BYTES >> 20);
    }
    Ok(json)
}

/// Turns an imported file into text, refusing files too big to import.
pub fn imported_text(bytes: Vec<u8>) -> Result<String> {
    if bytes.len() > MAX_IMPORT_BYTES {
        bail!("that file is too big to import (over {} MB)", MAX_IMPORT_BYTES >> 20);
    }
    String::from_utf8(bytes).context("not a text file")
}

pub struct BackupImport {
    pub feeds: usize,
    pub items: usize,
}

/// Merges a backup into the database.
pub fn import_backup(db: &Db, json: &str) -> Result<BackupImport> {
    #[derive(Deserialize)]
    struct Header {
        format: String,
        version: u32,
    }
    let header: Header = serde_json::from_str(json).context("not a Feedstr backup")?;
    if header.format != BACKUP_FORMAT {
        bail!("not a Feedstr backup");
    }
    if header.version > BACKUP_VERSION {
        bail!("this backup was made by a newer version of Feedstr");
    }
    let mut backup: Backup = serde_json::from_str(json).context("the backup is damaged")?;
    backup.subscriptions.salt = None;
    backup.subscriptions.erased_at = None;
    db.merge_subscriptions(&backup.subscriptions)?;
    for (url, feed) in &backup.feeds {
        db.merge_feed_items(url, feed)?;
    }
    db.import_state(&backup.state)?;
    let items = backup.feeds.values().map(|f| f.items.len()).sum();
    Ok(BackupImport { feeds: backup.subscriptions.feeds.values().filter(|s| s.0).count(), items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::View;
    use crate::docs::{self, Item};

    #[test]
    fn parses_opml_with_folders() {
        let xml = r#"<?xml version="1.0"?>
            <opml version="1.0"><head><title>Mine</title></head><body>
              <outline text="Tech">
                <outline type="rss" text="A &amp; B" xmlUrl="https://a.example/feed?x=1&amp;y=2" htmlUrl="https://a.example"/>
                <outline type="rss" title="Only title" xmlurl="https://b.example/rss"></outline>
              </outline>
              <outline type="rss" text="Duplicate" xmlUrl="https://b.example/rss"/>
              <outline text="Not a feed"/>
            </body></opml>"#;
        let feeds = parse_opml(xml).unwrap();
        assert_eq!(
            feeds,
            [
                OpmlFeed {
                    url: "https://a.example/feed?x=1&y=2".into(),
                    title: "A & B".into(),
                    site: "https://a.example".into(),
                    category: "Tech".into(),
                },
                OpmlFeed {
                    url: "https://b.example/rss".into(),
                    title: "Only title".into(),
                    site: String::new(),
                    category: "Tech".into(),
                },
            ]
        );
        assert!(parse_opml("<html><body>hi</body></html>").is_err());
        assert!(parse_opml("not xml at all <<<").is_err());
    }

    #[test]
    fn opml_round_trip() {
        let feed = |url: &str, category: &str| OpmlFeed {
            url: url.into(),
            title: "<Tom's> feed".into(),
            site: String::new(),
            category: category.into(),
        };
        let feeds = vec![feed("https://x.example/f?a=1&b=\"2\"", "News & more"), feed("https://y.example/", "")];
        assert_eq!(parse_opml(&write_opml(&feeds)).unwrap(), feeds);
    }

    #[test]
    fn opml_import_and_export() {
        let db = Db::open_memory().unwrap();
        db.set_subscribed("https://old.example/feed", true).unwrap();
        let xml = write_opml(&[
            OpmlFeed { url: "https://old.example/feed".into(), ..Default::default() },
            OpmlFeed { url: "https://new.example/feed".into(), title: "New".into(), ..Default::default() },
        ]);
        let import = import_opml(&db, &xml).unwrap();
        assert_eq!((import.added, import.already_subscribed), (vec!["https://new.example/feed".to_string()], 1));
        let exported = parse_opml(&export_opml(&db).unwrap()).unwrap();
        assert!(exported.iter().all(|f| f.category.is_empty()));
        let titles: Vec<&str> = exported.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, ["https://old.example/feed", "New"]);
    }

    fn sample() -> (Db, String, String) {
        let db = Db::open_memory().unwrap();
        let feed = "https://example.com/feed";
        db.set_subscribed(feed, true).unwrap();
        let published = now_ms() / 1000;
        // An old item too: backups hold everything, not just what's cached on relays.
        let items: Vec<(String, Item)> = [("a", published), ("b", 1)]
            .into_iter()
            .map(|(guid, published)| {
                (guid.to_string(), Item { title: guid.into(), published, t: published * 1000, ..Default::default() })
            })
            .collect();
        db.store_fetched(feed, "Example", "https://example.com", &items).unwrap();
        let (a, b) = (docs::item_key(feed, "a"), docs::item_key(feed, "b"));
        db.set_read(std::slice::from_ref(&a), true).unwrap();
        db.set_fav(&b, true).unwrap();
        db.ensure_salt().unwrap();
        (db, a, b)
    }

    #[test]
    fn backup_restores_everything_on_another_device() {
        let (db, a, b) = sample();
        let backup = export_backup(&db).unwrap();
        let other = Db::open_memory().unwrap();
        let import = import_backup(&other, &backup).unwrap();
        assert_eq!((import.feeds, import.items), (1, 2));
        let items = other.items(&View::All, false).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.key == a && !i.unread));
        assert!(items.iter().any(|i| i.key == b && i.fav));
        assert_eq!(other.feeds().unwrap()[0].title, "Example");
        // The salt stays with the account it belongs to.
        assert!(other.salt().unwrap().is_none());
    }

    #[test]
    fn importing_an_old_backup_keeps_newer_changes() {
        let (db, a, _) = sample();
        let backup = export_backup(&db).unwrap();
        db.set_read(std::slice::from_ref(&a), false).unwrap();
        db.set_subscribed("https://example.com/feed", false).unwrap();
        import_backup(&db, &backup).unwrap();
        let items = db.items(&View::Feed("https://example.com/feed".into()), false).unwrap();
        assert!(items.iter().any(|i| i.key == a && i.unread));
        assert!(db.subscribed_feeds().unwrap().is_empty());
    }

    #[test]
    fn refuses_files_too_big_to_import() {
        assert_eq!(imported_text(b"<opml/>".to_vec()).unwrap(), "<opml/>");
        assert!(imported_text(vec![b' '; MAX_IMPORT_BYTES + 1]).is_err());
        assert!(imported_text(vec![0xff, 0xfe]).is_err());
    }

    #[test]
    fn rejects_other_files() {
        let db = Db::open_memory().unwrap();
        assert!(import_backup(&db, "{\"hello\": 1}").is_err());
        assert!(import_backup(&db, "<opml/>").is_err());
        let newer = r#"{"format":"feedstr-backup","version":99}"#;
        assert!(import_backup(&db, newer).err().unwrap().to_string().contains("newer version"));
    }

    #[test]
    fn opml_folders_become_categories() {
        let db = Db::open_memory().unwrap();
        db.set_subscribed("https://kept.example/feed", true).unwrap();
        db.set_category("https://kept.example/feed", "Mine").unwrap();
        let xml = write_opml(&[
            OpmlFeed { url: "https://kept.example/feed".into(), category: "Theirs".into(), ..Default::default() },
            OpmlFeed { url: "https://new.example/feed".into(), category: "News".into(), ..Default::default() },
        ]);
        import_opml(&db, &xml).unwrap();
        assert_eq!(db.categories().unwrap(), ["Mine", "News"]);
        let exported = parse_opml(&export_opml(&db).unwrap()).unwrap();
        let categories: Vec<&str> = exported.iter().map(|f| f.category.as_str()).collect();
        assert_eq!(categories, ["Mine", "News"]);
    }

    #[test]
    fn importing_a_backup_never_erases() {
        let (db, _, _) = sample();
        let backup = export_backup(&db).unwrap();
        assert!(!backup.contains("erased_at"));
        // Even a hand-edited backup that says "erased" doesn't erase anything.
        let mut json: serde_json::Value = serde_json::from_str(&backup).unwrap();
        json["subscriptions"]["erased_at"] = (now_ms() + 1_000_000).into();
        import_backup(&db, &json.to_string()).unwrap();
        assert_eq!(db.erased_at().unwrap(), 0);
        assert_eq!(db.subscribed_feeds().unwrap().len(), 1);
    }
}
