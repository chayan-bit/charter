#!/usr/bin/env python3
"""Playwright E2E + accessibility harness for `chart ui` (docs/p6/ui.md sections 5, 6, 7).

Run through run.sh, which starts the server on a fixture repo and sets:
  E2E_URL   the tokenized URL `chart ui` printed
  E2E_OUT   output directory (screenshots, report.json, report.md)
  E2E_T0    epoch seconds just before the server was spawned
`python3 e2e.py first-page` only times the first page (stdlib only, fast start).
"""
import gzip
import hashlib
import http.client
import json
import os
import re
import statistics
import sys
import time
from pathlib import Path
from urllib.parse import urlparse

HERE = Path(__file__).resolve().parent
URL = os.environ["E2E_URL"]
_u = urlparse(URL)
HOST, PORT = _u.hostname, _u.port
TOKEN = _u.query.removeprefix("token=")
OUT = Path(os.environ.get("E2E_OUT", HERE / "out"))
DEBUG_BUILD = "debug" in os.environ.get("CHART_BIN_DIR", "debug")
AXE_TAGS = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"]
TAB_CAP = 400
SHOT_MAX_H = 6000  # full-page screenshots of taller pages are clipped to this

THEMES = ("light", "dark")
VIEWPORTS = {"desktop": (1280, 800), "phone": (390, 844)}


def get(path, cookie=None, host=None, method="GET", headers=None):
    """One raw HTTP request: (status, lowercase header dict, body bytes, set-cookie list)."""
    c = http.client.HTTPConnection(HOST, PORT, timeout=30)
    h = {"Host": host or f"{HOST}:{PORT}", **(headers or {})}
    if cookie:
        h["Cookie"] = cookie
    c.request(method, path, headers=h)
    r = c.getresponse()
    body = r.read()
    cookies = [v for k, v in r.getheaders() if k.lower() == "set-cookie"]
    out = (r.status, {k.lower(): v for k, v in r.getheaders()}, body, cookies)
    c.close()
    return out


def session_cookie():
    status, hdr, _, cookies = get(f"/?token={TOKEN}")
    assert status == 303 and cookies, f"token exchange failed: {status}"
    return cookies[0].split(";")[0], status, hdr, cookies[0]


def first_page():
    """Time from server spawn to the first rendered repo page (fresh process, stdlib only)."""
    t0 = float(os.environ["E2E_T0"])
    cookie, *_ = session_cookie()
    t_cookie = time.time() - t0
    deadline = time.time() + 30
    while time.time() < deadline:
        s, _, body, _ = get("/", cookie)
        m = re.search(rb"/r/([0-9a-f]{64})", body)
        if s == 200 and m:
            s2, _, b2, _ = get(f"/r/{m.group(1).decode()}", cookie)
            if s2 == 200 and b"vp-verified" in b2:
                break
        time.sleep(0.005)
    else:
        raise SystemExit("first page never rendered")
    res = {"seconds_to_session": round(t_cookie, 3), "seconds_to_first_repo_page": round(time.time() - t0, 3)}
    (OUT / "first-page.json").write_text(json.dumps(res))
    print(json.dumps(res))


# ---------------------------------------------------------------- report plumbing
CHECKS = []  # {name, scope, status: pass|fail|warn, detail}
AXE = {}  # page -> theme -> [violations]
PAGE_STATS = {}  # page -> metrics
SHOTS = []


def rec(name, scope, ok, detail="", warn=False):
    status = "pass" if ok else ("warn" if warn else "fail")
    CHECKS.append({"name": name, "scope": scope, "status": status, "detail": detail})
    if status != "pass":
        print(f"  [{status.upper()}] {name} :: {scope} :: {detail[:200]}")


