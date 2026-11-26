use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

use crate::model::{ErrorSummary, Node, NodeKind};

const MAX_ERROR_SAMPLES: usize = 50;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Children smaller than this are folded into a single "rest" node.
    pub min_node_size: u64,
    /// Don't descend into other mounted filesystems.
    pub one_filesystem: bool,
    /// Live counters for a progress display (updated while scanning).
    pub progress: Option<Arc<ScanProgress>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            min_node_size: 10 * 1024 * 1024,
            one_filesystem: true,
            progress: None,
        }
    }
}

/// Counters a UI can read from another thread while a scan runs.
#[derive(Debug, Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
}

struct Ctx {
    opts: ScanOptions,
    root_dev: u64,
    /// (dev, ino) of multiply-linked files already counted.
    seen_links: Mutex<HashSet<(u64, u64)>>,
    error_count: AtomicU64,
    error_samples: Mutex<Vec<String>>,
    skipped_mounts: Mutex<Vec<String>>,
}

/// What one scan found.
pub struct ScanOutput {
    pub tree: Node,
    pub errors: ErrorSummary,
    /// Mount points of other filesystems that were not entered.
    pub skipped_mounts: Vec<String>,
}

impl Ctx {
    fn record_error(&self, path: &Path, err: &std::io::Error) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        let mut samples = self.error_samples.lock().unwrap();
        if samples.len() < MAX_ERROR_SAMPLES {
            samples.push(format!("{}: {}", path.display(), err));
        }
    }

    /// Allocated bytes for a non-directory entry, counting hard links once.
    fn allocated_once(&self, meta: &Metadata) -> u64 {
        let bytes = meta.blocks() * 512;
        if meta.nlink() > 1
            && !self
                .seen_links
                .lock()
                .unwrap()
                .insert((meta.dev(), meta.ino()))
        {
            return 0;
        }
        bytes
    }
}

/// Scan `root` into a size tree, noting unreadable paths and skipped mount points.
pub fn scan(root: &Path, opts: ScanOptions) -> std::io::Result<ScanOutput> {
    // Follow the root if it's a link: `read_dir` does, so the device to stay on is
    // the target's, not the link's.
    let meta = fs::metadata(root)?;
    let ctx = Ctx {
        opts,
        root_dev: meta.dev(),
        seen_links: Mutex::new(HashSet::new()),
        error_count: AtomicU64::new(0),
        error_samples: Mutex::new(Vec::new()),
        skipped_mounts: Mutex::new(Vec::new()),
    };
    let tree = scan_dir(root, root.display().to_string(), &meta, &ctx);
    let errors = ErrorSummary {
        count: ctx.error_count.load(Ordering::Relaxed),
        samples: ctx.error_samples.into_inner().unwrap(),
    };
    let mut skipped_mounts = ctx.skipped_mounts.into_inner().unwrap();
    skipped_mounts.sort();
    Ok(ScanOutput {
        tree,
        errors,
        skipped_mounts,
    })
}

fn scan_dir(path: &Path, name: String, meta: &Metadata, ctx: &Ctx) -> Node {
    let min = ctx.opts.min_node_size;
    let mut node = Node::new(name, NodeKind::Dir);
    node.allocated = meta.blocks() * 512;
    node.newest_mtime = meta.mtime();

    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            ctx.record_error(path, &e);
            node.incomplete = true;
            return node;
        }
    };

    let mut rest = Node::new(String::new(), NodeKind::Rest);
    let mut subdirs = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                ctx.record_error(path, &e);
                node.incomplete = true;
                continue;
            }
        };
        let child_path = entry.path();
        let child_meta = match fs::symlink_metadata(&child_path) {
            Ok(m) => m,
            Err(e) => {
                ctx.record_error(&child_path, &e);
                node.incomplete = true;
                continue;
            }
        };
        let child_name = entry.file_name().to_string_lossy().into_owned();

        if child_meta.is_dir() {
            if ctx.opts.one_filesystem && child_meta.dev() != ctx.root_dev {
                // Mount point of another filesystem: note it rather than skip
                // silently, unless it holds no storage (automount placeholders such
                // as macOS's /home, or virtual filesystems like /proc).
                let holds_data = crate::system::fs_usage(&child_path).is_ok_and(|u| u.capacity > 0);
                if holds_data {
                    ctx.skipped_mounts
                        .lock()
                        .unwrap()
                        .push(child_path.display().to_string());
                }
                continue;
            }
            subdirs.push((child_path, child_name, child_meta));
            continue;
        }

        // Symlinks are never followed. Links to folders are always listed, so it's
        // visible that their contents are counted somewhere else.
        if child_meta.file_type().is_symlink() {
            let mut link = Node::new(child_name, NodeKind::Link);
            link.allocated = child_meta.blocks() * 512;
            link.files = 1;
            link.newest_mtime = child_meta.mtime();
            link.target = fs::read_link(&child_path)
                .ok()
                .map(|t| t.display().to_string());
            let to_dir = fs::metadata(&child_path).is_ok_and(|m| m.is_dir());
            place(&mut node, &mut rest, link, if to_dir { 0 } else { min });
            continue;
        }

        // Regular files, sockets, etc.
        let mut file = Node::new(child_name, NodeKind::File);
        file.allocated = ctx.allocated_once(&child_meta);
        file.logical = if child_meta.is_file() {
            child_meta.len()
        } else {
            0
        };
        file.files = 1;
        if let Some(p) = &ctx.opts.progress {
            p.files.fetch_add(1, Ordering::Relaxed);
            p.bytes.fetch_add(file.allocated, Ordering::Relaxed);
        }
        file.newest_mtime = child_meta.mtime();
        place(&mut node, &mut rest, file, min);
    }

    let children: Vec<Node> = subdirs
        .into_par_iter()
        .map(|(p, n, m)| scan_dir(&p, n, &m, ctx))
        .collect();
    for child in children {
        node.dirs += 1 + child.dirs;
        if let Some(p) = &ctx.opts.progress {
            p.dirs.fetch_add(1, Ordering::Relaxed);
        }
        node.incomplete |= child.incomplete;
        place(&mut node, &mut rest, child, min);
    }

    // Only list the leftovers alongside bigger siblings; otherwise the directory total says it all.
    if rest.allocated > 0 && !node.children.is_empty() {
        rest.name = "(smaller items)".to_string();
        node.children.push(rest);
    }
    node.children
        .sort_by_key(|c| std::cmp::Reverse(c.allocated));
    node
}

