//! TUI state and behaviour, independent of drawing so it can be unit-tested.

use std::collections::HashMap;
use std::path::Path;

use diskwise_core::rules::{Action, Safety};
use diskwise_core::{Finding, Node, NodeKind, Platform, Snapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Browse,
    Suggestions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Size,
    Name,
    /// Least recently modified first: what you probably forgot about.
    Oldest,
}

impl SortBy {
    pub fn next(self) -> Self {
        match self {
            SortBy::Size => SortBy::Name,
            SortBy::Name => SortBy::Oldest,
            SortBy::Oldest => SortBy::Size,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SortBy::Size => "size",
            SortBy::Name => "name",
            SortBy::Oldest => "oldest first",
        }
    }
}

/// Something to move to the Trash, as shown in the confirmation dialog.
#[derive(Debug, Clone)]
pub struct TrashTarget {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub enum Modal {
    Help,
    /// Explain why diskwise won't act (in-app, admin, info items, protected paths).
    Message {
        title: String,
        body: String,
    },
    ConfirmTrash {
        targets: Vec<TrashTarget>,
        /// Shown prominently; None means "nothing is lost".
        warning: Option<String>,
    },
}

/// The suggestion that covers a path, directly or through an ancestor.
pub struct Coverage<'a> {
    pub finding: &'a Finding,
    /// True if the path is one of the finding's items (not just inside one).
    pub direct: bool,
}

pub struct App {
    pub snap: Snapshot,
    pub findings: Vec<Finding>,
    platform: &'static Platform,
    pub root_display: String,
    /// Display path → index into `findings`, for every finding item.
    claimed: HashMap<String, usize>,
    /// Display path → bytes of safe/caution suggestions somewhere beneath it.
    cleanable_below: HashMap<String, u64>,
    /// Child indices from the tree root to the directory being browsed.
    pub cwd: Vec<usize>,
    pub selected: usize,
    pub tab: Tab,
    pub suggestion: usize,
    pub sort: SortBy,
    pub modal: Option<Modal>,
    pub status: Option<String>,
    pub quit: bool,
    /// Set when the user asks for a fresh scan; the event loop handles it.
    pub rescan_requested: bool,
}

impl App {
    pub fn new(snap: Snapshot, findings: Vec<Finding>) -> Self {
        let platform = snap.platform();
        // No trailing slash (except "/" itself): paths are joined and compared as
        // strings, and a root typed as "/home/v/" would otherwise match nothing.
        let mut root_display = platform.display_path(&snap.root);
        while root_display.len() > 1 && root_display.ends_with('/') {
            root_display.pop();
        }
        let mut app = App {
            snap,
            findings,
            platform,
            root_display,
            claimed: HashMap::new(),
            cleanable_below: HashMap::new(),
            cwd: Vec::new(),
            selected: 0,
            // Open on what to clean up; the browser is one key away.
            tab: Tab::Suggestions,
            suggestion: 0,
            sort: SortBy::Size,
            modal: None,
            status: None,
            quit: false,
            rescan_requested: false,
        };
        // The scanner sorts "(smaller items)" by size like everything else; here it
        // always goes last.
        sort_tree(&mut app.snap.tree, SortBy::Size);
        app.reindex();
        app
    }

    /// Rebuild the path → finding lookups after findings change.
    fn reindex(&mut self) {
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
        if self.suggestion >= self.findings.len() {
            self.suggestion = self.findings.len().saturating_sub(1);
        }
    }

    // ── Tree access ─────────────────────────────────────────────────────────

    fn node_at(&self, path: &[usize]) -> &Node {
        path.iter().fold(&self.snap.tree, |n, &i| &n.children[i])
    }

    fn node_at_mut(&mut self, path: &[usize]) -> &mut Node {
        path.iter()
            .fold(&mut self.snap.tree, |n, &i| &mut n.children[i])
    }

    pub fn current_dir(&self) -> &Node {
        self.node_at(&self.cwd)
    }

    pub fn current_path(&self) -> String {
        let mut p = self.root_display.clone();
        for depth in 1..=self.cwd.len() {
            p = join(&p, &self.node_at(&self.cwd[..depth]).name);
        }
        p
    }

