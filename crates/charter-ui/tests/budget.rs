//! docs/p6/ui.md section 6: HTML plus CSS per page under 20 KB gzipped on the fixture.
//! Run with `--nocapture` to see the per-page sizes. The long states (a 2,000-line blob or
//! diff, the cap of section 6) are bigger by nature and have their own raw-size cap.

mod common;

use std::io::Write;

use common::*;

const BUDGET: usize = 20 * 1024;

fn gz(b: &[u8]) -> usize {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).expect("gzip");
    e.finish().expect("gzip").len()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_page_is_under_20_kb_gzipped_with_its_css() {
    let f = fixture();
    let css = gz(&get("/assets/app.css").await.body);
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
        let html = gz(page(&p).await.as_bytes());
        println!("{p:<90} html {html:>6} B, with css {:>6} B", html + css);
        assert!(html + css < BUDGET, "{p}: {} B", html + css);
    }
}

/// Raw-byte caps of the long states (they were 2.2 MB and 340 KB before lean rows and flat
/// role runs). The blob is 2,000 highlighted lines of the e2e generator's line, which has
/// eight coloured runs in 55 characters, so it is near the floor of per-token markup; real
/// Rust source comes out at about 110 B a line.
const LONG_BLOB_RAW: usize = 400 * 1024;
const LONG_DIFF_RAW: usize = 300 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn long_blob_and_diff_stay_light_raw() {
    let f = fixture();
    let h = &f.hex;
    for (p, cap) in [
        (format!("/r/{h}/blob/main/long.rs"), LONG_BLOB_RAW),
        (format!("/r/{h}/commit/{}", f.first), LONG_DIFF_RAW),
    ] {
        let n = page(&p).await.len();
        println!("{p:<90} raw {n:>7} B");
        assert!(n < cap, "{p}: {n} B");
    }
}
