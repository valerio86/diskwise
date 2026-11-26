//! Exact duplicate files.
//!
//! The scan notes candidate files as it walks (`Snapshot::large_files`): at least
//! [`DEFAULT_MIN_SIZE`], in places where people keep their own files, and sharing
//! their size with another candidate. [`find`] then narrows them down in stages
//! (size → the first and last 64 KiB → the whole file, hashed with BLAKE3), so
//! most files are never read in full. Full hashes are cached by path, size, mtime
//! and inode, so later runs only read files that changed.
//!
//! Reclaimable space is what deleting the extra copies would *really* free, not
//! their apparent size. Copies that share storage free nothing:
//! - hard links (one file, several names);
//! - APFS clones (Finder's Duplicate, `cp -c`): macOS reports each file's
//!   *private size*, the bytes no other file shares;
//! - reflinks on btrfs/XFS (`cp --reflink`): Linux reports shared extents, and a
//!   file with any is counted as freeing nothing (an underestimate, never an
//!   overestimate).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::actions::Protection;
use crate::model::{LargeFile, Snapshot};

/// Smaller files rarely add up to much, and there are far more of them.
pub const DEFAULT_MIN_SIZE: u64 = 1 << 20;

/// Folders whose files belong to an app or tool, wherever they appear. Deleting
/// one "duplicate" inside them breaks the thing that owns it, and the rules
/// already say how to clear the ones that are caches.
const OWNED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "site-packages",
    "__pycache__",
    ".venv",
    ".Trash",
    ".Trashes",
    ".cache",
    "Caches",
];

/// Package folders (they look like single files in Finder) and app libraries.
const OWNED_EXTENSIONS: &[&str] = &[
    "app",
    "appex",
    "framework",
    "bundle",
    "plugin",
    "kext",
    "component",
    "vst3",
    "photoslibrary",
    "musiclibrary",
    "tvlibrary",
    "imovielibrary",
    "fcpbundle",
    "logicx",
    "band",
    "lrdata",
    "lrlibrary",
    "aplibrary",
    "xcodeproj",
    "xcworkspace",
    "xcarchive",
    "sparsebundle",
    "pkg",
];

/// Whether the scan should note files directly inside the scan root.
/// Inside or at a home folder: yes. A folder that contains the home folders
/// (`/`, `/Users`): no, only the homes within it. Anywhere else (`/srv`, an
/// external drive): yes, the user chose to scan it.
pub(crate) fn collect_at_root(display: &str, protection: &Protection) -> bool {
    protection.is_home(display)
        || protection.in_home(display)
        || !protection.contains_homes(display)
}

/// Whether to note files inside `child` (a folder called `name`, at display path
/// `child`), given whether its parent's files are noted.
pub(crate) fn collect_in(
    parent_collects: bool,
    parent: &str,
    name: &str,
    child: &str,
    protection: &Protection,
) -> bool {
    if OWNED_DIRS.contains(&name) {
        return false;
    }
    if let Some((_, ext)) = name.rsplit_once('.')
        && OWNED_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
    {
        return false;
    }
    if protection.is_home(parent) && (name == "Library" || name.starts_with('.')) {
        // App data (~/Library) and tools' own folders (~/.rustup, ~/.local, …).
        return false;
    }
    parent_collects || protection.is_home(child)
}

/// Keep only files whose size another candidate shares: the rest can't have a duplicate.
pub(crate) fn keep_size_collisions(files: &mut Vec<LargeFile>) {
    let mut counts: HashMap<u64, u32> = HashMap::new();
    for f in files.iter() {
        *counts.entry(f.size).or_default() += 1;
    }
    files.retain(|f| counts[&f.size] > 1);
    files.sort_by(|a, b| a.path.cmp(&b.path));
}

/// Counters a UI can read while [`find`] runs.
#[derive(Debug, Default)]
pub struct DupeProgress {
    /// Bytes that may need reading (known once sizes are grouped).
    pub total_bytes: AtomicU64,
    pub done_bytes: AtomicU64,
}

