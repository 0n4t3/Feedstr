## Warning!
*Feedstr is completely vibe coded, and for now, barely tested. **I DO NOT RECCOMEND** using it at this time, unless you just want to test. I intend to use it myself for a while, and maybe after a few months of daily driving I'll update this warning if it's working well and I intend to support it long term.*

# Feedstr
Feedstr was born out of the highly personalized annoyance of wanting private RSS feed sync, but not wanting to host my own FreshRSS instance (the only publicly accessible service on my home server right now). Feedstr attempts to solve that, syncing feeds, read/unread status, and favorites using the decentralized Nostr protocol - relying on either community relays (default), or a multi purpose self hosted relay.

## RSS++
In addition to standard RSS/ATOM feeds, Feedstr supports following additional content:
- **Nostr NIP-23 Blog Posts** - both of all followed users (enabled/disabled), and individually added feeds
- **YouTube channels**, which can be added by pasting a YouTube channel link, which can serve either all content or only standard videos (filtering out shorts & livestreams)
- Blogs on **Tor & I2P**, using your local Tor/I2P router as a proxy
- **Gemini Capsules**, which can be followed like standard HTTP websites

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

## Potential Future Features
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

## Installation
coming soon

## Usage
coming soon

## Credit:
Many functions of this app were built by skilled human authors. Credit to the authors of the following libraries:
* coming soon
