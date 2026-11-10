//! Static files compiled into the binary, served with a one-year immutable cache; pages link
//! them with `?v=<content hash>` so a new build never serves a stale copy.
//!
//! `htmx.min.js` is htmx 2.0.11 (0BSD) from
//! `https://unpkg.com/htmx.org@2.0.11/dist/htmx.min.js`, sha256
//! `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717`.

use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

const FILES: [(&str, &str, &[u8]); 4] = [
    (
        "app.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/app.css"),
    ),
    (
        "keys.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/keys.js"),
    ),
    (
        "live.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/live.js"),
    ),
    (
        "htmx.min.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../assets/htmx.min.js"),
    ),
];

/// Short hash of every asset, for cache-busting links.
pub fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        FILES.iter().for_each(|(_, _, b)| b.hash(&mut h));
        format!("{:08x}", h.finish() as u32)
    })
}

pub async fn serve(Path(name): Path<String>) -> Response {
    match FILES.iter().find(|(n, _, _)| *n == name) {
        Some((_, ct, body)) => (
            [
                (header::CONTENT_TYPE, *ct),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            *body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
