//! The Duplicates tab: checked in the background after the scan, then the user
//! picks, per copy, what happens to it (and which copy is kept).

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use diskwise_core::dupes::act::{self, How};
use diskwise_core::dupes::{self, Action, Copy, DupeProgress, Prefs, prefs};

use super::{App, Modal, ancestors, step};

/// A duplicated folder or file, as listed in the tab.
#[derive(Debug, Clone)]
pub struct DupeGroup {
    pub folder: bool,
    /// Files in one copy (1 for a file).
    pub files: u64,
    /// Size of one copy.
    pub size: u64,
    /// The keeper first.
    pub copies: Vec<Copy>,
    pub reclaimable: u64,
}

pub enum DupeState {
    NotStarted,
    Checking {
        progress: Arc<DupeProgress>,
        rx: mpsc::Receiver<dupes::Report>,
    },
    Ready {
        /// Groups whose copies already share storage (nothing to gain).
        shared: usize,
        /// Groups the user chose to keep as they are.
        kept: usize,
    },
    Unavailable(String),
}

/// One confirmed step (shared with the desktop app).
pub use act::Step as PlanItem;

impl App {
    /// Start (or restart) checking for duplicates in the background.
    pub fn start_dupes(&mut self) {
        if !self.live {
            self.dupe_state = DupeState::Unavailable(
                "Duplicates are checked on a fresh scan only: press r to scan again.".into(),
            );
            return;
        }
        if self.snap.large_files.is_empty() && self.snap.version < 5 {
            self.dupe_state =
                DupeState::Unavailable("This scan has no duplicate candidates.".into());
            return;
        }
        let progress = Arc::new(DupeProgress::default());
        let (tx, rx) = mpsc::channel();
        let snap = self.snap.clone();
        let prefs = self.prefs.clone();
        let cache = self.dupe_cache.clone();
        let p = progress.clone();
        std::thread::spawn(move || {
            let report = dupes::find(&snap, cache.as_deref(), Some(p), &prefs);
            let _ = tx.send(report);
        });
        self.dupe_state = DupeState::Checking { progress, rx };
    }

    pub fn dupes_checking(&self) -> bool {
        matches!(self.dupe_state, DupeState::Checking { .. })
    }

    /// Take the background result if it's ready. True if something changed.
    pub fn poll_dupes(&mut self) -> bool {
        let DupeState::Checking { rx, .. } = &self.dupe_state else {
            return false;
        };
        match rx.try_recv() {
            Ok(report) => {
                self.set_dupes(report);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.dupe_state =
                    DupeState::Unavailable("Checking for duplicates stopped unexpectedly.".into());
                true
            }
        }
    }

    pub fn set_dupes(&mut self, r: dupes::Report) {
        // Keep the selection on the same group if it's still there.
        let selected = self.selected_group().map(|g| g.copies[0].path.clone());
        let mut groups: Vec<DupeGroup> = r
            .folders
            .into_iter()
            .map(|g| DupeGroup {
                folder: true,
                files: g.files,
                size: g.size,
                copies: g.copies,
                reclaimable: g.reclaimable,
            })
            .collect();
        groups.extend(r.groups.into_iter().map(|g| DupeGroup {
            folder: false,
            files: 1,
            size: g.size,
            copies: g.copies,
            reclaimable: g.reclaimable,
        }));
        self.dupe_groups = groups;
        self.dupe_state = DupeState::Ready {
            shared: r.already_shared.len(),
            kept: r.kept.len() + r.kept_folders.len(),
        };
        self.dupe_sel = selected
            .and_then(|p| self.dupe_groups.iter().position(|g| g.copies[0].path == p))
            .unwrap_or(0)
            .min(self.dupe_groups.len().saturating_sub(1));
        self.copy_sel = 0;
    }

    pub fn selected_group(&self) -> Option<&DupeGroup> {
        self.dupe_groups.get(self.dupe_sel)
    }

    pub fn dupes_reclaimable(&self) -> u64 {
        self.dupe_groups.iter().map(|g| g.reclaimable).sum()
    }

