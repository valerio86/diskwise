// Stands in for Tauri's bridge so ui/ can be previewed in a browser with
// fixtures from a real snapshot (see write_preview_fixture in session.rs).
// URL parameters drive the starting state for screenshots:
//   ?path=/Users/x/Library   open a folder        &select=/Users/x/Library/Caches
//   &finding=python-venv     show a suggestion     &show=1  show it on the map
//   &tab=browse              start in Browse
//   &mode=list               list view             &trash=/path  open the trash dialog
//   &tab=dupes&dupe=3        a duplicate group (by position)  &live=1  as if freshly scanned
//   &dupeplan=1  its confirmation   &intentional=1  the folder chooser   &offer=1  "remember this?"
"use strict";
(() => {
  const F = window.FIXTURE;
  const listeners = {};
  const plan = (targets, warning) => ({
    type: "confirm",
    targets,
    total: targets.reduce((a, t) => a + t.bytes, 0),
    warning,
  });
  const handlers = {
    start_scan: () => {
      setTimeout(() => (listeners["scan-done"] || []).forEach((cb) => cb({ payload: { type: "ready" } })), 30);
      setTimeout(() => (listeners["dupes-done"] || []).forEach((cb) => cb({ payload: null })), 60);
    },
    dupes: () => F.dupes,
    dupe_set_action: ({ id, path, action }) => {
      const c = F.dupes.groups.find((g) => g.id === id).copies.find((x) => x.path === path);
      c.action = action;
      c.label = { trash: "trash", store_once: "store once", keep: "keep as is" }[action];
      return F.dupes;
    },
    dupe_set_keeper: () => ({ offer: { prefer: "/Users/x/Projects/site", over: "/Users/x/Downloads" } }),
    dupe_prefer_folder: () => null,
    dupe_folder_options: ({ path }) => {
      const parts = path.split("/");
      return [1, 2, 3].map((n) => parts.slice(0, parts.length - n).join("/")).filter((p) => p.split("/").length > 3);
    },
    dupe_mark_intentional: () => null,
    dupe_plan: ({ id }) => {
      const g = F.dupes.groups.find((x) => x.id === id);
      const steps = g.copies
        .slice(1)
        .filter((c) => c.action === "trash" || c.action === "store_once")
        .map((c) => ({ target: { keeper: g.copies[0].path, copy: c.path, folder: g.folder }, how: c.action, bytes: c.frees, synced: c.synced }));
      return { type: "confirm", keeper: g.copies[0].path, steps, total: steps.reduce((a, s) => a + s.bytes, 0), warning: null };
    },
    dupe_apply: () => ({ moved: 0, bytes: 0, failures: [], message: "Preview: nothing was changed." }),
    summary: () => F.summary,
    view: ({ path }) => {
      if (F.views[path]) return F.views[path];
      throw "That folder isn't in the preview fixture.";
    },
    plan_path: ({ path }) => plan([{ path, bytes: 330100000 }], null),
    plan_finding: ({ id }) => {
      const f = F.summary.findings.find((x) => x.id === id);
      return plan(f.items.map((i) => ({ path: i.path, bytes: i.bytes })), "Before you confirm: quit the app first.");
    },
    trash: ({ paths }) => ({ moved: paths.length, bytes: 0, failures: [], message: "Preview: nothing was moved." }),
    reveal: () => null,
  };
  window.__TAURI__ = {
    core: { invoke: async (cmd, args) => handlers[cmd](args || {}) },
    event: {
      listen: async (name, cb) => {
        (listeners[name] = listeners[name] || []).push(cb);
        return () => {};
      },
    },
  };
  window.addEventListener("load", () => {
    const q = new URLSearchParams(location.search);
    setTimeout(async () => {
      if (q.get("finding")) showSuggestion(q.get("finding"));
      if (q.get("show")) await showOnMap(finding(q.get("finding")));
      if (q.get("tab")) setTab(q.get("tab"));
      if (q.get("mode")) setMode(q.get("mode"));
      if (q.get("path")) await navigate(q.get("path"));
      if (q.get("select")) {
        const t = state.view.tiles.find((x) => x.path === q.get("select"));
        if (t) select(t);
      }
      if (q.get("trash")) showPlan(await invoke("plan_path", { path: q.get("trash") }));
      if (q.get("live")) {
        F.summary.live = true;
        state.summary.live = true;
      }
      if (q.get("dupe")) {
        await refreshDupes();
        showDupe(F.dupes.groups[Number(q.get("dupe"))].id);
      }
      const g = dupeGroup(state.dupe);
      if (q.get("dupeplan") && g) showDupePlan(g.id, await invoke("dupe_plan", { id: g.id }));
      if (q.get("intentional") && g) chooseIntentional(g.id, g.copies[1].path);
      if (q.get("offer")) offerFolderOrder({ prefer: g.copies[0].path.replace(/\/[^/]+$/, ""), over: g.copies[1].path.replace(/\/[^/]+$/, "") });
    }, 150);
  });
})();
