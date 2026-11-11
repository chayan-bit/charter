//! Every slice-1 page against a fixture repo pushed into a `MemArkiv` and synced by the UI's
//! own sync round: content, escaping of untrusted data, empty, error and long states.

mod common;

use axum::http::StatusCode;
use common::*;

fn short(s: &str) -> &str {
    &s[..7]
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_list_shows_the_synced_repo_verified() {
    let f = fixture();
    let h = page("/").await;
    assert!(
        h.contains(&format!("/r/{}", f.hex)) && h.contains(">demo<"),
        "{h}"
    );
    assert!(h.contains("sealmini verified") && h.contains("Verified"));
    assert!(h.contains("Updated <time title=\""), "last push, as a time");
    assert!(!h.contains("block <code>"), "no bare block number");
    assert!(h.contains("1 issue, 1 patch<"), "counted at sync: {h}");
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_home_lists_no_repos() {
    let d = tempfile::tempdir().expect("tmp");
    let r = send(app_for(d.path()), req("/")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.text().contains("No repositories on this computer yet"),
        "{}",
        r.text()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_home_has_readme_refs_clone_and_panel() {
    let f = fixture();
    let h = page(&format!("/r/{}", f.hex)).await;
    assert!(h.contains("<h1>Demo</h1>") && h.contains("<strong>world</strong>"));
    assert!(!h.contains("<script>alert"), "README is sanitized");
    assert!(h.contains("git clone arkiv::z"), "clone command");
    assert!(
        h.contains("refs/tags/v1") || h.contains(">v1<"),
        "tags listed"
    );
    assert!(h.contains(short(&f.main)) && h.contains("Maintainers"));
    assert!(
        !h.contains("Delegate"),
        "plain words outside Technical details"
    );
    assert!(h.contains("id=\"verify\"") && h.contains("ML-DSA-65"));
    assert!(h.contains("README.md") && h.contains("src/"), "root tree");
    // Also reachable by the z-base58 form of the rid.
    let z = charter_sync::ids::to_text(&f.rid);
    assert!(page(&format!("/r/{z}")).await.contains("<h1>Demo</h1>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn tree_lists_directories_first_and_walks_down() {
    let f = fixture();
    let h = page(&format!("/r/{}/tree/main/src", f.hex)).await;
    let (dir, file) = (
        h.find("deep/").expect("dir"),
        h.find("main.rs").expect("file"),
    );
    assert!(dir < file, "directories first");
    assert!(h.contains("rel=\"up\""));
    // Each entry names the commit that last changed it, subject escaped.
    let row = |name: &str| {
        let at = h.find(&format!(">{name}<")).expect("row");
        h[at..].split("</tr>").next().expect("tr").to_owned()
    };
    let main_rs = row("main.rs");
    assert!(
        main_rs.contains(&format!("/commit/{}\"", f.main)),
        "{main_rs}"
    );
    assert!(
        main_rs.contains(">fix: &#60;smaller&#62; main</a>"),
        "{main_rs}"
    );
    assert!(row("deep/").contains(&format!("/commit/{}\"", f.first)));
    let h = page(&format!("/r/{}/tree/{}/src/deep", f.hex, f.first)).await;
    assert!(h.contains("x.txt"));
    let r = get(&format!("/r/{}/tree/main/README.md", f.hex)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "a file is no directory");
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_is_highlighted_with_line_anchors_and_escaped() {
    let f = fixture();
    let h = page(&format!("/r/{}/blob/{}/src/main.rs", f.hex, f.first)).await;
    assert!(h.contains("<tr id=L2><td class=ln>"), "line anchors");
    assert!(h.contains("<x-") && h.contains("&lt;hi&gt;") && !h.contains("<hi>"));
    assert!(h.contains(&format!("/r/{}/raw/", f.hex)), "raw link");
}

#[tokio::test(flavor = "multi_thread")]
async fn long_blob_stops_at_2000_lines_until_show_all() {
    let f = fixture();
    let h = page(&format!("/r/{}/blob/main/long.rs", f.hex)).await;
    assert!(h.contains("id=L2000>") && !h.contains("id=L2001>"));
    assert!(h.contains("500 more lines") && h.contains("?all=1"));
    let h = page(&format!("/r/{}/blob/main/long.rs?all=1", f.hex)).await;
    assert!(h.contains("id=L2500>") && !h.contains("more lines"));
    let tail = &h[h.find("id=L2001>").expect("2001")..];
    assert!(
        !tail[..tail.find("</tr>").expect("row")].contains("<x-"),
        "plain past 2,000"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_blob_points_to_raw() {
    let f = fixture();
    let h = page(&format!("/r/{}/blob/main/bin.dat", f.hex)).await;
    assert!(h.contains("Binary or very large file"));
}

#[tokio::test(flavor = "multi_thread")]
async fn log_groups_commits_and_pages() {
    let f = fixture();
    let h = page(&format!("/r/{}/log/main", f.hex)).await;
    assert!(h.contains("fix: &#60;smaller&#62; main") && h.contains("feat: first"));
    assert!(
        h.find("fix:").expect("a") < h.find("feat: first").expect("b"),
        "newest first"
    );
    assert!(h.contains("<h2 class=\"h2 sec\">") && !h.contains("rel=\"next\""));
    let h = page(&format!("/r/{}/log/main?page=2", f.hex)).await;
    assert!(h.contains("No commits.") && h.contains("rel=\"prev\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_shows_message_anchors_and_collapsible_diff() {
    let f = fixture();
    let h = page(&format!("/r/{}/commit/{}", f.hex, f.main)).await;
    assert!(h.contains("fix: &#60;smaller&#62; main"));
    assert!(h.contains("<details class=\"file\" open") && h.contains("src/main.rs"));
    assert!(
        h.contains("<tr class=a>") && h.contains("<tr class=d>"),
        "added and removed lines"
    );
    assert!(
        h.contains("Pushed by")
            && h.contains("<code>main</code>")
            && h.contains("<code>refs/heads/main</code>"),
        "anchored by alice's head"
    );
    assert!(h.contains("unsigned"), "no git signature");
    let h = page(&format!("/r/{}/commit/{}", f.hex, f.first)).await;
    assert!(h.contains("The body of the first commit.") && h.contains("this is the first commit"));
}

#[tokio::test(flavor = "multi_thread")]
async fn issues_fold_into_a_closed_thread_with_sanitized_bodies() {
    let f = fixture();
    let h = page(&format!("/r/{}/issues", f.hex)).await;
    assert!(
        h.contains("Crash on &#60;b&#62;start&#60;/b&#62;")
            && h.contains("<span class=\"chip\">closed</span>")
    );
    assert!(h.contains("1 comment"));
    let h = page(&format!("/r/{}/issues/{}", f.hex, f.issue)).await;
    assert!(h.contains("<strong>crashes</strong>") && !h.contains("<script>x()"));
    assert!(h.contains("seen it too") && h.contains("<b>closed</b>"));
    assert!(
        h.contains("bounty") && h.contains("reason"),
        "other payload fields"
    );
    let r = get(&format!("/r/{}/issues/{}", f.hex, "0".repeat(64))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn patches_show_revisions_and_reviews() {
    let f = fixture();
    let h = page(&format!("/r/{}/patches", f.hex)).await;
    assert!(h.contains("Fix the crash") && h.contains("<span class=\"chip\">merged</span>"));
    assert!(
        h.contains("merged into main"),
        "inferred from the default branch"
    );
    let h = page(&format!("/r/{}/patches/{}", f.hex, f.patch)).await;
    assert!(h.contains("main already contains its latest commit") && h.contains(short(&f.main)));
    assert!(h.contains("Reviews") && h.contains("<b>approved</b>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn releases_list_artifacts_and_program_verdict() {
    let f = fixture();
    let h = page(&format!("/r/{}/releases", f.hex)).await;
    assert!(h.contains("<h2>v1</h2>") && h.contains("demo.tgz") && h.contains("abababa"));
    assert!(
        h.contains("not tracked by the rules"),
        "bounty.rhai has no releases"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn program_page_shows_pin_bounties_and_state() {
    let f = fixture();
    let h = page(&format!("/r/{}/program", f.hex)).await;
    assert!(h.contains("rhai/1") && h.contains("Bounties"));
    assert!(
        h.contains(&format!("/issues/{}", f.issue)),
        "bounty links its issue"
    );
    assert!(h.contains("&#34;bounty&#34;"), "state JSON, escaped");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_repos_refs_and_paths_are_404_pages() {
    let f = fixture();
    for p in [
        "/r/nope".to_owned(),
        format!("/r/{}", "0".repeat(64)),
        format!("/r/{}/tree/no-such-branch", f.hex),
        format!("/r/{}/blob/main/../../etc/passwd", f.hex),
        format!("/r/{}/blob/main/src%2F..%2F..%2Fx", f.hex),
        format!("/r/{}/commit/--all", f.hex),
        "/nowhere".to_owned(),
    ] {
        let r = get(&p).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{p}: {}", r.text());
        assert!(r.text().contains("Back to the repo list"), "{p}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_repo_before_its_first_ui_sync_is_pending() {
    let d = tempfile::tempdir().expect("tmp");
    let rid = [7u8; 32];
    drop(charter_sync::Repo::open(d.path(), &rid).expect("layout"));
    let hex = charter_sync::ids::hex32(&rid);
    let r = send(app_for(d.path()), req(&format!("/r/{hex}"))).await;
    assert_eq!(r.status, StatusCode::OK);
    let h = r.text();
    assert!(
        h.contains("seal pending") && h.contains("No verified commits"),
        "{h}"
    );
    let r = send(app_for(d.path()), req(&format!("/r/{hex}/issues"))).await;
    assert!(r.text().contains("No issues yet"));
}

/// Regression for the repo-home p99 spike: a page never waits on the lock a sync round or
/// push holds, and never sees a torn `view.json` or `cobs.json` while they are rewritten.
#[tokio::test(flavor = "multi_thread")]
async fn pages_render_while_a_round_holds_the_lock_and_rewrites_the_caches() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = fixture();
    let held = f.alice.repo(&f.rid);
    let dir = held.dir.clone();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let writer = std::thread::spawn(move || {
        let view: charter_ui::cache::RepoView = charter_ui::cache::read(&dir.join("view.json"))
            .expect("read")
            .expect("view");
        let cobs: charter_ui::cache::Cobs = charter_ui::cache::read(&dir.join("cobs.json"))
            .expect("read")
            .expect("cobs");
        while !stop2.load(Ordering::Relaxed) {
            charter_ui::cache::write(&dir.join("view.json"), &view).expect("view");
            charter_ui::cache::write(&dir.join("cobs.json"), &cobs).expect("cobs");
        }
    });
    let app = app();
    let paths = [
        (format!("/r/{}", f.hex), "<h1>Demo</h1>"),
        (format!("/r/{}/tree/main/src", f.hex), "deep/"),
        (format!("/r/{}/blob/main/src/main.rs", f.hex), "main"),
        (format!("/r/{}/commit/{}", f.hex, f.main), "smaller"),
        (format!("/r/{}/issues", f.hex), "Crash on"),
    ];
    for _ in 0..10 {
        for (p, want) in &paths {
            let call = send(app.clone(), req(p));
            let r = tokio::time::timeout(std::time::Duration::from_secs(10), call)
                .await
                .expect("a page must not wait on the repo lock");
            assert_eq!(r.status, StatusCode::OK, "{p}");
            let t = r.text();
            assert!(t.contains(want) && t.contains("seal verified"), "{p}: {t}");
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().expect("writer");
    drop(held);
}