    pub fn move_dupe(&mut self, delta: isize) {
        self.dupe_sel = step(self.dupe_sel, delta, self.dupe_groups.len());
        self.copy_sel = 0;
    }

    pub fn move_copy(&mut self, delta: isize) {
        let n = self.selected_group().map_or(0, |g| g.copies.len());
        self.copy_sel = step(self.copy_sel, delta, n);
    }

    fn save_prefs(&mut self) {
        if let Some(path) = &self.prefs_path
            && let Err(e) = self.prefs.save(path)
        {
            self.status = Some(format!("Couldn't save your choice: {e}"));
        }
    }

    /// Change what happens to the selected copy: Trash → Store once (where
    /// possible) → Keep as is (remembered) → Trash …
    pub fn cycle_action(&mut self) {
        let (i, sel) = (self.copy_sel, self.dupe_sel);
        let Some(g) = self.dupe_groups.get(sel) else {
            return;
        };
        if i == 0 {
            self.status = Some("This is the copy that's kept. Select another and press p to keep that one instead.".into());
            return;
        }
        let (keeper, copy) = (&g.copies[0], &g.copies[i]);
        if copy.action == Some(Action::Nothing) {
            self.status = Some(Action::Nothing.explain().into());
            return;
        }
        let options = dupes::choices(keeper, copy);
        let now = copy.action;
        let next = match options.iter().position(|a| Some(*a) == now) {
            Some(p) => options[(p + 1) % options.len()],
            None => options[0],
        };
        let path = copy.path.clone();
        let g = &mut self.dupe_groups[sel];
        g.copies[i].action = Some(next);
        g.reclaimable = dupes::reclaimable(&g.copies);
        let synced = g.copies[i].synced;
        if next == Action::Keep || now == Some(Action::Keep) {
            self.prefs.set_kept(&path, next == Action::Keep);
            self.save_prefs();
        }
        if next == Action::Trash && synced {
            self.status = Some("This copy is in a synced folder: moving it to the Trash removes it on every synced device too.".into());
        }
    }

    /// Keep the selected copy instead, and offer to remember the choice for
    /// the two folders involved.
    pub fn make_keeper(&mut self) {
        let (i, sel) = (self.copy_sel, self.dupe_sel);
        if i == 0 || sel >= self.dupe_groups.len() {
            return;
        }
        let previous = self.dupe_groups[sel].copies[0].path.clone();
        let g = &mut self.dupe_groups[sel];
        g.reclaimable = dupes::set_keeper(&mut g.copies, i, &self.prefs, &self.snap);
        let chosen = g.copies[0].path.clone();
        self.copy_sel = 0;
        self.modal = prefs::folder_pair(&chosen, &previous)
            .map(|(prefer, over)| Modal::PreferFolder { prefer, over });
        if self.modal.is_none() {
            self.status = Some(format!("Keeping {} instead.", super::tilde_path(&chosen)));
        }
    }

    /// "Always prefer copies in `prefer` over copies in `over`": saved, then
    /// every group is checked again.
    pub fn remember_folder_order(&mut self, prefer: &str, over: &str) {
        self.prefs.add_folder_order(prefer, over);
        self.save_prefs();
        self.start_dupes();
        self.status = Some(format!(
            "From now on, copies in {} are kept over copies in {}.",
            super::tilde_path(prefer),
            super::tilde_path(over)
        ));
    }

    /// Ask which folder around the selected copy holds intentional duplicates.
    pub fn request_intentional(&mut self) {
        let Some(g) = self.selected_group() else {
            return;
        };
        let path = g.copies[self.copy_sel].path.clone();
        let mut options: Vec<String> = Vec::new();
        if g.folder {
            options.push(path.clone());
        }
        for a in ancestors(&path) {
            // Never a whole home folder or anything above it.
            if self.protection.is_home(a)
                || self.protection.reason(a).is_some()
                || !a.starts_with(&self.root_display)
            {
                break;
            }
            options.push(a.to_string());
        }
        if options.is_empty() {
            self.status = Some("There's no folder here to mark.".into());
            return;
        }
        self.modal = Some(Modal::ChooseFolder {
            options,
            selected: 0,
        });
    }

