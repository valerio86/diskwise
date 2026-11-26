// diskwise desktop UI. Plain JS, no build step: talks to the Rust side through
// Tauri's global API (or the preview mock in dev/).
"use strict";

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const listen = (event, cb) => tauri.event.listen(event, (e) => cb(e.payload));

const SAFETY = {
  safe: { icon: "✓", group: "Safe to clean", total: "safe to clean now" },
  caution: { icon: "!", group: "Safe after a step", total: "after a quick step" },
  review: { icon: "?", group: "Your call", total: "worth reviewing" },
};

const state = {
  summary: null,
  view: null,
  selected: null, // path of the selected tile
  highlight: null, // index of the suggestion shown on the map
  open: new Set(), // suggestions expanded in the side panel
  mode: "map",
};

// ── Helpers ────────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);

/** Build an element: h("div", {class: "x", onclick}, child, "text", …). */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else if (k === "style") {
      for (const [prop, val] of Object.entries(v)) {
        if (val === null || val === undefined) continue;
        if (prop.startsWith("--")) el.style.setProperty(prop, val);
        else el.style[prop] = val;
      }
    }
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

/** Replace an element's children, skipping null/false (DOM's own replaceChildren
 *  would print them as text). */
function fill(el, ...children) {
  el.replaceChildren(...children.flat().filter((c) => c !== null && c !== undefined && c !== false));
}

