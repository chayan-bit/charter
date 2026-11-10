//! Request guard and response hardening (docs/p6/ui.md section 5).
//!
//! - Only `127.0.0.1:<port>` and `localhost:<port>` are accepted as `Host` (DNS rebinding).
//! - The printed URL carries a random token; `?token=` is exchanged once for an
//!   `HttpOnly; SameSite=Strict` cookie and a redirect that drops it from the address bar.
//! - Every request needs that cookie; every non-GET request also needs the session's CSRF
//!   token, in `x-csrf-token` (htmx) or as the `csrf` field of a urlencoded form (plain
//!   form posts), and an `Origin`, if sent, of this server.
//! - Every response gets a strict CSP and the usual hardening headers.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub const COOKIE: &str = "charter_ui";
pub const CSRF_HEADER: &str = "x-csrf-token";

/// No inline script or style at all: the critical CSS is one small same-origin file (about
/// 6 KB gzipped, cached), which is simpler than keeping a sha256 of an inline block in sync
/// with the stylesheet; htmx runs with its inline indicator styles and eval turned off.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'self'; frame-ancestors 'none'";

#[derive(Debug, Clone)]
pub struct Auth {
    pub token: String,
    pub csrf: String,
    hosts: [String; 2],
}

/// 32 random bytes, hex.
pub fn random_hex() -> std::io::Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(hex::encode(b))
}

impl Auth {
    pub fn new(port: u16) -> std::io::Result<Self> {
        Ok(Self::with(port, random_hex()?, random_hex()?))
    }

    pub fn with(port: u16, token: String, csrf: String) -> Self {
        Self {
            token,
            csrf,
            hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        }
    }

    /// The one URL `chart ui` prints.
    pub fn url(&self) -> String {
        format!("http://{}/?token={}", self.hosts[0], self.token)
    }
}

/// Equal-length comparison that does not stop at the first differing byte.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn cookie(h: &HeaderMap) -> Option<&str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| kv.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

fn query_token(req: &Request) -> Option<&str> {
    req.uri()
        .query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
}

fn deny(status: StatusCode, msg: &'static str) -> Response {
    let ct = [(header::CONTENT_TYPE, "text/plain; charset=utf-8")];
    (status, ct, msg).into_response()
}

/// `Ok(())` lets the request through; `Err` is the response to send instead.
fn check(auth: &Auth, req: &Request) -> Result<(), Response> {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    if !host.is_some_and(|h| auth.hosts.iter().any(|a| a == h)) {
        return Err(deny(StatusCode::FORBIDDEN, "bad host\n"));
    }
    if let Some(t) = query_token(req) {
        if !same(t, &auth.token) {
            return Err(deny(StatusCode::FORBIDDEN, "bad token\n"));
        }
        let set = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", auth.token);
        let to = req.uri().path().to_owned();
        let r = Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, to)
            .header(header::SET_COOKIE, set)
            .body(Body::empty());
        return Err(r.unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()));
    }
    if !cookie(req.headers()).is_some_and(|c| same(c, &auth.token)) {
        return Err(deny(
            StatusCode::UNAUTHORIZED,
            "open the URL that `chart ui` printed\n",
        ));
    }
    Ok(())
}

/// The largest form body accepted (a release form with notes stays far below).
const FORM_LIMIT: usize = 1 << 20;

fn is_form(req: &Request) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"))
}

/// Lets a write through only with the session's CSRF token and no foreign `Origin`; a
/// form body read to find the token is put back for the handler.
async fn csrf(auth: &Auth, req: Request) -> Result<Request, Response> {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return Ok(req);
    }
    let h = req.headers();
    let host = h.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = h.get(header::ORIGIN).map(|v| v.to_str().unwrap_or(""));
    if origin.is_some_and(|o| host.is_none_or(|h| o != format!("http://{h}"))) {
        return Err(deny(StatusCode::FORBIDDEN, "cross-origin write\n"));
    }
    let header_ok = h
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| same(c, &auth.csrf));
    if header_ok {
        return Ok(req);
    }
    if !is_form(&req) {
        return Err(deny(StatusCode::FORBIDDEN, "missing CSRF token\n"));
    }
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, FORM_LIMIT)
        .await
        .map_err(|_| deny(StatusCode::PAYLOAD_TOO_LARGE, "form too large\n"))?;
    let fields: Vec<(String, String)> = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();
    if !fields
        .iter()
        .any(|(k, v)| k == "csrf" && same(v, &auth.csrf))
    {
        return Err(deny(StatusCode::FORBIDDEN, "missing CSRF token\n"));
    }
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

/// The middleware: [`check`], then the handler, then the hardening headers on whatever
/// response goes out (denials included).
pub async fn guard(State(auth): State<std::sync::Arc<Auth>>, req: Request, next: Next) -> Response {
    let checked = match check(&auth, &req) {
        Ok(()) => csrf(&auth, req).await,
        Err(r) => Err(r),
    };
    let mut resp = match checked {
        Ok(req) => next.run(req).await,
        Err(r) => r,
    };
    let h = resp.headers_mut();
    // Appended, so a handler's own policy (raw's `sandbox`) is enforced as well as this one.
    h.append(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    for (k, v) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        // Not `no-referrer`: with it a JS-off form post carries `Origin: null`, refused above.
        (header::REFERRER_POLICY, "same-origin"),
        (header::X_FRAME_OPTIONS, "DENY"),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    h.entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole_and_cookies_parse_among_others() {
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; charter_ui=tok; b=2"),
        );
        assert_eq!(cookie(&h), Some("tok"));
        let a = Auth::with(8080, "t".into(), "c".into());
        assert_eq!(a.url(), "http://127.0.0.1:8080/?token=t");
        let r = random_hex().expect("rng");
        assert_eq!(r.len(), 64);
        assert_ne!(r, random_hex().expect("rng"));
    }
}