    pub fn mark_intentional(&mut self, folder: &str) {
        self.prefs.set_intentional(folder, true);
        self.save_prefs();
        self.start_dupes();
        let file = self
            .prefs_path
            .as_ref()
            .map(|p| {
                format!(
                    " (to undo, remove it from {})",
                    super::tilde_path(&p.display().to_string())
                )
            })
            .unwrap_or_default();
        self.status = Some(format!(
            "Duplicates in {} are now kept as they are{file}.",
            super::tilde_path(folder)
        ));
    }

    /// Ask to carry out the selected group's chosen actions.
    pub fn request_dupe_actions(&mut self) {
        if !self.live {
            self.modal = Some(Modal::Message {
                title: "Scan again first".into(),
                body: "This is a saved scan, so diskwise won't change anything from it. Press r to scan again.".into(),
            });
            return;
        }
        let Some(g) = self.selected_group() else {
            return;
        };
        let plan = act::plan(&g.copies, g.folder);
        if plan.steps.is_empty() {
            self.modal = Some(Modal::Message {
                title: "Nothing chosen yet".into(),
                body: if plan.undecided > 0 {
                    "The extra copies here are your call: select one with ← → and press space to choose what happens to it.".into()
                } else {
                    "Nothing in this group is set to change.".into()
                },
            });
            return;
        }
        let warning = plan.warning();
        let plan = plan.steps;
        self.modal = Some(Modal::ConfirmDupes { plan, warning });
    }

