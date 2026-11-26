//! Duplicate folders: report a copied folder once, at its top, instead of as
//! hundreds of duplicate files inside it.
//!
//! Candidates come from duplicate files: for two identical files, step up both
//! paths together while the names below still match (`A/Comms/x.pdf` and
//! `B/Comms/x.pdf` suggest `A/Comms` ~ `B/Comms`, then `A` ~ `B`). The scan's
//! totals rule most pairs out cheaply; the rest are checked exactly by walking
//! both folders and comparing every entry (names, kinds, sizes, contents).
//! Finder's own clutter (`.DS_Store` and the like) is ignored.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Copy, Live, Prefer, full_hash, inside, private_size, rank};
use crate::actions::Protection;
use crate::model::{NodeKind, Snapshot};

/// Files that differ between copies of a folder without meaning anything.
const IGNORED: &[&str] = &[
    ".DS_Store",
    ".localized",
    "Icon\r",
    "Thumbs.db",
    "desktop.ini",
];

/// Don't walk folders with more entries than this (a huge tree that only
/// *looks* duplicated isn't worth minutes of hashing).
const MAX_ENTRIES: usize = 200_000;

/// Totals from the scan may differ by Finder clutter; allow this much.
const TOTALS_SLACK: f64 = 0.01;

/// Folders with identical contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderGroup {
    /// Logical size of one copy.
    pub size: u64,
    /// Files in one copy.
    pub files: u64,
    /// The suggested folder to keep is first.
    pub copies: Vec<Copy>,
    /// Space freed by deleting every copy but the first.
    pub reclaimable: u64,
}

/// What a walk of one folder found, keyed by path relative to it.
enum Entry {
    Dir,
    File { size: u64, real: PathBuf },
    Link(PathBuf),
}

/// Totals of one verified copy.
struct Summary {
    files: u64,
    size: u64,
    allocated: u64,
    frees: u64,
}

pub(super) fn find(
    snap: &Snapshot,
    by_hash: &HashMap<String, Vec<Live>>,
    prefer: Prefer,
    protection: &Protection,
) -> Vec<FolderGroup> {
    let root = snap.display_root();
    let known: HashMap<&str, &str> = by_hash
        .iter()
        .flat_map(|(h, ls)| ls.iter().map(move |l| (l.file.path.as_str(), h.as_str())))
        .collect();

    let mut candidates: HashSet<(String, String)> = HashSet::new();
    for lives in by_hash.values().filter(|ls| ls.len() > 1) {
        for (i, a) in lives.iter().enumerate() {
            for b in &lives[i + 1..] {
                lockstep(
                    &a.file.path,
                    &b.file.path,
                    &root,
                    protection,
                    &mut candidates,
                );
            }
        }
    }

    // Shallowest first, so a copied folder is verified before its subfolders
    // (which then needn't be).
    let mut candidates: Vec<(String, String)> = candidates.into_iter().collect();
    candidates.sort_by_key(|(a, b)| (depth(a).min(depth(b)), a.clone(), b.clone()));

    let mut verified: HashSet<(String, String)> = HashSet::new();
    let mut summaries: HashMap<String, Summary> = HashMap::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (a, b) in candidates {
        if let (Some((pa, na)), Some((pb, nb))) = (a.rsplit_once('/'), b.rsplit_once('/'))
            && na == nb
            && verified.contains(&ordered(pa, pb))
        {
            // Inside two folders already known to be identical.
            verified.insert((a, b));
            continue;
        }
        if !totals_match(snap, &a, &b) {
            continue;
        }
        if let Some((sa, sb)) = verify(snap, &a, &b, &known) {
            summaries.insert(a.clone(), sa);
            summaries.insert(b.clone(), sb);
            verified.insert((a.clone(), b.clone()));
            pairs.push((a, b));
        }
    }

    // Copies of one folder: connected pairs (identity is transitive).
    let mut groups: Vec<Vec<String>> = Vec::new();
    for (a, b) in pairs {
        let ia = groups.iter().position(|g| g.contains(&a));
        let ib = groups.iter().position(|g| g.contains(&b));
        match (ia, ib) {
            (Some(i), Some(j)) if i != j => {
                let merged = groups.remove(i.max(j));
                groups[i.min(j)].extend(merged);
            }
            (Some(_), Some(_)) => {}
            (Some(i), None) => groups[i].push(b),
            (None, Some(j)) => groups[j].push(a),
            (None, None) => groups.push(vec![a, b]),
        }
    }
    groups.sort_by_key(|g| g.iter().map(|p| depth(p)).min());

    let home_base = snap.platform().home_base;
    let mut extras: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for members in groups {
        // A folder inside an extra copy found earlier is already covered.
        let members: Vec<String> = members
            .into_iter()
            .filter(|m| !extras.iter().any(|e| m == e || inside(m, e)))
            .collect();
        if members.len() < 2 {
            continue;
        }
        let entries: Vec<(Copy, u64, bool)> = members
            .iter()
            .filter_map(|m| {
                let s = summaries.get(m)?;
                let real = PathBuf::from(snap.real_path(m));
                let meta = std::fs::symlink_metadata(&real).ok()?;
                let copy = Copy {
                    path: m.clone(),
                    allocated: s.allocated,
                    frees: s.frees,
                    mtime: meta.mtime(),
                    created: crate::scan::birth_time(&meta),
                    shared: None,
                    action: None,
                };
                Some((copy, meta.dev(), crate::platform::can_clone(&real)))
            })
            .collect();
        if entries.len() < 2 {
            continue;
        }
        let first = &summaries[&entries[0].0.path];
        let (size, files) = (first.size, first.files);
        // A subfolder of a folder that is being kept is the one to keep here too.
        let keep_first = |p: &str| kept.iter().any(|k| inside(p, k));
        let copies = rank(entries, prefer, home_base, &keep_first);
        let reclaimable: u64 = copies[1..].iter().map(|c| c.frees).sum();
        if reclaimable == 0 {
            continue;
        }
        extras.extend(copies[1..].iter().map(|c| c.path.clone()));
        kept.push(copies[0].path.clone());
        out.push(FolderGroup {
            size,
            files,
            copies,
            reclaimable,
        });
    }
    out.sort_by(|a, b| {
        b.reclaimable
            .cmp(&a.reclaimable)
            .then(a.copies[0].path.cmp(&b.copies[0].path))
    });
    out
}