/** Decimal units, like Finder (and the CLI). */
function bytes(n) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${units[i]}`;
}

function ago(unix) {
  if (!unix) return "unknown";
  const days = Math.floor((state.summary.scanned_at - unix) / 86400);
  if (days <= 0) return "today";
  if (days === 1) return "yesterday";
  if (days <= 60) return `${days} days ago`;
  if (days <= 730) return `${Math.floor(days / 30)} months ago`;
  return `${Math.floor(days / 365)} years ago`;
}

function badge(safety) {
  return h("span", { class: `badge b-${safety}`, "aria-hidden": "true" });
}

function finding(i) {
  return state.summary.findings.find((f) => f.index === i);
}

let toastTimer;
function toast(message) {
  const t = $("toast");
  t.textContent = message;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 6000);
}

// ── Scanning ───────────────────────────────────────────────────────────────

function showScanning(title) {
  $("scanning").hidden = false;
  $("layout").hidden = true;
  $("scan-title").textContent = title;
  $("scan-counts").textContent = "";
  $("scan-note").textContent = "";
}

listen("scan-progress", (p) => {
  $("scan-title").textContent = `Scanning ${p.root}`;
  $("scan-counts").textContent =
    `${p.files.toLocaleString()} files · ${p.dirs.toLocaleString()} folders · ${bytes(p.bytes)} · ${Math.round(p.seconds)}s`;
  if (p.seconds > 4) {
    $("scan-note").textContent =
      "A whole-disk scan takes under a minute. Folders macOS protects are skipped unless diskwise has Full Disk Access.";
  }
});

listen("scan-done", async (done) => {
  if (done.type === "failed") {
    $("scan-title").textContent = "The scan didn't finish";
    $("scan-counts").textContent = done.message;
    $("rescan").hidden = false;
    return;
  }
  state.summary = await invoke("summary");
  state.highlight = null;
  state.open.clear();
  $("scanning").hidden = true;
  $("layout").hidden = false;
  $("rescan").hidden = false;
  renderDisk();
  renderSuggestions();
  await navigate(state.summary.root);
});

$("rescan").addEventListener("click", () => {
  showScanning("Starting the scan…");
  invoke("start_scan", { rescan: true }).catch((e) => toast(String(e)));
});

// ── Header ─────────────────────────────────────────────────────────────────

function renderDisk() {
  const d = state.summary.disk;
  $("disk").hidden = !d;
  if (!d) return;
  const used = d.capacity - d.free;
  const pct = Math.round((used / d.capacity) * 100);
  $("disk-fill").style.width = `${pct}%`;
  $("disk-meter").setAttribute("aria-label", `Disk ${pct}% full`);
  $("disk-pct").textContent = `${pct}% full`;
  $("disk-free").textContent = `${bytes(d.free)} free of ${bytes(d.capacity)}`;
}

// ── Navigation ─────────────────────────────────────────────────────────────

async function navigate(path, select = null) {
  try {
    state.view = await invoke("view", { path });
  } catch (e) {
    // The folder may have just been moved to the Trash: go to its parent.
    const parent = path.replace(/\/[^/]+$/, "") || "/";
    if (parent !== path) return navigate(parent);
    toast(String(e));
    return;
  }
  state.selected = select;
  renderCrumbs();
  renderBody();
  renderSelected();
}

function up() {
  const crumbs = state.view?.crumbs ?? [];
  if (crumbs.length > 1) navigate(crumbs[crumbs.length - 2].path, state.view.path);
}

function renderCrumbs() {
  const nav = $("crumbs");
  fill(nav);
  state.view.crumbs.forEach((c, i) => {
    const last = i === state.view.crumbs.length - 1;
    if (i > 0) nav.append(h("span", { class: "sep", "aria-hidden": "true" }, "›"));
    const label = i === 0 && c.name === "/" ? "Disk" : c.name;
    nav.append(
      h("button", { type: "button", "aria-current": last ? "page" : null, onclick: () => !last && navigate(c.path) }, label),
    );
  });
}

function renderBody() {
  renderChip();
  $("map").hidden = state.mode !== "map";
  $("list").hidden = state.mode !== "list";
  if (state.mode === "map") renderMap();
  else renderList();
}

function setMode(mode) {
  state.mode = mode;
  $("as-map").setAttribute("aria-selected", mode === "map");
  $("as-list").setAttribute("aria-selected", mode === "list");
  renderBody();
}
$("as-map").addEventListener("click", () => setMode("map"));
$("as-list").addEventListener("click", () => setMode("list"));

/** Does this tile contain (or is it) one of the highlighted suggestion's items? */
function isHit(tile) {
  if (state.highlight === null || !tile.path) return false;
  if (tile.finding === state.highlight) return true;
  const f = finding(state.highlight);
  return !!f && f.items.some((i) => i.path === tile.path || i.path.startsWith(tile.path + "/"));
}

function select(tile) {
  if (!tile.path) return;
  state.selected = tile.path;
  for (const el of document.querySelectorAll("[data-path]")) {
    const on = el.dataset.path === tile.path;
    if (el.classList.contains("tile")) el.setAttribute("aria-pressed", on);
    else el.setAttribute("aria-selected", on);
  }
  setSideTab("selected");
  renderSelected();
}

function openTile(tile) {
  if (tile.openable) navigate(tile.path);
}

// ── Treemap ────────────────────────────────────────────────────────────────

/** Squarified treemap (Bruls, Huizing, van Wijk). `values` sorted descending. */
function squarify(values, x, y, w, h) {
  const total = values.reduce((a, b) => a + b, 0);
  if (total <= 0 || w <= 0 || h <= 0) return [];
  const scale = (w * h) / total;
  const areas = values.map((v) => v * scale);
  const rects = [];
  const worst = (row, side) => {
    const s = row.reduce((a, b) => a + b, 0);
    const max = Math.max(...row);
    const min = Math.min(...row);
    return Math.max((side * side * max) / (s * s), (s * s) / (side * side * min));
  };
  const place = (row) => {
    const s = row.reduce((a, b) => a + b, 0);
    if (w >= h) {
      const cw = s / h;
      let yy = y;
      for (const a of row) {
        rects.push({ x, y: yy, w: cw, h: a / cw });
        yy += a / cw;
      }
      x += cw;
      w -= cw;
    } else {
      const rh = s / w;
      let xx = x;
      for (const a of row) {
        rects.push({ x: xx, y, w: a / rh, h: rh });
        xx += a / rh;
      }
      y += rh;
      h -= rh;
    }
  };
  let row = [];
  for (const a of areas) {
    const side = Math.min(w, h);
    if (row.length === 0 || worst([...row, a], side) <= worst(row, side)) row.push(a);
    else {
      place(row);
      row = [a];
    }
  }
  if (row.length) place(row);
  return rects;
}

function tileNote(t) {
  if (t.safety && t.direct) return `${SAFETY[t.safety].icon} ${finding(t.finding)?.title ?? ""}`;
  if (t.kind === "rest") return `${(t.files + t.dirs).toLocaleString()} small items`;
  if (t.cleanable > 0) return `${bytes(t.cleanable)} cleanable inside`;
  return null;
}

function renderMap() {
  const map = $("map");
  fill(map);
  map.classList.toggle("highlighting", state.highlight !== null);
  const tiles = state.view.tiles.filter((t) => t.bytes > 0);
  const { width, height } = map.getBoundingClientRect();
  if (!tiles.length) {
    map.append(h("div", { class: "map-empty" }, "Nothing here takes up space on its own."));
    return;
  }
  const rects = squarify(tiles.map((t) => t.bytes), 0, 0, width, height);
  const gap = 2;
  tiles.forEach((t, i) => {
    const r = rects[i];
    const w = r.w - gap;
    const hgt = r.h - gap;
    if (w < 1 || hgt < 1) return;
    const name = t.kind === "rest" ? "Smaller items" : t.kind === "dir" ? `${t.name}/` : t.name;
    const note = tileNote(t);
    const showText = w >= 64 && hgt >= 34;
    const showNote = note && w >= 110 && hgt >= 56;
    // Big folders show their own contents in miniature, so cleanable space
    // deeper down is visible without opening them.
    const top = 8 + (showText ? 36 : 0) + (showNote ? 18 : 0) + 4;
    const nested = !t.direct && t.inner.length > 0 && w >= 120 && hgt - top >= 40 ? innerTiles(t, 9, top, w - 18, hgt - top - 9) : [];
    const el = h(
      "button",
      {
        type: "button",
        class: `tile${isHit(t) ? " hit" : ""}`,
        "data-kind": t.kind,
        "data-safety": t.safety,
        "data-direct": t.safety ? String(t.direct) : null,
        "data-path": t.path,
        "aria-pressed": t.path && t.path === state.selected ? "true" : "false",
        "aria-label": `${name}, ${bytes(t.bytes)}${note ? `, ${note}` : ""}`,
        style: { left: `${r.x + gap / 2}px`, top: `${r.y + gap / 2}px`, width: `${w}px`, height: `${hgt}px` },
        onclick: () => select(t),
        ondblclick: () => openTile(t),
        onkeydown: (e) => e.key === "Enter" && (e.preventDefault(), openTile(t)),
        onmousemove: (e) => showTooltip(e, t),
        onmouseleave: hideTooltip,
      },
      showText ? h("span", { class: "t-name" }, name) : null,
      showText ? h("span", { class: "t-size" }, bytes(t.bytes)) : null,
      showNote ? h("span", { class: "t-note" }, note) : null,
      nested,
      // Too small for miniatures: a strip whose length is the cleanable share.
      t.cleanable > 0 && !t.direct && !nested.length
        ? h("span", { class: "t-strip", style: { width: `${Math.min(100, (t.cleanable / t.bytes) * 100)}%` } })
        : null,
    );
    map.append(el);
  });
  const links = state.view.tiles.filter((t) => t.kind === "link").length;
  if (links) map.setAttribute("title", `${links} link(s) to folders elsewhere are listed in the List view.`);
}

function innerTiles(t, x, y, w, hgt) {
  const rects = squarify(t.inner.map((g) => g.bytes), x, y, w, hgt);
  return t.inner
    .map((g, i) => {
      const r = rects[i];
      if (!r || r.w < 2 || r.h < 2) return null;
      return h("span", {
        class: "inner",
        "aria-hidden": "true",
        "data-kind": g.kind,
        "data-safety": g.safety,
        "data-direct": g.safety ? String(g.direct) : null,
        style: {
          left: `${r.x + 0.5}px`,
          top: `${r.y + 0.5}px`,
          width: `${r.w - 1}px`,
          height: `${r.h - 1}px`,
          // Cleanable space deeper inside fills the miniature from the bottom.
          "--fill": !g.safety && g.cleanable ? `${Math.min(100, (g.cleanable / g.bytes) * 100)}%` : null,
        },
      });
    })
    .filter(Boolean);
}

new ResizeObserver(() => state.view && state.mode === "map" && renderMap()).observe($("map"));

function showTooltip(e, t) {
  const tip = $("tooltip");
  const pct = state.view.bytes ? (t.bytes / state.view.bytes) * 100 : 0;
  const rows = [h("div", { class: "tt-name" }, t.kind === "rest" ? "Smaller items" : t.name)];
  rows.push(h("div", { class: "tt-row" }, `${bytes(t.bytes)} · ${pct.toFixed(1)}% of this folder`));
  if (t.safety) {
    const f = finding(t.finding);
    rows.push(h("div", { class: "tt-row" }, badge(t.safety), `${t.direct ? "" : "Inside "}${f?.title ?? ""}`));
  } else if (t.cleanable > 0) {
    rows.push(h("div", { class: "tt-row" }, badge("safe"), `${bytes(t.cleanable)} cleanable inside`));
  }
  if (t.openable) rows.push(h("div", { class: "tt-row" }, "Double-click to open"));
  fill(tip, ...rows);
  tip.hidden = false;
  const x = Math.min(e.clientX + 14, window.innerWidth - tip.offsetWidth - 8);
  const y = Math.min(e.clientY + 16, window.innerHeight - tip.offsetHeight - 8);
  tip.style.left = `${x}px`;
  tip.style.top = `${y}px`;
}
function hideTooltip() {
  $("tooltip").hidden = true;
}

// ── List view ──────────────────────────────────────────────────────────────

function renderList() {
  const list = $("list");
  // Same order as the map, but "smaller items" always last.
  const ordered = [...state.view.tiles.filter((t) => t.kind !== "rest"), ...state.view.tiles.filter((t) => t.kind === "rest")];
  const rows = ordered.map((t) => {
    const f = t.finding !== null ? finding(t.finding) : null;
    const pct = state.view.bytes ? (t.bytes / state.view.bytes) * 100 : 0;
    const name =
      t.kind === "rest"
        ? `${(t.files + t.dirs).toLocaleString()} smaller items`
        : t.kind === "link"
          ? `${t.name} → ${t.target ?? "?"}`
          : t.kind === "dir"
            ? `${t.name}/`
            : t.name;
    return h(
      "tr",
      {
        "data-path": t.path,
        "aria-selected": t.path && t.path === state.selected ? "true" : "false",
        onclick: () => select(t),
        ondblclick: () => openTile(t),
      },
      h("td", { class: "name", style: t.kind === "link" ? { fontStyle: "italic", color: "var(--text-2)" } : null }, name),
      h("td", { class: "num" }, bytes(t.bytes)),
      h("td", { class: "num" }, `${pct.toFixed(1)}%`),
      h("td", {}, f ? [badge(f.safety), " ", t.direct ? f.title : `Inside ${f.title}`] : t.cleanable ? `${bytes(t.cleanable)} cleanable inside` : ""),
    );
  });
  fill(list, 
    h(
      "table",
      {},
      h("thead", {}, h("tr", {}, h("th", {}, "Name"), h("th", { class: "num" }, "Size"), h("th", { class: "num" }, "Share"), h("th", {}, "Suggestion"))),
      h("tbody", {}, rows),
    ),
  );
}

// ── Side panel ─────────────────────────────────────────────────────────────

function setSideTab(tab) {
  $("tab-suggestions").setAttribute("aria-selected", tab === "suggestions");
  $("tab-selected").setAttribute("aria-selected", tab === "selected");
  $("pane-suggestions").hidden = tab !== "suggestions";
  $("pane-selected").hidden = tab !== "selected";
}
$("tab-suggestions").addEventListener("click", () => setSideTab("suggestions"));
$("tab-selected").addEventListener("click", () => setSideTab("selected"));

function renderSuggestions() {
  const s = state.summary;
  const pane = $("pane-suggestions");
  const totals = h(
    "div",
    { class: "totals" },
    ["safe", "caution", "review"].map((k) =>
      h("div", { class: "total" }, h("span", { class: "amount" }, bytes(s[k])), badge(k), h("span", { class: "label" }, SAFETY[k].total)),
    ),
  );
  const groups = ["safe", "caution", "review"].map((k) => {
    const fs = s.findings.filter((f) => f.safety === k);
    if (!fs.length) return null;
    return [h("div", { class: "group" }, SAFETY[k].group), ...fs.map(findingRow)];
  });
  const note = s.unreadable
    ? h("p", { class: "f-meta" }, `${s.unreadable.toLocaleString()} folders couldn't be read. Give diskwise Full Disk Access to include them.`)
    : null;
  fill(pane, totals, ...groups.flat().filter(Boolean), note);
}

