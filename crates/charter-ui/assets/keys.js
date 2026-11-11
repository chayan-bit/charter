// Keyboard map of docs/p6/ui-design.md section 6, the theme toggle (saved through
// POST /settings/theme), copy buttons and popover closing.
// Every action is also a plain link, so pages read the same with JavaScript off.
(function () {
  "use strict";
  var d = document, root = d.documentElement, pending = null, cur = -1;
  root.classList.add("has-js");

  function rows() {
    var l = d.querySelector("[data-list]");
    return l ? Array.prototype.slice.call(l.querySelectorAll("[data-row]")) : [];
  }
  function mark(i) {
    var r = rows();
    if (!r.length) return;
    cur = Math.max(0, Math.min(r.length - 1, i));
    r.forEach(function (x, j) { x.classList.toggle("cur", j === cur); });
    r[cur].scrollIntoView({ block: "nearest" });
  }
  function go(el) { if (el && el.href) el.click(); }
  function row() { return rows()[cur]; }
  function copy() {
    var el = row() || d.querySelector("[data-copy]");
    var t = el && el.getAttribute("data-copy");
    if (t && navigator.clipboard) navigator.clipboard.writeText(t);
  }
  function toggle(sel) {
    var el = sel instanceof Element ? sel : d.querySelector(sel);
    if (el) el.open = !el.open;
  }
  function file() {
    var fs = d.querySelectorAll("details.file"), y = window.innerHeight / 3;
    for (var i = 0; i < fs.length; i++) {
      if (fs[i].getBoundingClientRect().bottom > y) return fs[i];
    }
    return null;
  }
  var gTo = { h: "[data-go=h]", c: "[data-go=c]", l: "[data-go=l]", i: "[data-go=i]",
    p: "[data-go=p]", r: "[data-go=r]" };

  function key(e) {
    var t = e.target, k = e.key;
    var send = d.querySelector("form[data-confirm]");
    // The confirm dialog: Enter on the focused Cancel cancels; only Cmd/Ctrl+Enter spends.
    // requestSubmit ignores a disabled button (an exhausted RPC quota), so check it here.
    if (send && k === "Enter" && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      if (!send.querySelector("button[type=submit]:disabled")) send.requestSubmit();
      return;
    }
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    if (k === "Escape") {
      var cancel = send && send.querySelector("a.btn");
      if (cancel) { cancel.click(); return; }
      var pops = d.querySelectorAll("details.pop[open]");
      if (pops.length) { closePops(null); return; }
      var dlg = d.getElementById("help");
      if (dlg && dlg.open) dlg.close();
      if (t.blur) t.blur();
      return;
    }
    if (t.closest && t.closest("input, textarea, select, [contenteditable]")) return;
    if (pending === "g") {
      pending = null;
      if (k === "v") { var v = d.getElementById("verify"); if (v) { v.open = true; v.scrollIntoView(); } }
      else if (gTo[k]) go(d.querySelector(gTo[k]));
      return;
    }
    switch (k) {
      case "g": pending = "g"; setTimeout(function () { pending = null; }, 1200); return;
      case "j": mark(cur + 1); break;
      case "k": mark(cur - 1); break;
      case "Enter": case "o": if (!row()) return; go(row().querySelector("a")); break;
      case "u": go(d.querySelector("a[rel=up]")); break;
      case "/": var f = d.querySelector("input[type=search]"); if (!f) return; f.focus(); break;
      case "y": copy(); break;
      case "v": toggle("#verify"); break;
      case "x": toggle(file()); break;
      case "?": var h = d.getElementById("help"); if (h) h.showModal(); break;
      default: return;
    }
    e.preventDefault();
  }

  // A link to a closed <details> (the "Verification" tab) opens it; a long tab bar shows the current tab.
  function arrive() {
    var t = location.hash.length > 1 && d.getElementById(decodeURIComponent(location.hash.slice(1)));
    if (t && t.tagName === "DETAILS") t.open = true;
    var nav = d.querySelector(".tabs"), a = nav && nav.querySelector("[aria-current]");
    if (a) nav.scrollLeft = a.offsetLeft - (nav.clientWidth - a.offsetWidth) / 2;
  }
  function theme(b) {
    var next = { "": "light", light: "dark", dark: "" }[root.getAttribute("data-theme") || ""];
    var name = next || "system";
    if (next) root.setAttribute("data-theme", next); else root.removeAttribute("data-theme");
    var use = b.querySelector("use");
    if (use) use.setAttribute("href", use.getAttribute("href").replace(/#i-.*$/,
      "#i-" + { system: "circle-half", light: "sun", dark: "moon" }[name]));
    b.setAttribute("aria-label", "Theme: " + name + ". Change");
    var radio = d.querySelector("input[name=theme][value=" + name + "]");
    if (radio) radio.checked = true;
    var csrf = d.querySelector("meta[name=csrf-token]");
    fetch("/settings/theme", {
      method: "POST", credentials: "same-origin",
      headers: { "x-csrf-token": csrf ? csrf.content : "", "content-type": "application/x-www-form-urlencoded" },
      body: "theme=" + name
    }).catch(function () {});
  }
  function copyBtn(b) {
    var t = b.getAttribute("data-copy"), use = b.querySelector("use");
    if (!t || !navigator.clipboard) return;
    navigator.clipboard.writeText(t).then(function () {
      var was = use && use.getAttribute("href");
      if (use) use.setAttribute("href", was.replace(/#i-.*$/, "#i-check"));
      b.setAttribute("data-copied", "");
      setTimeout(function () { if (use) use.setAttribute("href", was); b.removeAttribute("data-copied"); }, 1200);
    });
  }
  // One popover at a time; a click outside closes it.
  function closePops(keep) {
    Array.prototype.forEach.call(d.querySelectorAll("details.pop[open]"), function (p) {
      if (p !== keep && !p.contains(keep)) p.open = false;
    });
  }

  d.addEventListener("keydown", key);
  d.addEventListener("click", function (e) {
    var t = e.target, ln = t.closest && t.closest(".blob td.ln");
    if (ln) { location.hash = ln.parentNode.id; return; }
    if (t.closest && t.closest("[data-dismiss]")) { t.closest(".toast").classList.add("gone"); return; }
    closePops(t.closest ? t.closest("details.pop") : null);
    var b = t.closest && t.closest("[data-help], [data-theme-toggle], button[data-copy]");
    if (t.matches && t.matches("input[readonly]")) t.select();
    if (!b) return;
    if (b.hasAttribute("data-copy")) copyBtn(b);
    else if (b.hasAttribute("data-help")) d.getElementById("help").showModal();
    else theme(b);
  });
  // A confirmed toast hides after 6 s unless hovered or focused; a failed one stays.
  function toast() {
    var t = d.querySelector(".toast.t-confirmed:not(.gone)");
    if (!t || t.dataset.timer) return;
    t.dataset.timer = "1";
    (function later() {
      setTimeout(function () {
        if (t.matches(":hover, :focus-within")) later(); else t.classList.add("gone");
      }, 6000);
    })();
  }
  d.addEventListener("htmx:afterSettle", function () { cur = -1; arrive(); toast(); });
  window.addEventListener("hashchange", arrive);
  arrive();
  toast();
})();
