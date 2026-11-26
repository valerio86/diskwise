//! Acting on a snapshot: finding nodes by path, refusing dangerous targets, moving
//! things to the Trash (and checking they're really gone), and updating the tree
//! afterwards. Shared by the TUI and the desktop app.

use std::path::Path;

use crate::model::{Node, NodeKind, Snapshot};
use crate::platform::Platform;

impl Snapshot {
    /// The scan root as users see it (macOS: `/` for the Data volume), without a
    /// trailing slash except for `/` itself.
    pub fn display_root(&self) -> String {
        let mut root = self.platform().display_path(&self.root);
        while root.len() > 1 && root.ends_with('/') {
            root.pop();
        }
        root
    }

    /// Display path of a node given its child-index path.
    pub fn path_of(&self, idx: &[usize]) -> String {
        let mut p = self.display_root();
        let mut node = &self.tree;
        for &i in idx {
            node = &node.children[i];
            p = join(&p, &node.name);
        }
        p
    }

    /// Child indices leading to `display` (a path at or under the root).
    pub fn locate(&self, display: &str) -> Option<Vec<usize>> {
        let root = self.display_root();
        let rest = display.strip_prefix(root.as_str())?;
        // "/home/v" must not match "/home/vx/…".
        if !(rest.is_empty() || rest.starts_with('/') || root.ends_with('/')) {
            return None;
        }
        let mut idx = Vec::new();
        let mut node = &self.tree;
        for name in rest.split('/').filter(|s| !s.is_empty()) {
            let i = node
                .children
                .iter()
                .position(|c| c.kind != NodeKind::Rest && c.name == name)?;
            idx.push(i);
            node = &node.children[i];
        }
        Some(idx)
    }

    pub fn node(&self, display: &str) -> Option<&Node> {
        let idx = self.locate(display)?;
        Some(idx.iter().fold(&self.tree, |n, &i| &n.children[i]))
    }

    /// The on-disk path for a display path. On macOS only some Data-volume folders
    /// are firmlinked at `/` (/Users is, /.adobeTemp isn't), so act on the path
    /// under the real scan root.
    pub fn real_path(&self, display: &str) -> String {
        let root = self.display_root();
        let rel = display
            .strip_prefix(root.as_str())
            .unwrap_or(display)
            .trim_start_matches('/');
        if rel.is_empty() {
            self.root.clone()
        } else {
            join(&self.root, rel)
        }
    }

    /// Remove a node and subtract its totals from every ancestor.
    pub fn remove(&mut self, display: &str) -> Option<Node> {
        let idx = self.locate(display)?;
        let (&last, parent) = idx.split_last()?;
        let mut node = &mut self.tree;
        for &i in parent {
            node = &mut node.children[i];
        }
        let removed = node.children.remove(last);
        // Walk the same path again, subtracting from each ancestor.
        let subtract = |n: &mut Node| {
            n.allocated = n.allocated.saturating_sub(removed.allocated);
            n.logical = n.logical.saturating_sub(removed.logical);
            n.files = n.files.saturating_sub(removed.files);
            if removed.kind == NodeKind::Dir {
                n.dirs = n.dirs.saturating_sub(1 + removed.dirs);
            }
        };
        let mut node = &mut self.tree;
        subtract(node);
        for &i in parent {
            node = &mut node.children[i];
            subtract(node);
        }
        Some(removed)
    }
}

/// Why diskwise refuses to move `path` to the Trash at all, if it does.
pub fn protected_reason(path: &str, platform: &Platform) -> Option<String> {
    let depth = path.split('/').filter(|s| !s.is_empty()).count();
    let parent = Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    if depth <= 1 {
        Some(format!(
            "{path} is a top-level system folder. diskwise won't move it to the Trash."
        ))
    } else if parent == platform.home_base {
        Some(format!(
            "{path} is a home folder. diskwise won't move it to the Trash."
        ))
    } else {
        None
    }
}

