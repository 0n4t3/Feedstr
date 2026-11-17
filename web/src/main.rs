//! Feedstr for the web: a way to read on a computer without the app.
//!
//! It runs the shared app (`feedstr-core`) in the browser. Browsers can't read
//! most feeds, so it shows what the other devices cached on relays (and
//! fetches Nostr long-form posts itself). The database lives in memory and,
//! with "Remember me", is kept between visits as a snapshot in IndexedDB.

#[cfg(target_arch = "wasm32")]
mod browser;

#[cfg(target_arch = "wasm32")]
fn main() {
    browser::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("The web version runs in a browser: build it with `trunk build --release` in web/.");
}
