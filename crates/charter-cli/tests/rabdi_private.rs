//! Private repos, programs on private repos, zero-knowledge proof attachments and timelock
//! as multi-user flows through the real binaries (docs/live/private-proofs.md).
//!
//! Each flow runs twice: `mem_*` on the file-persisted MemArkiv (free, part of the suite) and
//! `live_*` on Rabdi (ignored, spends testnet funds). Live peers are funded from a bank: the
//! wallet of the `keys init` home in `$CHARTER_LIVE_BANK`, filled once from the throwaway
//! key by `bank_fund` and emptied back into it by `bank_sweep`. Every live peer is swept back
//! to the bank at the end of its flow. Run `bank_fund` under the throwaway lock:
//!
//! `lockf <lock> cargo test -p charter-cli --test rabdi_private bank_fund -- --ignored`
//! `CHARTER_RABDI=1 CHARTER_LIVE_BANK=<home> CHARTER_LIVE_ROOT=<dir> cargo test -p charter-cli --test rabdi_private live_ -- --ignored --nocapture --test-threads 1`

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use charter_cli::backend::MEM_CHAIN_ID;
use charter_core::{AttrValue, Observed, Rid};
use charter_store::fake::FileMemArkiv;
use charter_store::{Pred, U256, attr};
use charter_sync::ids::{hex32, parse_url, to_text};
use charter_sync::program::proofs::{BLOB_CHUNK, BLOB_TYPE};
use charter_sync::{Keys, observe};
use serde_json::{Value, json};

const RECEIPT: &[u8] = include_bytes!("../../charter-zk/tests/fixtures/demo.receipt");
const PUBLIC: &str = include_str!("../../charter-zk/tests/fixtures/demo.public.json");
const BASE: &[u8] = include_bytes!("../../../zk/demo/base.rs");

/// Blocks of lifetime for every live entity: 1 day, so the evidence outlives the review.
const LIFETIME: &str = "43200";
/// Most the bank takes from the throwaway key, ever (the task budget).
const BANK_WEI: u128 = 50_000_000_000_000_000;
const MILLI: u128 = 1_000_000_000_000_000;

#[derive(Clone)]
enum Backend {
    Mem(PathBuf),
    Live(Option<String>),
}

struct Peer {
    name: &'static str,
    home: PathBuf,
    backend: Backend,
}

impl Peer {
    fn command(&self, program: &str, dir: &Path, args: &[&str]) -> Command {
        match &self.backend {
            Backend::Live(access) => {
                common::live::command(&self.home, access, LIFETIME, program, dir, args)
            }
            Backend::Mem(log) => {
                let user = common::e2e::User {
                    home: self.home.clone(),
                    log: log.clone(),
                };
                let mut c = user.cmd(program, dir);
                if program == "git" {
                    c.args(["-c", "user.name=t", "-c", "user.email=t@example.com"]);
                    c.args(["-c", "commit.gpgsign=false"]);
                }
                c.args(args);
                c
            }
        }
    }

    fn run(&self, program: &str, dir: &Path, args: &[&str]) -> Output {
        let out = self.command(program, dir, args).output().expect("spawn");
        common::live::note_stderr(&String::from_utf8_lossy(&out.stderr));
        out
    }