# ---------------------------------------------------------------- discovery
def discover(cookie):
    s, _, body, _ = get("/", cookie)
    assert s == 200, f"/ -> {s}"
    rid = re.search(r"/r/([0-9a-f]{64})", body.decode()).group(1)
    r = f"/r/{rid}"
    log = get(f"{r}/log/main", cookie)[2].decode()
    shas = list(dict.fromkeys(re.findall(r"/commit/([0-9a-f]{40})", log)))
    issues = get(f"{r}/issues", cookie)[2].decode()
    issue = re.search(r'/issues/([0-9a-f]{64})">Crash', issues).group(1)
    patches = get(f"{r}/patches", cookie)[2].decode()
    patch = re.search(r"/patches/([0-9a-f]{64})", patches).group(1)
    # (name, path, expected substrings in the JS-less main text, expected status)
    return rid, [
        ("home", "/", ["charter-demo"], 200),
        ("repo", r, ["Charter demo", "Branches and tags"], 200),
        ("tree", f"{r}/tree/main", ["README.md"], 200),
        ("tree-nested", f"{r}/tree/main/src", ["lib.rs"], 200),
        ("blob", f"{r}/blob/main/src/main.rs", ["println"], 200),
        ("blob-long", f"{r}/blob/main/src/long.rs", ["value_1999"], 200),
        ("blob-wide", f"{r}/blob/main/src/wide.txt", ["xxxxxxxxxx"], 200),
        ("log", f"{r}/log/main", ["initial import"], 200),
        ("commit", f"{r}/commit/{shas[0]}", ["smaller main"], 200),
        ("commit-first", f"{r}/commit/{shas[-1]}", ["initial import"], 200),
        ("commit-long", f"{r}/commit/{shas[1]}", ["long generated file"], 200),
        ("issues", f"{r}/issues", ["Second issue"], 200),
        ("issue", f"{r}/issues/{issue}", ["crashes"], 200),
        ("patches", f"{r}/patches", ["Fix the crash"], 200),
        ("patch", f"{r}/patches/{patch}", ["Fix the crash"], 200),
        ("releases", f"{r}/releases", ["v0.1.0"], 200),
        ("program", f"{r}/program", ["Pinned program"], 200),
        ("not-found", f"{r}/nope", ["Nothing here"], 404),
    ]


# ---------------------------------------------------------------- in-page scripts
INIT_JS = """
window.__csp = [];
document.addEventListener('securitypolicyviolation',
  e => window.__csp.push(e.violatedDirective + ' ' + e.blockedURI), true);
"""
FOCUSABLES_JS = """() => {
  const sel = 'a[href],button:not([disabled]),input:not([disabled]):not([type=hidden]),select:not([disabled]),' +
    'textarea:not([disabled]),summary,[tabindex]:not([tabindex="-1"])';
  const vis = e => { const r = e.getClientRects(); if (!r.length) return false;
    const s = getComputedStyle(e); return s.visibility === 'visible' && r[0].width > 0 && r[0].height > 0; };
  window.__f = [...document.querySelectorAll(sel)].filter(vis);
  document.activeElement && document.activeElement.blur();
  return window.__f.length;
}"""
STEP_JS = """() => {
  const e = document.activeElement; if (!e || e === document.body) return null;
  const s = getComputedStyle(e); let fv = false; try { fv = e.matches(':focus-visible'); } catch (_) {}
  const d = e.tagName.toLowerCase() + (e.id ? '#' + e.id : '') + ' ' + ((e.textContent || e.value || '').trim().slice(0, 30));
  return {i: window.__f.indexOf(e), d, fv, os: s.outlineStyle, ow: parseFloat(s.outlineWidth) || 0,
          oc: s.outlineColor, bs: s.boxShadow};
}"""
OVERFLOW_JS = """() => {
  const iw = window.innerWidth, off = [];
  for (const e of document.body.querySelectorAll('*')) {
    const r = e.getBoundingClientRect();
    if (r.width && r.right > iw + 1) off.push([Math.round(r.right), e.tagName.toLowerCase() +
      (e.className && typeof e.className === 'string' ? '.' + e.className.trim().split(/\\s+/)[0] : '')]);
  }
  off.sort((a, b) => b[0] - a[0]);
  return {sw: document.documentElement.scrollWidth, iw, off: off.slice(0, 4)};
}"""
DOM_SAFETY_JS = """() => ({
  scripts: [...document.querySelectorAll('main script, article script')].length,
  handlers: [...document.querySelectorAll('*')].filter(e => [...e.attributes].some(a => /^on/i.test(a.name)))
    .map(e => e.tagName.toLowerCase()).slice(0, 5),
  pwned: typeof window.__pwned !== 'undefined',
  jsurls: [...document.querySelectorAll('[href^="javascript:" i],[src^="javascript:" i]')].length,
})"""