/// One copy of a duplicated file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Copy {
    /// Display path.
    pub path: String,
    pub allocated: u64,
    /// What deleting this copy would free: 0 for hard links and full clones.
    pub frees: u64,
    pub mtime: i64,
    /// Creation time (0 if unknown).
    pub created: i64,
    /// Why it frees less than its size, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<Shared>,
    /// What to do with this copy; `None` for the one to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Action>,
}

/// What diskwise suggests for an extra copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// A clear stray copy: next to the keeper, named like it plus "copy", "(1)",
    /// "_1"… Nothing is likely to refer to it, so move it to the Trash.
    Trash,
    /// Nothing says which copy is the stray, and something may refer to this
    /// path: replace it with a clone of the keeper. Both paths keep working and
    /// the contents are stored once.
    StoreOnce,
    /// In doubt, but a clone isn't possible (another volume, or a filesystem
    /// without clones): only the Trash is left, so it's the user's call.
    Review,
    /// Already shares its storage with the keeper (hard link or clone): nothing to gain.
    Nothing,
}

/// Which copy to keep when nothing else (location, name) decides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Prefer {
    /// The oldest copy: most likely the original.
    #[default]
    Original,
    /// The most recently made copy: for people who copy files to where they
    /// want them and leave the old ones behind.
    NewestCopy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shared {
    /// The file has other names (hard links).
    HardLink,
    /// Some or all of its storage is shared with another file (clone / reflink).
    Clone,
}

/// Files with identical contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub size: u64,
    /// BLAKE3 of the contents, hex.
    pub hash: String,
    /// The suggested copy to keep is first; the rest are the extra copies.
    pub copies: Vec<Copy>,
    /// Space freed by deleting every copy but the first.
    pub reclaimable: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    /// Groups that free space, most reclaimable first.
    pub groups: Vec<Group>,
    /// Duplicates whose copies already share storage, so deleting frees nothing.
    pub already_shared: Vec<Group>,
    pub candidates: u64,
    /// Bytes actually read this run (cache hits read nothing).
    pub read_bytes: u64,
    /// Candidates skipped because they changed or vanished since the scan.
    pub changed: u64,
    pub errors: u64,
}

impl Report {
    pub fn reclaimable(&self) -> u64 {
        self.groups.iter().map(|g| g.reclaimable).sum()
    }
}

/// A candidate as it is on disk now.
struct Live {
    file: LargeFile,
    real: PathBuf,
    /// Bytes no other file shares (APFS private size / no shared extents).
    private: Option<u64>,
    /// Whether its filesystem can make clones (APFS, btrfs, XFS).
    can_clone: bool,
}

