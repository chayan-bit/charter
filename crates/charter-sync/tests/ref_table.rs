//! Issue #24 (docs/p5/head-model.md section 14): a device with more refs than the head body
//! holds keeps them in a ref table; push, fetch, clone and the program fold all see every
//! ref, and a later push that moves one ref reuses the table.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use charter_core::{HeadFormat, Rid};
use charter_program::Event;
use common::*;

const TAGS: usize = 5_000;

fn tag(i: usize) -> String {
    format!("refs/tags/v{i:05}")
}

/// `TAGS` tags on HEAD, written as packed refs (one file instead of 5,000 git calls).
fn tag_all(work: &Path) {
    let oid = rev(work, "HEAD");
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    for i in 0..TAGS {
        packed.push_str(&format!("{oid} {}\n", tag(i)));
    }
    std::fs::write(work.join(".git/packed-refs"), packed).expect("packed-refs");
}

fn pin_program_args() -> (&'static str, charter_core::ProgramPin) {
    let src = include_str!("../../../programs/bounty.rhai");
    let pin = charter_core::ProgramPin {
        engine: "rhai/1".into(),
        hash: charter_program::Program::hash(src),
    };
    (src, pin)
}

async fn pin_program(net: &Net, p: &Peer, rid: &Rid) {
    let (src, pin) = pin_program_args();
    charter_sync::program::publish(net, &p.cfg, &p.keys, rid, "rhai/1", src)
        .await
        .expect("publish");
    let st = charter_sync::state::State::default();
    charter_sync::identity::propose(net, &p.cfg, &p.keys, rid, &st, |b| charter_core::IdBody {
        program: Some(pin),
        ..b.clone()
    })
    .await
    .expect("pin");
}

/// The one device head of `p` as `viewer` sees it.
fn own_head(view: &charter_sync::View, p: &Peer) -> charter_core::DeviceHead {
    let v2 = view.peers[&p.keys.nid()].v2.as_ref().expect("v2");
    let (_, h) = v2.heads().next().expect("a head");
    h.clone()
}