    fn ok(&self, program: &str, dir: &Path, args: &[&str]) -> String {
        let out = self.run(program, dir, args);
        assert!(
            out.status.success(),
            "{}: {program} {args:?} failed",
            self.name
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Runs a command that must fail; returns its stderr.
    fn fails(&self, program: &str, dir: &Path, args: &[&str]) -> String {
        let out = self.run(program, dir, args);
        assert!(
            !out.status.success(),
            "{}: {program} {args:?} succeeded",
            self.name
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn arkiv(&self, args: &[&str]) -> String {
        self.ok(env!("CARGO_BIN_EXE_chart"), &self.home, args)
    }

    fn arkiv_fails(&self, args: &[&str]) -> String {
        self.fails(env!("CARGO_BIN_EXE_chart"), &self.home, args)
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        self.ok("git", dir, args).trim().to_owned()
    }

    fn keys(&self) -> Keys {
        Keys::require(&self.home.join(".charter")).expect("keys")
    }

    fn nid(&self) -> String {
        to_text(&self.keys().nid())
    }

    fn post(&self, url: &str, ty: &str, action: &str, object: Option<&str>, p: &Value) -> String {
        let payload = p.to_string();
        let mut args = vec![
            "op", "post", "--repo", url, "--type", ty, "--action", action,
        ];
        if let Some(o) = object {
            args.extend(["--object", o, "--parent", o]);
        }
        args.extend(["--payload-json", &payload]);
        self.arkiv(&args).trim().to_owned()
    }
}

/// One flow's peers, directory, clock and backend hooks.
struct World<'a> {
    root: PathBuf,
    backend: Backend,
    t0: Instant,
    tag: String,
    fund: &'a dyn Fn(&Peer, u128),
    raw: &'a dyn Fn(Pred) -> Vec<Observed>,
}

impl World<'_> {
    fn step(&self, what: &str) {
        eprintln!("== [{:7.1}s] {what}", self.t0.elapsed().as_secs_f64());
    }

    fn peer(&self, name: &'static str, wei: u128) -> Peer {
        let home = self.root.join(name);
        std::fs::create_dir_all(&home).expect("home");
        let p = Peer {
            name,
            home,
            backend: self.backend.clone(),
        };
        p.arkiv(&["keys", "init"]);
        if wei > 0 {
            (self.fund)(&p, wei);
        }
        p
    }

    fn dir(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }
}

fn field<'a>(out: &'a str, key: &str) -> &'a str {
    common::e2e::field(out, key)
}

fn rid_of(url: &str) -> Rid {
    parse_url(url.trim_start_matches("arkiv::"))
        .expect("url")
        .rid
}

fn commit_file(p: &Peer, work: &Path, file: &str, text: &[u8], msg: &str) -> String {
    std::fs::write(work.join(file), text).expect("write");
    p.git(work, &["add", "-A"]);
    p.git(work, &["commit", "-q", "-m", msg]);
    p.git(work, &["rev-parse", "HEAD"])
}

fn sha_hex(b: &[u8]) -> String {
    hex::encode(charter_crypto::sha256(b))
}

fn random_hex() -> String {
    hex::encode(charter_store::random_salt().to_be_bytes())
}

fn state(status: &str) -> Value {
    serde_json::from_str(field(status, "state:")).expect("state json")
}

/// Asserts no entity created by `creators` carries the rid, its text forms, the repo name or
/// a content marker in any attribute, and that queries by rid and by name find nothing.
fn assert_unlinkable(w: &World, creators: &BTreeSet<[u8; 20]>, rid: &Rid, needles: &[&str]) {
    let rows = (w.raw)(observe::creators(creators));
    assert!(!rows.is_empty(), "the flow wrote entities");
    let texts: Vec<String> = [hex32(rid), to_text(rid)]
        .into_iter()
        .chain(needles.iter().map(|s| (*s).to_owned()))
        .collect();
    for o in &rows {
        for a in &o.attributes {
            let leak = match &a.value {
                AttrValue::Bytes32(b) => b == rid,
                AttrValue::Bytes(b) => {
                    b.windows(32).any(|x| x == rid)
                        || texts
                            .iter()
                            .any(|t| b.windows(t.len()).any(|x| x == t.as_bytes()))
                }
                AttrValue::Str(s) => texts.iter().any(|t| s.contains(t.as_str())),
                AttrValue::U64(_) => false,
            };
            assert!(!leak, "entity {} leaks in {}", hex32(&o.key), a.name);
        }
    }
    let by_rid = (w.raw)(observe::b32("ag.rid", rid));
    assert!(
        by_rid.is_empty(),
        "a raw query by rid finds {}",
        by_rid.len()
    );
    for name in needles.iter().take(1) {
        let by_name = (w.raw)(Pred::eq(attr("ag.name"), *name));
        assert!(by_name.is_empty(), "a raw query by name finds rows");
    }
    let kinds: BTreeSet<String> = rows
        .iter()
        .flat_map(|o| &o.attributes)
        .filter(|a| a.name == "ag.kind")
        .map(|a| format!("{:?}", a.value))
        .collect();
    w.step(&format!(
        "raw: {} entities by the members' addresses, kinds {kinds:?}, none linkable; by rid 0, by name 0",
        rows.len()
    ));
    for o in rows.iter().take(40) {
        eprintln!(
            "   entity {} block {} creator 0x{}",
            hex32(&o.key),
            o.block,
            hex::encode(o.creator)
        );
    }
}

