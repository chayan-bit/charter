// Keyboard map of docs/p6/ui-design.md section 6, and the theme toggle.
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
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    if (k === "Escape") {
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

  function theme() {
    var next = { "": "light", light: "dark", dark: "" }[root.getAttribute("data-theme") || ""];
    if (next) root.setAttribute("data-theme", next); else root.removeAttribute("data-theme");
    d.cookie = "theme=" + (next || "system") + "; Path=/; SameSite=Strict; Max-Age=31536000";
  }

  d.addEventListener("keydown", key);
  d.addEventListener("click", function (e) {
    var b = e.target.closest && e.target.closest("[data-help], [data-theme-toggle]");
    if (!b) return;
    if (b.hasAttribute("data-help")) d.getElementById("help").showModal(); else theme();
  });
  d.addEventListener("htmx:afterSettle", function () { cur = -1; });
})();