    pub fn entries(&self) -> &[Node] {
        &self.current_dir().children
    }

    pub fn selected_node(&self) -> Option<&Node> {
        self.entries().get(self.selected)
    }

    /// Display path of an entry in the current directory (None for "(smaller items)").
    pub fn entry_path(&self, entry: &Node) -> Option<String> {
        (entry.kind != NodeKind::Rest).then(|| join(&self.current_path(), &entry.name))
    }

    /// Child-index path to a display path, if it's in the (pruned) tree.
    pub fn locate(&self, path: &str) -> Option<Vec<usize>> {
        let rest = path.strip_prefix(self.root_display.as_str())?;
        // "/home/v" must not match "/home/vx/…".
        if !(rest.is_empty() || rest.starts_with('/') || self.root_display.ends_with('/')) {
            return None;
        }
        let rel = rest.trim_start_matches('/');
        let mut idx = Vec::new();
        let mut node = &self.snap.tree;
        for name in rel.split('/').filter(|s| !s.is_empty()) {
            let i = node
                .children
                .iter()
                .position(|c| c.kind != NodeKind::Rest && c.name == name)?;
            idx.push(i);
            node = &node.children[i];
        }
        Some(idx)
    }

    // ── Suggestions ────────────────────────────────────────────────────────

    pub fn coverage(&self, path: &str) -> Option<Coverage<'_>> {
        if let Some(&i) = self.claimed.get(path) {
            return Some(Coverage {
                finding: &self.findings[i],
                direct: true,
            });
        }
        ancestors(path).find_map(|a| {
            self.claimed.get(a).map(|&i| Coverage {
                finding: &self.findings[i],
                direct: false,
            })
        })
    }

    pub fn cleanable_below(&self, path: &str) -> u64 {
        self.cleanable_below.get(path).copied().unwrap_or(0)
    }

    pub fn selected_finding(&self) -> Option<&Finding> {
        self.findings.get(self.suggestion)
    }

    // ── Navigation ─────────────────────────────────────────────────────────

    pub fn move_selection(&mut self, delta: isize) {
        match self.tab {
            Tab::Browse => self.selected = step(self.selected, delta, self.entries().len()),
            Tab::Suggestions => self.suggestion = step(self.suggestion, delta, self.findings.len()),
        }
    }

    pub fn select_first(&mut self) {
        match self.tab {
            Tab::Browse => self.selected = 0,
            Tab::Suggestions => self.suggestion = 0,
        }
    }

    pub fn select_last(&mut self) {
        match self.tab {
            Tab::Browse => self.selected = self.entries().len().saturating_sub(1),
            Tab::Suggestions => self.suggestion = self.findings.len().saturating_sub(1),
        }
    }

    /// Enter the selected directory (Browse) or jump to the suggestion's top item.
    pub fn open(&mut self) {
        match self.tab {
            Tab::Browse => {
                let Some(node) = self.selected_node() else {
                    return;
                };
                let (kind, has_children, path) =
                    (node.kind, !node.children.is_empty(), self.entry_path(node));
                match kind {
                    NodeKind::Dir if has_children => {
                        self.cwd.push(self.selected);
                        self.selected = 0;
                        self.status = None;
                    }
                    NodeKind::Dir => {
                        self.status = Some(format!(
                            "Nothing inside is over {} on its own.",
                            diskwise_core::human_bytes(self.snap.min_node_size)
                        ))
                    }
                    NodeKind::Link => {
                        if let Some(p) = path {
                            self.follow_link(&p);
                        }
                    }
                    _ => {}
                }
            }
            Tab::Suggestions => {
                let Some(path) = self
                    .selected_finding()
                    .and_then(|f| f.items.first())
                    .map(|i| i.path.clone())
                else {
                    self.status = Some(
                        "This suggestion explains system space; there's nothing to browse.".into(),
                    );
                    return;
                };
                self.reveal(&path);
            }
        }
    }

    /// Jump to where a link points, if that was scanned; otherwise say where it goes.
    fn follow_link(&mut self, link: &str) {
        match std::fs::canonicalize(self.real_path(link)) {
            Ok(t) => {
                let target = self.platform.display_path(&t.display().to_string());
                if self.locate(&target).is_some() {
                    self.reveal(&target);
                    self.status = Some(format!("Followed the link to {target}."));
                } else {
                    self.status = Some(format!(
                        "This links to {target}, outside what's shown here. Its contents are counted there, not in this folder."
                    ));
                }
            }
            Err(_) => {
                self.status = Some("This link points to something that no longer exists.".into())
            }
        }
    }

    /// Show `path` selected in the browser.
    pub fn reveal(&mut self, path: &str) {
        match self.locate(path) {
            Some(mut idx) if !idx.is_empty() => {
                self.selected = idx.pop().unwrap();
                self.cwd = idx;
                self.tab = Tab::Browse;
                self.status = None;
            }
            _ => self.status = Some(format!("{path} isn't in the scanned tree.")),
        }
    }

    pub fn up(&mut self) {
        if let Some(i) = self.cwd.pop() {
            self.selected = i;
            self.status = None;
        }
    }

    pub fn toggle_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Browse => Tab::Suggestions,
            Tab::Suggestions => Tab::Browse,
        };
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        // Sorting reorders children, so `cwd` indices go stale: remember the current
        // directory and selection by name, then find them again.
        let cwd_path = self.current_path();
        let selected_name = self.selected_node().map(|n| n.name.clone());
        sort_tree(&mut self.snap.tree, self.sort);
        self.cwd = self.locate(&cwd_path).unwrap_or_default();
        self.selected = selected_name
            .and_then(|name| self.entries().iter().position(|n| n.name == name))
            .unwrap_or(0);
        self.status = Some(format!("Sorted by {}.", self.sort.label()));
    }

    // ── Moving to Trash ────────────────────────────────────────────────────

    /// Ask to move the selected entry (Browse) or a suggestion's items to the Trash.
    pub fn request_trash(&mut self) {
        match self.tab {
            Tab::Browse => {
                let Some(node) = self.selected_node() else {
                    return;
                };
                let Some(path) = self.entry_path(node) else {
                    self.modal = Some(Modal::Message {
                        title: "Not a single item".into(),
                        body: "\"(smaller items)\" groups many small files and folders. Open the folder they belong to in a file manager to pick what to remove.".into(),
                    });
                    return;
                };
                let target = TrashTarget {
                    path: path.clone(),
                    bytes: node.allocated,
                };
                let link_target = (node.kind == NodeKind::Link)
                    .then(|| node.target.clone().unwrap_or_else(|| "its target".into()));
                if let Some(reason) = self.protected_reason(&path) {
                    self.modal = Some(Modal::Message {
                        title: "Protected folder".into(),
                        body: reason,
                    });
                    return;
                }
                if let Some(points_to) = link_target {
                    self.modal = Some(Modal::ConfirmTrash {
                        targets: vec![target],
                        warning: Some(format!(
                            "This is a link. Only the link is moved to the Trash, not {points_to}, so this frees almost no space."
                        )),
                    });
                    return;
                }
                let coverage = self.coverage(&path).map(|c| (c.finding.clone(), c.direct));
                self.modal = Some(match coverage {
                    Some((f, _)) if !matches!(f.action, Action::Trash | Action::Archive) => {
                        refusal(&f)
                    }
                    Some((f, direct)) => Modal::ConfirmTrash {
                        targets: vec![target],
                        warning: warning_for(&f, direct),
                    },
                    None => Modal::ConfirmTrash {
                        targets: vec![target],
                        warning: Some(
                            "No suggestion covers this item. Make sure you don't need it.".into(),
                        ),
                    },
                });
            }
            Tab::Suggestions => {
                let Some(f) = self.selected_finding().cloned() else {
                    return;
                };
                if !matches!(f.action, Action::Trash | Action::Archive) || f.items.is_empty() {
                    self.modal = Some(refusal(&f));
                    return;
                }
                let (allowed, protected): (Vec<_>, Vec<_>) = f
                    .items
                    .iter()
                    .partition(|i| self.protected_reason(&i.path).is_none());
                if allowed.is_empty() {
                    self.modal = Some(Modal::Message {
                        title: "Nothing to move".into(),
                        body: "Every item in this suggestion is a top-level or home folder, which diskwise won't move to the Trash.".into(),
                    });
                    return;
                }
                let mut warning = warning_for(&f, true);
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
                let targets = allowed
                    .into_iter()
                    .map(|i| TrashTarget {
                        path: i.path.clone(),
                        bytes: i.bytes,
                    })
                    .collect();
                self.modal = Some(Modal::ConfirmTrash { targets, warning });
            }
        }
    }

    /// Paths diskwise refuses to trash however they're selected.
    fn protected_reason(&self, path: &str) -> Option<String> {
        diskwise_core::actions::protected_reason(path, self.platform)
    }

    /// Move the confirmed targets to the Trash with `trasher`, then verify each is
    /// gone and update the tree and suggestions. Returns a summary for the status line.
    pub fn apply_trash(
        &mut self,
        targets: &[TrashTarget],
        trasher: impl Fn(&Path) -> Result<(), String>,
    ) -> String {
        let mut moved = 0u64;
        let mut moved_count = 0;
        let mut failures = Vec::new();
        for t in targets {
            let real = self.real_path(&t.path);
            match trasher(Path::new(&real)) {
                Err(e) => failures.push(format!("{}: {e}", t.path)),
                // Verify: the item must really be gone from its old location.
                Ok(()) if std::fs::symlink_metadata(&real).is_ok() => {
                    failures.push(format!("{}: still there after moving to Trash", t.path))
                }
                Ok(()) => {
                    self.remove_from_tree(&t.path);
                    self.remove_from_findings(&t.path, t.bytes);
                    moved += t.bytes;
                    moved_count += 1;
                }
            }
        }
        self.reindex();
        self.selected = self.selected.min(self.entries().len().saturating_sub(1));

        let mut msg = if moved_count > 0 {
            format!(
                "Moved {moved_count} item(s), {}, to the Trash. The space is freed when the Trash is emptied.",
                diskwise_core::human_bytes(moved)
            )
        } else {
            "Nothing was moved.".to_string()
        };
        if !failures.is_empty() {
            msg.push_str(&format!(
                " {} failed: {}",
                failures.len(),
                failures.join("; ")
            ));
        }
        msg
    }

    /// The on-disk path for a display path. On macOS the scan root is the Data
    /// volume and display paths drop its prefix, but only some of its folders are
    /// firmlinked at `/` (e.g. /Users, not /.adobeTemp), so always act on the
    /// path under the real scan root.
    fn real_path(&self, display: &str) -> String {
        self.snap.real_path(display)
    }

    fn remove_from_tree(&mut self, path: &str) {
        let Some(idx) = self.locate(path) else { return };
        let Some((&last, parent_idx)) = idx.split_last() else {
            return;
        };
        let removed = self.node_at_mut(parent_idx).children.remove(last);
        // Subtract from every ancestor, root included.
        for depth in 0..=parent_idx.len() {
            let n = self.node_at_mut(&parent_idx[..depth]);
            n.allocated = n.allocated.saturating_sub(removed.allocated);
            n.logical = n.logical.saturating_sub(removed.logical);
            n.files = n.files.saturating_sub(removed.files);
            if removed.kind == NodeKind::Dir {
                n.dirs = n.dirs.saturating_sub(1 + removed.dirs);
            }
        }
        // Keep the browser pointing at a valid directory.
        if self.cwd.starts_with(&idx) {
            self.cwd.truncate(parent_idx.len());
            self.selected = last;
        } else if self.cwd.len() > parent_idx.len()
            && self.cwd[..parent_idx.len()] == *parent_idx
            && self.cwd[parent_idx.len()] > last
        {
            self.cwd[parent_idx.len()] -= 1;
        }
    }

    /// Remove `path` (and anything inside it) from the findings. A finding whose
    /// last item is gone is dropped; findings that never had items (system
    /// explanations) are kept.
    fn remove_from_findings(&mut self, path: &str, bytes: u64) {
        let inside = |p: &str| p == path || p.starts_with(&format!("{path}/"));
        // An item that *contains* the removed path (e.g. trashing Chrome/ inside the
        // Caches/Google item) loses those bytes but stays.
        let contains = |item: &str| path.starts_with(&format!("{item}/"));
        self.findings.retain_mut(|f| {
            if f.items.is_empty() {
                return true;
            }
            for item in f.items.iter_mut().filter(|i| contains(&i.path)) {
                item.bytes = item.bytes.saturating_sub(bytes);
                f.bytes = f.bytes.saturating_sub(bytes);
            }
            let removed: u64 = f
                .items
                .iter()
                .filter(|i| inside(&i.path))
                .map(|i| i.bytes)
                .sum();
            f.items.retain(|i| !inside(&i.path));
            f.bytes = f.bytes.saturating_sub(removed);
            !f.items.is_empty()
        });
    }
}

