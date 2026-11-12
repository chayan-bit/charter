//! Static files compiled into the binary, served with a one-year immutable cache; pages link
//! them with `?v=<content hash>` so a new build never serves a stale copy.
//!
//! `htmx.min.js` is htmx 2.0.11 (0BSD) from
//! `https://unpkg.com/htmx.org@2.0.11/dist/htmx.min.js`, sha256
//! `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717`.
//!
//! Fonts (OFL-1.1, licences in `assets/licenses/`), from github.com/google/fonts at
//! c30b221beb0c1da59d563feb91ec8656a1fa972a, subset to Latin-1 plus the punctuation,
//! arrows and math signs pages use (`pyftsubset`, woff2):
//! `schibsted-grotesk.woff2` from `ofl/schibstedgrotesk/SchibstedGrotesk[wght].ttf`
//! (weights 400 to 700), `plex-mono-400.woff2` and `plex-mono-500.woff2` from
//! `ofl/ibmplexmono/IBMPlexMono-{Regular,Medium}.ttf`.
//!
//! `icons.svg` is a sprite of 31 Phosphor "light" icons (MIT, `@phosphor-icons/core`
//! 2.1.1), each a `<symbol id="i-<name>">`; pages use `<use href="/assets/icons.svg?v=..#i-x">`.
//! `favicon.svg` is its seal-check symbol in the proof green.

use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

const FILES: [(&str, &str, &[u8]); 9] = [
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
    (
        "icons.svg",
        "image/svg+xml",
        include_bytes!("../assets/icons.svg"),
    ),
    (
        "favicon.svg",
        "image/svg+xml",
        include_bytes!("../assets/favicon.svg"),
    ),
    (
        "schibsted-grotesk.woff2",
        "font/woff2",
        include_bytes!("../assets/schibsted-grotesk.woff2"),
    ),
    (
        "plex-mono-400.woff2",
        "font/woff2",
        include_bytes!("../assets/plex-mono-400.woff2"),
    ),
    (
        "plex-mono-500.woff2",
        "font/woff2",
        include_bytes!("../assets/plex-mono-500.woff2"),
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