/// What `git clone` does through the helper, with the refs written in one `update-ref`.
async fn clone_all(net: &Net, p: &Peer, rid: &Rid, dest: &Path) -> charter_sync::View {
    use std::io::Write;
    let repo = p.repo(rid);
    let view = Box::pin(charter_sync::sync(
        net,
        &repo,
        &charter_sync::Want::default(),
    ))
    .await
    .expect("clone sync");
    let listing = charter_sync::listing(&view, None);
    git(
        dest.parent().expect("parent"),
        &["init", "-q", &dest.to_string_lossy()],
    );
    let oids: BTreeSet<String> = listing.refs.iter().map(|(_, o)| o.clone()).collect();
    let oids: Vec<String> = oids.into_iter().collect();
    charter_sync::git::Git::work(Some(dest))
        .fetch_pack_from(&repo.git_dir(), &oids)
        .expect("objects");
    let mut child = std::process::Command::new("git")
        .args(["update-ref", "--stdin"])
        .current_dir(dest)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    for (name, oid) in &listing.refs {
        writeln!(stdin, "update {name} {oid}").expect("write");
    }
    drop(stdin);
    assert!(child.wait().expect("update-ref").success());
    view
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_with_5000_refs_pushes_fetches_clones_and_folds() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "many-refs").await;
    pin_program(&net, &alice, &rid).await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"one")], "feat: one");
    tag_all(&work);
    let mut refs: Vec<String> = (0..TAGS).map(tag).collect();
    refs.push("refs/heads/main".into());
    let names: Vec<&str> = refs.iter().map(String::as_str).collect();
    Box::pin(push2(&net, &alice, &rid, &work, &names))
        .await
        .expect("a push of 5,000 tags lands");

    let bob = Peer::new(&net, 2);
    let view = Box::pin(charter_sync::sync(&net, &bob.repo(&rid), &bob.want()))
        .await
        .expect("fetch");
    let head = own_head(&view, &alice);
    assert_eq!(head.format, HeadFormat::V4, "the refs went into a table");
    let table = head.refs_table.clone().expect("a ref table");
    let listed = view.peers[&alice.keys.nid()].refs_body().expect("refs");
    assert_eq!(listed.refs.len(), TAGS + 1, "every tag and main");
    assert!(listed.get(&tag(TAGS - 1)).is_some());

    // One more commit on main: the table is named again, only main changes inline.
    commit(&work, &[("a", b"two")], "feat: two");
    let main = rev(&work, "main");
    Box::pin(push2(&net, &alice, &rid, &work, &["refs/heads/main"]))
        .await
        .expect("a small push of a large device");
    let view = Box::pin(charter_sync::sync(&net, &bob.repo(&rid), &bob.want()))
        .await
        .expect("fetch again");
    let head = own_head(&view, &alice);
    let again = head.refs_table.clone().expect("still a table");
    assert_eq!(again.table_id, table.table_id, "the table is reused");
    assert_eq!(
        head.body.refs.len(),
        TAGS + 1,
        "effective refs are complete"
    );
    let listed = view.peers[&alice.keys.nid()].refs_body().expect("refs");
    assert_eq!(
        hex::encode(listed.get("refs/heads/main").expect("main")),
        main
    );

    // The pinned program folds both pushes, the table's tags included.
    let enforced = view.program.as_ref().expect("the fold ran");
    let pushed: BTreeSet<&str> = enforced
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Push(u) => Some(u.name.as_str()),
            Event::Op(_) => None,
        })
        .collect();
    assert!(pushed.contains(tag(0).as_str()) && pushed.contains(tag(TAGS - 1).as_str()));
    assert!(pushed.contains("refs/heads/main"));

    // A fresh clone lists and checks out every ref.
    let carol = Peer::new(&net, 3);
    let dest = tmp.path().join("carol");
    let view = Box::pin(clone_all(&net, &carol, &rid, &dest)).await;
    assert!(view.program.is_some(), "the clone folds too");
    let tags = git(&dest, &["tag", "-l"]);
    assert_eq!(tags.lines().count(), TAGS);
    assert_eq!(
        rev(&dest, "refs/tags/v04999"),
        rev(&work, "refs/tags/v04999")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_moves_back_to_a_v3_head_when_its_refs_fit_again() {
    const N: usize = 1_500;
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "shrink").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"one")], "feat: one");
    let oid = rev(&work, "HEAD");
    let packed: String = (0..N).map(|i| format!("{oid} {}\n", tag(i))).collect();
    std::fs::write(work.join(".git/packed-refs"), packed).expect("packed-refs");
    let mut refs: Vec<String> = (0..N).map(tag).collect();
    refs.push("refs/heads/main".into());
    let names: Vec<&str> = refs.iter().map(String::as_str).collect();
    Box::pin(push2(&net, &alice, &rid, &work, &names))
        .await
        .expect("push");
    let view = Box::pin(charter_sync::sync(&net, &alice.repo(&rid), &alice.want()))
        .await
        .expect("sync");
    assert_eq!(own_head(&view, &alice).format, HeadFormat::V4);

    // Delete all but ten tags: the body fits again and the head is a v3 head once more.
    let gone: Vec<String> = (10..N).map(|i| format!(":{}", tag(i))).collect();
    let names: Vec<&str> = gone.iter().map(String::as_str).collect();
    Box::pin(push2(&net, &alice, &rid, &work, &names))
        .await
        .expect("delete push");
    let bob = Peer::new(&net, 2);
    let view = Box::pin(charter_sync::sync(&net, &bob.repo(&rid), &bob.want()))
        .await
        .expect("fetch");
    let head = own_head(&view, &alice);
    assert_eq!(head.format, HeadFormat::V3);
    assert!(head.refs_table.is_none());
    let listed = view.peers[&alice.keys.nid()].refs_body().expect("refs");
    assert_eq!(listed.refs.len(), 11, "ten tags and main");
}
