//! Acting on duplicates: move an extra copy to the Trash, or "store once"
//! (replace it with a clone of the keeper, so both paths keep working and the
//! contents are stored once).
//!
//! Everything is checked again right before acting, whatever the report said:
//! the copy must be unchanged since the scan, not protected, and *still
//! identical* to the keeper (re-hashed, or for folders re-compared entry by
//! entry), so the last copy of anything is never the one that goes.

use std::collections::HashMap;
use std::fs::{self, FileTimes};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::folders::{self, Entry};
use super::{full_hash, inside, private_size};
use crate::actions::Protection;
use crate::model::Snapshot;

/// What to do with an extra copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    Trash,
    StoreOnce,
}

/// One extra copy and the copy it duplicates (display paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub keeper: String,
    pub copy: String,
    /// Folder copies are compared and acted on as whole folders.
    pub folder: bool,
}

/// What an action did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    /// Files replaced by clones (store once); 0 for the Trash.
    pub cloned: u64,
    /// Files skipped by store once because they already share storage.
    pub already_shared: u64,
    /// Store once on a folder stopped partway: what went wrong. The files
    /// counted in `cloned` were replaced; the rest weren't touched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

impl Done {
    /// Whether anything was freed (a Trash move, or at least one clone).
    pub fn freed_anything(&self, how: How) -> bool {
        how == How::Trash || self.cloned > 0
    }
}

/// One step of a group's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub target: Target,
    pub how: How,
    /// What it frees.
    pub bytes: u64,
    /// The copy is in a synced folder (trashing it removes it everywhere).
    pub synced: bool,
}

/// What carrying out a group's chosen actions would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupPlan {
    pub steps: Vec<Step>,
    /// Extra copies still marked "your call": left as they are.
    pub undecided: usize,
}

impl GroupPlan {
    pub fn total(&self) -> u64 {
        self.steps.iter().map(|s| s.bytes).sum()
    }