/// Find exact duplicates among a snapshot's candidates. `cache` is a path to the
/// hash cache (read and rewritten), or `None` for no cache.
pub fn find(
    snap: &Snapshot,
    cache: Option<&Path>,
    progress: Option<Arc<DupeProgress>>,
    prefer: Prefer,
) -> Report {
    let mut report = Report {
        candidates: snap.large_files.len() as u64,
        ..Default::default()
    };
    if snap.platform() != crate::platform::CURRENT {
        // Another machine's paths: nothing here to read.
        return report;
    }
    let progress = progress.unwrap_or_default();
    let mut cached = cache.map(HashCache::load).unwrap_or_default();

    // Stage 0: still there and unchanged since the scan.
    let live: Vec<Live> = snap
        .large_files
        .par_iter()
        .filter_map(|f| {
            let real = PathBuf::from(snap.real_path(&f.path));
            let meta = std::fs::symlink_metadata(&real).ok()?;
            let same = meta.is_file()
                && meta.len() == f.size
                && meta.mtime() == f.mtime
                && meta.ino() == f.ino
                && meta.dev() == f.dev;
            same.then(|| Live {
                private: private_size(&real),
                can_clone: crate::platform::can_clone(&real),
                file: f.clone(),
                real,
            })
        })
        .collect();
    report.changed = snap.large_files.len() as u64 - live.len() as u64;

    // Stage 1: sizes, with hard links of one file counted as one.
    let mut by_size: HashMap<u64, Vec<Live>> = HashMap::new();
    for l in live {
        by_size.entry(l.file.size).or_default().push(l);
    }
    let size_groups: Vec<Vec<Live>> = by_size.into_values().filter(|g| g.len() > 1).collect();
    progress.total_bytes.store(
        size_groups.iter().flatten().map(|l| l.file.size).sum(),
        Ordering::Relaxed,
    );

    // Stages 2 and 3: head+tail hash, then full hash (cached).
    let read = AtomicU64::new(0);
    let errors = AtomicU64::new(0);
    let hashed: Vec<(String, Live)> = size_groups
        .into_par_iter()
        .flat_map_iter(|group| {
            let mut out = Vec::new();
            let mut by_inode: HashMap<(u64, u64), String> = HashMap::new();
            // Head and tail first, for every file: cheap, and it keeps copies with
            // and without a cached full hash in the same subgroup.
            for sub in split_by(group, |l| {
                read.fetch_add(l.file.size.min(2 * EDGE), Ordering::Relaxed);
                edge_hash(&l.real, l.file.size)
                    .inspect_err(|_| {
                        errors.fetch_add(1, Ordering::Relaxed);
                    })
                    .ok()
            }) {
                for l in sub {
                    let key = (l.file.dev, l.file.ino);
                    let hash = if let Some(h) = cached.get(&l.file) {
                        Some(h.to_string())
                    } else if let Some(h) = by_inode.get(&key) {
                        Some(h.clone())
                    } else {
                        read.fetch_add(l.file.size, Ordering::Relaxed);
                        full_hash(&l.real)
                            .inspect_err(|_| {
                                errors.fetch_add(1, Ordering::Relaxed);
                            })
                            .ok()
                    };
                    progress
                        .done_bytes
                        .fetch_add(l.file.size, Ordering::Relaxed);
                    if let Some(h) = hash {
                        by_inode.insert(key, h.clone());
                        out.push((h, l));
                    }
                }
            }
            out
        })
        .collect();
    report.read_bytes = read.into_inner();
    report.errors = errors.into_inner();

    let mut by_hash: HashMap<String, Vec<Live>> = HashMap::new();
    for (h, l) in hashed {
        cached.insert(&l.file, &h);
        by_hash.entry(h).or_default().push(l);
    }
    if let Some(path) = cache {
        cached.save(path);
    }

    for (hash, copies) in by_hash {
        if copies.len() < 2 {
            continue;
        }
        let group = build_group(hash, copies, prefer);
        if group.reclaimable > 0 {
            report.groups.push(group);
        } else {
            report.already_shared.push(group);
        }
    }
    report
        .groups
        .sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable).then(a.hash.cmp(&b.hash)));
    report
        .already_shared
        .sort_by(|a, b| b.size.cmp(&a.size).then(a.hash.cmp(&b.hash)));
    report
}

/// Split `group` by `key`, dropping items without one and subgroups of one.
fn split_by<F>(group: Vec<Live>, key: F) -> Vec<Vec<Live>>
where
    F: Fn(&Live) -> Option<String>,
{
    let mut m: HashMap<String, Vec<Live>> = HashMap::new();
    for l in group {
        if let Some(k) = key(&l) {
            m.entry(k).or_default().push(l);
        }
    }
    m.into_values().filter(|g| g.len() > 1).collect()
}

fn build_group(hash: String, live: Vec<Live>, prefer: Prefer) -> Group {
    let size = live[0].file.size;
    let mut copies: Vec<(Copy, u64, bool)> = live
        .into_iter()
        .map(|l| {
            let (frees, shared) = if l.file.nlink > 1 {
                // Deleting one name of a hard-linked file frees nothing.
                (0, Some(Shared::HardLink))
            } else {
                match l.private {
                    Some(p) if p < l.file.allocated => (p, Some(Shared::Clone)),
                    _ => (l.file.allocated, None),
                }
            };
            let copy = Copy {
                path: l.file.path,
                allocated: l.file.allocated,
                frees,
                mtime: l.file.mtime,
                created: l.file.created,
                shared,
                action: None,
            };
            (copy, l.file.dev, l.can_clone)
        })
        .collect();
    copies.sort_by(|a, b| keep_order(&a.0, &b.0, prefer));
    let (keeper, keeper_dev, _) = &copies[0];
    let (keeper_path, keeper_dev) = (keeper.path.clone(), *keeper_dev);
    for (c, dev, can_clone) in &mut copies[1..] {
        c.action = Some(if c.frees == 0 {
            Action::Nothing
        } else if is_named_copy(&keeper_path, &c.path) {
            Action::Trash
        } else if *dev == keeper_dev && *can_clone {
            Action::StoreOnce
        } else {
            Action::Review
        });
    }
    let copies: Vec<Copy> = copies.into_iter().map(|(c, _, _)| c).collect();
    // Other names of a hard-linked file already count as freeing nothing (and
    // deleting every name of one would free it once: underestimated, never over).
    let reclaimable = copies[1..].iter().map(|c| c.frees).sum();
    Group {
        size,
        hash,
        copies,
        reclaimable,
    }
}

