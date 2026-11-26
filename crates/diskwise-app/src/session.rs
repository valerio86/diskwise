//! Everything the desktop UI asks for, computed from one scan. Plain Rust (no
//! Tauri types), so it can be unit-tested and used to make preview fixtures.

use std::collections::HashMap;

use diskwise_core::actions::{move_to_trash, protected_reason};
use diskwise_core::rules::{Action, Category, Safety};
use diskwise_core::{Finding, NodeKind, RuleSet, Snapshot, analyze, human_bytes};
use serde::Serialize;

/// One scan plus its analysis, with lookups rebuilt whenever either changes.
pub struct Session {
    pub snap: Snapshot,
    rules: RuleSet,
    now: i64,
    findings: Vec<Finding>,
    /// Display path → index into `findings`, for every finding item.
    claimed: HashMap<String, usize>,
    /// Display path → safe/caution bytes somewhere beneath it.
    cleanable_below: HashMap<String, u64>,
}

#[derive(Serialize)]
pub struct Crumb {
    pub name: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct Tile {
    pub name: String,
    pub path: Option<String>,
    /// "dir", "file", "link" or "rest" (many small items).
    pub kind: &'static str,
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub changed: i64,
    pub target: Option<String>,
    /// The suggestion covering this tile, directly or through a parent.
    pub finding: Option<usize>,
    pub safety: Option<Safety>,
    pub direct: bool,
    /// Safe/caution bytes inside (folders only).
    pub cleanable: u64,
    pub openable: bool,
    /// A folder's own contents, drawn small inside its tile so cleanable space
    /// deeper down is visible without opening it.
    pub inner: Vec<InnerTile>,
}

#[derive(Serialize)]
pub struct InnerTile {
    pub bytes: u64,
    pub safety: Option<Safety>,
    pub direct: bool,
    pub kind: &'static str,
    /// Safe/caution bytes deeper inside (drawn as a partial fill).
    pub cleanable: u64,
}

/// How many of a folder's children to draw inside its tile.
const INNER_TILES: usize = 48;

#[derive(Serialize)]
pub struct View {
    pub path: String,
    pub crumbs: Vec<Crumb>,
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub tiles: Vec<Tile>,
}

#[derive(Serialize)]
pub struct ItemView {
    pub path: String,
    pub bytes: u64,
    pub source: Option<String>,
}

#[derive(Serialize)]
pub struct FindingView {
    pub index: usize,
    pub id: String,
    pub title: String,
    pub category: Category,
    pub category_label: String,
    pub safety: Safety,
    pub action: Action,
    pub action_label: String,
    pub why: String,
    pub how: String,
    pub prevent: Option<String>,
    pub bytes: u64,
    pub size_known: bool,
    pub items: Vec<ItemView>,
    /// Whether diskwise can move this finding's items to the Trash itself.
    pub can_trash: bool,
    /// Deepest folder containing every item, for showing them on the map.
    pub common_parent: Option<String>,
}

#[derive(Serialize)]
pub struct Disk {
    pub capacity: u64,
    pub free: u64,
}

#[derive(Serialize)]
pub struct Summary {
    pub root: String,
    pub os: String,
    pub scanned_at: i64,
    pub disk: Option<Disk>,
    pub findings: Vec<FindingView>,
    pub safe: u64,
    pub caution: u64,
    pub review: u64,
    pub unreadable: u64,
}

#[derive(Serialize)]
pub struct TrashTarget {
    pub path: String,
    pub bytes: u64,
}

/// What would happen if the user asked to trash something: either a plan to
/// confirm, or a refusal explaining what to do instead.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Plan {
    Confirm {
        targets: Vec<TrashTarget>,
        total: u64,
        warning: Option<String>,
    },
    Refuse {
        title: String,
        body: String,
    },
}

#[derive(Serialize)]
pub struct TrashResult {
    pub moved: usize,
    pub bytes: u64,
    pub failures: Vec<String>,
    pub message: String,
}

