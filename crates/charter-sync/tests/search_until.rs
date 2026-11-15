//! `search_until`: verification stops once the caller says so (the UI's quota mark), and
//! `search` itself still verifies every candidate.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use charter_store::fake::Meter;
use charter_sync::search::{SearchQuery, search, search_until};
use common::*;

#[tokio::test]
async fn it_stops_verifying_when_told_and_costs_only_what_it_read() {
    let net = Net::new();
    let p = Peer::new(&net, 1);
    for n in ["ab-one", "ab-two", "ab-three"] {
        init(&net, &p, n).await;
    }
    let q = SearchQuery {
        prefix: Some("ab-".into()),
        limit: 10,
        ..SearchQuery::default()
    };
    let m = Meter::new(&net);
    let (hits, stopped) = search_until(&m, &q, || true).await.expect("search");
    assert!((hits.len(), stopped) == (0, true));
    assert_eq!(m.metrics().queries, 1, "the candidate query only");
    m.reset();
    let asked = AtomicUsize::new(0);
    let stop = || asked.fetch_add(1, Ordering::SeqCst) >= 1;
    let (hits, stopped) = search_until(&m, &q, stop).await.expect("search");
    assert!(
        stopped && hits.len() == 1,
        "one candidate verified, then stopped"
    );
    assert_eq!(
        m.metrics().queries,
        1 + 3,
        "candidates plus one verification"
    );
    m.reset();
    let all = search(&m, &q).await.expect("search");
    assert_eq!(all.len(), 3);
    assert_eq!(m.metrics().queries, 1 + 3 * 3);
}