/// Items 1 and 2: members, roles, outsiders, removal and a program on a private repo.
fn private_flow(w: &World) {
    let alice = w.peer("alice", 16 * MILLI);
    let bob = w.peer("bob", 12 * MILLI);
    let carol = w.peer("carol", 2 * MILLI);
    let dave = w.peer("dave", 0);
    let name = format!("vault-{}", w.tag);
    let marker = format!("secret-{}", w.tag);

    w.step("alice: init --private");
    let url = field(
        &alice.arkiv(&["init", "--private", "--name", &name]),
        "url:",
    )
    .to_owned();
    let rid = rid_of(&url);
    eprintln!("   rid {} ({})", to_text(&rid), hex32(&rid));
    let work = PathBuf::from(w.dir("alice-work"));
    alice.git(&w.root, &["init", "-q", "-b", "main", &w.dir("alice-work")]);
    let readme = format!("v0 {marker}\n");
    let c0 = commit_file(&alice, &work, "README", readme.as_bytes(), "feat: v0");
    w.step("alice: git push v0");
    alice.git(&work, &["push", "-q", &url, "main"]);

    w.step("bob, carol: member keypackage");
    let kp_bob = field(&bob.arkiv(&["member", "keypackage"]), "key package:").to_owned();
    let kp_carol = field(&carol.arkiv(&["member", "keypackage"]), "key package:").to_owned();
    w.step("alice: member add bob (write), carol (read)");
    let add = |kp: &str, role: &str| {
        let args = [
            "member",
            "add",
            "--repo",
            &url,
            "--key-package",
            kp,
            "--role",
            role,
        ];
        alice.arkiv(&args)
    };
    assert!(add(&kp_bob, "write").contains("as Write"));
    assert!(add(&kp_carol, "read").contains("as Read"));
    let members = alice.arkiv(&["member", "list", "--repo", &url]);
    assert!(
        members.contains(&format!("{} Write", bob.nid())),
        "{members}"
    );
    assert!(
        members.contains(&format!("{} Read", carol.nid())),
        "{members}"
    );
    eprintln!("   {}", members.replace('\n', " | "));

    w.step("bob: clone (joins from the welcome)");
    let bob_clone = PathBuf::from(w.dir("bob-clone"));
    bob.git(&w.root, &["clone", "-q", &url, &w.dir("bob-clone")]);
    assert_eq!(bob.git(&bob_clone, &["rev-parse", "HEAD"]), c0);
    assert_eq!(
        std::fs::read_to_string(bob_clone.join("README")).expect("readme"),
        readme
    );

    w.step("carol: clone (read role)");
    let carol_clone = PathBuf::from(w.dir("carol-clone"));
    carol.git(&w.root, &["clone", "-q", &url, &w.dir("carol-clone")]);
    assert_eq!(carol.git(&carol_clone, &["rev-parse", "HEAD"]), c0);
    w.step("carol: git push refused (read role)");
    commit_file(&carol, &carol_clone, "CAROL", b"carol\n", "feat: carol");
    let err = carol.fails("git", &carol_clone, &["push", &url, "main"]);
    assert!(err.contains("E_ROLE_DENIED"), "{err}");
    carol.git(&carol_clone, &["reset", "-q", "--hard", &c0]);
    w.step("carol: member add refused (not admin)");
    let err = carol.arkiv_fails(&["member", "add", "--repo", &url, "--key-package", &kp_bob]);
    assert!(err.contains("E_ROLE_DENIED"), "{err}");

    w.step("dave (outsider): clone, op list, member list refused");
    let err = dave.fails("git", &w.root, &["clone", "-q", &url, &w.dir("dave-clone")]);
    assert!(err.contains("E_NOT_MEMBER"), "{err}");
    let err = dave.arkiv_fails(&["op", "list", "--repo", &url]);
    assert!(err.contains("E_NOT_MEMBER"), "{err}");
    let search = dave.arkiv(&["search", "--name", &name]);
    assert!(
        !search.contains(&to_text(&rid)),
        "search finds the private repo: {search}"
    );

    w.step("item 2: alice pins branch-protection on the private repo");
    let pin = common::programs::program("branch-protection");
    let pinned = alice.arkiv(&["program", "pin", "--repo", &url, "--file", &pin]);
    assert!(pinned.contains("program:        rhai/1"), "{pinned}");

    w.step("alice: direct push refused by the program");
    commit_file(&alice, &work, "DIRECT", b"direct\n", "feat: direct");
    let err = alice.fails("git", &work, &["push", &url, "main"]);
    assert!(err.contains("E_PROGRAM_REJECTED"), "{err}");
    alice.git(&work, &["reset", "-q", "--hard", &c0]);

    w.step("bob: commit v1, push to his namespace, post a patch");
    let c1 = commit_file(
        &bob,
        &bob_clone,
        "README",
        b"v1 by bob\n",
        "feat: v1 by bob",
    );
    bob.git(&bob_clone, &["push", "-q", &url, "main"]);
    let patch = bob.post(
        &url,
        "xyz.arkiv.patch",
        "create",
        None,
        &json!({"title": "v1", "head": c1}),
    );
    w.step("alice: review and approve at c1");
    let review = alice.post(
        &url,
        "xyz.arkiv.review",
        "create",
        None,
        &json!({"patch": patch}),
    );
    let approve = json!({"patch": patch, "head": c1});
    alice.post(&url, "xyz.arkiv.review", "approve", Some(&review), &approve);
    w.step("alice: fetch bob's peer refs, push c1 (accepted)");
    let bob_url = format!("{url}/{}", bob.nid());
    alice.git(&work, &["fetch", "-q", &bob_url, "main"]);
    assert_eq!(alice.git(&work, &["rev-parse", "FETCH_HEAD"]), c1);
    alice.git(&work, &["merge", "-q", "--ff-only", "FETCH_HEAD"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    w.step("carol: fresh clone gets the program's head");
    let carol2 = PathBuf::from(w.dir("carol-clone-2"));
    carol.git(&w.root, &["clone", "-q", &url, &w.dir("carol-clone-2")]);
    assert_eq!(carol.git(&carol2, &["rev-parse", "HEAD"]), c1);
    let status = carol.arkiv(&["program", "status", "--repo", &url]);
    assert!(
        status.contains(&format!("head: refs/heads/main {c1}")),
        "{status}"
    );
    let err = dave.arkiv_fails(&["program", "status", "--repo", &url]);
    assert!(err.contains("E_NOT_MEMBER"), "{err}");

    w.step("alice: member remove bob (new epoch)");
    let bob_nid = bob.nid();
    alice.arkiv(&["member", "remove", "--repo", &url, "--member", &bob_nid]);
    let members = alice.arkiv(&["member", "list", "--repo", &url]);
    assert!(!members.contains(&bob_nid), "{members}");
    eprintln!("   {}", members.replace('\n', " | "));

    w.step("alice: v2 after removal through patch and approval, push");
    let marker2 = format!("after-removal-{}", w.tag);
    let v2 = format!("v2 {marker2}\n");
    let c2 = commit_file(&alice, &work, "README", v2.as_bytes(), "feat: v2");
    let patch2 = alice.post(
        &url,
        "xyz.arkiv.patch",
        "create",
        None,
        &json!({"title": "v2", "head": c2}),
    );
    let review2 = alice.post(
        &url,
        "xyz.arkiv.review",
        "create",
        None,
        &json!({"patch": patch2}),
    );
    let approve2 = json!({"patch": patch2, "head": c2});
    alice.post(
        &url,
        "xyz.arkiv.review",
        "approve",
        Some(&review2),
        &approve2,
    );
    alice.git(&work, &["push", "-q", &url, "main"]);

    w.step("carol: pull sees v2");
    carol.git(&carol2, &["pull", "-q", "--ff-only"]);
    assert_eq!(carol.git(&carol2, &["rev-parse", "HEAD"]), c2);
    assert_eq!(
        std::fs::read_to_string(carol2.join("README")).expect("readme"),
        v2
    );

    w.step("bob (removed): fresh clone and op list cannot see v2");
    let out = bob.run(
        "git",
        &w.root,
        &["clone", "-q", &url, &w.dir("bob-clone-2")],
    );
    let bob2 = PathBuf::from(w.dir("bob-clone-2"));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.status.success() {
        let head = bob.git(&bob2, &["rev-parse", "HEAD"]);
        eprintln!("   bob's clone succeeded at {head}");
        assert_ne!(
            head, c2,
            "a removed member read content sealed after removal"
        );
        let text = std::fs::read_to_string(bob2.join("README")).unwrap_or_default();
        assert!(!text.contains(&marker2));
    } else {
        eprintln!("   bob's clone failed: {}", stderr.trim());
    }
    let listed = bob.run(
        env!("CARGO_BIN_EXE_chart"),
        &bob.home,
        &["op", "list", "--repo", &url],
    );
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        !listed.contains(&patch2),
        "a removed member lists an op sealed after removal"
    );
    assert!(
        listed.contains(&patch),
        "bob still reads the ops he could read before"
    );

    let creators: BTreeSet<[u8; 20]> = [&alice, &bob, &carol]
        .iter()
        .flat_map(|p| {
            let k = p.keys();
            [
                k.control_address(&rid),
                k.bulk_address(&rid),
                k.wallet().address(),
            ]
            .map(|a| a.into_array())
        })
        .collect();
    assert_unlinkable(w, &creators, &rid, &[&name, &marker, &marker2]);
}

/// The receipt as blob ops, skipping chunks already in `posted` (blobs are content
/// addressed), and the proof entry that references them.
fn post_receipt(p: &Peer, url: &str, receipt: &[u8], posted: &mut BTreeSet<String>) -> Value {
    let chunks: Vec<&[u8]> = receipt.chunks(BLOB_CHUNK).collect();
    for c in &chunks {
        if posted.insert(sha_hex(c)) {
            p.post(
                url,
                BLOB_TYPE,
                "create",
                None,
                &json!({"data": hex::encode(c)}),
            );
        }
    }
    let public: Value = serde_json::from_str(PUBLIC).expect("fixture");
    let hashes: Vec<String> = chunks.iter().map(|c| sha_hex(c)).collect();
    json!({"kind": "risc0", "receipt": sha_hex(receipt), "chunks": hashes, "public": public})
}

/// A sealed object and its commit; returns the object id.
fn sealed_commit(
    p: &Peer,
    url: &str,
    commitment: &str,
    issue: Option<&str>,
    proofs: Value,
) -> String {
    let obj = p.post(
        url,
        "xyz.arkiv.sealed",
        "create",
        None,
        &json!({"title": "sealed fix"}),
    );
    let mut payload =
        json!({"object": hex::encode([0x50; 32]), "commitment": commitment, "proofs": proofs});
    if let Some(i) = issue {
        payload["issue"] = i.into();
    }
    p.post(url, "xyz.arkiv.sealed", "commit", Some(&obj), &payload);
    obj
}

/// Items 3 and 4: sealed-patch proofs and a timelocked key reveal, on a private repo (bob is
/// a read member, dave an outsider) or a public one (bob is any reader).
fn zk_flow(w: &World, private: bool, wait_for_timelock: bool) {
    let alice = w.peer("alice", 245 * MILLI / 10);
    let bob = w.peer("bob", 25 * MILLI / 10);
    let dave = w.peer("dave", 0);
    let public: Value = serde_json::from_str(PUBLIC).expect("fixture");
    let key_hash = public["key_hash"].as_str().expect("key_hash").to_owned();
    let spec_hash = public["spec_hash"].as_str().expect("spec_hash").to_owned();

    w.step(&format!(
        "alice: init (private: {private}), push the demo base file"
    ));
    let name = format!("zk-{}", w.tag);
    let mut init = vec!["init", "--name", &name];
    if private {
        init.push("--private");
    }
    let url = field(&alice.arkiv(&init), "url:").to_owned();
    let rid = rid_of(&url);
    eprintln!("   rid {} ({})", to_text(&rid), hex32(&rid));
    let work = PathBuf::from(w.dir("alice-work"));
    alice.git(&w.root, &["init", "-q", "-b", "main", &w.dir("alice-work")]);
    let c0 = commit_file(&alice, &work, "base.rs", BASE, "feat: base");
    alice.git(&work, &["push", "-q", &url, "main"]);
    if private {
        let kp = field(&bob.arkiv(&["member", "keypackage"]), "key package:").to_owned();
        let add = [
            "member",
            "add",
            "--repo",
            &url,
            "--key-package",
            &kp,
            "--role",
            "read",
        ];
        alice.arkiv(&add);
    }

    w.step("alice: pin sealed-patch.rhai");
    let pin = common::programs::program("sealed-patch");
    alice.arkiv(&["program", "pin", "--repo", &url, "--file", &pin]);

    w.step("alice: issues requiring a proof (demo spec, other spec)");
    let issue = |spec: &str| {
        let p = json!({"title": "fix add", "proof": "required", "spec_hash": spec});
        alice.post(&url, "xyz.arkiv.issue", "create", None, &p)
    };
    let issue_demo = issue(&spec_hash);
    let issue_other = issue(&sha_hex(b"+ something else\n"));

    w.step("alice: receipt as blob ops, plus one tampered chunk");
    let mut posted = BTreeSet::new();
    let entry = post_receipt(&alice, &url, RECEIPT, &mut posted);
    let mut bad = RECEIPT.to_vec();
    // The last byte, so only the short final chunk differs and one small blob is posted.
    bad[RECEIPT.len() - 1] ^= 1;
    let tampered = post_receipt(&alice, &url, &bad, &mut posted);
    eprintln!("   {} blob ops", posted.len());

    w.step("alice: sealed commits (proven, missing, tampered, other spec)");
    let good = sealed_commit(
        &alice,
        &url,
        &key_hash,
        Some(&issue_demo),
        json!([entry.clone()]),
    );
    let missing = sealed_commit(&alice, &url, &key_hash, Some(&issue_demo), json!([]));
    let bad_obj = sealed_commit(
        &alice,
        &url,
        &key_hash,
        Some(&issue_demo),
        json!([tampered]),
    );
    let other = sealed_commit(&alice, &url, &key_hash, Some(&issue_other), json!([entry]));

    w.step("timelock: alice commits to a key and seals it to drand quicknet");
    let key = random_hex();
    let tl = sealed_commit(&alice, &url, &sha_hex(key.as_bytes()), None, json!([]));
    let key_file = w.root.join("key.txt");
    std::fs::write(&key_file, &key).expect("key");
    let blob_file = w.root.join("key.tlock");
    let until = if wait_for_timelock { "+150s" } else { "+1h" };
    let (kf, bf) = (key_file.to_string_lossy(), blob_file.to_string_lossy());
    let sealed = alice.arkiv(&[
        "timelock", "seal", "--until", until, "--in", &kf, "--out", &bf,
    ]);
    eprintln!("   {}", sealed.trim());
    std::fs::remove_file(&key_file).expect("rm key");
    let blob = std::fs::read(&blob_file).expect("blob");
    let unlock_at = charter_timelock::SealedBlob::from_bytes(&blob)
        .expect("blob")
        .unlock_at();
    let tl_payload = json!({"sealed": tl, "blob": hex::encode(&blob)});
    alice.post(&url, "xyz.arkiv.timelock", "create", None, &tl_payload);

    w.step("bob (fresh member): clone and fold");
    let bob_clone = PathBuf::from(w.dir("bob-clone"));
    bob.git(&w.root, &["clone", "-q", &url, &w.dir("bob-clone")]);
    assert_eq!(bob.git(&bob_clone, &["rev-parse", "HEAD"]), c0);
    let status = bob.arkiv(&["program", "status", "--repo", &url]);
    let st = state(&status);
    eprintln!("   state {st}");
    assert_eq!(st["sealed"][&good]["proven"], true, "{status}");
    assert_eq!(st["sealed"][&good]["issue"], issue_demo.as_str());
    for (case, obj) in [
        ("missing", &missing),
        ("tampered", &bad_obj),
        ("other spec", &other),
    ] {
        assert!(
            st["sealed"][obj].is_null(),
            "{case} proof accepted: {status}"
        );
    }
    assert!(
        status.contains(&format!("\"object\":\"{tl}\"")),
        "release_key owed: {status}"
    );
    if private {
        let err = dave.arkiv_fails(&["program", "status", "--repo", &url]);
        assert!(err.contains("E_NOT_MEMBER"), "{err}");
    }

    w.step("bob: reads the timelock op, opens it too early");
    let ops = bob.arkiv(&[
        "op",
        "list",
        "--repo",
        &url,
        "--type",
        "xyz.arkiv.timelock",
        "--json",
    ]);
    let ops: Value = serde_json::from_str(&ops).expect("json");
    let tl_bytes = hex::decode(ops[0]["payload"]["blob"].as_str().expect("blob")).expect("hex");
    let fetched = w.root.join("fetched.tlock");
    std::fs::write(&fetched, &tl_bytes).expect("write");
    let opened = w.root.join("opened.txt");
    let (ff, of) = (fetched.to_string_lossy(), opened.to_string_lossy());
    let err = bob.arkiv_fails(&["timelock", "open", "--in", &ff, "--out", &of]);
    assert!(err.contains("E_TOO_EARLY"), "{err}");
    assert!(!opened.exists());
    if !wait_for_timelock {
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let wait = unlock_at.saturating_sub(now) + 6;
    w.step(&format!("waiting {wait} s for drand round to pass"));
    std::thread::sleep(Duration::from_secs(wait));

    w.step("fresh client: opens the timelock after the round");
    let fresh = w.peer("erin", 0);
    fresh.arkiv(&["timelock", "open", "--in", &ff, "--out", &of]);
    assert_eq!(std::fs::read_to_string(&opened).expect("opened"), key);
    w.step("bob: posts the reveal with the opened key");
    bob.post(
        &url,
        "xyz.arkiv.sealed",
        "reveal",
        Some(&tl),
        &json!({"key": key}),
    );
    let status = bob.arkiv(&["program", "status", "--repo", &url]);
    assert_eq!(state(&status)["revealed"][&tl], key.as_str(), "{status}");
    assert!(
        !status.contains(&format!("\"object\":\"{tl}\"")),
        "{status}"
    );
    eprintln!("   state {}", state(&status));
}

// ---- MemArkiv rehearsals -------------------------------------------------------------

fn mem_world<R>(f: impl FnOnce(&World) -> R) -> R {
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let fund = |p: &Peer, _wei: u128| {
        let wallet = p.keys().wallet().address().to_string();
        common::e2e::fund(&log, &wallet);
    };
    let raw = |pred: Pred| {
        charter_cli::block_on(async {
            let mem = FileMemArkiv::open(&log, MEM_CHAIN_ID).expect("open");
            observe::all(&mem, pred).await.expect("query")
        })
        .expect("runtime")
    };
    let w = World {
        root: tmp.path().to_path_buf(),
        backend: Backend::Mem(log.clone()),
        t0: Instant::now(),
        tag: random_hex()[..8].to_owned(),
        fund: &fund,
        raw: &raw,
    };
    f(&w)
}

#[test]
fn mem_private_members_roles_removal_and_program() {
    mem_world(private_flow);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "a 60 KB --payload-json blob op passes the 32K Windows command line"
)]
fn mem_zk_sealed_patch_proofs_and_timelock() {
    mem_world(|w| zk_flow(w, true, false));
}

#[test]
#[cfg_attr(
    windows,
    ignore = "a 60 KB --payload-json blob op passes the 32K Windows command line"
)]
fn mem_zk_sealed_patch_proofs_and_timelock_public() {
    mem_world(|w| zk_flow(w, false, false));
}