function findingRow(f) {
  const open = state.open.has(f.index);
  const size = f.size_known ? bytes(f.bytes) : "size ?";
  const row = h(
    "div",
    { class: "finding", "data-open": String(open) },
    h(
      "button",
      {
        type: "button",
        "aria-expanded": String(open),
        onclick: () => {
          open ? state.open.delete(f.index) : state.open.add(f.index);
          renderSuggestions();
        },
      },
      badge(f.safety),
      h("span", { class: "f-size" }, size),
      h("span", { class: "f-title" }, f.title),
    ),
  );
  if (!open) return row;
  const items = f.items.slice(0, 5).map((i) =>
    h("li", {}, h("span", { class: "i-size" }, bytes(i.bytes)), h("span", { class: "i-path", title: i.path }, `‎${i.path}${i.source ? `  (from ${i.source})` : ""}`)),
  );
  if (f.items.length > 5) items.push(h("li", {}, h("span", { class: "i-size" }, ""), `and ${f.items.length - 5} more`));
  row.append(
    h(
      "div",
      { class: "f-body" },
      h("p", { class: "f-why" }, f.why),
      h("p", { class: "f-how" }, f.how),
      f.prevent ? h("p", { class: "f-prevent" }, `To stop it coming back: ${f.prevent}`) : null,
      items.length ? h("ul", { class: "items" }, items) : null,
      h("p", { class: "f-meta" }, `${f.category_label} · ${f.action_label}`),
      h(
        "div",
        { class: "actions" },
        f.items.length ? h("button", { type: "button", class: "btn primary", onclick: () => showOnMap(f) }, "Show on map") : null,
        f.can_trash
          ? h("button", { type: "button", class: "btn", onclick: async () => showPlan(await invoke("plan_finding", { index: f.index })) }, f.items.length > 1 ? `Move ${f.items.length} items to Trash…` : "Move to Trash…")
          : null,
      ),
    ),
  );
  return row;
}