/// Add `child`'s totals to `parent`, keeping it as a listed child only if it is big enough.
fn place(parent: &mut Node, rest: &mut Node, child: Node, min: u64) {
    parent.allocated += child.allocated;
    parent.logical += child.logical;
    parent.files += child.files;
    parent.newest_mtime = parent.newest_mtime.max(child.newest_mtime);
    if child.allocated >= min {
        parent.children.push(child);
    } else {
        rest.allocated += child.allocated;
        rest.logical += child.logical;
        rest.files += child.files;
        if child.kind == NodeKind::Dir {
            rest.dirs += 1 + child.dirs;
        }
        rest.newest_mtime = rest.newest_mtime.max(child.newest_mtime);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("diskwise-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, bytes: usize) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![1u8; bytes]).unwrap();
    }

    #[test]
    fn totals_and_folding() {
        let dir = tmpdir("totals");
        fs::create_dir(dir.join("big")).unwrap();
        write(&dir.join("big/a.bin"), 2 * 1024 * 1024);
        write(&dir.join("small.txt"), 100);

        let opts = ScanOptions {
            min_node_size: 1024 * 1024,
            ..Default::default()
        };
        let ScanOutput { tree, errors, .. } = scan(&dir, opts).unwrap();

        assert_eq!(errors.count, 0);
        assert_eq!(tree.files, 2);
        assert_eq!(tree.dirs, 1);
        assert!(tree.logical == 2 * 1024 * 1024 + 100);
        assert_eq!(tree.children[0].name, "big");
        assert_eq!(tree.children[0].children[0].name, "a.bin");
        let rest = tree
            .children
            .iter()
            .find(|c| c.kind == NodeKind::Rest)
            .unwrap();
        assert_eq!(rest.files, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn links_to_folders_are_listed_and_not_followed() {
        let dir = tmpdir("symlinks");
        fs::create_dir(dir.join("real")).unwrap();
        write(&dir.join("real/big.bin"), 2 * 1024 * 1024);
        std::os::unix::fs::symlink(dir.join("real"), dir.join("shortcut")).unwrap();
        std::os::unix::fs::symlink("big.bin", dir.join("real/file-link")).unwrap();

        let opts = ScanOptions {
            min_node_size: 1024 * 1024,
            ..Default::default()
        };
        let tree = scan(&dir, opts).unwrap().tree;
        let link = tree
            .children
            .iter()
            .find(|c| c.name == "shortcut")
            .expect("dir link listed");
        assert_eq!(link.kind, NodeKind::Link);
        assert_eq!(
            link.target.as_deref(),
            Some(dir.join("real").to_str().unwrap())
        );
        assert!(link.children.is_empty(), "not followed");
        // Counted once (via real/), not again through the link.
        assert!(tree.allocated < 3 * 1024 * 1024);
        // A tiny link to a file is folded like any small file.
        let real = tree.children.iter().find(|c| c.name == "real").unwrap();
        assert!(real.children.iter().all(|c| c.name != "file-link"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn symlinked_root_is_followed() {
        let dir = tmpdir("linkroot");
        fs::create_dir_all(dir.join("target/sub")).unwrap();
        write(&dir.join("target/sub/f"), 2 * 1024 * 1024);
        std::os::unix::fs::symlink(dir.join("target"), dir.join("root-link")).unwrap();
        let out = scan(
            &dir.join("root-link"),
            ScanOptions {
                min_node_size: 0,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(out.skipped_mounts.is_empty());
        assert_eq!(out.tree.children[0].name, "sub");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hard_links_counted_once() {
        let dir = tmpdir("links");
        write(&dir.join("one"), 1024 * 1024);
        fs::hard_link(dir.join("one"), dir.join("two")).unwrap();

        let tree = scan(&dir, ScanOptions::default()).unwrap().tree;
        let file_alloc: u64 = fs::symlink_metadata(dir.join("one")).unwrap().blocks() * 512;
        let dir_alloc: u64 = fs::symlink_metadata(&dir).unwrap().blocks() * 512;
        assert_eq!(tree.allocated, file_alloc + dir_alloc);
        fs::remove_dir_all(&dir).unwrap();
    }
}