/// Which copy to keep: the one in the most deliberate place, then the original
/// or the newest copy (by creation time, else mtime), then the shortest path.
fn keep_order(a: &Copy, b: &Copy, prefer: Prefer) -> std::cmp::Ordering {
    let age = |c: &Copy| if c.created > 0 { c.created } else { c.mtime };
    let by_age = match prefer {
        Prefer::Original => age(a).cmp(&age(b)),
        Prefer::NewestCopy => age(b).cmp(&age(a)),
    };
    copy_penalty(&a.path)
        .cmp(&copy_penalty(&b.path))
        .then(by_age)
        .then(a.path.len().cmp(&b.path.len()))
        .then(a.path.cmp(&b.path))
}

/// Higher means "more likely the stray copy".
fn copy_penalty(path: &str) -> u32 {
    let lower = path.to_lowercase();
    let mut p = 0;
    for dir in ["/downloads/", "/desktop/", "/tmp/", "/temp/"] {
        if lower.contains(dir) {
            p += 2;
        }
    }
    let name = lower.rsplit('/').next().unwrap_or("");
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    if stem.ends_with(" copy")
        || stem.contains(" copy ")
        || stem.ends_with("-copy")
        || stem.ends_with("_copy")
        || numbered_copy(stem)
    {
        p += 3;
    }
    for word in ["backup", "copy of", "old", "archive"] {
        if lower.split('/').any(|seg| {
            seg == word || seg.starts_with(&format!("{word} ")) || seg.contains(&format!(" {word}"))
        }) {
            p += 1;
        }
    }
    p
}

/// Whether `copy` is in the same folder as `keeper` and named like it plus a copy
/// marker: `a copy.jpg`, `a copy 2.jpg`, `a-copy.jpg`, `a (1).jpg`, `a(1).jpg`,
/// `a_1.jpg`, `a-2.jpg`, `a 2.jpg`, `Copy of a.jpg`. Judged against the keeper's
/// name, so `IMG_1.jpg` alone never looks like a copy.
fn is_named_copy(keeper: &str, copy: &str) -> bool {
    let split = |p: &str| -> (String, String, String) {
        let (dir, name) = p.rsplit_once('/').unwrap_or(("", p));
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s, e),
            _ => (name, ""),
        };
        (dir.to_string(), stem.to_lowercase(), ext.to_lowercase())
    };
    let (kd, ks, ke) = split(keeper);
    let (cd, cs, ce) = split(copy);
    if kd != cd || ke != ce || ks.is_empty() {
        return false;
    }
    if cs.strip_prefix("copy of ").is_some_and(|rest| rest == ks) {
        return true;
    }
    let Some(rest) = cs.strip_prefix(ks.as_str()) else {
        return false;
    };
    let small_number =
        |s: &str| !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(n) = rest.strip_prefix(" copy") {
        return n.is_empty() || n.strip_prefix(' ').is_some_and(small_number);
    }
    if rest == "-copy" || rest == "_copy" {
        return true;
    }
    if let Some(inner) = rest.strip_prefix(" (").or_else(|| rest.strip_prefix('(')) {
        return inner.strip_suffix(')').is_some_and(small_number);
    }
    rest.strip_prefix(['_', '-', ' ']).is_some_and(small_number)
}