async function showOnMap(f) {
  state.highlight = f.index;
  setMode("map");
  await navigate(f.common_parent ?? state.summary.root);
}

function clearHighlight() {
  if (state.highlight === null) return;
  state.highlight = null;
  renderBody();
}

function renderChip() {
  const chip = $("chip");
  const f = state.highlight !== null ? finding(state.highlight) : null;
  chip.hidden = !f;
  if (!f) return;
  fill(
    chip,
    badge(f.safety),
    h("span", {}, `Showing ${f.title}`),
    h("button", { type: "button", "aria-label": "Stop showing this suggestion", onclick: clearHighlight }, "✕"),
  );
}

function renderSelected() {
  const pane = $("pane-selected");
  const t = state.view?.tiles.find((x) => x.path && x.path === state.selected);
  if (!t) {
    fill(pane, h("p", { class: "empty" }, "Select something on the map to see what it is and what you can do with it."));
    return;
  }
  const facts = [bytes(t.bytes)];
  if (t.kind === "dir") facts.push(`${t.files.toLocaleString()} files`, `${t.dirs.toLocaleString()} folders`);
  facts.push(`changed ${ago(t.changed)}`);
  const f = t.finding !== null ? finding(t.finding) : null;
  fill(pane, 
    h("p", { class: "sel-path" }, t.path),
    h("p", { class: "sel-facts" }, facts.join(" · ")),
    t.kind === "link" ? h("p", { class: "f-why" }, `A link to ${t.target}. What it points to is stored, and counted, there.`) : null,
    f
      ? h(
          "div",
          { class: "callout" },
          h("h3", {}, badge(f.safety), t.direct ? f.title : `Inside "${f.title}"`),
          t.direct ? h("p", { class: "f-why" }, f.why) : null,
          h("p", { class: "f-how" }, f.how),
        )
      : t.cleanable
        ? h("div", { class: "callout" }, h("h3", {}, badge("safe"), `${bytes(t.cleanable)} cleanable inside`), h("p", { class: "f-why" }, "Open it to see where."))
        : h("p", { class: "f-meta" }, "No suggestion covers this item."),
    h(
      "div",
      { class: "actions" },
      t.openable ? h("button", { type: "button", class: "btn primary", onclick: () => navigate(t.path) }, "Open") : null,
      h("button", { type: "button", class: "btn", onclick: () => invoke("reveal", { path: t.path }).catch((e) => toast(String(e))) }, "Show in Finder"),
      h("button", { type: "button", class: "btn", onclick: async () => showPlan(await invoke("plan_path", { path: t.path })) }, "Move to Trash…"),
    ),
  );
}

