//! The time, and waiting, natively and in the browser (where std's clocks
//! panic and tokio has no timer).

use std::time::Duration;

/// Milliseconds since the epoch.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_millis() as i64
}

/// Milliseconds since the epoch.
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: Duration) {
    // setTimeout takes at most about 24 days; callers waiting longer just wait again.
    let ms = duration.as_millis().min(i32::MAX as u128) as i32;
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let global = js_sys::global();
        let set_timeout: js_sys::Function =
            js_sys::Reflect::get(&global, &"setTimeout".into()).expect("setTimeout exists").into();
        let _ = set_timeout.call2(&global, &resolve, &ms.into());
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}