/// `name (2)` or `name(2)`: how browsers and Finder name a second download/copy.
fn numbered_copy(stem: &str) -> bool {
    let Some(inner) = stem.strip_suffix(')') else {
        return false;
    };
    inner
        .rsplit_once('(')
        .is_some_and(|(before, n)| !before.is_empty() && n.len() <= 3 && n.parse::<u32>().is_ok())
}

const EDGE: u64 = 64 * 1024;

fn edge_hash(path: &Path, size: u64) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; EDGE as usize];
    let n = read_full(&mut f, &mut buf)?;
    h.update(&buf[..n]);
    if size > 2 * EDGE {
        f.seek(SeekFrom::Start(size - EDGE))?;
        let n = read_full(&mut f, &mut buf)?;
        h.update(&buf[..n]);
    }
    Ok(format!("edge:{}", h.finalize().to_hex()))
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn full_hash(path: &Path) -> std::io::Result<String> {
    let mut h = blake3::Hasher::new();
    h.update_reader(File::open(path)?)?;
    Ok(h.finalize().to_hex().to_string())
}

/// Bytes of `path` that no other file shares, if the filesystem can say.
fn private_size(path: &Path) -> Option<u64> {
    crate::platform::private_size(path)
}

/// Full-content hashes from earlier runs.
#[derive(Default, Serialize, Deserialize)]
struct HashCache {
    version: u32,
    /// Display path → entry.
    entries: HashMap<String, CacheEntry>,
    #[serde(skip)]
    used: HashSet<String>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    size: u64,
    mtime: i64,
    ino: u64,
    hash: String,
}

const CACHE_VERSION: u32 = 1;