/// Move one path to the system Trash, then check it's really gone from where it
/// was. On macOS this uses NSFileManager rather than asking Finder: no extra
/// permission prompt, and it works over SSH (but Finder's "Put Back" isn't offered).
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    #[allow(unused_mut)] // only mutated on macOS
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx.delete(path).map_err(|e| e.to_string())?;
    verify_gone(path)
}

/// A trash call that "succeeds" but leaves the item in place must not be trusted.
pub fn verify_gone(path: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(path).is_ok() {
        Err("still there after moving to Trash".into())
    } else {
        Ok(())
    }
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
    use crate::model::{ErrorSummary, SNAPSHOT_VERSION};
    use crate::platform::{LINUX, MACOS};

    fn n(name: &str, kind: NodeKind, size: u64, children: Vec<Node>) -> Node {
        let mut node = Node::new(name.into(), kind);
        node.allocated = size + children.iter().map(|c| c.allocated).sum::<u64>();
        node.files = if kind == NodeKind::File {
            1
        } else {
            children.iter().map(|c| c.files).sum()
        };
        node.dirs = children
            .iter()
            .map(|c| c.dirs + (c.kind == NodeKind::Dir) as u64)
            .sum();
        node.children = children;
        node
    }

    fn snap(root: &str, os: &str) -> Snapshot {
        let tree = n(
            root,
            NodeKind::Dir,
            0,
            vec![n(
                "a",
                NodeKind::Dir,
                0,
                vec![
                    n(
                        "b",
                        NodeKind::Dir,
                        0,
                        vec![n("f", NodeKind::File, 100, vec![])],
                    ),
                    n("g", NodeKind::File, 50, vec![]),
                ],
            )],
        );
        Snapshot {
            version: SNAPSHOT_VERSION,
            os: os.into(),
            root: root.into(),
            scanned_at: 0,
            duration_ms: 0,
            min_node_size: 0,
            tree,
            errors: ErrorSummary::default(),
            skipped_mounts: vec![],
            system: None,
        }
    }

    #[test]
    fn locate_and_paths() {
        let s = snap("/r/", "linux");
        assert_eq!(s.display_root(), "/r");
        assert_eq!(s.locate("/r/a/b"), Some(vec![0, 0]));
        assert_eq!(s.path_of(&[0, 0]), "/r/a/b");
        assert!(s.locate("/rx/a").is_none());
        assert_eq!(s.node("/r/a/g").unwrap().allocated, 50);
    }

    #[test]
    fn remove_updates_ancestors() {
        let mut s = snap("/r", "linux");
        let removed = s.remove("/r/a/b").unwrap();
        assert_eq!(removed.allocated, 100);
        assert_eq!(s.tree.allocated, 50);
        assert_eq!(s.node("/r/a").unwrap().allocated, 50);
        assert_eq!(s.tree.files, 1);
        assert_eq!(s.tree.dirs, 1);
        assert!(s.node("/r/a/b").is_none());
    }

    #[test]
    fn real_path_on_macos_data_volume() {
        let s = snap("/System/Volumes/Data", "macos");
        assert_eq!(s.display_root(), "/");
        assert_eq!(
            s.real_path("/.adobeTemp"),
            "/System/Volumes/Data/.adobeTemp"
        );
        assert_eq!(s.real_path("/Users/v"), "/System/Volumes/Data/Users/v");
        assert_eq!(s.locate("/a/b"), Some(vec![0, 0]));
    }

    #[test]
    fn protection() {
        assert!(protected_reason("/usr", &LINUX).is_some());
        assert!(protected_reason("/home/v", &LINUX).is_some());
        assert!(protected_reason("/Users/v", &MACOS).is_some());
        assert!(protected_reason("/Users/v/Library/Caches/x", &MACOS).is_none());
    }

    #[test]
    fn verify_gone_catches_survivors() {
        let f = std::env::temp_dir().join(format!("diskwise-gone-{}", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        assert!(verify_gone(&f).is_err());
        std::fs::remove_file(&f).unwrap();
        assert!(verify_gone(&f).is_ok());
    }
}
