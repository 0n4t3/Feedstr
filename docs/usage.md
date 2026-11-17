---
layout: page
title: Usage guide
permalink: /usage/
---

This guide walks through setting up Feedstr and using its features. Feedstr
works the same way on Linux, Android and the web, apart from the differences
noted under [Platform notes](#platform-notes).

- [Installing](#installing)
- [Signing in](#signing-in)
- [Adding feeds](#adding-feeds)
- [Reading](#reading)
- [Organizing feeds](#organizing-feeds)
- [Syncing](#syncing)
- [Tor and I2P](#tor-and-i2p)
- [Filters and the feed report](#filters-and-the-feed-report)
- [Appearance](#appearance)
- [Import, export and backups](#import-export-and-backups)
- [Deleting your data](#deleting-your-data)
- [Platform notes](#platform-notes)
- [Keyboard shortcuts](#keyboard-shortcuts)

## Installing

Downloads are on the
[latest release](https://github.com/0n4t3/feedstr/releases/latest).

- **Linux:** download `feedstr.flatpak` and install it with
  `flatpak install feedstr.flatpak`, then run it from your app launcher or
  with `flatpak run top.dns7.feedstr.Feedstr`.
- **Android:** download the APK and install it. Your phone may ask you to
  allow installing apps from your browser or file manager first.
- **Web:** open the [web client](https://feedstr.dns7.top/client/) in your browser, or host it yourself (see
  [Platform notes](#web)).

## Signing in

Feedstr has no accounts. Your data is synced under a Nostr key and
encrypted to it, so whoever holds the key can read it. When you first open
Feedstr, choose one of these:

- **Create a Sync Key:** Feedstr makes a new key (`nsec1…`) for you. Copy it
  somewhere safe, like a password manager. If you lose it, your synced data
  can't be recovered, and anyone who has it can read your data.
- **Sync Key:** paste a key you already have, such as one created on another
  device or your existing Nostr key.
- **Remote signer:** paste a `bunker://…` address (NIP-46). The key stays with
  the signer and never touches this device.
- **Signer app** (Android only, shown when one is installed): Amber or another
  NIP-55 signer app keeps the key. Feedstr asks it for permission to sign and
  encrypt its own data. Approve this so it can sync in the background.
- **Browser extension** (web only): a NIP-07 extension that supports NIP-44.

To use Feedstr on another device, sign in there with the same key or signer.

Feedstr remembers how you signed in. If signing in fails when the app starts
(for example, a signer app was uninstalled), Feedstr retries a few times.
After that it shows a bar with **Try again** and **Sign out**. You can keep
reading in the meantime, but nothing syncs until it's fixed.

**Signing out** uploads your remaining changes first. If some changes
couldn't be saved to a relay, Feedstr asks before discarding them.

## Adding feeds

Use **Add feed** and paste an address. You can choose a category for the
feed at the same time. Feedstr accepts:

- **Websites and feeds:** an RSS, Atom or JSON Feed address, or a site's
  address (Feedstr looks for its feed).
- **YouTube channels:** a channel's page (`youtube.com/@name`,
  `/channel/UC…`, `/c/…` or `/user/…`) or its feed address. Tick **Hide
  livestreams and Shorts** to follow only regular videos.
- **Nostr writers:** an npub, nprofile or NIP-05 address
  (`name@example.com`). Their long-form posts (NIP-23) become a feed. You
  don't need to follow them on Nostr.
- **Websites on Nostr (nsites):** `nsite:npub1…`, `nsite:npub1…/name`, or the
  site's address on a gateway (like `https://npub1….nsite.lol`). If the site
  has its own feed, you get that. Otherwise each new or changed page shows up
  as an article.
- **Gemini feeds:** a `gemini://` address, either a gemlog's page of dated
  links or a feed served over Gemini. See [Gemini](#gemini) below.
- **`.onion` and `.i2p` addresses:** these are always fetched through Tor or
  I2P. See [Tor and I2P](#tor-and-i2p).

### Following everyone you follow on Nostr

In **Settings → Socials**, turn on the **Nostr Follows** feed. It collects the
long-form posts of everyone in your Nostr follow list (from your other Nostr
apps). At the top level of the sidebar it shows as a group with one entry per
person. Put it in a category to show it as a single feed instead.

### Gemini

Gemini posts rarely include their text in the feed, so Feedstr reads the
pages of the 20 newest entries and shows them as articles. Older entries
appear with just their link.

- **Open in Gemini browser** hands the address to your Gemini browser, like
  Lagrange. If you don't have one, Feedstr tells you and you can copy the
  address instead.
- Feedstr remembers the certificate each capsule showed the first time. If it
  changes, the feed stops updating and **Settings → Feed report** offers
  **Trust the new certificate**. Capsules renew their certificates now and
  then, but a change can also mean someone else is answering in its place.

## Reading

Choose a feed or category in the sidebar to see its articles, then open an
article to read it.

- **Unread filter:** show only unread articles. Each device remembers whether
  it's on.
- **Search:** the search box above the article list filters the current view
  by title, author and text.
- **Swipe** an article sideways in the list, with a finger or by dragging
  with the mouse. Swipe left to mark it read (or unread again), or right to
  star it (or unstar it).
- **Select several articles** by long-pressing one (or right-click or
  Ctrl+click on desktop and web). While any are selected, tap to add or remove
  more. The bar above the list can then mark them read or unread, star or
  unstar them, or select every listed article. Press Back or Esc to cancel.
- **Mark all as read** covers every unread article in the view that matches
  the search, including ones not loaded in the list yet.
- **Mark older as read** (the ✔ menu above the list, or right-click or
  long-press a feed or category) marks unread articles older than a number of
  days as read.
- **Open in browser** opens an article's web page. Nostr posts also offer
  **Open in Ditto** and **Open in Nostr app**.
- **Sharing:** on Android, **Share…** opens the system share sheet. On desktop
  and web, **Copy URL** copies the link.

The article list shows the newest 1,000 articles and loads more as you scroll
to the end. Long status messages at the top are cut short; tap or click one
to read all of it.

## Organizing feeds

### Categories

- Right-click a feed and choose **Move to category**, or pick a category when
  adding a feed.
- Click a category in the sidebar to read all of its feeds together.
- Right-click a category to rename or remove it.
- You can fold categories up in the sidebar. Each device remembers which ones
  are folded.

Categories sync to your other devices and become folders in OPML exports.

### Local Only categories

To keep a category's feeds on one device, right-click it and choose **Make
Local Only**, or untick it in **Settings → Sync → Groups**. Your other
devices drop those feeds, and their articles aren't cached on relays.
Read/unread and favorite state still syncs. Syncing the group again shares
this device's version with your other devices.

### Unsubscribing

Right-click (or long-press) a feed in the sidebar and choose
**Unsubscribe**. The [feed report](#feed-report) also has
an unsubscribe button for each feed it lists.

## Syncing

### When Feedstr syncs

- **At start:** it downloads changes from your relays, fetches every feed,
  and uploads what changed.
- **When you press Sync** (or R).
- **On a schedule,** if you choose one in **Settings → Sync**: every 15 or 30
  minutes, every hour, or every 3 hours.
- **On close:** it uploads your remaining changes. The window waits up to 30
  seconds, or you can quit without saving. Unsaved changes stay on the device
  and go out with the next sync.

Automatic syncs wait until 10 minutes after the last complete one, so
reopening the app often doesn't sync every time. Pressing Sync always syncs.

While syncing, the status at the top shows what Feedstr is doing, with a
progress bar.

**Manual only** (in **Settings → Sync**) turns off all automatic syncing.
Nothing syncs until you press Sync, except when you sign in or out.

### Relays

Relays are the servers your encrypted data is stored on. Feedstr starts with
relay.ditto.pub, nos.lol and nostr.mom. You can add or remove relays in
**Settings → Sync**, and **Use the default relays** goes back to the
defaults. Your relay list syncs to your other devices.

### What's stored where

Both settings are in **Settings → Sync**:

- **Cached on relays:** how many recent articles of each feed are saved to
  relays (by default up to 50 per feed from the last 30 days). Favorites are
  always included. This is what the web version shows, and it applies to all
  your devices.
- **Stored on this device:** everything is kept by default. You can choose to
  delete articles after a number of days. Favorites, articles still in their
  feed, and articles cached on relays are always kept.

Read/unread state older than 60 days isn't synced. Favorites always are.

## Tor and I2P

Desktop and Android can fetch feeds through Tor or I2P, using proxies running
on your device. The web version can't.

1. Install and run Tor (the tor service on Linux, or Orbot on Android) and/or
   an I2P router (I2P or i2pd).
2. Check the proxy addresses in **Settings → Tor & I2P**. The defaults are
   `127.0.0.1:9050` for Tor and `127.0.0.1:4444` for I2P. If you use Tor
   Browser's Tor, use `127.0.0.1:9150`.
3. When adding a feed, choose **Through Tor** or **Through I2P** instead of
   **Directly**. For an existing feed, right-click (or long-press) it in the
   sidebar and use **Fetch it**. The sidebar then shows "· Tor" or "· I2P"
   after the feed's name.

Things to know:

- `.onion` addresses always go through Tor and `.i2p` addresses through I2P,
  whatever you choose.
- A feed set to use a proxy is never fetched without it. If the proxy isn't
  running, the feed fails to update and says which proxy it couldn't reach.
- Which feeds use Tor or I2P syncs to your other devices. The proxy addresses
  are set on each device.
- Gemini feeds can use Tor but not I2P.
- Fetching clearnet feeds through I2P needs your router's outproxy.
- Only the feed goes through the proxy. Links you open from its articles open
  in your browser as usual.

### Send everything through Tor

The **Send everything through Tor** switch in **Settings → Tor & I2P** sends
every connection Feedstr makes through Tor: all feeds, nsites, Nostr relays
(sync included), remote signers, and NIP-05 and YouTube lookups. `.i2p` sites
still go through I2P. If Tor isn't running, nothing is fetched or synced.
While it's on, unencrypted relays (`ws://`) and `.i2p` relays aren't used.

## Filters and the feed report

### Filters

In **Settings → Filters**, hide articles that contain a word or phrase, in
all feeds, one category or one feed. Matching ignores case, matches whole
words only, and looks at titles and article text. Favorites are never
hidden. Filters sync to your other devices.

### Feed report

**Settings → Feed report** lists feeds whose last update failed, that have no
posts, or whose newest post is older than a number of days (365 by default).
Each one has buttons to copy its address or unsubscribe.

## Appearance

In **Settings → Appearance**:

- **Interface size:** from 50% to 200%.
- **Article list layout:** list, compact list (one line per article), cards
  (a grid with a text preview) or compact cards. You can also switch layouts
  from the ▦ menu above the article list. With cards, an article opens full
  width; press Back or Esc to return to the cards.
- **Theme:** system, light or dark.

## Import, export and backups

In **Settings → Import & export**:

- **Feeds (OPML):** move your subscriptions to or from another feed reader.
  Categories become folders, and folders become categories. Nostr feeds are
  left out of OPML exports.
- **Full backup (JSON):** your feeds, every stored article, read/unread state
  and favorites. Importing a backup merges it with what you have: newer
  changes win and nothing is deleted, so an old backup can't undo anything.
  Backups don't include your Sync Key, relay list or device settings.

Files over 100 MB can't be imported. Imported data goes to your relays with
the next sync, or when you close the app.

## Deleting your data

**Settings → Account → Delete all Feedstr data** removes your feeds, stored
articles, read/unread state, favorites and settings from your relays and
from this device, then signs you out. Your other devices erase their copies
when they next sync. Only Feedstr's data is deleted, not your Nostr profile
or posts.

## Platform notes

### Linux

Your data is stored in `~/.local/share/feedstr/` (or
`~/.var/app/top.dns7.feedstr.Feedstr/data/feedstr/` for the Flatpak).

### Android

- On narrow screens Feedstr shows one pane at a time: feeds, articles or an
  article. ☰ switches to the feeds.
- Changes are saved to your relays whenever the app goes to the background.
  A sync still under way when you switch apps finishes in the background,
  with a "Syncing…" notification if it takes more than a few seconds.
- Coming back to the app after 15 minutes or more syncs like a fresh start.
- A **Paste** button shows above the keyboard while you edit a text field,
  for keyboards without a clipboard.
- While your Sync Key is shown in full, Feedstr is hidden from screenshots,
  screen recordings and the recent apps list.
- App data isn't included in Android's cloud backups, because it contains
  your Sync Key. Use a [full backup](#import-export-and-backups) instead.

### Web

Browsers can't fetch most feeds, so the web version shows the articles your
other devices cached on relays (see [What's stored where](#whats-stored-where)).
It can read, star, search and organize them, change settings, and import and
export. It fetches Nostr long-form posts and nsites by itself. A feed added
on the web is subscribed to everywhere: your other devices fetch it, and its
articles show up after they sync.

- **Remember me** (on by default) keeps your data and sign-in in the browser,
  so your next visit starts signed in. Untick it to keep nothing. Signing out
  also deletes what's kept.
- Changes are uploaded a few seconds after you make them.
- The web version syncs when the page opens and every 10 minutes while it's
  open, unless you change this in **Settings → Sync**.
- If you open Feedstr in a second tab, the first one steps aside and offers
  **Use it here**.
- YouTube channels can only be added by their `/channel/UC…` address or feed
  address, not `@name`.
- Feedstr needs WebGL.

**Hosting it yourself:** serve the files from `feedstr-web.tar.gz` over HTTPS
at an address of their own (a separate domain or subdomain, not a folder next
to other pages). With **Remember me**, your Sync Key is kept in the browser's
storage for that address, where any page at the same address could read it.
A folder is only safe next to pages you control that run no scripts: that's
how this site serves the [web client](https://feedstr.dns7.top/client/),
on a domain of its own whose other pages are plain text.
If your host lets you set headers, also send
`Content-Security-Policy: frame-ancestors 'none'` and
`X-Content-Type-Options: nosniff`.

## Keyboard shortcuts

| Key | Action |
|---|---|
| R | Sync |
| Ctrl + / Ctrl − | Make the interface bigger or smaller |
| Ctrl 0 | Reset the interface size |
| Esc | Go back (close settings, an article opened from cards, or a selection) |