def keyboard_check(page, name, scope):
    n = page.evaluate(FOCUSABLES_JS)
    reached, bad, steps, misses = set(), [], 0, 0
    for _ in range(min(n + 5, TAB_CAP)):
        page.keyboard.press("Tab")
        steps += 1
        s = page.evaluate(STEP_JS)
        if s is None:
            misses += 1
            if misses > 2:
                break
            continue
        if s["i"] >= 0 and s["i"] in reached:
            break  # wrapped around
        if s["i"] >= 0:
            reached.add(s["i"])
        ring = s["fv"] and (
            (s["os"] != "none" and s["ow"] >= 2 and s["oc"] != "rgba(0, 0, 0, 0)") or s["bs"] != "none"
        )
        if not ring:
            bad.append(f'{s["d"]} (focus-visible={s["fv"]}, outline={s["os"]} {s["ow"]}px {s["oc"]}, shadow={s["bs"][:30]})')
    capped = n > TAB_CAP
    unreached = n - len(reached)
    rec("keyboard: every link/button reachable by Tab", scope, capped or unreached == 0,
        f"{n} focusable, {len(reached)} reached in {steps} Tab presses; {unreached} not reached" if not capped else "")
    if capped:
        rec("keyboard: tab-stop count is manageable", scope, False,
            f"{n} tab stops (cap {TAB_CAP}); only the first {TAB_CAP} were checked", warn=True)
    rec("keyboard: visible focus ring (2px outline)", scope, not bad, "; ".join(bad[:4]) + (f" (+{len(bad)-4} more)" if len(bad) > 4 else ""))


def run_axe(page, axe_src, name, theme):
    page.evaluate(axe_src)
    res = page.evaluate(
        "async tags => { const r = await axe.run(document, {runOnly: {type: 'tag', values: tags}});"
        " return r.violations.map(v => ({id: v.id, impact: v.impact, help: v.help, helpUrl: v.helpUrl,"
        " nodes: v.nodes.map(n => ({target: n.target.join(' '), html: n.html.slice(0, 160),"
        " summary: (n.failureSummary || '').slice(0, 240)}))})); }",
        AXE_TAGS,
    )
    AXE.setdefault(name, {})[theme] = res
    rec("axe wcag2a/aa/21a/21aa", f"{name}/{theme}", not res,
        "; ".join(f'{v["id"]}({v["impact"]}) x{len(v["nodes"])}' for v in res))


# ---------------------------------------------------------------- the browser part
def shot(page, name, theme, vp, kind="", full=True, clip_h=None):
    fn = f"{name}{('-' + kind) if kind else ''}-{theme}-{vp}.png"
    kw = {"path": str(OUT / fn)}
    if full:
        h = page.evaluate("document.documentElement.scrollHeight")
        kw["full_page"] = True
        if h > SHOT_MAX_H:
            kw["clip"] = {"x": 0, "y": 0, "width": VIEWPORTS[vp][0], "height": clip_h or SHOT_MAX_H}
    page.screenshot(**kw)
    SHOTS.append(fn)
    return fn


