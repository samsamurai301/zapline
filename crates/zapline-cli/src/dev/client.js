// zapline dev: live reload, CSS hot swap, the build error overlay and the
// rebuild badge. Served by the `zapline dev` proxy at /__zapline/client.js
// and added to every HTML page it forwards; never part of your app.
(() => {
  "use strict";
  if (window.__zaplineDev) return;
  window.__zaplineDev = true;

  const SCROLL_KEY = "__zapline_scroll";
  const here = () => location.pathname + location.search;

  // ---- reload, keeping the scroll position --------------------------------------------

  try {
    const saved = sessionStorage.getItem(SCROLL_KEY);
    if (saved) {
      sessionStorage.removeItem(SCROLL_KEY);
      const [path, x, y] = JSON.parse(saved);
      if (path === here()) {
        const restore = () => window.scrollTo(x, y);
        if (document.readyState === "complete") restore();
        else window.addEventListener("load", restore, { once: true });
      }
    }
  } catch (_) {}

  function reload() {
    try {
      sessionStorage.setItem(SCROLL_KEY, JSON.stringify([here(), window.scrollX, window.scrollY]));
    } catch (_) {}
    location.reload();
  }

  // ---- CSS hot swap ---------------------------------------------------------------------

  // `app.3f2a1b9c.css` (a zapline asset URL) is `app.css` on disk.
  const fileName = (path) => {
    const last = path.split("/").filter(Boolean).pop() || "";
    return last.replace(/\.[0-9a-f]{8}(\.[^.]+)$/i, "$1");
  };

  function swapCss(paths) {
    const links = [...document.querySelectorAll('link[rel~="stylesheet"][href]')].filter((l) => {
      try {
        return new URL(l.href).origin === location.origin;
      } catch (_) {
        return false;
      }
    });
    const names = new Set(paths.map(fileName));
    let targets = links.filter((l) => names.has(fileName(new URL(l.href).pathname)));
    // An @import-ed or otherwise unlinked file: refresh every stylesheet.
    if (targets.length === 0) targets = links;
    for (const old of targets) {
      const url = new URL(old.href);
      url.searchParams.set("zl", Date.now().toString(36));
      const next = old.cloneNode();
      next.href = url.href;
      // Swap once the new sheet has loaded, so the page never flashes unstyled.
      const done = () => old.remove();
      next.addEventListener("load", done, { once: true });
      next.addEventListener("error", done, { once: true });
      old.after(next);
    }
    return targets.length;
  }

  // ---- overlay and badge ------------------------------------------------------------------

  const CSS = `
    :host { all: initial; }
    .backdrop { position: fixed; inset: 0; z-index: 2147483647; background: rgba(10, 12, 16, .66);
      display: flex; align-items: flex-start; justify-content: center; overflow: auto;
      font: 14px/1.5 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }
    .panel { margin: 6vh 16px; width: min(960px, 100%); background: #16181d; color: #e6e6e6;
      border-top: 4px solid #ff5d5d; border-radius: 8px; box-shadow: 0 20px 60px rgba(0,0,0,.5); }
    header { display: flex; gap: 12px; align-items: baseline; padding: 16px 20px 8px; }
    h1 { margin: 0; font-size: 18px; color: #ff7b7b; font-weight: 600; }
    .note { color: #9aa0aa; font-size: 13px; padding: 0 20px 12px; }
    button { margin-left: auto; background: none; border: 1px solid #3a3f4a; color: #c8ccd4;
      border-radius: 6px; padding: 2px 10px; cursor: pointer; font: inherit; }
    button:hover { background: #262a33; }
    .error { border-top: 1px solid #2a2e37; padding: 12px 20px 16px; }
    .where { font: 12px ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; color: #8ab4ff; }
    .message { font-weight: 600; margin: 4px 0 8px; color: #fff; }
    pre { margin: 0; padding: 12px; background: #0e0f12; border-radius: 6px; overflow: auto;
      font: 12.5px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; color: #d7dae0; }
    footer { padding: 10px 20px 14px; color: #6f7682; font-size: 12px; border-top: 1px solid #2a2e37; }
    .badge { position: fixed; right: 14px; bottom: 14px; z-index: 2147483646; display: flex; gap: 8px;
      align-items: center; padding: 6px 12px; border-radius: 999px; background: #16181d; color: #e6e6e6;
      font: 12px/1.4 ui-sans-serif, system-ui, sans-serif; box-shadow: 0 4px 16px rgba(0,0,0,.35); }
    .spin { width: 10px; height: 10px; border-radius: 50%; border: 2px solid #4b8bff;
      border-right-color: transparent; animation: spin .8s linear infinite; }
    @keyframes spin { to { transform: rotate(360deg); } }
  `;

  let root = null;
  function shadow() {
    if (root && root.host.isConnected) return root;
    const host = document.createElement("zapline-dev");
    root = host.attachShadow({ mode: "open" });
    // A constructed stylesheet is not inline CSS, so `style-src 'self'` allows it.
    try {
      const sheet = new CSSStyleSheet();
      sheet.replaceSync(CSS);
      root.adoptedStyleSheets = [sheet];
    } catch (_) {
      const style = document.createElement("style");
      style.textContent = CSS;
      root.appendChild(style);
    }
    (document.body || document.documentElement).appendChild(host);
    return root;
  }

  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };

  let dismissed = null;
  function showProblem(problem) {
    hideProblem();
    const key = JSON.stringify(problem);
    if (dismissed === key) return;
    const backdrop = el("div", "backdrop");
    backdrop.dataset.part = "overlay";
    const panel = el("div", "panel");
    const header = el("header");
    header.appendChild(el("h1", null, problem.title));
    const close = el("button", null, "Close (Esc)");
    const dismiss = () => {
      dismissed = key;
      hideProblem();
    };
    close.addEventListener("click", dismiss);
    header.appendChild(close);
    panel.appendChild(header);
    if (problem.note) panel.appendChild(el("div", "note", problem.note));
    for (const e of problem.errors || []) {
      const box = el("div", "error");
      if (e.file) {
        box.appendChild(el("div", "where", e.line ? `${e.file}:${e.line}:${e.column}` : e.file));
      }
      box.appendChild(el("div", "message", e.message));
      if (e.rendered) box.appendChild(el("pre", null, e.rendered));
      panel.appendChild(box);
    }
    panel.appendChild(el("footer", null, "Fix the code and save: the page updates by itself."));
    backdrop.appendChild(panel);
    backdrop.addEventListener("click", (ev) => {
      if (ev.target === backdrop) dismiss();
    });
    shadow().appendChild(backdrop);
    document.addEventListener("keydown", onKey);
    function onKey(ev) {
      if (ev.key === "Escape") {
        document.removeEventListener("keydown", onKey);
        dismiss();
      }
    }
  }

  function hideProblem() {
    if (root) root.querySelectorAll('[data-part="overlay"]').forEach((n) => n.remove());
  }

  function showBadge(text) {
    hideBadge();
    const badge = el("div", "badge");
    badge.dataset.part = "badge";
    badge.appendChild(el("span", "spin"));
    badge.appendChild(el("span", null, text));
    shadow().appendChild(badge);
  }

  function hideBadge() {
    if (root) root.querySelectorAll('[data-part="badge"]').forEach((n) => n.remove());
  }

  // ---- events -------------------------------------------------------------------------------

  const whenReady = (f) =>
    document.body ? f() : document.addEventListener("DOMContentLoaded", f, { once: true });

  let epoch = null;
  const events = new EventSource("/__zapline/events");
  const on = (name, f) =>
    events.addEventListener(name, (ev) => {
      let data = {};
      try {
        data = JSON.parse(ev.data || "{}");
      } catch (_) {}
      whenReady(() => f(data));
    });

  // A new `zapline dev` session (it was restarted): this page may be stale.
  on("hello", (d) => {
    if (epoch !== null && epoch !== d.epoch) reload();
    epoch = d.epoch;
  });
  on("building", () => showBadge("Rebuilding…"));
  on("ready", () => hideBadge());
  on("reload", () => reload());
  on("css", (d) => {
    const n = swapCss(d.paths || []);
    if (n === 0) reload();
  });
  on("problem", (d) => {
    hideBadge();
    showProblem(d);
  });
  on("clear", () => {
    dismissed = null;
    hideProblem();
  });
})();