/// Step up from two identical files, recording each pair of folders whose
/// contents below match by name so far.
fn lockstep(
    a: &str,
    b: &str,
    root: &str,
    protection: &Protection,
    out: &mut HashSet<(String, String)>,
) {
    let (mut a, mut b) = (a, b);
    loop {
        let (Some((da, na)), Some((db, nb))) = (a.rsplit_once('/'), b.rsplit_once('/')) else {
            return;
        };
        if na != nb || da == db || da.is_empty() || db.is_empty() {
            return;
        }
        // One inside the other, outside the scan, or a folder diskwise never acts on.
        if inside(da, db) || inside(db, da) || !inside(da, root) || !inside(db, root) {
            return;
        }
        if protection.reason(da).is_some() || protection.reason(db).is_some() {
            return;
        }
        out.insert(ordered(da, db));
        a = da;
        b = db;
    }
}

fn ordered(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn depth(p: &str) -> usize {
    p.matches('/').count()
}

/// The scan's totals for both folders are close enough to be worth a walk.
/// Folders folded into "(smaller items)" aren't in the tree: too small to report.
fn totals_match(snap: &Snapshot, a: &str, b: &str) -> bool {
    let (Some(na), Some(nb)) = (snap.node(a), snap.node(b)) else {
        return false;
    };
    if na.kind != NodeKind::Dir || nb.kind != NodeKind::Dir || na.incomplete || nb.incomplete {
        return false;
    }
    let close = |x: u64, y: u64| {
        let (lo, hi) = (x.min(y) as f64, x.max(y) as f64);
        hi - lo <= (hi * TOTALS_SLACK).max(64.0 * 1024.0)
    };
    // File counts may differ by a few .DS_Store files.
    let (lo, hi) = (na.files.min(nb.files), na.files.max(nb.files));
    close(na.logical, nb.logical) && hi - lo <= (hi / 100).max(4)
}

/// Compare two folders entry by entry; the totals of each if identical.
fn verify(
    snap: &Snapshot,
    a: &str,
    b: &str,
    known: &HashMap<&str, &str>,
) -> Option<(Summary, Summary)> {
    let ra = PathBuf::from(snap.real_path(a));
    let rb = PathBuf::from(snap.real_path(b));
    let ea = walk(&ra)?;
    let eb = walk(&rb)?;
    if ea.len() != eb.len() {
        return None;
    }
    let mut sa = Summary {
        files: 0,
        size: 0,
        allocated: 0,
        frees: 0,
    };
    let mut sb = Summary {
        files: 0,
        size: 0,
        allocated: 0,
        frees: 0,
    };
    for ((rel_a, x), (rel_b, y)) in ea.iter().zip(eb.iter()) {
        if rel_a != rel_b {
            return None;
        }
        match (x, y) {
            (Entry::Dir, Entry::Dir) => {}
            (Entry::Link(t), Entry::Link(u)) if t == u => {}
            (Entry::File { size: s, real: pa }, Entry::File { size: t, real: pb }) if s == t => {
                let hash = |display: String, real: &Path| -> Option<String> {
                    match known.get(display.as_str()) {
                        Some(h) => Some(h.to_string()),
                        None => full_hash(real).ok(),
                    }
                };
                if hash(format!("{a}/{rel_a}"), pa)? != hash(format!("{b}/{rel_b}"), pb)? {
                    return None;
                }
                add(&mut sa, pa, *s)?;
                add(&mut sb, pb, *t)?;
            }
            _ => return None,
        }
    }
    (sa.files > 0).then_some((sa, sb))
}

/// Count one file of a verified copy: what deleting it would free (hard links
/// and clones free nothing, as for single files).
fn add(s: &mut Summary, real: &Path, size: u64) -> Option<()> {
    let meta = std::fs::symlink_metadata(real).ok()?;
    let allocated = meta.blocks() * 512;
    let frees = if meta.nlink() > 1 {
        0
    } else {
        private_size(real).map_or(allocated, |p| p.min(allocated))
    };
    s.files += 1;
    s.size += size;
    s.allocated += allocated;
    s.frees += frees;
    Some(())
}

/// Every entry under `root`, by relative path. `None` if any can't be read or
/// named exactly (then the folders can't be compared or acted on safely).
fn walk(root: &Path) -> Option<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            if IGNORED.contains(&name.as_str()) || name.starts_with("._") {
                continue;
            }
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let e = if meta.file_type().is_symlink() {
                Entry::Link(std::fs::read_link(&path).ok()?)
            } else if meta.is_dir() {
                stack.push((path, child.clone()));
                Entry::Dir
            } else if meta.is_file() {
                Entry::File {
                    size: meta.len(),
                    real: path,
                }
            } else {
                // Sockets, pipes, devices: not something to compare or delete.
                return None;
            };
            out.insert(child, e);
            if out.len() > MAX_ENTRIES {
                return None;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::{Action, find as find_dupes};
    use super::*;
    use std::io::Write;

    fn dir(tag: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("folders-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn content(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(17).wrapping_add(seed))
            .collect()
    }

    /// A small project: big files, small files, a subfolder.
    fn project(at: &Path, small_note: &[u8]) {
        write(&at.join("photos/a.mov"), &content(1, 2 << 20));
        write(&at.join("photos/b.mov"), &content(2, 3 << 20));
        write(&at.join("letters/c.pdf"), &content(3, 2 << 20));
        write(&at.join("letters/note.txt"), small_note);
    }

    fn scan(root: &Path) -> Snapshot {
        let opts = crate::ScanOptions {
            min_node_size: 0,
            ..Default::default()
        };
        crate::snapshot(root, opts, false).unwrap()
    }

    #[test]
    fn copied_folder_is_reported_once_at_the_top() {
        let d = dir("copy");
        project(&d.join("Documents/FLC"), b"hello");
        write(&d.join("Documents/unrelated.txt"), b"only here");
        project(&d.join("Old/FLC"), b"hello");
        // Finder clutter differs and doesn't matter.
        write(&d.join("Old/FLC/.DS_Store"), b"view settings");
        // A loose duplicate elsewhere is still a file-level duplicate.
        write(&d.join("Misc/a.mov"), &content(1, 2 << 20));

        let r = find_dupes(&scan(&d), None, None, Prefer::Original);
        assert_eq!(r.folders.len(), 1, "{:?}", r.folders);
        let g = &r.folders[0];
        assert_eq!(g.files, 4);
        assert_eq!(g.copies.len(), 2);
        assert!(
            g.copies[0].path.ends_with("/Documents/FLC"),
            "{:?}",
            g.copies
        );
        assert!(g.copies[1].path.ends_with("/Old/FLC"));
        assert!(g.reclaimable >= 7 << 20);
        // Files inside Old/FLC aren't listed again; Misc/a.mov still is,
        // against the kept copy.
        let paths: Vec<&str> = r
            .groups
            .iter()
            .flat_map(|g| g.copies.iter().map(|c| c.path.as_str()))
            .collect();
        assert!(paths.iter().all(|p| !p.contains("/Old/FLC/")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("/Misc/a.mov")));
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("/Documents/FLC/photos/a.mov"))
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_small_difference_means_not_identical() {
        let d = dir("differ");
        project(&d.join("A/FLC"), b"hello");
        project(&d.join("B/FLC"), b"hellp");
        let r = find_dupes(&scan(&d), None, None, Prefer::Original);
        // The top folders differ (note.txt), but their photos subfolders don't.
        let tops: Vec<&str> = r
            .folders
            .iter()
            .map(|g| g.copies[0].path.as_str())
            .collect();
        assert!(tops.iter().all(|p| !p.ends_with("/FLC")), "{tops:?}");
        assert!(tops.iter().any(|p| p.ends_with("/FLC/photos")), "{tops:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn three_copies_and_a_named_copy() {
        let d = dir("three");
        project(&d.join("Documents/FLC"), b"x");
        project(&d.join("Archive/FLC"), b"x");
        project(&d.join("Documents/FLC copy"), b"x");
        let r = find_dupes(&scan(&d), None, None, Prefer::Original);
        assert_eq!(r.folders.len(), 1, "{:?}", r.folders);
        let g = &r.folders[0];
        assert_eq!(g.copies.len(), 3, "{:?}", g.copies);
        let action = |s: &str| {
            g.copies
                .iter()
                .find(|c| c.path.ends_with(s))
                .unwrap()
                .action
        };
        assert_eq!(action("/Documents/FLC copy"), Some(Action::Trash));
        assert!(
            g.copies[0].path.ends_with("/Documents/FLC")
                || g.copies[0].path.ends_with("/Archive/FLC")
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn lockstep_works_from_the_root() {
        let p = Protection::new(&crate::platform::MACOS, vec!["/Users/v".into()]);
        let mut out = HashSet::new();
        lockstep("/Users/v/A/x/f", "/Users/v/B/x/f", "/", &p, &mut out);
        assert!(
            out.contains(&("/Users/v/A/x".to_string(), "/Users/v/B/x".to_string())),
            "{out:?}"
        );
        assert!(out.contains(&("/Users/v/A".to_string(), "/Users/v/B".to_string())));
        assert_eq!(out.len(), 2, "stops below the home folder: {out:?}");
    }

    #[test]
    fn files_in_a_kept_folder_are_kept() {
        let d = dir("keptfiles");
        // An older loose copy with a better claim by age alone…
        write(&d.join("Misc/a.mov"), &content(1, 2 << 20));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // …but Documents/FLC is the folder being kept, so its file wins.
        project(&d.join("Documents/FLC"), b"n");
        write(&d.join("Documents/other.txt"), b"only here");
        project(&d.join("Old/FLC"), b"n");
        let r = find_dupes(&scan(&d), None, None, Prefer::Original);
        assert!(
            r.folders[0].copies[0].path.ends_with("/Documents/FLC"),
            "{:?}",
            r.folders
        );
        let g = r
            .groups
            .iter()
            .find(|g| g.copies.iter().any(|c| c.path.ends_with("/Misc/a.mov")))
            .unwrap();
        assert!(
            g.copies[0].path.ends_with("/Documents/FLC/photos/a.mov"),
            "{:?}",
            g.copies
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn walk_ignores_clutter_and_refuses_odd_entries() {
        let d = dir("walk");
        write(&d.join("a/x"), b"1");
        write(&d.join("a/.DS_Store"), b"2");
        write(&d.join("a/._x"), b"3");
        let w = walk(&d).unwrap();
        assert!(w.contains_key("a") && w.contains_key("a/x"));
        assert_eq!(w.len(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