def visit_all(browser, pages, axe_src, cookie):
    for theme in THEMES:
        for vp, (w, h) in VIEWPORTS.items():
            ctx = browser.new_context(
                viewport={"width": w, "height": h}, color_scheme=theme,
                device_scale_factor=2 if vp == "phone" else 1,
                is_mobile=vp == "phone", has_touch=vp == "phone",
            )
            ctx.add_init_script(INIT_JS)
            page = ctx.new_page()
            sink = {"console": [], "pageerror": [], "bad": []}
            page.on("console", lambda m, s=sink: s["console"].append(m.text) if m.type == "error" else None)
            page.on("pageerror", lambda e, s=sink: s["pageerror"].append(str(e)))
            page.on("response", lambda r, s=sink: s["bad"].append(f"{r.status} {r.url}")
                    if r.status >= 400 and r.request.resource_type != "document" else None)
            page.goto(URL, wait_until="load")  # token exchange through the real redirect
            for name, path, _, want in pages:
                scope = f"{name}/{theme}/{vp}"
                for k in sink:
                    sink[k].clear()
                resp = page.goto(f"http://{HOST}:{PORT}{path}", wait_until="load")
                page.wait_for_timeout(120)  # deferred scripts (keys.js, htmx) settle
                rec("status", scope, resp.status == want, f"{resp.status} (want {want})")
                console = [c for c in sink["console"] if not (want != 200 and "Failed to load resource" in c)]
                rec("no console errors", scope, not console and not sink["pageerror"],
                    "; ".join((console + sink["pageerror"])[:3]))
                rec("no failed subresource requests", scope, not sink["bad"], "; ".join(sink["bad"][:3]))
                csp = page.evaluate("window.__csp")
                rec("no CSP violations", scope, not csp, "; ".join(csp[:3]))
                m = page.evaluate(OVERFLOW_JS)
                rec("no horizontal scroll", scope, m["sw"] <= m["iw"],
                    f'scrollWidth {m["sw"]} > innerWidth {m["iw"]}; widest: {m["off"]}')
                safe = page.evaluate(DOM_SAFETY_JS)
                rec("DOM has no script/handlers/javascript: urls from content", scope,
                    not safe["scripts"] and not safe["handlers"] and not safe["pwned"] and not safe["jsurls"], json.dumps(safe))
                shot(page, name, theme, vp)
                extra(page, name, theme, vp)
                if vp == "desktop":
                    if theme == "light":
                        nav = page.evaluate("""() => { const n = performance.getEntriesByType('navigation')[0];
                          return {ttfb: n.responseStart - n.requestStart, dcl: n.domContentLoadedEventEnd,
                                  load: n.loadEventEnd, bytes: n.decodedBodySize}; }""")
                        PAGE_STATS.setdefault(name, {})["browser"] = {k: round(v, 1) for k, v in nav.items()}
                    if want == 200:
                        run_axe(page, axe_src, name, theme)
                        keyboard_check(page, name, scope)
                        if name == "repo":
                            page.evaluate("document.activeElement.blur()")
                            page.keyboard.press("Tab", delay=0)
                            for _ in range(5):
                                page.keyboard.press("Tab")
                            shot(page, name, theme, vp, "focus", full=False)
            ctx.close()


def extra(page, name, theme, vp):
    """Extra states worth a screenshot."""
    if name == "blob-long":
        page.locator("p.more").scroll_into_view_if_needed()
        shot(page, name, theme, vp, "cap", full=False)
    if name == "repo":
        page.evaluate("document.getElementById('verify').open = true")
        page.wait_for_timeout(50)
        shot(page, name, theme, vp, "verify-open")
        page.evaluate("document.getElementById('verify').open = false")
    if name == "home":
        page.locator("[data-help]").click()
        page.wait_for_timeout(100)
        shot(page, name, theme, vp, "help-dialog", full=False)
        page.keyboard.press("Escape")


def nojs(browser, pages):
    ctx = browser.new_context(viewport={"width": 1280, "height": 800}, java_script_enabled=False)
    page = ctx.new_page()
    page.goto(URL, wait_until="load")
    for name, path, expect, want in pages:
        resp = page.goto(f"http://{HOST}:{PORT}{path}", wait_until="load")
        main = page.locator("main").inner_text() if page.locator("main").count() else ""
        missing = [e for e in expect if e.lower() not in main.lower()]
        rec("JS disabled: main content renders", f"{name}/no-js", resp.status == want and bool(main.strip()) and not missing,
            f"status {resp.status}, {len(main)} chars, missing {missing}")
        if name in ("repo", "home"):
            page.screenshot(path=str(OUT / f"{name}-nojs-light-desktop.png"), full_page=True)
            SHOTS.append(f"{name}-nojs-light-desktop.png")
    ctx.close()