    /// A warning to show with the confirmation, if any.
    pub fn warning(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.steps.iter().any(|s| s.synced && s.how == How::Trash) {
            parts.push(
                "A copy in a synced folder will be removed on every synced device too.".to_string(),
            );
        }
        if self.undecided > 0 {
            parts.push(format!(
                "{} undecided {} left as {}.",
                self.undecided,
                if self.undecided == 1 {
                    "copy is"
                } else {
                    "copies are"
                },
                if self.undecided == 1 {
                    "it is"
                } else {
                    "they are"
                }
            ));
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

/// The steps for a group (keeper first in `copies`): each extra copy set to
/// Trash or Store once.
pub fn plan(copies: &[super::Copy], folder: bool) -> GroupPlan {
    let keeper = &copies[0].path;
    let mut out = GroupPlan::default();
    for c in &copies[1..] {
        let how = match c.action {
            Some(super::Action::Trash) => How::Trash,
            Some(super::Action::StoreOnce) => How::StoreOnce,
            Some(super::Action::Review) => {
                out.undecided += 1;
                continue;
            }
            _ => continue,
        };
        out.steps.push(Step {
            target: Target {
                keeper: keeper.clone(),
                copy: c.path.clone(),
                folder,
            },
            how,
            bytes: c.frees,
            synced: c.synced,
        });
    }
    out
}

/// Every reason not to act on `t` right now.
pub fn check(snap: &Snapshot, protection: &Protection, t: &Target) -> Result<(), String> {
    if snap.platform() != crate::platform::CURRENT {
        return Err("This scan came from another operating system, so its paths don't refer to this machine.".into());
    }
    if t.copy == t.keeper || inside(&t.copy, &t.keeper) || inside(&t.keeper, &t.copy) {
        return Err(format!(
            "{} and {} overlap, so one can't stand in for the other.",
            t.copy, t.keeper
        ));
    }
    if let Some(reason) = protection.reason(&t.copy) {
        return Err(reason);
    }
    let changed = |p: &str| format!("{p} has changed since the scan. Scan again first.");
    let (copy, keeper) = (real(snap, &t.copy), real(snap, &t.keeper));
    if t.folder {
        for (display, path) in [(&t.copy, &copy), (&t.keeper, &keeper)] {
            let meta = fs::symlink_metadata(path).map_err(|_| changed(display))?;
            if !meta.is_dir() {
                return Err(changed(display));
            }
        }
        if folders::verify(snap, &t.keeper, &t.copy, &HashMap::new()).is_none() {
            return Err(format!(
                "{} is no longer identical to {}. Scan again first.",
                t.copy, t.keeper
            ));
        }
        return Ok(());
    }
    for display in [&t.copy, &t.keeper] {
        let rec = snap
            .large_files
            .iter()
            .find(|f| &f.path == display)
            .ok_or_else(|| format!("{display} isn't in the scan."))?;
        let meta = fs::symlink_metadata(real(snap, display)).map_err(|_| changed(display))?;
        let same = meta.is_file()
            && meta.len() == rec.size
            && meta.mtime() == rec.mtime
            && meta.ino() == rec.ino
            && meta.dev() == rec.dev;
        if !same {
            return Err(changed(display));
        }
    }
    let hash = |p: &Path, d: &str| full_hash(p).map_err(|e| format!("Couldn't read {d}: {e}"));
    if hash(&copy, &t.copy)? != hash(&keeper, &t.keeper)? {
        return Err(format!(
            "{} is no longer identical to {}.",
            t.copy, t.keeper
        ));
    }
    Ok(())
}

/// Check, then act. `trash` moves a path to the Trash (`actions::move_to_trash`
/// in the apps, a stand-in in tests).
pub fn apply(
    snap: &Snapshot,
    protection: &Protection,
    t: &Target,
    how: How,
    trash: &dyn Fn(&Path) -> Result<(), String>,
) -> Result<Done, String> {
    check(snap, protection, t)?;
    let (copy, keeper) = (real(snap, &t.copy), real(snap, &t.keeper));
    match how {
        How::Trash => {
            trash(&copy)?;
            crate::actions::verify_gone(&copy)?;
            Ok(Done::default())
        }
        How::StoreOnce => {
            if !crate::platform::can_clone(&copy) {
                return Err(format!("{}'s volume can't make clones.", t.copy));
            }
            if fs::symlink_metadata(&copy).map(|m| m.dev()).ok()
                != fs::symlink_metadata(&keeper).map(|m| m.dev()).ok()
            {
                return Err(format!(
                    "{} and {} are on different volumes, so they can't share storage.",
                    t.copy, t.keeper
                ));
            }
            if t.folder {
                store_once_folder(&keeper, &copy)
            } else {
                let mut done = Done::default();
                if store_once_file(&keeper, &copy)? {
                    done.cloned += 1;
                } else {
                    done.already_shared += 1;
                }
                Ok(done)
            }
        }
    }
}

/// Keep the snapshot in step after an action: a trashed copy leaves the tree
/// and the candidate list; a cloned one gets its new inode.
pub fn record(snap: &mut Snapshot, t: &Target, how: How) {
    match how {
        How::Trash => {
            snap.remove(&t.copy);
            super::forget(snap, &t.copy);
        }
        How::StoreOnce => {
            let root = snap.display_root();
            let real_root = snap.root.clone();
            for f in snap
                .large_files
                .iter_mut()
                .filter(|f| f.path == t.copy || inside(&f.path, &t.copy))
            {
                let path = real_path(&root, &real_root, &f.path);
                if let Ok(m) = fs::symlink_metadata(path) {
                    f.ino = m.ino();
                    f.dev = m.dev();
                    f.mtime = m.mtime();
                    f.allocated = m.blocks() * 512;
                }
            }
        }
    }
}

fn real(snap: &Snapshot, display: &str) -> PathBuf {
    PathBuf::from(snap.real_path(display))
}

fn real_path(display_root: &str, real_root: &str, display: &str) -> PathBuf {
    let rel = display
        .strip_prefix(display_root)
        .unwrap_or(display)
        .trim_start_matches('/');
    Path::new(real_root).join(rel)
}

/// Replace every file of `copy` (a folder identical to `keeper`) with a clone
/// of its counterpart. Folders themselves stay as they are.
fn store_once_folder(keeper: &Path, copy: &Path) -> Result<Done, String> {
    let entries = folders::walk(copy).ok_or("The folder has entries diskwise can't handle.")?;
    let mut done = Done::default();
    for (rel, e) in entries {
        if let Entry::File { real, .. } = e {
            match store_once_file(&keeper.join(&rel), &real) {
                Ok(true) => done.cloned += 1,
                Ok(false) => done.already_shared += 1,
                // Nothing replaced yet: a plain failure.
                Err(e) if done.cloned == 0 => return Err(e),
                // Some files were replaced: report that, so callers record it.
                Err(e) => {
                    done.stopped = Some(format!(
                        "{e} ({} file(s) before it were already stored once; the rest weren't changed)",
                        done.cloned
                    ));
                    break;
                }
            }
        }
    }
    Ok(done)
}

/// Replace `copy` with a clone of `keeper`, keeping the copy's own metadata.
/// Returns false (doing nothing) if the copy already shares its storage.
fn store_once_file(keeper: &Path, copy: &Path) -> Result<bool, String> {
    let meta = fs::symlink_metadata(copy).map_err(|e| format!("{}: {e}", copy.display()))?;
    if meta.nlink() > 1 || private_size(copy) == Some(0) {
        // A hard link (replacing one name would split it) or already a clone.
        return Ok(false);
    }
    let name = copy
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: unusual name", copy.display()))?;
    let tmp = copy.with_file_name(format!(".{name}.diskwise-{}", std::process::id()));
    let result = (|| {
        crate::platform::clone_file(keeper, &tmp).map_err(|e| format!("cloning: {e}"))?;
        copy_metadata(&meta, copy, &tmp).map_err(|e| format!("carrying over details: {e}"))?;
        let (a, b) = (full_hash(&tmp), full_hash(copy));
        match (a, b) {
            (Ok(a), Ok(b)) if a == b => {}
            _ => return Err("the clone doesn't match the copy".to_string()),
        }
        fs::rename(&tmp, copy).map_err(|e| format!("replacing: {e}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
        .map(|()| true)
        .map_err(|e| format!("{}: {e}. Nothing was changed.", copy.display()))
}

/// Give `to` the metadata of `from` (whose `fs::metadata` is `meta`): extended
/// attributes (incl. Finder tags), owner, permissions, access/modification
/// and (macOS) creation dates, and (macOS) file flags. ACLs aren't carried over.
fn copy_metadata(meta: &fs::Metadata, from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // The clone came with the keeper's flags and mode: a locked (uchg) or
    // read-only keeper would block every change below. Unlock it and make it
    // writable for us first; the copy's own mode and flags go on last.
    crate::platform::clear_flags(to)?;
    fs::set_permissions(to, fs::Permissions::from_mode(0o600))?;
    // The clone came with the keeper's attributes: replace them with the copy's.
    for name in xattr::list(to)? {
        xattr::remove(to, &name)?;
    }
    for name in xattr::list(from)? {
        if let Some(value) = xattr::get(from, &name)? {
            xattr::set(to, &name, &value)?;
        }
    }
    let to_meta = fs::symlink_metadata(to)?;
    if (to_meta.uid(), to_meta.gid()) != (meta.uid(), meta.gid()) {
        std::os::unix::fs::lchown(to, Some(meta.uid()), Some(meta.gid()))?;
    }
    let mut times = FileTimes::new()
        .set_accessed(meta.accessed()?)
        .set_modified(meta.modified()?);
    times = crate::platform::with_created(times, meta);
    // Read-only is enough to set times as the owner.
    fs::File::open(to)?.set_times(times)?;
    fs::set_permissions(to, meta.permissions())?;
    crate::platform::copy_flags(meta, to)
}

#[cfg(test)]
mod tests {
    use super::super::{Prefs, find};
    use super::*;
    use crate::platform::CURRENT;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, UNIX_EPOCH};

    fn dir(tag: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("act-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::canonicalize(d).unwrap()
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(path).unwrap().write_all(bytes).unwrap();
    }

    fn content(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
            .collect()
    }

    fn scan(root: &Path) -> Snapshot {
        let opts = crate::ScanOptions {
            min_node_size: 0,
            ..Default::default()
        };
        crate::snapshot(root, opts, false).unwrap()
    }

    fn protection() -> Protection {
        Protection::for_platform(CURRENT)
    }

    fn target(d: &Path, keeper: &str, copy: &str, folder: bool) -> Target {
        Target {
            keeper: d.join(keeper).display().to_string(),
            copy: d.join(copy).display().to_string(),
            folder,
        }
    }

    fn no_trash(_: &Path) -> Result<(), String> {
        panic!("must not trash")
    }

    #[test]
    fn store_once_keeps_the_copys_details() {
        let d = dir("storeonce");
        if !crate::platform::can_clone(&d) {
            return; // e.g. ext4 on a Linux CI runner
        }
        let a = content(1, 2 << 20);
        write(&d.join("keep/a.bin"), &a);
        write(&d.join("copy/a.bin"), &a);
        let copy = d.join("copy/a.bin");
        xattr::set(d.join("keep/a.bin"), "user.diskwise-test", b"keeper").unwrap();
        xattr::set(&copy, "user.diskwise-test", b"copy").unwrap();
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o640)).unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        fs::File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old).set_accessed(old))
            .unwrap();

        let mut snap = scan(&d);
        let t = target(&d, "keep/a.bin", "copy/a.bin", false);
        let done = apply(&snap, &protection(), &t, How::StoreOnce, &no_trash).unwrap();
        assert_eq!(done.cloned, 1);
        record(&mut snap, &t, How::StoreOnce);

        assert_eq!(fs::read(&copy).unwrap(), a);
        assert_eq!(
            xattr::get(&copy, "user.diskwise-test").unwrap().unwrap(),
            b"copy"
        );
        let m = fs::metadata(&copy).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o640);
        assert_eq!(m.modified().unwrap(), old);
        assert!(
            fs::read_dir(d.join("copy")).unwrap().count() == 1,
            "no temp file left"
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            private_size(&copy),
            Some(0),
            "now shares the keeper's storage"
        );

        // The report now sees nothing to gain, and a second store once does nothing.
        let r = find(&snap, None, None, &Prefs::default());
        assert!(r.groups.is_empty(), "{:?}", r.groups);
        let again = apply(&snap, &protection(), &t, How::StoreOnce, &no_trash).unwrap();
        assert_eq!(again.already_shared, 1);
        fs::remove_dir_all(&d).unwrap();
    }

    /// Read-only copies, and a keeper that is hidden and locked in Finder:
    /// the clone must end up with the copy's mode and flags, not the keeper's.
    #[cfg(target_os = "macos")]
    #[test]
    fn store_once_read_only_and_flags() {
        use std::os::darwin::fs::MetadataExt;
        let d = dir("flags");
        let a = content(9, 2 << 20);
        write(&d.join("keep/a.bin"), &a);
        write(&d.join("copy/a.bin"), &a);
        let copy = d.join("copy/a.bin");
        let keep = d.join("keep/a.bin");
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o444)).unwrap();
        let chflags = |flag: &str, p: &Path| {
            assert!(
                std::process::Command::new("chflags")
                    .arg(flag)
                    .arg(p)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        chflags("hidden", &keep);
        chflags("uchg", &keep);
        let snap = scan(&d);
        let t = target(&d, "keep/a.bin", "copy/a.bin", false);
        let result = apply(&snap, &protection(), &t, How::StoreOnce, &no_trash);
        chflags("nouchg", &keep); // so the test folder can be removed
        assert_eq!(result.unwrap().cloned, 1);
        let m = fs::symlink_metadata(&copy).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o444);
        assert_eq!(m.st_flags(), 0, "not hidden or locked like the keeper");
        assert_eq!(private_size(&copy), Some(0));
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_when_no_longer_identical_or_changed() {
        let d = dir("refuse");
        let a = content(2, 2 << 20);
        write(&d.join("keep/a.bin"), &a);
        write(&d.join("copy/a.bin"), &a);
        let snap = scan(&d);
        let t = target(&d, "keep/a.bin", "copy/a.bin", false);
        // The keeper is edited after the scan: the copy may be the last good one.
        let mut b = a.clone();
        b[10] ^= 1;
        write(&d.join("keep/a.bin"), &b);
        let err = apply(&snap, &protection(), &t, How::Trash, &no_trash).unwrap_err();
        assert!(
            err.contains("changed") || err.contains("identical"),
            "{err}"
        );
        // Keeper and copy the same path, or nested: refused.
        let same = target(&d, "keep/a.bin", "keep/a.bin", false);
        assert!(check(&snap, &protection(), &same).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn trash_a_folder_copy_and_record_it() {
        let d = dir("trashfolder");
        let a = content(3, 2 << 20);
        write(&d.join("A/FLC/x.mov"), &a);
        write(&d.join("A/FLC/note.txt"), b"n");
        write(&d.join("B/FLC/x.mov"), &a);
        write(&d.join("B/FLC/note.txt"), b"n");
        let mut snap = scan(&d);
        let t = target(&d, "A/FLC", "B/FLC", true);
        let trashed = std::cell::RefCell::new(Vec::new());
        let fake = |p: &Path| {
            trashed.borrow_mut().push(p.to_path_buf());
            fs::remove_dir_all(p).map_err(|e| e.to_string())
        };
        apply(&snap, &protection(), &t, How::Trash, &fake).unwrap();
        assert_eq!(trashed.borrow().as_slice(), &[d.join("B/FLC")]);
        record(&mut snap, &t, How::Trash);
        assert!(snap.large_files.iter().all(|f| !f.path.contains("/B/FLC")));
        assert!(snap.node(&t.copy).is_none());
        // A folder that differs is refused.
        write(&d.join("C/FLC/x.mov"), &a);
        write(&d.join("C/FLC/note.txt"), b"m");
        let snap = scan(&d);
        let t = target(&d, "A/FLC", "C/FLC", true);
        assert!(check(&snap, &protection(), &t).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn store_once_a_folder() {
        let d = dir("storefolder");
        if !crate::platform::can_clone(&d) {
            return;
        }
        let a = content(4, 2 << 20);
        write(&d.join("A/P/x.mov"), &a);
        write(&d.join("A/P/sub/y.txt"), b"small");
        write(&d.join("B/P/x.mov"), &a);
        write(&d.join("B/P/sub/y.txt"), b"small");
        let snap = scan(&d);
        let t = target(&d, "A/P", "B/P", true);
        let done = apply(&snap, &protection(), &t, How::StoreOnce, &no_trash).unwrap();
        assert_eq!(done.cloned, 2);
        assert_eq!(fs::read(d.join("B/P/sub/y.txt")).unwrap(), b"small");
        fs::remove_dir_all(&d).unwrap();
    }
}