// ---- Rabdi ----------------------------------------------------------------------------

fn bank() -> Keys {
    let home = std::env::var("CHARTER_LIVE_BANK").expect("set CHARTER_LIVE_BANK");
    Keys::require(&PathBuf::from(home).join(".charter")).expect("bank keys")
}

fn live_world(name: &str, f: impl FnOnce(&World)) {
    if !common::live::enabled() {
        return;
    }
    let s = common::live::secrets();
    let rpc = common::live::connect(&s);
    let bank = bank().wallet();
    let fmt = charter_cli::units::fmt_glm;
    let before = common::live::balance(&rpc, bank.address());
    eprintln!("bank {} before: {} tGLM", bank.address(), fmt(before));
    let base = std::env::var("CHARTER_LIVE_ROOT").expect("set CHARTER_LIVE_ROOT");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let root = PathBuf::from(base).join(format!("{name}-{stamp}"));
    std::fs::create_dir_all(&root).expect("root");
    let fund = |p: &Peer, wei: u128| {
        let to = p.keys().wallet().address();
        common::live::transfer(&rpc, &bank, to, U256::from(wei));
        eprintln!(
            "   funded {} wallet {to} with {} tGLM",
            p.name,
            fmt(U256::from(wei))
        );
    };
    let raw = |pred: Pred| {
        charter_cli::block_on(observe::all(&rpc, pred))
            .expect("runtime")
            .expect("query")
    };
    let w = World {
        root: root.clone(),
        backend: Backend::Live(s.access.clone()),
        t0: Instant::now(),
        tag: random_hex()[..8].to_owned(),
        fund: &fund,
        raw: &raw,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&w)));
    for entry in std::fs::read_dir(&root).expect("root").flatten() {
        let home = entry.path();
        let Ok(keys) = Keys::require(&home.join(".charter")) else {
            continue;
        };
        let rids = common::live::rids(&home);
        for rid in &rids {
            for (role, a) in [
                ("control", keys.control_address(rid)),
                ("bulk", keys.bulk_address(rid)),
            ] {
                let b = common::live::balance(&rpc, a);
                eprintln!(
                    "   {} {role} {a}: {} tGLM before sweep",
                    entry.file_name().to_string_lossy(),
                    fmt(b)
                );
            }
        }
        common::live::sweep(&rpc, &keys, &rids, bank.address());
    }
    let after = common::live::balance(&rpc, bank.address());
    eprintln!(
        "bank after: {} tGLM, flow spent {} tGLM, {} rate-limit waits, {:.1} s",
        fmt(after),
        fmt(before.saturating_sub(after)),
        common::live::RATE_LIMITED.load(std::sync::atomic::Ordering::Relaxed),
        w.t0.elapsed().as_secs_f64()
    );
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