impl HashCache {
    fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<HashCache>(&b).ok())
            .filter(|c| c.version == CACHE_VERSION)
            .unwrap_or_default()
    }

    fn get(&self, f: &LargeFile) -> Option<&str> {
        let e = self.entries.get(&f.path)?;
        (e.size == f.size && e.mtime == f.mtime && e.ino == f.ino).then_some(e.hash.as_str())
    }

    fn insert(&mut self, f: &LargeFile, hash: &str) {
        self.used.insert(f.path.clone());
        self.entries.insert(
            f.path.clone(),
            CacheEntry {
                size: f.size,
                mtime: f.mtime,
                ino: f.ino,
                hash: hash.to_string(),
            },
        );
    }

    /// Write the cache, keeping entries from this run and older ones whose
    /// file still looks the same (so scanning one folder doesn't forget another).
    fn save(mut self, path: &Path) {
        let used = std::mem::take(&mut self.used);
        self.entries.retain(|p, e| {
            used.contains(p)
                || std::fs::symlink_metadata(p)
                    .is_ok_and(|m| m.len() == e.size && m.mtime() == e.mtime && m.ino() == e.ino)
        });
        self.version = CACHE_VERSION;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write then rename, so an interrupted run never leaves a torn cache.
        let tmp = path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec(&self)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Where the hash cache lives by default.
pub fn default_cache_path() -> Option<PathBuf> {
    crate::platform::cache_dir().map(|d| d.join("hashes.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{LINUX, MACOS};
    use std::io::Write;

    fn dir(tag: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("dupes-{tag}-{}", std::process::id()));
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
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    fn scan(root: &Path) -> Snapshot {
        let opts = crate::ScanOptions {
            min_node_size: 0,
            ..Default::default()
        };
        crate::snapshot(root, opts, false).unwrap()
    }

    #[test]
    fn finds_duplicates_and_skips_lookalikes() {
        let d = dir("basic");
        let a = content(1, 2 << 20);
        write(&d.join("Photos/a.jpg"), &a);
        write(&d.join("Downloads/a (1).jpg"), &a);
        write(&d.join("Old/a copy.jpg"), &a);
        // Same size, same head and tail, different middle: not a duplicate.
        let mut b = a.clone();
        b[1 << 20] ^= 0xff;
        write(&d.join("Other/b.jpg"), &b);
        // Unique size: never even a candidate.
        write(&d.join("c.bin"), &content(2, 3 << 20));
        // Too small.
        write(&d.join("s1"), &content(3, 1000));
        write(&d.join("s2"), &content(3, 1000));
        // Inside a package/owned folder: never touched.
        write(&d.join("node_modules/x/a.jpg"), &a);

        let snap = scan(&d);
        assert_eq!(snap.large_files.len(), 4, "{:?}", snap.large_files);
        let cache = d.join("cache/hashes.json");
        let r = find(&snap, Some(&cache), None, Prefer::Original);
        assert_eq!(r.groups.len(), 1);
        let g = &r.groups[0];
        assert_eq!(g.copies.len(), 3);
        assert!(
            g.copies[0].path.ends_with("/Photos/a.jpg"),
            "{:?}",
            g.copies
        );
        assert!(g.reclaimable >= 2 * (2 << 20));

        // Second run is served from the cache.
        let r2 = find(&snap, Some(&cache), None, Prefer::Original);
        assert_eq!(r2.groups.len(), 1);
        assert!(
            r2.read_bytes < r.read_bytes,
            "{} vs {}",
            r2.read_bytes,
            r.read_bytes
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn hard_links_free_nothing() {
        let d = dir("hardlinks");
        let a = content(4, 2 << 20);
        write(&d.join("one.bin"), &a);
        std::fs::hard_link(d.join("one.bin"), d.join("two.bin")).unwrap();
        let snap = scan(&d);
        let r = find(&snap, None, None, Prefer::Original);
        assert!(r.groups.is_empty(), "{:?}", r.groups);
        assert_eq!(r.already_shared.len(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clones_free_nothing() {
        let d = dir("clones");
        write(&d.join("one.bin"), &content(5, 2 << 20));
        let ok = std::process::Command::new("cp")
            .args(["-c"])
            .arg(d.join("one.bin"))
            .arg(d.join("two.bin"))
            .status()
            .unwrap()
            .success();
        assert!(ok);
        // The repo (and CI's macOS runners) live on APFS, where a clone shares
        // every block with its original.
        assert_eq!(private_size(&d.join("two.bin")), Some(0));
        write(&d.join("three.bin"), &content(5, 2 << 20));
        assert!(private_size(&d.join("three.bin")).unwrap() >= 2 << 20);
        let r = find(&scan(&d), None, None, Prefer::Original);
        // three.bin is a real copy, so one copy's worth is reclaimable; the clone frees nothing.
        assert_eq!(r.groups.len(), 1, "{:?}", r);
        let g = &r.groups[0];
        assert_eq!(g.copies.len(), 3);
        assert!(
            g.reclaimable >= 2 << 20 && g.reclaimable < 2 * (2 << 20),
            "{}",
            g.reclaimable
        );
        assert!(g.copies.iter().any(|c| c.shared == Some(Shared::Clone)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn changed_files_are_skipped() {
        let d = dir("changed");
        let a = content(6, 2 << 20);
        write(&d.join("x/a"), &a);
        write(&d.join("y/a"), &a);
        let snap = scan(&d);
        std::fs::remove_file(d.join("y/a")).unwrap();
        let r = find(&snap, None, None, Prefer::Original);
        assert_eq!(r.changed, 1);
        assert!(r.groups.is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn where_files_are_noted() {
        let p = Protection::new(&MACOS, vec!["/Users/v".into()]);
        assert!(!collect_at_root("/", &p));
        assert!(!collect_at_root("/Users", &p));
        assert!(collect_at_root("/Users/v", &p));
        assert!(collect_at_root("/Users/v/Pictures", &p));
        assert!(collect_at_root("/Volumes/Backup", &p));
        // Descending from / reaches homes and only homes.
        assert!(!collect_in(false, "/", "Applications", "/Applications", &p));
        assert!(collect_in(false, "/Users", "v", "/Users/v", &p));
        assert!(collect_in(false, "/Users", "w", "/Users/w", &p), "any home");
        assert!(!collect_in(
            true,
            "/Users/v",
            "Library",
            "/Users/v/Library",
            &p
        ));
        assert!(!collect_in(
            true,
            "/Users/v",
            ".rustup",
            "/Users/v/.rustup",
            &p
        ));
        assert!(collect_in(
            true,
            "/Users/v",
            "Pictures",
            "/Users/v/Pictures",
            &p
        ));
        assert!(collect_in(
            true,
            "/Users/v/Code",
            ".config",
            "/Users/v/Code/.config",
            &p
        ));
        assert!(!collect_in(
            true,
            "/Users/v/Pictures",
            "Photos Library.photoslibrary",
            "/Users/v/Pictures/Photos Library.photoslibrary",
            &p
        ));
        assert!(!collect_in(
            true,
            "/Users/v/Code/x",
            ".git",
            "/Users/v/Code/x/.git",
            &p
        ));
        let l = Protection::new(&LINUX, vec![]);
        assert!(collect_at_root("/srv/share", &l));
        assert!(!collect_at_root("/", &l));
    }

    #[test]
    fn named_copies() {
        let yes = [
            "a copy.jpg",
            "a copy 2.jpg",
            "A-copy.JPG",
            "a_copy.jpg",
            "a (1).jpg",
            "a(12).jpg",
            "a_1.jpg",
            "a-2.jpg",
            "a 3.jpg",
            "Copy of a.jpg",
        ];
        for name in yes {
            assert!(is_named_copy("/p/a.jpg", &format!("/p/{name}")), "{name}");
        }
        let no = [
            "/q/a (1).jpg",  // another folder
            "/p/a (1).png",  // another type
            "/p/ab.jpg",     // another name
            "/p/a_2024.jpg", // a year, not a copy number
            "/p/a copy x.jpg",
            "/p/b.jpg",
        ];
        for path in no {
            assert!(!is_named_copy("/p/a.jpg", path), "{path}");
        }
        // Relative to the keeper: IMG_1 isn't a copy of IMG.
        assert!(!is_named_copy("/p/IMG_2.jpg", "/p/IMG_1.jpg"));
    }

    #[test]
    fn actions_and_preference() {
        let d = dir("actions");
        let a = content(7, 2 << 20);
        write(&d.join("Pictures/a.jpg"), &a);
        write(&d.join("Pictures/a (1).jpg"), &a);
        // Same name elsewhere, made later (creation times are whole seconds): in doubt.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(&d.join("Projects/survey/a.jpg"), &a);
        let snap = scan(&d);
        let r = find(&snap, None, None, Prefer::Original);
        let g = &r.groups[0];
        let action = |suffix: &str| {
            g.copies
                .iter()
                .find(|c| c.path.ends_with(suffix))
                .unwrap()
                .action
        };
        assert_eq!(g.copies[0].action, None);
        assert_eq!(action("/Pictures/a (1).jpg"), Some(Action::Trash));
        let in_doubt = if crate::platform::can_clone(&d) {
            Action::StoreOnce
        } else {
            Action::Review
        };
        assert!(
            g.copies[0].path.ends_with("/Pictures/a.jpg"),
            "{:?}",
            g.copies
        );
        assert_eq!(action("/Projects/survey/a.jpg"), Some(in_doubt));

        // Preferring the newest copy flips the choice between the two plain names.
        let newest = find(&snap, None, None, Prefer::NewestCopy);
        let keeper = &newest.groups[0].copies[0].path;
        if snap.large_files.iter().all(|f| f.created > 0) {
            assert!(keeper.ends_with("/Projects/survey/a.jpg"), "{keeper}");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stray_copies_rank_last() {
        assert!(copy_penalty("/Users/v/Downloads/a.jpg") > copy_penalty("/Users/v/Pictures/a.jpg"));
        assert!(copy_penalty("/Users/v/Pictures/a (2).jpg") > 0);
        assert!(copy_penalty("/Users/v/Pictures/M2U00014(1).MPG") > 0);
        assert_eq!(copy_penalty("/Users/v/Pictures/(2).jpg"), 0);
        assert_eq!(copy_penalty("/Users/v/Pictures/scan (2024).jpg"), 0);
        assert!(copy_penalty("/Users/v/Pictures/a copy.jpg") > 0);
        assert!(copy_penalty("/Users/v/Backup/a.jpg") > 0);
        assert_eq!(copy_penalty("/Users/v/Pictures/Holiday (Rome).jpg"), 0);
        assert_eq!(copy_penalty("/Users/v/Pictures/golden.jpg"), 0);
    }
}