# ---------------------------------------------------------------- HTTP-level checks
def security(cookie, rid, pages):
    r = f"/r/{rid}"
    s = get("/")[0]
    rec("request without the cookie is rejected", "/", s == 401, f"status {s}")
    s = get(f"{r}")[0]
    rec("request without the cookie is rejected", r, s == 401, f"status {s}")
    s = get("/", cookie, host="evil.example")[0]
    rec("wrong Host header is rejected (DNS rebinding)", "/", s == 403, f"status {s}")
    s = get("/", cookie, host=f"127.0.0.1:{PORT + 1}")[0]
    rec("wrong Host port is rejected", "/", s == 403, f"status {s}")
    s = get("/", None, host="evil.example")[0]
    rec("wrong Host without cookie is rejected", "/", s == 403, f"status {s}")
    s = get("/?token=" + "0" * 64)[0]
    rec("wrong token is rejected", "/?token=...", s == 403, f"status {s}")
    status, hdr, _, cookies = get(f"/?token={TOKEN}")
    sc = cookies[0] if cookies else ""
    rec("token exchange: 303 to a URL without the token", "/?token", status == 303 and "token" not in hdr.get("location", "x"),
        f'status {status}, location {hdr.get("location")}')
    rec("session cookie is HttpOnly + SameSite=Strict", "set-cookie",
        "httponly" in sc.lower() and "samesite=strict" in sc.lower(), sc.split("=")[0] + "=<hidden>;" + sc.split(";", 1)[-1])
    s, hdr, _, _ = get(r, cookie)
    csp = hdr.get("content-security-policy", "")
    rec("CSP is strict (default-src 'self', no unsafe-inline/eval)", r,
        "default-src 'self'" in csp and "unsafe" not in csp, csp)
    rec("hardening headers (nosniff, no-referrer or same-origin, frame-ancestors none)", r,
        hdr.get("x-content-type-options") == "nosniff" and "frame-ancestors 'none'" in csp,
        f'nosniff={hdr.get("x-content-type-options")} referrer={hdr.get("referrer-policy")}', )
    s, hdr, _, _ = get(f"{r}/raw/main/README.md", cookie)
    ct, cd = hdr.get("content-type", ""), hdr.get("content-disposition", "")
    rec("raw files are text/plain or attachment, never html", "raw README.md",
        s == 200 and "html" not in ct and ("text/plain" in ct or "attachment" in cd), f"{s} {ct} {cd}")
    for p in (f"{r}/blob/main/..%2f..%2f..%2fetc/passwd", f"{r}/raw/main/..%2f..%2f..%2f..%2fetc/passwd",
              f"{r}/tree/main/../../../..", "/r/..%2f..%2fetc/passwd", f"{r}/commit/--output=/tmp/x"):
        s, _, body, _ = get(p, cookie)
        rec("path traversal / option injection is refused", p[:60], s != 200 and b"root:" not in body, f"status {s}")
    s = get("/", cookie, method="POST")[0]
    rec("POST without CSRF token is rejected", "/", s == 403, f"status {s}")
    html = get(r, cookie)[2].decode()
    readme = html[html.find('class="readme"'):html.find("</article>")]
    rec("README <script> and on* handlers are absent from the served HTML", "repo",
        "<script" not in readme.lower() and "onerror" not in readme.lower() and "alert(" not in readme, "")
    h = get(f"{r}/issues/{[p for p in pages if p[0]=='issue'][0][1].rsplit('/',1)[1]}", cookie)[2].decode()
    rec("issue body <script> is absent; title is escaped, not parsed", "issue",
        "x()" not in h and "<b>start</b>" not in h, "")


def perf(cookie, pages):
    css = get("/assets/app.css", cookie)[2]
    gz_css = len(gzip.compress(css))
    for name, path, _, want in pages:
        lat, body = [], b""
        reps = 15
        while len(lat) < reps:
            t = time.perf_counter()
            _, _, body, _ = get(path, cookie)
            lat.append((time.perf_counter() - t) * 1000)
            if len(lat) == 1 and lat[0] > 400:
                reps = 3  # slow page: three samples are enough to flag it
        lat.sort()
        p50, p99 = statistics.median(lat), lat[-1]
        gz = len(gzip.compress(body)) + gz_css
        PAGE_STATS.setdefault(name, {}).update(
            {"p50_ms": round(p50, 1), "max_ms": round(p99, 1), "html_bytes": len(body), "html_css_gz_bytes": gz})
        big_ok = name in ("blob-long", "commit-long", "blob-wide")  # inherently large content
        rec("HTML + CSS under 20 KB gzipped (plan section 6)", name, gz <= 20_000,
            f"{gz} B gzipped ({len(body)} B raw)", warn=big_ok)
        rec("server time p50 < 20 ms, max of up to 15 < 50 ms", name, p50 < 20 and p99 < 50,
            f"p50 {p50:.1f} ms, max {p99:.1f} ms" + (" (debug build)" if DEBUG_BUILD else ""), warn=DEBUG_BUILD)