#[test]
#[ignore = "live Rabdi: private repo members, roles, removal and a program; spends bank funds"]
fn live_private_members_roles_removal_and_program() {
    live_world("private", private_flow);
}

#[test]
#[ignore = "live Rabdi: sealed-patch proofs and timelock on a public repo; spends bank funds"]
fn live_zk_sealed_patch_proofs_and_timelock() {
    // Public, to fit the remaining testnet budget; the private variant runs on the fake.
    live_world("zk", |w| zk_flow(w, false, true));
}

#[test]
#[ignore = "live Rabdi: moves BANK_WEI from the throwaway key to the bank; run under the lock"]
fn bank_fund() {
    if !common::live::enabled() {
        return;
    }
    let s = common::live::secrets();
    let rpc = common::live::connect(&s);
    let to = bank().wallet().address();
    let fmt = charter_cli::units::fmt_glm;
    eprintln!(
        "throwaway before: {}",
        fmt(common::live::balance(&rpc, s.key.address()))
    );
    common::live::transfer(&rpc, &s.key, to, U256::from(BANK_WEI));
    eprintln!("bank {to}: {}", fmt(common::live::balance(&rpc, to)));
}

#[test]
#[ignore = "live Rabdi: returns the bank's balance to the throwaway key"]
fn bank_sweep() {
    if !common::live::enabled() {
        return;
    }
    let s = common::live::secrets();
    let rpc = common::live::connect(&s);
    let keys = bank();
    common::live::sweep(&rpc, &keys, &[], s.key.address());
    let fmt = charter_cli::units::fmt_glm;
    eprintln!(
        "bank after: {}",
        fmt(common::live::balance(&rpc, keys.wallet().address()))
    );
    eprintln!(
        "throwaway after: {}",
        fmt(common::live::balance(&rpc, s.key.address()))
    );
}

