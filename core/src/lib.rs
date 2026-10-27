//! Feedstr: a feed reader that syncs over Nostr.
//!
//! Everything except each platform's entry point and packaging lives here,
//! so the Linux, Android, Web and Windows builds can share it.

mod clock;
mod db;
mod docs;
mod feeds;
mod html;
mod import_export;
mod longform;
mod nostr;
mod sync;
mod ui;
mod worker;

pub use db::{Db, Store};
pub use import_export::MAX_IMPORT_BYTES;
pub use ui::App;
pub use worker::{FileKind, Platform, PlatformFuture, Remote, SignerApp};
