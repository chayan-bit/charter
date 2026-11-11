//! Server time per page on the fixture (docs/p6/ui.md section 6: under 20 ms p50, under
//! 50 ms p99). Ignored by default because timings belong to a quiet machine; run with
//! `cargo test -p charter-ui --release --test render_time -- --ignored --nocapture`.

mod common;

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use common::*;

const RUNS: usize = 60;

fn pct(sorted: &[Duration], p: usize) -> f64 {
    let i = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[i].as_secs_f64() * 1e3
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing; run on a quiet machine"]
async fn page_render_times() {
    let f = fixture();
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
        format!("/r/{h}/branches"),
        "/wallet".to_owned(),
        "/settings".to_owned(),
    ];
    let app = app();
    for p in pages {
        if send(app.clone(), req(&p)).await.status != StatusCode::OK {
            println!("{p:<90} (not served)");
            continue;
        }
        let mut t = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let start = Instant::now();
            let r = send(app.clone(), req(&p)).await;
            t.push(start.elapsed());
            assert_eq!(r.status, StatusCode::OK, "{p}");
        }
        t.sort();
        println!(
            "{p:<90} p50 {:>6.2} ms  p99 {:>6.2} ms",
            pct(&t, 50),
            pct(&t, 99)
        );
    }
}