// ── Dialogs ────────────────────────────────────────────────────────────────

function showPlan(plan) {
  const dialog = $("dialog");
  const body = $("dialog-body");
  const actions = $("dialog-actions");
  if (plan.type === "refuse") {
    $("dialog-title").textContent = plan.title;
    fill(body, h("p", {}, plan.body));
    fill(actions, h("button", { class: "btn primary", value: "ok" }, "OK"));
    dialog.showModal();
    return;
  }
  $("dialog-title").textContent = plan.targets.length > 1 ? `Move ${plan.targets.length} items to the Trash?` : "Move to the Trash?";
  fill(body, 
    h(
      "ul",
      { class: "targets" },
      plan.targets.map((t) => h("li", {}, h("span", { class: "i-size" }, bytes(t.bytes)), h("span", { class: "i-path" }, t.path))),
    ),
    h("p", {}, `${bytes(plan.total)} in total. You can take items back out of the Trash until it's emptied.`),
    plan.warning ? h("p", { class: "warning" }, plan.warning) : null,
  );
  fill(actions, 
    h("button", { class: "btn", value: "cancel" }, "Cancel"),
    h("button", { class: "btn danger", value: "trash" }, "Move to Trash"),
  );
  dialog.returnValue = "";
  dialog.onclose = async () => {
    if (dialog.returnValue !== "trash") return;
    const result = await invoke("trash", { paths: plan.targets.map((t) => t.path) });
    toast(result.failures.length ? `${result.message} ${result.failures.join("; ")}` : result.message);
    state.summary = await invoke("summary");
    renderDisk();
    renderSuggestions();
    await navigate(state.view.path);
  };
  dialog.showModal();
}

// ── Keyboard ───────────────────────────────────────────────────────────────

document.addEventListener("keydown", (e) => {
  if ($("dialog").open || $("layout").hidden) return;
  if (e.key === "Backspace" || (e.key === "Escape" && state.highlight === null)) {
    e.preventDefault();
    up();
  } else if (e.key === "Escape") {
    clearHighlight();
  }
});

// ── Start ──────────────────────────────────────────────────────────────────

invoke("start_scan", { rescan: false }).catch((e) => {
  $("scan-title").textContent = "Couldn't start";
  $("scan-counts").textContent = String(e);
});