    /// Carry out a confirmed plan, checking each step again right before it.
    pub fn apply_dupes(
        &mut self,
        plan: &[PlanItem],
        trasher: impl Fn(&Path) -> Result<(), String>,
    ) -> String {
        let (mut freed, mut trashed, mut stored) = (0u64, 0, 0);
        let mut failures = Vec::new();
        for item in plan {
            match act::apply(
                &self.snap,
                &self.protection,
                &item.target,
                item.how,
                &trasher,
            ) {
                Ok(_) => {
                    if item.how == How::Trash {
                        self.remove_from_tree(&item.target.copy);
                        self.remove_from_findings(&item.target.copy, item.bytes);
                        trashed += 1;
                    } else {
                        stored += 1;
                    }
                    act::record(&mut self.snap, &item.target, item.how);
                    freed += item.bytes;
                }
                Err(e) => failures.push(e),
            }
        }
        self.reindex();
        self.selected = self.selected.min(self.entries().len().saturating_sub(1));
        self.start_dupes();
        let mut parts = Vec::new();
        if trashed > 0 {
            parts.push(format!("moved {trashed} copy(ies) to the Trash"));
        }
        if stored > 0 {
            parts.push(format!("stored {stored} copy(ies) once"));
        }
        let mut msg = if parts.is_empty() {
            "Nothing was changed.".to_string()
        } else {
            let mut m = parts.join(" and ");
            m[..1].make_ascii_uppercase();
            let when = if trashed > 0 {
                " (Trash space is freed when the Trash is emptied)"
            } else {
                ""
            };
            format!("{m}: {}{when}.", diskwise_core::human_bytes(freed))
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

    /// Show the selected copy (or the nearest folder of it that's listed) in Browse.
    pub fn reveal_copy(&mut self) {
        let Some(g) = self.selected_group() else {
            return;
        };
        let path = g.copies[self.copy_sel].path.clone();
        let shown = std::iter::once(path.as_str())
            .chain(ancestors(&path))
            .find(|p| self.locate(p).is_some_and(|i| !i.is_empty()))
            .map(String::from);
        match shown {
            Some(p) => self.reveal(&p),
            None => self.status = Some(format!("{path} isn't in the scanned tree.")),
        }
    }
}

/// Where preferences are saved (none in tests, so they never touch the real file).
pub fn prefs_location() -> Option<PathBuf> {
    if cfg!(test) {
        None
    } else {
        Prefs::default_path()
    }
}

/// The hash cache (none in tests).
pub fn cache_location() -> Option<PathBuf> {
    if cfg!(test) {
        None
    } else {
        dupes::default_cache_path()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Tab;
    use super::*;
    use std::io::Write;

    fn dir(tag: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("tui-dupes-{tag}-{}", std::process::id()));
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

    fn app_for(d: &Path) -> App {
        let opts = diskwise_core::ScanOptions {
            min_node_size: 0,
            ..Default::default()
        };
        let snap = diskwise_core::snapshot(d, opts, false).unwrap();
        let mut app = App::new(snap, vec![]);
        let report = dupes::find(&app.snap, None, None, &app.prefs.clone());
        app.set_dupes(report);
        app.tab = Tab::Duplicates;
        app
    }

    #[test]
    fn choose_and_carry_out() {
        let d = dir("carry");
        let a: Vec<u8> = (0..(2u32 << 20)).map(|i| (i % 251) as u8).collect();
        write(&d.join("Docs/a.bin"), &a);
        write(&d.join("Docs/a copy.bin"), &a);
        write(&d.join("Other/a.bin"), &a);
        write(&d.join("Docs/notes.txt"), b"only here");
        write(&d.join("Other/more.txt"), b"different");
        let mut app = app_for(&d);
        assert_eq!(app.dupe_groups.len(), 1);
        let g = app.selected_group().unwrap().clone();
        assert!(g.copies[0].path.ends_with("/Docs/a.bin"), "{:?}", g.copies);
        let named = g
            .copies
            .iter()
            .position(|c| c.path.ends_with("a copy.bin"))
            .unwrap();
        assert_eq!(g.copies[named].action, Some(Action::Trash));

        // The other copy: in doubt. Choose Trash for it with space.
        let other = g
            .copies
            .iter()
            .position(|c| c.path.ends_with("Other/a.bin"))
            .unwrap();
        app.copy_sel = other;
        let before = app.selected_group().unwrap().copies[other].action;
        app.cycle_action();
        let after = app.selected_group().unwrap().copies[other].action;
        assert_ne!(before, after);
        while app.selected_group().unwrap().copies[other].action != Some(Action::Trash) {
            app.cycle_action();
        }

        app.request_dupe_actions();
        let Some(Modal::ConfirmDupes { plan, .. }) = app.modal.take() else {
            panic!("expected a confirmation");
        };
        assert_eq!(plan.len(), 2);
        let trashed = std::cell::RefCell::new(Vec::new());
        let msg = app.apply_dupes(&plan, |p: &Path| {
            trashed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_file(p).map_err(|e| e.to_string())
        });
        assert!(msg.starts_with("Moved 2 copy(ies) to the Trash"), "{msg}");
        assert_eq!(trashed.borrow().len(), 2);
        assert!(d.join("Docs/a.bin").exists(), "the keeper stays");
        // The candidate list no longer has the trashed copies.
        assert!(
            app.snap
                .large_files
                .iter()
                .all(|f| f.path.ends_with("Docs/a.bin"))
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn swapping_keeper_offers_folder_preference() {
        let d = dir("swap");
        let a: Vec<u8> = (0..(2u32 << 20)).map(|i| (i % 241) as u8).collect();
        write(&d.join("Projects/x/a.bin"), &a);
        write(&d.join("Projects/x/notes.txt"), b"n");
        write(&d.join("Downloads/a.bin"), &a);
        write(&d.join("Downloads/other.txt"), b"o");
        let mut app = app_for(&d);
        let keeper = app.selected_group().unwrap().copies[0].path.clone();
        app.copy_sel = 1;
        app.make_keeper();
        assert_ne!(app.selected_group().unwrap().copies[0].path, keeper);
        assert!(
            matches!(app.modal, Some(Modal::PreferFolder { .. })),
            "{:?}",
            app.modal
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn saved_scans_are_never_acted_on() {
        let d = dir("saved");
        let a: Vec<u8> = vec![7; 2 << 20];
        write(&d.join("A/a.bin"), &a);
        write(&d.join("A/x.txt"), b"x");
        write(&d.join("B/a.bin"), &a);
        let mut app = app_for(&d);
        app.live = false;
        app.request_dupe_actions();
        assert!(matches!(app.modal, Some(Modal::Message { .. })));
        app.modal = None;
        app.start_dupes();
        assert!(matches!(app.dupe_state, DupeState::Unavailable(_)));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