impl Session {
    pub fn new(snap: Snapshot, rules: RuleSet, now: i64) -> Self {
        let mut s = Session {
            snap,
            rules,
            now,
            findings: Vec::new(),
            claimed: HashMap::new(),
            cleanable_below: HashMap::new(),
        };
        s.reanalyze();
        s
    }

    /// Recompute findings from the (possibly updated) tree. Cheap: milliseconds.
    fn reanalyze(&mut self) {
        self.findings = analyze(&self.snap, &self.rules, self.now);
        self.claimed.clear();
        self.cleanable_below.clear();
        for (i, f) in self.findings.iter().enumerate() {
            let counts = f.reclaimable() && f.safety != Safety::Review;
            for item in &f.items {
                self.claimed.insert(item.path.clone(), i);
                if counts {
                    for a in ancestors(&item.path) {
                        *self.cleanable_below.entry(a.to_string()).or_default() += item.bytes;
                    }
                }
            }
        }
    }

    fn coverage(&self, path: &str) -> Option<(usize, bool)> {
        if let Some(&i) = self.claimed.get(path) {
            return Some((i, true));
        }
        ancestors(path).find_map(|a| self.claimed.get(a).map(|&i| (i, false)))
    }

    pub fn summary(&self) -> Summary {
        let sum = |s: Safety| {
            self.findings
                .iter()
                .filter(|f| f.safety == s && f.reclaimable())
                .map(|f| f.bytes)
                .sum()
        };
        let disk = self.snap.system.as_ref().and_then(|s| match &s.container {
            Some(c) => Some(Disk {
                capacity: c.capacity,
                free: c.free,
            }),
            None => s.filesystem.map(|u| Disk {
                capacity: u.capacity,
                free: u.free,
            }),
        });
        Summary {
            root: self.snap.display_root(),
            os: self.snap.platform().os.to_string(),
            scanned_at: self.snap.scanned_at,
            disk,
            findings: self
                .findings
                .iter()
                .enumerate()
                .map(|(i, f)| self.finding_view(i, f))
                .collect(),
            safe: sum(Safety::Safe),
            caution: sum(Safety::Caution),
            review: sum(Safety::Review),
            unreadable: self.snap.errors.count,
        }
    }

    fn finding_view(&self, index: usize, f: &Finding) -> FindingView {
        FindingView {
            index,
            id: f.id.clone(),
            title: f.title.clone(),
            category: f.category,
            category_label: f.category.to_string(),
            safety: f.safety,
            action: f.action,
            action_label: f.action.to_string(),
            why: f.why.clone(),
            how: f.how.clone(),
            prevent: f.prevent.clone(),
            bytes: f.bytes,
            size_known: f.size_known,
            items: f
                .items
                .iter()
                .map(|i| ItemView {
                    path: i.path.clone(),
                    bytes: i.bytes,
                    source: i.source.clone(),
                })
                .collect(),
            can_trash: matches!(f.action, Action::Trash | Action::Archive) && !f.items.is_empty(),
            common_parent: common_parent(f.items.iter().map(|i| i.path.as_str())),
        }
    }

