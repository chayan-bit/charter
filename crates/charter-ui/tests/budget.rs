//! docs/p6/ui.md section 6: HTML plus CSS per page under 20 KB gzipped on the fixture, as
//! the server sends them (it gzips for clients that accept it). Run with `--nocapture` to
//! see the per-page sizes. The long states (a 2,000-line blob or diff, the cap of section
//! 6) are bigger by nature and have their own gzipped cap.

mod common;

use axum::http::{StatusCode, header};
use common::*;

const BUDGET: usize = 20 * 1024;

/// The gzipped body the server sends for `path` to a client that accepts gzip.
async fn wire(path: &str) -> usize {
    let mut r = req(path);
    r.headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().expect("value"));
    let r = send(app(), r).await;
    assert_eq!(r.status, StatusCode::OK, "{path}");
    assert_eq!(r.header(header::CONTENT_ENCODING), "gzip", "{path}");
    r.body.len()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_page_is_under_20_kb_gzipped_with_its_css() {
    let f = fixture();
    let css = wire("/assets/app.css").await;
    let h = &f.hex;
    let pages = [
        "/".to_owned(),
        format!("/r/{h}"),
        format!("/r/{h}/tree/main/src"),
        format!("/r/{h}/blob/main/src/main.rs"),
        format!("/r/{h}/log/main"),
        format!("/r/{h}/commit/{}", f.main),
        format!("/r/{h}/issues"),
        format!("/r/{h}/issues/{}", f.issue),
        format!("/r/{h}/patches"),
        format!("/r/{h}/patches/{}", f.patch),
        format!("/r/{h}/releases"),
        format!("/r/{h}/program"),
    ];
    println!("app.css {css} B gzipped");
    for p in pages {
        let html = wire(&p).await;
        println!("{p:<90} html {html:>6} B, with css {:>6} B", html + css);
        assert!(html + css < BUDGET, "{p}: {} B", html + css);
    }
}

/// Gzipped caps of the long states: 2,000 highlighted lines of the e2e generator's line
/// (eight coloured runs in 55 characters, near the floor of per-token markup) and the
/// first commit's diff, which adds that file.
const LONG_BLOB: usize = 40 * 1024;
const LONG_DIFF: usize = 40 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn long_blob_and_diff_stay_light_gzipped() {
    let f = fixture();
    let h = &f.hex;
    for (p, cap) in [
        (format!("/r/{h}/blob/main/long.rs"), LONG_BLOB),
        (format!("/r/{h}/commit/{}", f.first), LONG_DIFF),
    ] {
        let n = wire(&p).await;
        println!("{p:<90} gzipped {n:>7} B");
        assert!(n < cap, "{p}: {n} B");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_clients_get_identity() {
    let r = get("/").await;
    assert_eq!(r.header(header::CONTENT_ENCODING), "");
    assert!(r.text().contains("<html"));
}
