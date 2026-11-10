//! docs/p6/ui.md 4.5: the per-page event stream carries its repo's events, unbuffered, and
//! the pages mark what each event refreshes.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{StatusCode, header};
use charter_ui::security::Auth;
use charter_ui::sync_loop::Kind;
use charter_ui::{App, Shared, router};
use common::*;
use futures_util::StreamExt;
use tower::ServiceExt;

#[tokio::test(flavor = "multi_thread")]
async fn an_event_stream_carries_its_repos_events_and_nothing_else() {
    let f = fixture();
    let shared = Arc::new(Shared::default());
    let app = App::new(
        f.alice.home.path().to_path_buf(),
        shared.clone(),
        CSRF.into(),
    );
    let r = router(
        Arc::new(app),
        Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())),
    );
    let mut req = req(&format!("/r/{}/events", f.hex));
    req.headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().expect("value"));
    let resp = r.oneshot(req).await.expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert!(
        h[header::CONTENT_TYPE]
            .to_str()
            .expect("ct")
            .starts_with("text/event-stream")
    );
    assert!(
        !h.contains_key(header::CONTENT_ENCODING),
        "events are never gzipped, so each one is flushed"
    );
    // The handler subscribed before it answered, so nothing below is missed.
    shared.emit([9; 32], Kind::Push);
    shared.emit(f.rid, Kind::Push);
    shared.emit(f.rid, Kind::Op);
    shared.emit(f.rid, Kind::Sync);
    let mut body = resp.into_body().into_data_stream();
    let mut got = String::new();
    while got.matches("event: ").count() < 3 {
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("an event in time")
            .expect("open")
            .expect("chunk");
        got.push_str(&String::from_utf8_lossy(&chunk));
    }
    let names: Vec<&str> = got
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .collect();
    assert_eq!(names, ["push", "op", "sync"], "{got}");
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_mark_what_each_event_refreshes_and_the_panel_comes_alone() {
    let f = fixture();
    let home = page(&format!("/r/{}", f.hex)).await;
    let events = format!("data-events=\"/r/{}/events\"", f.hex);
    assert!(home.contains(&events), "{home}");
    assert!(home.contains("<script src=\"/assets/live.js"));
    for (p, want) in [
        (format!("/r/{}", f.hex), "id=\"live\" data-live=\"push\""),
        (
            format!("/r/{}/log/main", f.hex),
            "id=\"live\" data-live=\"push\"",
        ),
        (
            format!("/r/{}/issues", f.hex),
            "id=\"live\" data-live=\"op\"",
        ),
        (
            format!("/r/{}/issues/{}", f.hex, f.issue),
            "id=\"live\" data-live=\"op\"",
        ),
        (
            format!("/r/{}/releases", f.hex),
            "id=\"live\" data-live=\"op\"",
        ),
    ] {
        let html = page(&p).await;
        assert!(html.contains(want), "{p}");
        assert!(html.contains("id=\"tabs\" data-live=\"op\""), "{p}");
        assert!(html.contains("data-live=\"sync\""), "{p}");
    }
    let panel = page(&format!("/r/{}/verify", f.hex)).await;
    assert!(
        panel.trim_start().starts_with("<details id=\"verify\"") && !panel.contains("<main"),
        "{panel}"
    );
    let index = page("/").await;
    assert!(!index.contains("data-events"), "no stream off repo pages");
}