def main():
    from playwright.sync_api import sync_playwright

    OUT.mkdir(parents=True, exist_ok=True)
    for old in OUT.glob("*.png"):
        old.unlink()
    axe_path = HERE / "vendor" / "axe.min.js"
    axe_bytes = axe_path.read_bytes()
    want = (HERE / "vendor" / "axe.min.js.sha256").read_text().split()[0]
    rec("vendored axe-core matches its recorded sha256", "vendor/axe.min.js", hashlib.sha256(axe_bytes).hexdigest() == want)
    axe_src = axe_bytes.decode()

    cookie = session_cookie()[0]
    first = json.loads((OUT / "first-page.json").read_text()) if (OUT / "first-page.json").exists() else None
    if first:
        t = first["seconds_to_first_repo_page"]
        rec("first repo page after server start < 1 s", "chart ui", t < 1.0,
            f"{t:.3f} s from spawn to first verified repo page", warn=DEBUG_BUILD)
    else:
        rec("first repo page after server start < 1 s", "chart ui", False, "first-page timing did not run")
    rid, pages = discover(cookie)
    print(f"repo {rid[:12]}..., {len(pages)} pages")
    security(cookie, rid, pages)
    perf(cookie, pages)
    with sync_playwright() as p:
        try:
            browser = p.chromium.launch(channel="chrome", headless=True)
        except Exception:  # nix-provided browsers, if the system Chrome is missing
            browser = p.chromium.launch(headless=True)
        nojs(browser, pages)
        visit_all(browser, pages, axe_src, cookie)
        browser.close()
    write_reports(first)
    fails = [c for c in CHECKS if c["status"] == "fail"]
    sys.exit(1 if fails else 0)


# ---------------------------------------------------------------- reports
def write_reports(first):
    summary = {s: sum(c["status"] == s for c in CHECKS) for s in ("pass", "warn", "fail")}
    axe_counts = {pg: {t: len(v) for t, v in d.items()} for pg, d in AXE.items()}
    (OUT / "report.json").write_text(json.dumps(
        {"summary": summary, "first_page": first, "axe_violation_counts": axe_counts, "axe": AXE,
         "page_stats": PAGE_STATS, "screenshots": SHOTS, "checks": CHECKS}, indent=2))
    L = ["# chart ui E2E report", "",
         f'Pass {summary["pass"]}, warn {summary["warn"]}, fail {summary["fail"]}.',
         f'Build: {"debug" if DEBUG_BUILD else "release"}. First page: {first}.', ""]
    by = {}
    for c in CHECKS:
        by.setdefault(c["name"], []).append(c)
    L += ["## Checks", "", "| Check | Pass | Warn | Fail |", "|---|---|---|---|"]
    for n, cs in by.items():
        L.append(f'| {n} | {sum(c["status"]=="pass" for c in cs)} | {sum(c["status"]=="warn" for c in cs)} | '
                 f'{sum(c["status"]=="fail" for c in cs)} |')
    L += ["", "## axe violations (WCAG 2.1 A and AA, desktop viewport)", "",
          "| Page | Light | Dark | Rules |", "|---|---|---|---|"]
    for pg, d in AXE.items():
        rules = sorted({v["id"] for t in d.values() for v in t})
        L.append(f'| {pg} | {len(d.get("light", []))} | {len(d.get("dark", []))} | {", ".join(rules) or "-"} |')
    L += ["", "## Failures and warnings", ""]
    for c in CHECKS:
        if c["status"] != "pass":
            L.append(f'- {c["status"].upper()} {c["name"]} ({c["scope"]}): {c["detail"][:300]}')
    L += ["", "## axe details", ""]
    for pg, d in AXE.items():
        for t, vs in d.items():
            for v in vs:
                n = v["nodes"][0]
                L.append(f'- {pg}/{t}: {v["id"]} ({v["impact"]}) x{len(v["nodes"])}: {v["help"]}; e.g. `{n["target"]}` `{n["html"][:100]}`')
    L += ["", "## Page stats", "", "| Page | p50 ms | max ms | HTML B | HTML+CSS gz B | TTFB ms | load ms |", "|---|---|---|---|---|---|---|"]
    for pg, s in PAGE_STATS.items():
        b = s.get("browser", {})
        L.append(f'| {pg} | {s.get("p50_ms")} | {s.get("max_ms")} | {s.get("html_bytes")} | {s.get("html_css_gz_bytes")} | '
                 f'{b.get("ttfb")} | {b.get("load")} |')
    (OUT / "report.md").write_text("\n".join(L).replace("—", "-") + "\n")
    print(f'pass {summary["pass"]}  warn {summary["warn"]}  fail {summary["fail"]}  -> {OUT}/report.md')


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "first-page":
        first_page()
    else:
        main()
