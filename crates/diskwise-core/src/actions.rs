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

/// Folders that belong to the operating system or a package manager. They, and
/// everything inside them, are never moved to the Trash: clearing anything there
/// goes through the owning tool (suggestions say how).
const MACOS_SYSTEM: &[&str] = &[
    "/System",
    "/Library",
    "/private",
    "/usr",
    "/bin",
    "/sbin",
    "/cores",
    "/opt/homebrew",
];
const LINUX_SYSTEM: &[&str] = &[
    "/usr",
    "/etc",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/boot",
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/snap",
    "/var/lib",
    "/var/db",
    "/var/spool",
];

/// Which paths diskwise refuses to move to the Trash, however they're selected.
#[derive(Debug, Clone)]
pub struct Protection {
    /// Folders that contain home folders: /Users, /home, and where those really
    /// are (Fedora Silverblue: /home → /var/home).
    home_bases: Vec<String>,
    /// Home folders themselves, wherever they live ($HOME, the user database).
    homes: Vec<String>,
    /// $HOME (and where it really is): a person's home even outside `home_bases`.
    own_home: Vec<String>,
    system: &'static [&'static str],
}

impl Protection {
    /// Protection for `platform`. On this machine's own platform it also reads
    /// $HOME and /etc/passwd, so relocated home folders are protected too.
    pub fn for_platform(platform: &Platform) -> Self {
        let mut p = Self::new(platform, Vec::new());
        if platform == crate::platform::CURRENT {
            if let Some(base) = canonical(platform.home_base) {
                p.home_bases.push(base);
            }
            if let Ok(h) = std::env::var("HOME")
                && h.len() > 1
            {
                p.own_home.push(h.trim_end_matches('/').to_string());
                p.own_home.extend(canonical(&h));
            }
            let mut homes: Vec<String> = std::env::var("HOME").ok().into_iter().collect();
            if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
                homes.extend(passwd_homes(&passwd));
            }
            for h in homes {
                p.homes.push(h.trim_end_matches('/').to_string());
                p.homes.extend(canonical(&h));
            }
            p.homes.sort();
            p.homes.dedup();
            p.home_bases.dedup();
        }
        p
    }

    /// Protection from explicit facts (tests, or another machine's snapshot).
    pub fn new(platform: &Platform, homes: Vec<String>) -> Self {
        Protection {
            home_bases: vec![platform.home_base.to_string()],
            homes,
            own_home: Vec::new(),
            system: if platform.os == "macos" {
                MACOS_SYSTEM
            } else {
                LINUX_SYSTEM
            },
        }
    }

    /// Why `path` (a display path) must never be moved to the Trash, if it mustn't.
    pub fn reason(&self, path: &str) -> Option<String> {
        let path = path.trim_end_matches('/');
        let depth = path.split('/').filter(|s| !s.is_empty()).count();
        let parent = Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        if depth <= 1 {
            Some(format!(
                "{path} is a top-level system folder. diskwise won't move it to the Trash."
            ))
        } else if self.homes.iter().any(|h| h == path)
            || self.home_bases.iter().any(|b| b == parent)
        {
            Some(format!(
                "{path} is a home folder. diskwise won't move it to the Trash."
            ))
        } else {
            self.system.iter().find(|s| path == **s || inside(path, s)).map(|sys| {
                format!(
                    "{path} is part of the operating system or a package manager ({sys}). diskwise won't move it to the Trash: removing files there can break your system."
                )
            })
        }
    }

    /// Whether `path` is a home folder itself.
    pub fn is_home(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        let parent = Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        self.homes.iter().any(|h| h == path) || self.home_bases.iter().any(|b| b == parent)
    }

    /// Whether `path` is a person's home folder: one under the home base
    /// (/Users/x, /home/x) or $HOME. Unlike `is_home`, not service accounts'
    /// homes from the user database (/var/lib/postgresql, /var/db/…).
    pub fn is_user_home(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        let parent = Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        self.own_home.iter().any(|h| h == path) || self.home_bases.iter().any(|b| b == parent)
    }

    /// Whether `path` is, or contains, the folder home folders live in.
    pub fn contains_homes(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        let holds = |p: &String| p == path || path.is_empty() || inside(p, path);
        self.home_bases.iter().any(holds)
    }

    /// Whether `path` is inside someone's home folder (where users keep their own files).
    pub fn in_home(&self, path: &str) -> bool {
        self.homes.iter().any(|h| inside(path, h))
            || self
                .home_bases
                .iter()
                .any(|b| inside(path, b) && path[b.len()..].trim_start_matches('/').contains('/'))
    }
}

fn inside(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn canonical(path: &str) -> Option<String> {
    let c = std::fs::canonicalize(path).ok()?.display().to_string();
    (c != path.trim_end_matches('/')).then_some(c)
}

/// Home directories from /etc/passwd (field 6), skipping "/" and empty ones.
fn passwd_homes(passwd: &str) -> Vec<String> {
    passwd
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split(':').nth(5))
        .filter(|h| h.len() > 1)
        .map(String::from)
        .collect()
}

