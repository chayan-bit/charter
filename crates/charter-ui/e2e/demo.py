"""Records the demo video of `chart ui` as webm clips: python3 demo.py (see demo.sh).

Env: DEMO_URL  the first line `chart ui` prints (carries the session token; never printed here)
     DEMO_OUT  directory for the webm clips
     DEMO_REPO, DEMO_EXPIRED  repo ids of a verified and an expired repo in the cache
"""
import os, re, time
from playwright.sync_api import sync_playwright

url, out = os.environ["DEMO_URL"], os.environ["DEMO_OUT"]
repo, expired = os.environ["DEMO_REPO"], os.environ["DEMO_EXPIRED"]
base = re.match(r"(http://[^/]+)", url).group(1)
R = f"{base}/r/{repo}"


def pause(s=1.6):
    time.sleep(s)


def scroll(pg, y):
    pg.evaluate(f"window.scrollTo({{top: {y}, behavior: 'smooth'}})")
    pause(1.0)


def find_patch(br):
    """The first patch whose files page has a collapsed (lazy) file; None if there is none."""
    pg = br.new_context(viewport={"width": 1280, "height": 720}).new_page()
    pg.goto(url, wait_until="load")
    pause(20)  # first sync round
    pg.goto(f"{R}/patches", wait_until="load")
    ids = [m for h in pg.eval_on_selector_all('a[href*="/patches/"]', "e => e.map(a => a.href)")
           if (m := re.search(r"/patches/([0-9a-f]{64})$", h))]
    for pid in dict.fromkeys(m.group(1) for m in ids):
        pg.goto(f"{R}/patches/{pid}/files", wait_until="load")
        if pg.query_selector("details.file:not([open]) .lazy"):
            pg.context.close()
            return pid
    pg.context.close()
    return None


def desktop(br, pid):
    ctx = br.new_context(viewport={"width": 1280, "height": 720}, color_scheme="light",
                         record_video_dir=out, record_video_size={"width": 1280, "height": 720})
    pg = ctx.new_page()
    pg.goto(url, wait_until="load"); pause(2.5)  # the session URL lands on home; headless shows no URL bar
    pg.goto(R, wait_until="load"); pause(2.5); scroll(pg, 400); pause(1.2)
    pg.goto(f"{R}/log/main", wait_until="load"); pause(2.0)
    pg.goto(pg.eval_on_selector('a[href*="/commit/"]', "a => a.href"), wait_until="load")
    pause(1.5); scroll(pg, 500); pause(1.5)
    pg.goto(f"{R}/releases", wait_until="load"); pause(2.5)
    pg.goto(f"{R}/patches", wait_until="load"); pause(2.0)
    if pid:
        pg.goto(f"{R}/patches/{pid}", wait_until="load"); pause(2.0)
        pg.goto(f"{R}/patches/{pid}/files", wait_until="load"); pause(1.5)
        pg.click("details.file:not([open]) > summary"); pause(2.5); scroll(pg, 400); pause(1.2)
    pg.goto(f"{base}/r/{expired}", wait_until="load"); pause(2.5)
    pg.click("details.rawerr > summary"); pause(2.5)
    pg.click("[data-theme-toggle]"); pause(2.5)  # system -> light -> dark, whichever it takes
    for _ in range(2):
        if pg.evaluate("document.documentElement.dataset.theme") == "dark":
            break
        pg.click("[data-theme-toggle]"); pause(1.0)
    pg.goto(R, wait_until="load"); pause(2.5)
    ctx.close()


def phone(br):
    ctx = br.new_context(viewport={"width": 390, "height": 844}, color_scheme="dark", device_scale_factor=1,
                         is_mobile=True, has_touch=True, record_video_dir=out,
                         record_video_size={"width": 390, "height": 844})
    pg = ctx.new_page()
    pg.goto(url, wait_until="load"); pause(2.0)
    pg.goto(R, wait_until="load"); pause(2.0); scroll(pg, 400); pause(1.0)
    pg.goto(f"{R}/releases", wait_until="load"); pause(2.5)
    pg.goto(f"{base}/r/{expired}", wait_until="load"); pause(2.5)
    ctx.close()


with sync_playwright() as p:
    try:
        br = p.chromium.launch(channel="chrome", headless=True)
    except Exception:
        br = p.chromium.launch(headless=True)
    pid = find_patch(br)
    desktop(br, pid)
    phone(br)
    br.close()
