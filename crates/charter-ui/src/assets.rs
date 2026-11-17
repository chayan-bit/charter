//! Static files compiled into the binary, served with a one-year immutable cache; pages link
//! them with `?v=<content hash>` so a new build never serves a stale copy.
//!
//! `htmx.min.js` is htmx 2.0.11 (0BSD) from
//! `https://unpkg.com/htmx.org@2.0.11/dist/htmx.min.js`, sha256
//! `d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717`.
//!
//! Fonts (OFL-1.1, licence in `assets/licenses/geist-OFL.txt`), Geist and Geist Mono variable
//! fonts from github.com/vercel/geist-font (`fonts/Geist/variable/Geist[wght].ttf`,
//! `fonts/GeistMono/variable/GeistMono[wght].ttf`), subset to Latin-1 plus the punctuation,
//! arrows and math signs pages use (fontTools `subset`, woff2): `geist.woff2` and
//! `geist-mono.woff2`, both with the weight axis (100 to 900) kept.
//!
//! `icons.svg` is a sprite of 42 Phosphor "light" icons (MIT, `@phosphor-icons/core`
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
        "search.css",
        "text/css; charset=utf-8",
        include_bytes!("../assets/search.css"),
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
        "geist.woff2",
        "font/woff2",
        include_bytes!("../assets/geist.woff2"),
    ),
    (
        "geist-mono.woff2",
        "font/woff2",
        include_bytes!("../assets/geist-mono.woff2"),
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

/// The CSS with its comments removed and its whitespace squeezed (none around `{ } ; , >`
/// or after `:`, one space elsewhere): the source keeps both for readers, the pages do not pay
/// for them.
fn strip_css(src: &str) -> String {
    let mut bare = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find("/*") {
        bare.push_str(&rest[..i]);
        rest = rest[i + 2..]
            .split_once("*/")
            .map_or("", |(_, after)| after);
    }
    bare.push_str(rest);
    let mut out = String::with_capacity(bare.len());
    let (mut space, mut prev) = (false, ' ');
    for c in bare.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space
            && !out.is_empty()
            && !matches!(prev, '{' | '}' | ';' | ',' | '>' | ':')
            && !matches!(c, '{' | '}' | ';' | ',' | '>')
        {
            out.push(' ');
        }
        space = false;
        out.push(c);
        prev = c;
    }
    let mut out = out.replace(";}", "}");
    out.push('\n');
    out
}

/// What `/assets/<name>` sends: the file, with CSS stripped once.
fn served(name: &'static str, body: &'static [u8]) -> &'static [u8] {
    static CSS: OnceLock<Vec<(&'static str, Vec<u8>)>> = OnceLock::new();
    if !name.ends_with(".css") {
        return body;
    }
    let all = CSS.get_or_init(|| {
        FILES
            .iter()
            .filter(|(n, _, _)| n.ends_with(".css"))
            .map(|(n, _, b)| (*n, strip_css(&String::from_utf8_lossy(b)).into_bytes()))
            .collect()
    });
    all.iter()
        .find(|(n, _)| *n == name)
        .map_or(body, |(_, b)| b)
}

pub async fn serve(Path(name): Path<String>) -> Response {
    match FILES.iter().find(|(n, _, _)| *n == name) {
        Some((n, ct, body)) => (
            [
                (header::CONTENT_TYPE, *ct),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            served(n, body),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_loses_comments_and_whitespace_but_no_rule() {
        let src = "/* head */\n.a { color: red; margin: 0 1px; }\n\n  @media (x) {\n    .b > .c :hover { top: 0; } /* tail */\n  }\n";
        assert_eq!(
            strip_css(src),
            ".a{color:red;margin:0 1px}@media (x){.b>.c :hover{top:0}}\n"
        );
        assert_eq!(strip_css("a{} /* open"), "a{}\n");
    }

    #[test]
    fn shipped_css_has_no_comments_and_keeps_every_rule() {
        for (n, _, b) in FILES.iter().filter(|(n, _, _)| n.ends_with(".css")) {
            let src = String::from_utf8_lossy(b);
            let out = String::from_utf8_lossy(served(n, b)).into_owned();
            assert!(!out.contains("/*"), "{n}");
            let braces = |t: &str| (t.matches('{').count(), t.matches('}').count());
            assert_eq!(braces(&src), braces(&out), "{n}");
        }
    }
}