/// Every reason not to move `display` to the Trash right now, checked in one
/// place for the TUI and the desktop app alike: the item must be in the scan
/// (and not the scan root itself), not protected, named exactly, and still
/// there on disk looking like what was scanned.
pub fn check_trash(snap: &Snapshot, display: &str, protection: &Protection) -> Result<(), String> {
    let Some(idx) = snap.locate(display) else {
        return Err(format!("{display} isn't in the scan."));
    };
    if idx.is_empty() {
        return Err(format!(
            "{display} is the folder that was scanned. diskwise only moves things inside it."
        ));
    }
    if let Some(reason) = protection.reason(display) {
        return Err(reason);
    }
    let mut node = &snap.tree;
    for &i in &idx {
        node = &node.children[i];
        if node.lossy_name {
            return Err(format!(
                "\"{}\" has characters in its name that diskwise can't represent exactly, so it can't act on it safely. Use Finder or a terminal instead.",
                node.name
            ));
        }
    }
    if snap.platform() != crate::platform::CURRENT {
        return Err("This scan came from another operating system, so its paths don't refer to this machine.".into());
    }
    let real = snap.real_path(display);
    let meta = std::fs::symlink_metadata(&real).map_err(|_| {
        format!("{display} isn't there any more. Scan again to see the current state.")
    })?;
    let kind_now = if meta.file_type().is_symlink() {
        NodeKind::Link
    } else if meta.is_dir() {
        NodeKind::Dir
    } else {
        NodeKind::File
    };
    let changed =
        kind_now != node.kind || (node.kind == NodeKind::File && meta.len() != node.logical);
    if changed {
        return Err(format!(
            "{display} has changed since the scan. Scan again before moving it to the Trash."
        ));
    }
    Ok(())
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
            large_files: vec![],
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
        let mac = Protection::new(&MACOS, vec![]);
        assert!(mac.reason("/Users/v").is_some(), "home folder");
        assert!(mac.reason("/Users/v/Library/Caches/x").is_none());
        // System and package-manager trees, at any depth.
        assert!(mac.reason("/opt/homebrew").is_some());
        assert!(mac.reason("/opt/homebrew/Cellar/x").is_some());
        assert!(mac.reason("/private/var").is_some());
        assert!(mac.reason("/Applications/Some.app").is_none());
        let linux = Protection::new(&LINUX, vec![]);
        assert!(linux.reason("/usr").is_some() && linux.reason("/usr/lib/x").is_some());
        assert!(linux.reason("/var/lib/docker").is_some());
        assert!(linux.reason("/home/v").is_some());
        assert!(linux.reason("/home/v/proj").is_none());
        assert!(linux.reason("/srv/share/old").is_none());
    }

    #[test]
    fn relocated_homes_are_protected() {
        // Fedora Silverblue: /home is a link to /var/home, so scanned paths are /var/home/….
        let mut p = Protection::new(&LINUX, vec!["/var/home/v".into()]);
        p.home_bases.push("/var/home".into());
        assert!(p.reason("/var/home/v").is_some());
        assert!(
            p.reason("/var/home/other").is_some(),
            "any folder directly in a home base"
        );
        assert!(p.reason("/var/home/v/proj").is_none());
        assert!(p.in_home("/var/home/v/proj"));
        assert!(!p.in_home("/srv/share"));
        assert_eq!(
            passwd_homes(
                "root:x:0:0::/root:/bin/sh\nv:x:1000:1000::/var/home/v:/bin/bash\nnobody:x:65534:65534::/:/usr/sbin/nologin\n"
            ),
            vec!["/root", "/var/home/v"]
        );
    }

    #[test]
    fn check_trash_refusals() {
        // Not the system temp dir: on macOS that's under /private, which is protected.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/g"), vec![0u8; 50]).unwrap();
        let r = std::fs::canonicalize(&root).unwrap().display().to_string();
        let mut s = snap(&r, crate::platform::CURRENT.os);
        s.tree.children[0].children[1].logical = 50; // a/g as scanned
        let p = Protection::new(crate::platform::CURRENT, vec![]);

        assert!(
            check_trash(&s, &r, &p).unwrap_err().contains("was scanned"),
            "the root"
        );
        assert!(check_trash(&s, &format!("{r}/a/b"), &p).is_ok());
        assert!(check_trash(&s, &format!("{r}/a/g"), &p).is_ok());
        // Changed since the scan: a file of a different size.
        std::fs::write(root.join("a/g"), vec![0u8; 80]).unwrap();
        assert!(
            check_trash(&s, &format!("{r}/a/g"), &p)
                .unwrap_err()
                .contains("changed")
        );
        // Gone since the scan.
        std::fs::remove_dir(root.join("a/b")).unwrap();
        assert!(
            check_trash(&s, &format!("{r}/a/b"), &p)
                .unwrap_err()
                .contains("isn't there")
        );
        // A name stored lossily (not valid UTF-8 on disk).
        s.tree.children[0].children[0].lossy_name = true;
        assert!(
            check_trash(&s, &format!("{r}/a/b"), &p)
                .unwrap_err()
                .contains("characters")
        );
        // A snapshot from the other OS.
        s.os = if s.os == "macos" {
            "linux".into()
        } else {
            "macos".into()
        };
        assert!(check_trash(&s, &format!("{r}/a/g"), &p).is_err());
        std::fs::remove_dir_all(&root).unwrap();
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
