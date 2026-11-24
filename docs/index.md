---
layout: home
title: Feedstr
---

<img src="logo.png" alt="Feedstr logo" width="112" height="112">

Mildy improved reading. A feed reader, written in Rust, built to fetch content over a number of different protocols, with optional E2E sync.

[Usage guide](usage/)

**Warning!** *Feedstr is heavily AI assisted, and has only had a couple weeks of real world usage. Use with caution.*

# Feedstr
Feedstr was born out of the highly personalized annoyance of wanting private RSS feed sync, but not wanting to host my own FreshRSS instance (the only publicly accessible service on my home server at that time). Since then, however, it's also grown to support a handful of additional protocols beyond standard RSS. Feedstr offers both optional E2E encrypted sync (relying on either community relays or self hosted relays), and supports the following additonal protocols:

- **Nostr NIP-23 Blogs** - both of all followed users (enabled/disabled), and individually added feeds
- **YouTube channels**, which can be added by pasting a YouTube channel link, following either all content or only standard videos (filtering out shorts & livestreams)
- Feeds on **Tor & I2P**, using your local Tor/I2P router as a proxy for Onion/I2P domains
- **Gemini Capsules**, which can be followed like standard HTTP websites
- **NSites**, which can be followed via RSS links within them, or by listing page updates if no RSS link on said NSite is present 

## Installation
* Linux: [Download the latest release](https://github.com/0n4t3/Feedstr/releases), and install flatpak if you don't already have it installed. Navigate to the folder you saved it in, and run `flatpak install --user ./[Feedstr Release]`
* Android: [Download the latest release](https://github.com/0n4t3/Feedstr/releases), using your browser or Obtanium. *Note: [Keep Android Open](https://keepandroidopen.org/). I don't have a Google approved key, if you want to install it you'll need to enable doing so - or better yet - switch to a version of non-fisher-price Android like Graphene or Lineage.*
* Web: [Open the web client](https://feedstr.dns7.top/client/) in any WebGL supported browser. *NOTE: The web client is unable to fetch standard feeds (only Nostr NIP-23 feeds) due to browser sandboxing. It requires a Desktop/Mobile device to fetch feeds and sync them first.*

#### Updates
There's currently no means of automatically updating Feedstr, aside from Android via Obtanium. [Add the repo's RSS feed](https://github.com/0n4t3/Feedstr/releases.atom) to your Feedstr's installation to track updates and manually install updates.

## Usage

#### Setting Up:
* Don't want to sync? In the desktop or Android app, choose "Disable Sync" on the first screen and Feedstr works as a plain feed reader, no Nostr key needed. You can turn on sync later in the "Sync" settings, and what you already have comes along.
* If you don't know what the heck a Nostr is, choose "Create a sync key" at the bottom and save it in your password manager. *(you do use a password manager, right? because you should.)* This is a private key, it keeps your data end to end encrypted. Don't lose it or give it to anyone else.
* If you do know what a Nostr is, sign in with your nsec bunker - or if you're on the web client - your web extension. Configure your relays in the "Sync" settings (such as adding any paid/self-hosted relays you use), and you'll likely want to enable displaying NIP-23 blog posts from users you already follow in the "Socials" section.

#### Adding Content
In Settings/"Import & Export" you can import an OPML subscription list. You can also export both OPML or all data from Feedstr, which I'd recommend doing as a backup once you get things how you like them. You can also add feeds individually with the "+" button:
* **Standard feeds** over HTTPS(s) or the Gemini protocol work like any other feed reader. Add a link to the feed; if you add a root (sub)domain Feedstr will try to locate the RSS link itself.
* Feedstr fetches **YouTube** feeds automatically. Paste a channel link and it'll locate the RSS, and give you the option to show all content or filter out non-standard content (shorts & livestreams).
* **Nostr NIP-23** blogs can be followed directly, without effecting your Nostr-wide follow list. Add them by adding the user's NPUB or NIP-05 username.
* **NSites** can be followed with `nsite:npub` or with a gateway URL. If the NSite has an RSS feed, Feedstr will use that. If it doesn't, it'll track all page updates.
* **Tor & I2P** sites can be followed as well, and any .onion or .i2p domain will automatically be tagged to be fetched using those protocols.

#### Tor & I2P Setup
Rolling your own darknet router is probably one step below the rolling your own cryptography on the 'please don't do that' ladder, so Feedstr relies on your Tor & I2P router for fetching content on those networks. See the [Tor & I2P section](https://feedstr.dns7.top/usage/#tor-and-i2p) of the guide. Once setup, if the router is running it'll fetch any feeds on those networks. Additionally, in the Tor/I2P settings you can set the program to use Tor to fetch all clearnet content as well as Onion services.

Nothing will break, however, if you fetch your feeds without running Tor or I2P. You obviously won't see feed updates, but the feature was designed with the intention that you may only actually have Tor & I2P running on some devices but not others, or only occasionally. All your other feeds will fetch, and you'll be able to see the existing Tor/I2P feeds that were previously fetched and cached.

## Features
- **Zero tracking:** no outgoing connections ever, aside from fetching feeds
  and syncing
- **End-to-end encrypted sync:** no need to trust a community relay
- **Linux (Flatpak), Android, and web** support
- **A lightweight, multi-purpose server:** a Nostr relay can do quite a bit
  more than just sync feeds
- **OPML** import and export
- **Sandboxing:** bare-bones permissions for the Flatpak and Android
  versions, and total sandboxing in the web version
- **Local Only categories:** Fine tune what's actually synced

#### Potential Future Features
- Windows builds
- PWA support
- Social bookmarking and quotes (optional)
- Fetching links and bookmarks from ActivityPub, AT Protocol and Nostr users
- A tool to import and export full feed data (read status, favorites, etc.)
  from FreshRSS and other readers
- A page archive tool
- Text To Speech reading
- Feed subgroups
- Text-to-speech narration

## Building
Coming soon. For now, ask a clanker.
