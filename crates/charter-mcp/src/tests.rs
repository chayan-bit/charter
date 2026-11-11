//! Tool dispatch against the real `chart` binary on the file-persisted fake chain.
//! `cargo test --workspace` builds that binary into the same target directory.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../charter-zk/tests/fixtures");

/// `target/<profile>/chart`, beside this test's `deps/` directory.
fn cli() -> PathBuf {
    let exe = std::env::current_exe().expect("test exe");
    let bin = exe
        .parent()
        .and_then(Path::parent)
        .expect("target dir")
        .join("chart");
    assert!(
        bin.is_file(),
        "{} missing: run `cargo build -p charter-cli` first",
        bin.display()
    );
    bin
}

fn server(root: &Path) -> ArkivGit {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let s = |p: &Path| p.to_string_lossy().into_owned();
    let env = [
        ("HOME", s(&home)),
        ("CHARTER_HOME", s(&home.join(".charter"))),
        ("CHARTER_BACKEND", format!("mem:{}", s(&root.join("chain")))),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_AUTHOR_NAME", "t".into()),
        ("GIT_AUTHOR_EMAIL", "t@example.com".into()),
        ("GIT_COMMITTER_NAME", "t".into()),
        ("GIT_COMMITTER_EMAIL", "t@example.com".into()),
    ];
    let env = env.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    let work = root.to_path_buf();
    ArkivGit::new(Runner {
        bin: cli(),
        env,
        work,
    })
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

/// A funded identity, a repo with one commit on `main`; returns its url.
async fn repo(s: &ArkivGit, root: &Path) -> String {
    let r = &s.runner;
    r.arkiv(&args(&["keys", "init"])).await.expect("keys");
    r.arkiv(&args(&["fund", "--dev", "1000"]))
        .await
        .expect("fund");
    let out = r
        .arkiv(&args(&["init", "--name", "mcp"]))
        .await
        .expect("init");
    let url = out
        .lines()
        .find_map(|l| l.strip_prefix("url: "))
        .expect("url")
        .to_owned();
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");
    std::fs::write(work.join("README"), "v0\n").expect("write");
    for a in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &["commit", "-q", "-m", "feat: v0"],
        &["push", "-q", &url, "main"],
    ] {
        r.git(&work, &args(a)).await.expect("git");
    }
    url
}

fn ok(r: ToolResult) -> Value {
    serde_json::from_str(&r.expect("tool ok")).expect("json")
}

