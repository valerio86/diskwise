// Stands in for Tauri's bridge so ui/ can be previewed in a browser with
// fixtures from a real snapshot (see write_preview_fixture in session.rs).
// URL parameters drive the starting state for screenshots:
//   ?path=/Users/x/Library   open a folder        &select=/Users/x/Library/Caches
//   &finding=3               expand suggestion 3   &show=1  show it on the map
//   &mode=list               list view             &trash=/path  open the trash dialog
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
    },
    summary: () => F.summary,
    view: ({ path }) => {
      if (F.views[path]) return F.views[path];
      throw "That folder isn't in the preview fixture.";
    },
    plan_path: ({ path }) => plan([{ path, bytes: 330100000 }], null),
    plan_finding: ({ index }) => {
      const f = F.summary.findings.find((x) => x.index === index);
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
      if (q.get("mode")) setMode(q.get("mode"));
      if (q.get("path")) await navigate(q.get("path"));
      if (q.get("select")) {
        const t = state.view.tiles.find((x) => x.path === q.get("select"));
        if (t) select(t);
      }
      if (q.get("finding")) {
        const f = finding(Number(q.get("finding")));
        state.open.add(f.index);
        renderSuggestions();
        if (q.get("show")) await showOnMap(f);
      }
      if (q.get("trash")) showPlan(await invoke("plan_path", { path: q.get("trash") }));
    }, 150);
  });
})();