    /// The contents of one folder, ready to lay out as a treemap.
    pub fn view(&self, path: Option<&str>) -> Option<View> {
        let root = self.snap.display_root();
        let path = path.unwrap_or(&root).to_string();
        let node = self.snap.node(&path)?;
        let mut crumbs = vec![Crumb {
            name: root.clone(),
            path: root.clone(),
        }];
        let rel = path
            .strip_prefix(root.as_str())
            .unwrap_or("")
            .trim_start_matches('/');
        let mut acc = root.clone();
        for part in rel.split('/').filter(|s| !s.is_empty()) {
            acc = join(&acc, part);
            crumbs.push(Crumb {
                name: part.to_string(),
                path: acc.clone(),
            });
        }
        let tiles = node
            .children
            .iter()
            .map(|c| {
                let child_path = (c.kind != NodeKind::Rest).then(|| join(&path, &c.name));
                let cov = child_path.as_deref().and_then(|p| self.coverage(p));
                Tile {
                    name: c.name.clone(),
                    kind: kind_name(c.kind),
                    bytes: c.allocated,
                    files: c.files,
                    dirs: c.dirs,
                    changed: c.newest_mtime,
                    target: c.target.clone(),
                    finding: cov.map(|(i, _)| i),
                    safety: cov.map(|(i, _)| self.findings[i].safety),
                    direct: cov.is_some_and(|(_, d)| d),
                    cleanable: child_path
                        .as_deref()
                        .and_then(|p| self.cleanable_below.get(p))
                        .copied()
                        .unwrap_or(0),
                    openable: c.kind == NodeKind::Dir && !c.children.is_empty(),
                    inner: match (&child_path, c.kind) {
                        (Some(cp), NodeKind::Dir) => c
                            .children
                            .iter()
                            .filter(|g| g.allocated > 0)
                            .take(INNER_TILES)
                            .map(|g| {
                                let gp = (g.kind != NodeKind::Rest).then(|| join(cp, &g.name));
                                let cov = gp.as_deref().and_then(|p| self.coverage(p));
                                InnerTile {
                                    bytes: g.allocated,
                                    safety: cov.map(|(i, _)| self.findings[i].safety),
                                    direct: cov.is_some_and(|(_, d)| d),
                                    kind: kind_name(g.kind),
                                    cleanable: gp
                                        .as_deref()
                                        .and_then(|p| self.cleanable_below.get(p))
                                        .copied()
                                        .unwrap_or(0),
                                }
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                    path: child_path,
                }
            })
            .collect();
        Some(View {
            path,
            crumbs,
            bytes: node.allocated,
            files: node.files,
            dirs: node.dirs,
            tiles,
        })
    }

    /// Plan trashing one path the user picked on the map.
    pub fn plan_path(&self, path: &str) -> Plan {
        let Some(node) = self.snap.node(path) else {
            return refuse("Not found", format!("{path} isn't in the scan any more."));
        };
        if let Some(reason) = protected_reason(path, self.snap.platform()) {
            return refuse("Protected folder", reason);
        }
        let target = vec![TrashTarget {
            path: path.to_string(),
            bytes: node.allocated,
        }];
        if node.kind == NodeKind::Link {
            let to = node.target.clone().unwrap_or_else(|| "its target".into());
            return confirm(
                target,
                Some(format!(
                    "This is a link. Only the link is moved to the Trash, not {to}, so this frees almost no space."
                )),
            );
        }
        match self.coverage(path) {
            Some((i, direct)) => {
                let f = &self.findings[i];
                if !matches!(f.action, Action::Trash | Action::Archive) {
                    return refusal(f);
                }
                confirm(target, warning_for(f, direct))
            }
            None => confirm(
                target,
                Some("No suggestion covers this item. Make sure you don't need it.".into()),
            ),
        }
    }

    /// Plan trashing every item of a suggestion.
    pub fn plan_finding(&self, index: usize) -> Plan {
        let Some(f) = self.findings.get(index) else {
            return refuse("Not found", "That suggestion no longer exists.".into());
        };
        if !matches!(f.action, Action::Trash | Action::Archive) || f.items.is_empty() {
            return refusal(f);
        }
        let platform = self.snap.platform();
        let (allowed, protected): (Vec<_>, Vec<_>) = f
            .items
            .iter()
            .partition(|i| protected_reason(&i.path, platform).is_none());
        if allowed.is_empty() {
            return refuse(
                "Nothing to move",
                "Every item in this suggestion is a top-level or home folder, which diskwise won't move to the Trash.".into(),
            );
        }
        let mut warning = warning_for(f, true);
        if !protected.is_empty() {
            let note = format!(
                "Left out (protected folders): {}.",
                protected
                    .iter()
                    .map(|i| i.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            warning = Some(match warning {
                Some(w) => format!("{w} {note}"),
                None => note,
            });
        }
        confirm(
            allowed
                .into_iter()
                .map(|i| TrashTarget {
                    path: i.path.clone(),
                    bytes: i.bytes,
                })
                .collect(),
            warning,
        )
    }

    /// Move confirmed paths to the Trash, verify each is gone, then update the
    /// tree and re-run the analysis so every total stays consistent.
    pub fn trash(
        &mut self,
        paths: &[String],
        trasher: impl Fn(&std::path::Path) -> Result<(), String>,
    ) -> TrashResult {
        let platform = self.snap.platform();
        let mut moved = 0;
        let mut bytes = 0;
        let mut failures = Vec::new();
        for p in paths {
            // Re-check here: the UI only sends what a plan allowed, but don't rely on it.
            if let Some(reason) = protected_reason(p, platform) {
                failures.push(reason);
                continue;
            }
            let Some(size) = self.snap.node(p).map(|n| n.allocated) else {
                failures.push(format!("{p}: not in the scan"));
                continue;
            };
            let real = self.snap.real_path(p);
            match trasher(std::path::Path::new(&real)) {
                Ok(()) => {
                    self.snap.remove(p);
                    moved += 1;
                    bytes += size;
                }
                Err(e) => failures.push(format!("{p}: {e}")),
            }
        }
        self.reanalyze();
        let mut message = if moved > 0 {
            format!(
                "Moved {moved} item{} ({}) to the Trash. The space comes back when you empty the Trash.",
                if moved == 1 { "" } else { "s" },
                human_bytes(bytes)
            )
        } else {
            "Nothing was moved.".to_string()
        };
        if !failures.is_empty() {
            message.push_str(&format!(" {} failed.", failures.len()));
        }
        TrashResult {
            moved,
            bytes,
            failures,
            message,
        }
    }

    /// Trash with the real system Trash.
    pub fn trash_for_real(&mut self, paths: &[String]) -> TrashResult {
        self.trash(paths, move_to_trash)
    }
}

fn kind_name(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Dir => "dir",
        NodeKind::File => "file",
        NodeKind::Link => "link",
        NodeKind::Rest => "rest",
    }
}

fn confirm(targets: Vec<TrashTarget>, warning: Option<String>) -> Plan {
    let total = targets.iter().map(|t| t.bytes).sum();
    Plan::Confirm {
        targets,
        total,
        warning,
    }
}

fn refuse(title: &str, body: String) -> Plan {
    Plan::Refuse {
        title: title.into(),
        body,
    }
}

fn refusal(f: &Finding) -> Plan {
    let why = match f.action {
        Action::InApp => {
            "This should be cleared from inside the app that owns it, or the app may get confused or put it straight back."
        }
        Action::Admin => {
            "This is owned by the system and needs your admin password, so diskwise won't touch it."
        }
        Action::Info => "There's nothing here to delete: this explains where space went.",
        _ => "diskwise won't move this to the Trash.",
    };
    refuse(&f.title, format!("{why}\n\nWhat to do: {}", f.how))
}

fn warning_for(f: &Finding, direct: bool) -> Option<String> {
    let inside = if direct {
        String::new()
    } else {
        format!(" (this is inside \"{}\")", f.title)
    };
    match (f.safety, f.action) {
        (_, Action::Archive) => Some(format!("Your data{inside}: back it up first. {}", f.how)),
        (Safety::Review, _) => Some(format!("Review first{inside}: {}", f.how)),
        (Safety::Caution, _) => Some(format!("Before you confirm{inside}: {}", f.how)),
        (Safety::Safe, _) => None,
    }
}

/// Prefixes of a display path, nearest first, excluding the path itself.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let mut end = Some(path.len());
    std::iter::from_fn(move || {
        let cut = path[..end?].rfind('/')?;
        end = if cut == 0 { None } else { Some(cut) };
        Some(if cut == 0 { "/" } else { &path[..cut] })
    })
}

/// Deepest folder containing all paths (one path: its parent).
fn common_parent<'a>(mut paths: impl Iterator<Item = &'a str>) -> Option<String> {
    let first = paths.next()?;
    let mut prefix: Vec<&str> = first.split('/').collect();
    prefix.pop(); // the item itself
    for p in paths {
        let parts: Vec<&str> = p.split('/').collect();
        let n = prefix
            .iter()
            .zip(&parts[..parts.len() - 1])
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(n);
    }
    let joined = prefix.join("/");
    Some(if joined.is_empty() {
        "/".into()
    } else {
        joined
    })
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskwise_core::model::{ErrorSummary, Node, SNAPSHOT_VERSION};
    use diskwise_core::platform::LINUX;

    fn n(name: &str, kind: NodeKind, size: u64, children: Vec<Node>) -> Node {
        let mut node = Node::new(name.into(), kind);
        node.allocated = size + children.iter().map(|c| c.allocated).sum::<u64>();
        node.files = if kind == NodeKind::File {
            1
        } else {
            children.iter().map(|c| c.files).sum()
        };
        node.children = children;
        node
    }

    /// A Linux home with a pip cache (safe), a venv (caution) and documents.
    fn session(root: &str) -> Session {
        let tree = n(
            root,
            NodeKind::Dir,
            0,
            vec![n(
                "home",
                NodeKind::Dir,
                0,
                vec![n(
                    "v",
                    NodeKind::Dir,
                    0,
                    vec![
                        n(
                            ".cache",
                            NodeKind::Dir,
                            0,
                            vec![n(
                                "pip",
                                NodeKind::Dir,
                                0,
                                vec![n("w", NodeKind::File, 300 << 20, vec![])],
                            )],
                        ),
                        n(
                            "proj",
                            NodeKind::Dir,
                            0,
                            vec![n(
                                ".venv",
                                NodeKind::Dir,
                                0,
                                vec![n("lib", NodeKind::File, 200 << 20, vec![])],
                            )],
                        ),
                        n(
                            "docs",
                            NodeKind::Dir,
                            0,
                            vec![n("thesis.pdf", NodeKind::File, 100 << 20, vec![])],
                        ),
                    ],
                )],
            )],
        );
        let snap = Snapshot {
            version: SNAPSHOT_VERSION,
            os: "linux".into(),
            root: root.into(),
            scanned_at: 0,
            duration_ms: 0,
            min_node_size: 0,
            tree,
            errors: ErrorSummary::default(),
            skipped_mounts: vec![],
            system: None,
        };
        Session::new(snap, RuleSet::builtin_for(&LINUX).unwrap(), 1_800_000_000)
    }

    #[test]
    fn view_tags_tiles_with_suggestions() {
        let s = session("/");
        let v = s.view(Some("/home/v")).unwrap();
        assert_eq!(
            v.crumbs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["/", "home", "v"]
        );
        let cache = v.tiles.iter().find(|t| t.name == ".cache").unwrap();
        assert_eq!(cache.cleanable, 300 << 20, "pip cache inside");
        assert!(cache.finding.is_none());
        let pip = &s.view(Some("/home/v/.cache")).unwrap().tiles[0];
        assert_eq!((pip.safety, pip.direct), (Some(Safety::Safe), true));
        // The pip cache shows up, coloured, inside the .cache tile.
        assert_eq!(cache.inner.len(), 1);
        assert_eq!(
            (cache.inner[0].safety, cache.inner[0].direct),
            (Some(Safety::Safe), true)
        );
        let docs = v.tiles.iter().find(|t| t.name == "docs").unwrap();
        assert!(docs.finding.is_none() && docs.cleanable == 0);
    }

    #[test]
    fn plans_refuse_and_warn() {
        let s = session("/");
        assert!(
            matches!(s.plan_path("/home/v"), Plan::Refuse { .. }),
            "home folder"
        );
        assert!(
            matches!(
                s.plan_path("/home/v/.cache/pip"),
                Plan::Confirm { warning: None, .. }
            ),
            "safe"
        );
        assert!(
            matches!(
                s.plan_path("/home/v/proj/.venv"),
                Plan::Confirm {
                    warning: Some(_),
                    ..
                }
            ),
            "caution"
        );
        assert!(
            matches!(
                s.plan_path("/home/v/docs"),
                Plan::Confirm {
                    warning: Some(_),
                    ..
                }
            ),
            "uncovered"
        );
    }

    #[test]
    fn trash_updates_tree_and_suggestions() {
        let mut s = session("/");
        let before = s.summary();
        let pip = before
            .findings
            .iter()
            .find(|f| f.id == "package-manager-caches")
            .unwrap();
        assert_eq!(pip.common_parent.as_deref(), Some("/home/v/.cache"));
        // A trasher that claims success but leaves the file: nothing changes.
        let r = s.trash(&["/home/v/.cache/pip".into()], |_| {
            Err("still there after moving to Trash".into())
        });
        assert_eq!(r.moved, 0);
        assert!(
            s.summary()
                .findings
                .iter()
                .any(|f| f.id == "package-manager-caches")
        );
        // A real move: the tree shrinks and the suggestion disappears.
        let r = s.trash(&["/home/v/.cache/pip".into()], |_| Ok(()));
        assert_eq!((r.moved, r.bytes), (1, 300 << 20));
        assert_eq!(s.snap.tree.allocated, 300 << 20);
        assert!(
            s.summary()
                .findings
                .iter()
                .all(|f| f.id != "package-manager-caches")
        );
        // Protected paths are refused even if the UI sent them.
        let r = s.trash(&["/home/v".into()], |_| Ok(()));
        assert_eq!(r.moved, 0);
        assert_eq!(r.failures.len(), 1);
    }

    /// Writes dev/fixture.js (git-ignored: it holds real paths) for the preview
    /// harness: DISKWISE_SNAPSHOT=snap.json cargo test -p diskwise-app fixture -- --ignored
    #[test]
    #[ignore]
    fn write_preview_fixture() {
        let file = std::env::var("DISKWISE_SNAPSHOT").expect("set DISKWISE_SNAPSHOT");
        let snap: Snapshot = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        let rules = RuleSet::builtin_for(snap.platform()).unwrap();
        let s = Session::new(snap, rules, 1_795_000_000);
        let summary = s.summary();
        // Every folder down to a few levels, plus each suggestion's folder.
        let mut paths = vec![s.snap.display_root()];
        let mut i = 0;
        while i < paths.len() && paths.len() < 400 {
            let p = paths[i].clone();
            if let Some(v) = s.view(Some(&p))
                && v.crumbs.len() <= 5
            {
                paths.extend(
                    v.tiles
                        .iter()
                        .filter(|t| t.openable)
                        .filter_map(|t| t.path.clone()),
                );
            }
            i += 1;
        }
        paths.extend(
            summary
                .findings
                .iter()
                .filter_map(|f| f.common_parent.clone()),
        );
        let views: std::collections::BTreeMap<String, View> = paths
            .into_iter()
            .filter_map(|p| s.view(Some(&p)).map(|v| (p, v)))
            .collect();
        let js = format!(
            "window.FIXTURE = {{ summary: {}, views: {} }};\n",
            serde_json::to_string(&summary).unwrap(),
            serde_json::to_string(&views).unwrap()
        );
        std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "/dev/fixture.js"), js).unwrap();
    }

    #[test]
    fn common_parents() {
        assert_eq!(
            common_parent(["/a/b/c", "/a/b/d"].into_iter()).as_deref(),
            Some("/a/b")
        );
        assert_eq!(
            common_parent(["/a/b/c"].into_iter()).as_deref(),
            Some("/a/b")
        );
        assert_eq!(
            common_parent(["/a/x", "/b/y"].into_iter()).as_deref(),
            Some("/")
        );
    }
}