fn refusal(f: &Finding) -> Modal {
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
    Modal::Message {
        title: f.title.clone(),
        body: format!("{why}\n\nWhat to do: {}", f.how),
    }
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

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).clamp(0, len as isize - 1) as usize
}

fn sort_tree(node: &mut Node, sort: SortBy) {
    node.children.sort_by(|a, b| {
        // "(smaller items)" always last.
        (a.kind == NodeKind::Rest)
            .cmp(&(b.kind == NodeKind::Rest))
            .then_with(|| match sort {
                SortBy::Size => b.allocated.cmp(&a.allocated),
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Oldest => a.newest_mtime.cmp(&b.newest_mtime),
            })
    });
    for c in &mut node.children {
        sort_tree(c, sort);
    }
}

#[cfg(test)]
pub(super) use test_support::sample_app;

#[cfg(test)]
mod test_support {
    use super::*;
    use diskwise_core::model::{ErrorSummary, SNAPSHOT_VERSION};
    use diskwise_core::rules::Category;
    use diskwise_core::suggest::Item;

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut n = Node::new(name.into(), NodeKind::Dir);
        n.allocated = children.iter().map(|c| c.allocated).sum();
        n.files = children.iter().map(|c| c.files).sum();
        n.dirs = children
            .iter()
            .map(|c| c.dirs + (c.kind == NodeKind::Dir) as u64)
            .sum();
        n.children = children;
        n
    }

    fn file(name: &str, size: u64) -> Node {
        let mut n = Node::new(name.into(), NodeKind::File);
        n.allocated = size;
        n.files = 1;
        n
    }

    fn finding(id: &str, safety: Safety, action: Action, paths: &[(&str, u64)]) -> Finding {
        Finding {
            id: id.into(),
            title: id.into(),
            category: Category::Cache,
            safety,
            action,
            why: String::new(),
            how: "do the thing".into(),
            prevent: None,
            bytes: paths.iter().map(|p| p.1).sum(),
            size_known: true,
            items: paths
                .iter()
                .map(|(p, b)| Item {
                    path: (*p).into(),
                    bytes: *b,
                    newest_mtime: 0,
                    is_dir: true,
                    source: None,
                })
                .collect(),
        }
    }

    /// A Linux-style snapshot rooted at `root` (a real temp dir, so trashing can be
    /// exercised with a fake trasher that deletes files).
    pub fn sample_app(root: &str) -> App {
        let tree = dir(
            root,
            vec![dir(
                "home",
                vec![dir(
                    "v",
                    vec![
                        dir("cache", vec![file("a", 300), file("b", 100)]),
                        dir("docs", vec![file("c", 200)]),
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
        let findings = vec![
            finding(
                "cache",
                Safety::Safe,
                Action::Trash,
                &[(&format!("{root}/home/v/cache"), 400)],
            ),
            finding(
                "docs",
                Safety::Review,
                Action::InApp,
                &[(&format!("{root}/home/v/docs"), 200)],
            ),
            finding("system", Safety::Review, Action::Info, &[]),
        ];
        App::new(snap, findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app_at(root: &str) -> App {
        sample_app(root)
    }

    #[test]
    fn opens_on_suggestions() {
        assert_eq!(app_at("/r").tab, Tab::Suggestions);
    }

    #[test]
    fn navigation_and_coverage() {
        let mut app = app_at("/r");
        app.tab = Tab::Browse;
        assert_eq!(app.current_path(), "/r");
        app.open(); // home
        app.open(); // v
        assert_eq!(app.current_path(), "/r/home/v");
        assert_eq!(app.selected_node().unwrap().name, "cache");
        assert!(app.coverage("/r/home/v/cache").unwrap().direct);
        assert!(!app.coverage("/r/home/v/cache/a").unwrap().direct);
        assert!(app.coverage("/r/home/v").is_none());
        // Safe suggestions count toward "cleanable inside" for every ancestor;
        // review ones don't.
        assert_eq!(app.cleanable_below("/r/home/v"), 400);
        assert_eq!(app.cleanable_below("/r"), 400);
        app.up();
        assert_eq!(app.current_path(), "/r/home");
    }

    #[test]
    fn suggestion_jumps_to_item() {
        let mut app = app_at("/r");
        app.suggestion = 1; // docs
        app.open();
        assert_eq!(app.tab, Tab::Browse);
        assert_eq!(app.current_path(), "/r/home/v");
        assert_eq!(app.selected_node().unwrap().name, "docs");
    }

    #[test]
    fn refuses_in_app_and_protected_items() {
        let mut app = app_at("/r");
        app.reveal("/r/home/v/docs");
        app.request_trash();
        assert!(
            matches!(app.modal.take(), Some(Modal::Message { .. })),
            "in-app item"
        );
        app.reveal("/r/home/v");
        app.request_trash();
        // /r/home/v is not under the Linux home base (/home), but depth 3 is fine;
        // the real home folder is protected:
        assert!(matches!(app.modal.take(), Some(Modal::ConfirmTrash { .. })));
        assert!(app.protected_reason("/home/v").is_some());
        assert!(app.protected_reason("/usr").is_some());
        assert!(app.protected_reason("/home/v/cache").is_none());
    }

    #[test]
    fn smaller_items_always_last() {
        let mut snap = sample_app("/r").snap;
        let mut rest = Node::new(String::new(), NodeKind::Rest);
        rest.allocated = 10_000;
        snap.tree.children.insert(0, rest);
        let app = App::new(snap, vec![]);
        assert_eq!(app.entries().last().unwrap().kind, NodeKind::Rest);
    }

    #[test]
    fn sort_keeps_position() {
        let mut app = app_at("/r");
        app.reveal("/r/home/v/docs");
        app.cycle_sort(); // name
        assert_eq!(app.current_path(), "/r/home/v");
        assert_eq!(app.selected_node().unwrap().name, "docs");
        assert_eq!(app.entries()[0].name, "cache");
    }

    #[test]
    fn trash_verifies_and_updates_tree() {
        let root = std::env::temp_dir().join(format!("diskwise-tui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/v/cache")).unwrap();
        std::fs::create_dir_all(root.join("home/v/docs")).unwrap();
        let root_s = root.display().to_string();
        let mut app = app_at(&root_s);
        let cache = format!("{root_s}/home/v/cache");
        let docs = format!("{root_s}/home/v/docs");
        app.reveal(&cache);

        // A trasher that "succeeds" without removing anything must not be trusted.
        let msg = app.apply_trash(
            &[TrashTarget {
                path: cache.clone(),
                bytes: 400,
            }],
            |_| Ok(()),
        );
        assert!(msg.contains("still there"), "{msg}");
        assert_eq!(app.snap.tree.allocated, 600);

        // A real removal updates totals, drops the emptied suggestion, keeps others.
        let msg = app.apply_trash(
            &[TrashTarget {
                path: cache.clone(),
                bytes: 400,
            }],
            |p| std::fs::remove_dir_all(p).map_err(|e| e.to_string()),
        );
        assert!(msg.starts_with("Moved 1 item"), "{msg}");
        assert_eq!(app.snap.tree.allocated, 200);
        assert_eq!(app.snap.tree.files, 1);
        assert!(app.locate(&cache).is_none());
        assert!(app.findings.iter().all(|f| f.id != "cache"));
        assert!(app.findings.iter().any(|f| f.id == "system"));
        assert_eq!(app.cleanable_below(&format!("{root_s}/home/v")), 0);
        assert_eq!(app.selected_node().unwrap().name, "docs");
        assert!(app.coverage(&docs).is_some());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn root_with_trailing_slash_still_locates() {
        let mut snap = sample_app("/r").snap;
        snap.root = "/r/".into();
        let mut app = App::new(snap, vec![]);
        assert_eq!(app.root_display, "/r");
        assert!(app.locate("/r/home/v/cache").is_some());
        // Path boundaries are respected: "/r" is not a prefix of "/rx".
        assert!(app.locate("/rx/home").is_none());
        app.reveal("/r/home/v/docs");
        assert_eq!(app.selected_node().unwrap().name, "docs");
    }

    #[test]
    fn trashing_inside_an_item_shrinks_that_item() {
        let root = std::env::temp_dir().join(format!("diskwise-inside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/v/cache")).unwrap();
        std::fs::write(root.join("home/v/cache/a"), b"x").unwrap();
        let r = root.display().to_string();
        let mut app = app_at(&r);
        let inner = format!("{r}/home/v/cache/a");
        app.apply_trash(
            &[TrashTarget {
                path: inner,
                bytes: 300,
            }],
            |p| std::fs::remove_file(p).map_err(|e| e.to_string()),
        );
        let cache = app.findings.iter().find(|f| f.id == "cache").unwrap();
        assert_eq!(cache.bytes, 100, "400 - 300 trashed from inside it");
        assert_eq!(cache.items[0].bytes, 100);
        assert_eq!(app.cleanable_below(&format!("{r}/home/v")), 100);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn trash_all_explains_protected_items() {
        let mut app = app_at("/r");
        // A finding whose only item is a protected top-level folder.
        app.findings = vec![finding_for_test("top", "/usr")];
        app.reindex();
        app.tab = Tab::Suggestions;
        app.request_trash();
        assert!(
            matches!(app.modal.take(), Some(Modal::Message { .. })),
            "no empty dialog"
        );
        // Mixed: the protected one is named in the warning.
        app.findings = vec![
            finding_for_test("mixed", "/usr"),
            finding_for_test("x", "/r/home/v/cache"),
        ];
        let extra = app.findings[1].items[0].clone();
        app.findings[0].items.push(extra);
        app.reindex();
        app.suggestion = app.findings.iter().position(|f| f.id == "mixed").unwrap();
        app.request_trash();
        match app.modal.take() {
            Some(Modal::ConfirmTrash { targets, warning }) => {
                assert_eq!(targets.len(), 1);
                assert!(warning.unwrap().contains("/usr"));
            }
            other => panic!("expected a confirmation, got {other:?}"),
        }
    }

    fn finding_for_test(id: &str, path: &str) -> Finding {
        let mut f = sample_app("/r").findings.remove(0);
        f.id = id.into();
        f.items[0].path = path.into();
        f
    }

    #[test]
    fn links_warn_and_follow() {
        let root = std::env::temp_dir().join(format!("diskwise-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home/v/docs")).unwrap();
        std::os::unix::fs::symlink(root.join("home/v/docs"), root.join("home/v/shortcut")).unwrap();
        // Canonical form, as the scanner records roots (macOS temp dirs are symlinks).
        let r = std::fs::canonicalize(&root).unwrap().display().to_string();
        let mut app = app_at(&r);
        let mut link = Node::new("shortcut".into(), NodeKind::Link);
        link.target = Some(format!("{r}/home/v/docs"));
        let v = app.locate(&format!("{r}/home/v")).unwrap();
        app.node_at_mut(&v).children.push(link);

        app.reveal(&format!("{r}/home/v/shortcut"));
        app.request_trash();
        match app.modal.take() {
            Some(Modal::ConfirmTrash { warning, .. }) => {
                assert!(warning.unwrap().contains("Only the link"))
            }
            other => panic!("expected a confirmation, got {other:?}"),
        }
        app.open();
        assert_eq!(app.selected_node().unwrap().name, "docs");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn real_path_undoes_macos_display_mapping() {
        let mut app = app_at("/System/Volumes/Data");
        app.snap.os = "macos".into();
        app.root_display = "/".into();
        assert_eq!(
            app.real_path("/.adobeTemp"),
            "/System/Volumes/Data/.adobeTemp"
        );
        assert_eq!(app.real_path("/Users/v"), "/System/Volumes/Data/Users/v");
    }

    #[test]
    fn ancestors_of_paths() {
        assert_eq!(
            ancestors("/a/b/c").collect::<Vec<_>>(),
            vec!["/a/b", "/a", "/"]
        );
        assert_eq!(ancestors("/a").collect::<Vec<_>>(), vec!["/"]);
    }
}