/// Tops up one live peer from the bank, to finish a flow that ran short:
/// `CHARTER_LIVE_PAY_HOME=<peer home> CHARTER_LIVE_PAY_WEI=<wei>`.
#[test]
#[ignore = "live Rabdi: moves bank funds to one peer wallet"]
fn bank_pay() {
    if !common::live::enabled() {
        return;
    }
    let s = common::live::secrets();
    let rpc = common::live::connect(&s);
    let home = PathBuf::from(std::env::var("CHARTER_LIVE_PAY_HOME").expect("pay home"));
    let wei: u128 = std::env::var("CHARTER_LIVE_PAY_WEI")
        .expect("pay wei")
        .parse()
        .expect("wei");
    let to = Keys::require(&home.join(".charter"))
        .expect("keys")
        .wallet()
        .address();
    common::live::transfer(&rpc, &bank().wallet(), to, U256::from(wei));
    eprintln!("paid {to} {}", charter_cli::units::fmt_glm(U256::from(wei)));
}

/// Sweeps every peer home under `CHARTER_LIVE_COLLECT` (a flow root) back to the bank.
#[test]
#[ignore = "live Rabdi: returns peer leftovers under a flow root to the bank"]
fn bank_collect() {
    if !common::live::enabled() {
        return;
    }
    let s = common::live::secrets();
    let rpc = common::live::connect(&s);
    let root = PathBuf::from(std::env::var("CHARTER_LIVE_COLLECT").expect("root"));
    let bank = bank().wallet().address();
    for entry in std::fs::read_dir(&root).expect("root").flatten() {
        if let Ok(keys) = Keys::require(&entry.path().join(".charter")) {
            common::live::sweep(&rpc, &keys, &common::live::rids(&entry.path()), bank);
        }
    }
    let fmt = charter_cli::units::fmt_glm;
    eprintln!("bank after: {}", fmt(common::live::balance(&rpc, bank)));
}
