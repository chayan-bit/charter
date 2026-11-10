// Live updates (docs/p6/ui.md 4.5): one EventSource per page, on the repo's
// /r/<rid>/events. A push, op or sync event refetches the regions marked
// data-live="<event>" (from data-src, else this page) and swaps each one by id; a region
// holding focus is left as it is. With JS off none of this runs and pages are unchanged.
(function () {
  "use strict";
  var d = document, es = null, src = null, KINDS = ["push", "op", "sync"];

  function regions(kinds) {
    return Array.prototype.filter.call(d.querySelectorAll("[data-live][id]"), function (el) {
      var mine = el.getAttribute("data-live").split(" ");
      return kinds.some(function (k) { return mine.indexOf(k) >= 0; });
    });
  }

  function swap(old, doc) {
    var fresh = doc.getElementById(old.id);
    if (!fresh || !old.isConnected || old.contains(d.activeElement)) return;
    fresh = d.importNode(fresh, true);
    if (old.tagName === "DETAILS") fresh.open = old.open;
    old.replaceWith(fresh);
    if (window.htmx) window.htmx.process(fresh);
  }

  function refresh(kinds) {
    var byUrl = {};
    regions(kinds).forEach(function (el) {
      var u = el.getAttribute("data-src") || location.pathname + location.search;
      (byUrl[u] = byUrl[u] || []).push(el);
    });
    Object.keys(byUrl).forEach(function (u) {
      fetch(u, { credentials: "same-origin" }).then(function (r) {
        return r.ok ? r.text() : null;
      }).then(function (text) {
        if (text === null) return;
        var doc = new DOMParser().parseFromString(text, "text/html");
        byUrl[u].forEach(function (el) { swap(el, doc); });
        d.dispatchEvent(new CustomEvent("live:swap", { detail: kinds }));
      }).catch(function () {});
    });
  }

  function connect() {
    var m = d.querySelector("main[data-events]");
    var want = m ? m.getAttribute("data-events") : null;
    if (want === src && es) return;
    if (es) es.close();
    es = null;
    src = want;
    if (!want || !window.EventSource) return;
    es = new EventSource(want);
    KINDS.forEach(function (k) { es.addEventListener(k, function () { refresh([k]); }); });
    es.addEventListener("resync", function () { refresh(KINDS); });
  }

  connect();
  d.addEventListener("htmx:afterSettle", connect);
  window.addEventListener("pagehide", function () { if (es) es.close(); es = null; });
  window.addEventListener("pageshow", connect);
})();