fn list(repo: &str, ty: Option<&str>, action: Option<&str>) -> Parameters<ListArgs> {
    Parameters(ListArgs {
        repo: repo.into(),
        type_name: ty.map(Into::into),
        action: action.map(Into::into),
        object: None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_dispatch_to_the_cli_on_the_fake_chain() {
    let tmp = tempfile::tempdir().expect("tmp");
    let s = server(tmp.path());
    let url = repo(&s, tmp.path()).await;

    let post = |ty: &str, action: &str, payload: Value| {
        Parameters(PostArgs {
            repo: url.clone(),
            type_name: ty.into(),
            action: action.into(),
            object: None,
            parents: vec![],
            payload,
        })
    };
    let issue = ok(s
        .op_post(post(
            "xyz.arkiv.issue",
            "create",
            json!({"title": "t", "bounty": 5}),
        ))
        .await);
    let id = issue["id"].as_str().expect("id").to_owned();
    assert_eq!(id.len(), 64);

    let issues = ok(s
        .op_list(list(&url, Some("xyz.arkiv.issue"), Some("create")))
        .await);
    assert_eq!(issues[0]["id"], id.as_str());
    assert_eq!(issues[0]["payload"]["bounty"], 5);
    let none = ok(s.op_list(list(&url, None, Some("merged"))).await);
    assert_eq!(none, json!([]));

    let clone = tmp.path().join("clone");
    let cloned = s
        .repo_clone(Parameters(CloneArgs {
            repo: url.trim_start_matches("arkiv::").into(),
            dir: clone.clone(),
        }))
        .await;
    assert_eq!(ok(cloned)["dir"], clone.to_string_lossy().as_ref());
    assert_eq!(
        std::fs::read_to_string(clone.join("README")).expect("readme"),
        "v0\n"
    );
    std::fs::write(clone.join("README"), "v1\n").expect("edit");
    let pushed = ok(s
        .git_push(Parameters(PushArgs {
            repo: url.clone(),
            dir: clone.clone(),
            branch: "agent/fix".into(),
            message: Some("fix: v1".into()),
        }))
        .await);
    let head = s.runner.git(&clone, &args(&["rev-parse", "HEAD"])).await;
    assert_eq!(
        pushed["head"].as_str(),
        Some(head.expect("rev-parse").trim())
    );
    let keys = s
        .runner
        .arkiv(&args(&["keys", "show"]))
        .await
        .expect("keys");
    let nid = keys.split_whitespace().nth(1).expect("nid");
    let peer = format!("{url}/{nid}");
    let remote = s.runner.git(&clone, &args(&["ls-remote", &peer])).await;
    assert!(remote.expect("ls-remote").contains("refs/heads/agent/fix"));

    // Proof files must sit in the working area, as an agent's session dir holds them.
    for f in ["demo.receipt", "demo.public.json"] {
        std::fs::copy(format!("{FIXTURES}/{f}"), tmp.path().join(f)).expect("fixture");
    }
    let attach = |payload: Value| {
        Parameters(AttachArgs {
            repo: url.clone(),
            type_name: None,
            action: None,
            object: None,
            parents: vec![],
            payload,
            receipt: "demo.receipt".into(),
            public: tmp.path().join("demo.public.json"),
        })
    };
    let blobs = || s.op_list(list(&url, Some("xyz.arkiv.blob"), None));
    let patch = ok(s
        .attach_proof(attach(json!({"head": "h", "issue": id})))
        .await);
    let n = ok(blobs().await).as_array().expect("array").len();
    assert!(n > 1, "the receipt spans several blobs");
    ok(s.attach_proof(attach(json!({"head": "h2"}))).await);
    assert_eq!(
        ok(blobs().await).as_array().expect("array").len(),
        n,
        "blobs are reused"
    );
    let patches = ok(s.op_list(list(&url, Some("xyz.arkiv.patch"), None)).await);
    assert_eq!(patches[0]["id"], patch["id"]);
    let proof = &patches[0]["payload"]["proofs"][0];
    assert_eq!(
        (
            proof["kind"].as_str(),
            proof["chunks"].as_array().map(Vec::len)
        ),
        (Some("risc0"), Some(n))
    );

    let st = s
        .program_status(Parameters(RepoArg { repo: url.clone() }))
        .await
        .expect("status");
    assert!(st.contains("no program pinned"), "{st}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_failures_come_back_as_coded_tool_errors() {
    let tmp = tempfile::tempdir().expect("tmp");
    let s = server(tmp.path());
    let url = repo(&s, tmp.path()).await;

    let bad = s
        .op_post(Parameters(PostArgs {
            repo: url.clone(),
            type_name: "xyz.arkiv.issue".into(),
            action: "create".into(),
            object: Some("zz".into()),
            parents: vec![],
            payload: json!({}),
        }))
        .await;
    let e: Value = serde_json::from_str(&bad.expect_err("bad object id")).expect("json");
    assert!(
        e["code"].as_str().is_some_and(|c| c.starts_with("E_")),
        "{e}"
    );
    assert_eq!(e["exit"], 1);

    let clone = s
        .repo_clone(Parameters(CloneArgs {
            repo: url.clone(),
            dir: tmp.path().join("work"),
        }))
        .await;
    let e: Value = serde_json::from_str(&clone.expect_err("dir exists")).expect("json");
    assert_eq!(e["exit"], 128, "{e}");

    let audit = s.escrow_status(Parameters(RepoArg { repo: url })).await;
    let e: Value = serde_json::from_str(&audit.expect_err("no escrow")).expect("json");
    assert!(e["code"].is_string(), "{e}");
}

/// Tool arguments come from a possibly prompt-injected agent: none of them reaches git as an
/// option, and no path leaves the server's working area.
#[tokio::test(flavor = "multi_thread")]
async fn agent_arguments_cannot_inject_options_or_leave_the_work_area() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().canonicalize().expect("canonical");
    let s = server(&root);
    let url = repo(&s, &root).await;
    // A template whose post-checkout hook leaves a marker: `--template=t` as the clone dir
    // would run it (relative to the working area, where the server runs git).
    let hooks = root.join("t/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks");
    let marker = root.join("pwned");
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).expect("hook");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    let clone = |repo: &str, dir: &str| {
        s.repo_clone(Parameters(CloneArgs {
            repo: repo.into(),
            dir: dir.into(),
        }))
    };
    assert!(clone(&url, "--template=t").await.is_err());
    assert!(!marker.exists(), "an option in `dir` ran a hook");
    for dir in ["../escape", "/tmp/escape", "a/../../escape"] {
        assert!(clone(&url, dir).await.is_err(), "{dir} left the work area");
    }
    for repo in ["--upload-pack=touch x", "arkiv::-x", "arkiv::zz/../x", ""] {
        assert!(clone(repo, "ok").await.is_err(), "{repo:?} is no repo url");
    }
    // Nothing outside the work area can be posted as a "receipt" (an exfiltration path).
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    let secret = elsewhere.path().join("seed");
    std::fs::write(&secret, b"TOPSECRET seed bytes").expect("secret");
    let public = root.join("demo.public.json");
    std::fs::copy(format!("{FIXTURES}/demo.public.json"), &public).expect("public");
    let attach = |receipt: PathBuf| {
        s.attach_proof(Parameters(AttachArgs {
            repo: url.clone(),
            type_name: None,
            action: None,
            object: None,
            parents: vec![],
            payload: json!({"head": "h"}),
            receipt,
            public: public.clone(),
        }))
    };
    assert!(
        attach(secret).await.is_err(),
        "a file outside the work area was posted"
    );
    let blobs = s.op_list(list(&url, Some("xyz.arkiv.blob"), None)).await;
    assert_eq!(ok(blobs), json!([]), "nothing was posted");
    let push = s
        .git_push(Parameters(PushArgs {
            repo: url.clone(),
            dir: "/".into(),
            branch: "x".into(),
            message: None,
        }))
        .await;
    assert!(push.is_err(), "git_push ran outside the work area");
}
