//! docs/p6/ui.md section 5, one test per rule.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;

fn raw_req(method: Method, path: &str, host: Option<&str>, cookie: bool) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(h) = host {
        b = b.header(header::HOST, h);
    }
    if cookie {
        b = b.header(header::COOKIE, format!("charter_ui={TOKEN}"));
    }
    b.body(Body::empty()).expect("request")
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_bound_host_is_served() {
    for host in [
        None,
        Some("evil.example:4321"),
        Some("127.0.0.1:9999"),
        Some("127.0.0.1"),
    ] {
        let r = send(app(), raw_req(Method::GET, "/", host, true)).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{host:?}");
    }
    let r = send(
        app(),
        raw_req(Method::GET, "/", Some("localhost:4321"), true),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_url_token_becomes_a_strict_cookie_and_leaves_the_address_bar() {
    let r = send(
        app(),
        raw_req(
            Method::GET,
            &format!("/r/x?token={TOKEN}"),
            Some(HOST),
            false,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.header(header::LOCATION), "/r/x");
    let c = r.header(header::SET_COOKIE);
    assert!(c.starts_with(&format!("charter_ui={TOKEN};")), "{c}");
    assert!(
        c.contains("HttpOnly") && c.contains("SameSite=Strict"),
        "{c}"
    );
    let r = send(
        app(),
        raw_req(Method::GET, "/?token=wrong", Some(HOST), false),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_request_needs_the_session_cookie() {
    for p in ["/", "/assets/app.css", "/nowhere"] {
        let r = send(app(), raw_req(Method::GET, p, Some(HOST), false)).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{p}");
    }
    let mut q = raw_req(Method::GET, "/", Some(HOST), false);
    q.headers_mut()
        .insert(header::COOKIE, "charter_ui=t0kem".parse().expect("hv"));
    assert_eq!(send(app(), q).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_need_the_csrf_token() {
    let r = send(app(), raw_req(Method::POST, "/", Some(HOST), true)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let mut q = raw_req(Method::POST, "/", Some(HOST), true);
    q.headers_mut()
        .insert("x-csrf-token", CSRF.parse().expect("hv"));
    let r = send(app(), q).await;
    assert_eq!(
        r.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "past the guard; no write routes yet"
    );
    assert!(
        page("/")
            .await
            .contains(&format!("name=\"csrf-token\" content=\"{CSRF}\""))
    );
}

fn form(body: &str, origin: Option<&str>) -> Request<Body> {
    let mut b = Request::post("/")
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("charter_ui={TOKEN}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(o) = origin {
        b = b.header(header::ORIGIN, o);
    }
    b.body(Body::from(body.to_owned())).expect("request")
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_form_posts_carry_the_csrf_token_as_a_field_from_this_origin() {
    let past = StatusCode::METHOD_NOT_ALLOWED;
    let ok = format!("a=1&csrf={CSRF}&b=2");
    assert_eq!(send(app(), form(&ok, None)).await.status, past);
    let here = format!("http://{HOST}");
    assert_eq!(send(app(), form(&ok, Some(&here))).await.status, past);
    for (body, origin) in [
        ("csrf=wrong", None),
        ("a=1", None),
        (ok.as_str(), Some("http://evil.example")),
        (ok.as_str(), Some("null")),
    ] {
        let r = send(app(), form(body, origin)).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{body} {origin:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn responses_carry_a_strict_csp_and_hardening_headers() {
    for r in [
        get("/").await,
        get("/nowhere").await,
        send(app(), raw_req(Method::GET, "/", Some(HOST), false)).await,
    ] {
        let csp = r.header(header::CONTENT_SECURITY_POLICY);
        assert!(csp.starts_with("default-src 'self'") && csp.contains("frame-ancestors 'none'"));
        assert!(!csp.contains("unsafe"), "{csp}");
        assert_eq!(r.header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
        // `no-referrer` would make browsers send `Origin: null` on plain form posts.
        assert_eq!(r.header(header::REFERRER_POLICY), "same-origin");
    }
    assert_eq!(get("/").await.header(header::CACHE_CONTROL), "no-store");
    let css = get("/assets/app.css").await;
    assert!(css.header(header::CACHE_CONTROL).contains("immutable"));
    assert!(css.header(header::CONTENT_TYPE).starts_with("text/css"));
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_have_no_inline_script_or_style() {
    let f = fixture();
    let h = &f.hex;
    for p in [
        "/".to_owned(),
        format!("/r/{h}"),
        format!("/r/{h}/tree/main/src"),
        format!("/r/{h}/blob/main/src/main.rs"),
        format!("/r/{h}/log/main"),
        format!("/r/{h}/commit/{}", f.main),
        format!("/r/{h}/issues/{}", f.issue),
        format!("/r/{h}/patches/{}", f.patch),
        format!("/r/{h}/releases"),
        format!("/r/{h}/program"),
    ] {
        let html = page(&p).await;
        assert!(!html.contains("<style") && !html.contains("style="), "{p}");
        let inline = html
            .match_indices("<script")
            .any(|(i, _)| !html[i..].starts_with("<script src="));
        assert!(!inline, "{p}");
        assert!(
            !html.contains("onerror=") && !html.contains("onclick="),
            "{p}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_files_are_plain_text_or_attachments() {
    let f = fixture();
    let r = get(&format!("/r/{}/raw/main/README.md", f.hex)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header(header::CONTENT_TYPE), "text/plain; charset=utf-8");
    assert!(
        r.text().contains("<script>alert(1)</script>"),
        "served as is, as text"
    );
    let r = get(&format!("/r/{}/raw/main/bin.dat", f.hex)).await;
    assert_eq!(r.header(header::CONTENT_TYPE), "application/octet-stream");
    assert_eq!(
        r.header(header::CONTENT_DISPOSITION),
        "attachment; filename=\"bin.dat\""
    );
    assert_eq!(r.body, vec![0, 1, 2, 255]);
}

/// Raw files: only allowlisted image types inline; SVG and HTML as text; other binaries as
/// a download; every raw response sandboxed on top of the page CSP.
#[tokio::test(flavor = "multi_thread")]
async fn raw_serves_only_allowlisted_images_inline_and_sandboxes_everything() {
    let f = fixture();
    let cases = [
        ("logo.png", "image/png", false),
        ("evil.svg", "text/plain; charset=utf-8", false),
        ("page.html", "text/plain; charset=utf-8", false),
        ("bin.dat", "application/octet-stream", true),
        ("README.md", "text/plain; charset=utf-8", false),
    ];
    for (name, ct, download) in cases {
        let r = get(&format!("/r/{}/raw/main/{name}", f.hex)).await;
        assert_eq!(r.status, StatusCode::OK, "{name}");
        assert_eq!(r.header(header::CONTENT_TYPE), ct, "{name}");
        let disp = r.header(header::CONTENT_DISPOSITION);
        assert_eq!(disp.starts_with("attachment"), download, "{name}: {disp}");
        let csp: Vec<&str> = r
            .headers
            .get_all(header::CONTENT_SECURITY_POLICY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert!(csp.contains(&"sandbox"), "{name}: {csp:?}");
        assert!(
            csp.iter().any(|c| c.starts_with("default-src 'self'")),
            "{name}"
        );
        assert_eq!(r.header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    }
}
