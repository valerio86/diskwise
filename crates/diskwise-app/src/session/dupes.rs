//! The Duplicates section: state held in the session, checked in the
//! background (main.rs runs `dupes::find` on a copy of the snapshot and hands
//! the report back), and the user's choices per copy.

use std::path::{Path, PathBuf};

use diskwise_core::dupes::act::{self, How, Step};
use diskwise_core::dupes::{self, Action, Copy, Prefs, Report, Shared, prefs};
use diskwise_core::human_bytes;
use serde::Serialize;

use super::{Session, TrashResult, ancestors};

pub enum DupeStatus {
    NotStarted,
    Checking,
    Ready { shared: usize, kept: usize },
    Unavailable(String),
}

/// A duplicated folder or file, with the user's choices so far.
pub struct GroupState {
    /// Stable while the group is listed (the keeper can change).
    pub id: String,
    pub folder: bool,
    pub files: u64,
    pub size: u64,
    pub copies: Vec<Copy>,
    pub reclaimable: u64,
}

/// Everything needed to check in the background.
pub struct DupeJob {
    pub generation: u64,
    pub snap: diskwise_core::Snapshot,
    pub prefs: Prefs,
    pub cache: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct CopyView {
    pub path: String,
    pub frees: u64,
    /// "keep" for the kept copy, else the action's name (snake_case).
    pub action: String,
    pub label: String,
    pub explain: String,
    /// What the user can switch this copy to.
    pub choices: Vec<Choice>,
    pub synced: bool,
    /// "hard_link" or "clone" when it shares storage.
    pub shared: Option<&'static str>,
}

#[derive(Serialize)]
pub struct Choice {
    pub action: Action,
    pub label: &'static str,
}

#[derive(Serialize)]
pub struct GroupView {
    pub id: String,
    pub folder: bool,
    pub files: u64,
    pub size: u64,
    pub reclaimable: u64,
    pub copies: Vec<CopyView>,
    /// Copies still marked "your call".
    pub undecided: usize,
    /// Steps "Do it" would carry out.
    pub steps: usize,
}

#[derive(Serialize)]
pub struct DupesView {
    /// "not_started", "checking", "ready" or "unavailable".
    pub status: &'static str,
    pub message: Option<String>,
    pub reclaimable: u64,
    pub shared: usize,
    pub kept: usize,
    pub groups: Vec<GroupView>,
}

/// What "Do it" would do for a group, or why it can't.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DupePlan {
    Confirm {
        keeper: String,
        steps: Vec<Step>,
        total: u64,
        warning: Option<String>,
    },
    Refuse {
        title: String,
        body: String,
    },
}

/// After the user picks another keeper: an offer to remember it.
#[derive(Serialize)]
pub struct KeeperResult {
    pub offer: Option<FolderOffer>,
}

#[derive(Serialize)]
pub struct FolderOffer {
    pub prefer: String,
    pub over: String,
}

impl Session {
    /// Prepare a background check, or say why there won't be one.
    pub fn begin_dupes(&mut self) -> Option<DupeJob> {
        self.dupe_generation += 1;
        if !self.live {
            self.dupe_status = DupeStatus::Unavailable(
                "Duplicates are checked on a fresh scan only. Scan again to see them.".into(),
            );
            return None;
        }
        self.dupe_status = DupeStatus::Checking;
        Some(DupeJob {
            generation: self.dupe_generation,
            snap: self.snap.clone(),
            prefs: self.prefs.clone(),
            cache: self.dupe_cache.clone(),
        })
    }

    /// Take a finished check, unless a newer one has started since.
    pub fn finish_dupes(&mut self, generation: u64, r: Report) {
        if generation != self.dupe_generation {
            return;
        }
        let groups = r
            .folders
            .into_iter()
            .map(|g| (true, g.files, g.size, g.copies, g.reclaimable))
            .chain(
                r.groups
                    .into_iter()
                    .map(|g| (false, 1, g.size, g.copies, g.reclaimable)),
            );
        self.dupe_groups = groups
            .map(|(folder, files, size, copies, reclaimable)| GroupState {
                id: copies
                    .iter()
                    .map(|c| c.path.as_str())
                    .min()
                    .unwrap_or("")
                    .to_string(),
                folder,
                files,
                size,
                copies,
                reclaimable,
            })
            .collect();
        self.dupe_status = DupeStatus::Ready {
            shared: r.already_shared.len(),
            kept: r.kept.len() + r.kept_folders.len(),
        };
    }

    pub fn dupes_view(&self) -> DupesView {
        let (status, message, shared, kept) = match &self.dupe_status {
            DupeStatus::NotStarted => ("not_started", None, 0, 0),
            DupeStatus::Checking => ("checking", None, 0, 0),
            DupeStatus::Ready { shared, kept } => ("ready", None, *shared, *kept),
            DupeStatus::Unavailable(m) => ("unavailable", Some(m.clone()), 0, 0),
        };
        DupesView {
            status,
            message,
            reclaimable: self.dupe_groups.iter().map(|g| g.reclaimable).sum(),
            shared,
            kept,
            groups: self.dupe_groups.iter().map(group_view).collect(),
        }
    }

    fn group_mut(&mut self, id: &str) -> Result<&mut GroupState, String> {
        self.dupe_groups
            .iter_mut()
            .find(|g| g.id == id)
            .ok_or_else(|| "That group of duplicates isn't listed any more.".to_string())
    }

    fn save_prefs(&self) -> Result<(), String> {
        match &self.prefs_path {
            Some(p) => self
                .prefs
                .save(p)
                .map_err(|e| format!("Couldn't save your choice: {e}")),
            None => Ok(()),
        }
    }

    /// Set what happens to one extra copy (only to one of its `choices`).
    pub fn dupe_set_action(&mut self, id: &str, path: &str, action: Action) -> Result<(), String> {
        let g = self.group_mut(id)?;
        let i = g
            .copies
            .iter()
            .position(|c| c.path == path)
            .ok_or("That copy isn't in the group.")?;
        if i == 0 {
            return Err("That's the copy being kept.".into());
        }
        if !dupes::choices(&g.copies[0], &g.copies[i]).contains(&action) {
            return Err("That isn't possible for this copy.".into());
        }
        let was = g.copies[i].action;
        g.copies[i].action = Some(action);
        g.reclaimable = dupes::reclaimable(&g.copies);
        if action == Action::Keep || was == Some(Action::Keep) {
            self.prefs.set_kept(path, action == Action::Keep);
            self.save_prefs()?;
        }
        Ok(())
    }

    /// Keep `path` instead of the current keeper; offers to remember it for the folders.
    pub fn dupe_set_keeper(&mut self, id: &str, path: &str) -> Result<KeeperResult, String> {
        let gi = self
            .dupe_groups
            .iter()
            .position(|g| g.id == id)
            .ok_or("That group of duplicates isn't listed any more.")?;
        // Distinct fields: the group is changed, the snapshot and prefs only read.
        let g = &mut self.dupe_groups[gi];
        let i = g
            .copies
            .iter()
            .position(|c| c.path == path)
            .ok_or("That copy isn't in the group.")?;
        if i == 0 {
            return Ok(KeeperResult { offer: None });
        }
        let previous = g.copies[0].path.clone();
        g.reclaimable = dupes::set_keeper(&mut g.copies, i, &self.prefs, &self.snap);
        Ok(KeeperResult {
            offer: prefs::folder_pair(path, &previous)
                .map(|(prefer, over)| FolderOffer { prefer, over }),
        })
    }

    pub fn dupe_prefer_folder(&mut self, prefer: &str, over: &str) -> Result<(), String> {
        self.prefs.add_folder_order(prefer, over);
        self.save_prefs()
    }

    /// Folders around a copy that could be marked as holding intentional duplicates.
    pub fn dupe_folder_options(&self, id: &str, path: &str) -> Vec<String> {
        let folder = self
            .dupe_groups
            .iter()
            .find(|g| g.id == id)
            .is_some_and(|g| g.folder);
        let root = self.snap.display_root();
        let mut out = Vec::new();
        if folder {
            out.push(path.to_string());
        }
        for a in ancestors(path) {
            if self.protection.is_home(a)
                || self.protection.reason(a).is_some()
                || !a.starts_with(&root)
            {
                break;
            }
            out.push(a.to_string());
        }
        out
    }

    pub fn dupe_mark_intentional(&mut self, folder: &str) -> Result<(), String> {
        self.prefs.set_intentional(folder, true);
        self.save_prefs()
    }

    pub fn dupe_plan(&self, id: &str) -> DupePlan {
        if !self.live {
            return DupePlan::Refuse {
                title: "Scan again first".into(),
                body: "This is a saved scan, so diskwise won't change anything from it.".into(),
            };
        }
        let Some(g) = self.dupe_groups.iter().find(|g| g.id == id) else {
            return DupePlan::Refuse {
                title: "Not listed any more".into(),
                body: "That group of duplicates has changed. Look at the list again.".into(),
            };
        };
        let plan = act::plan(&g.copies, g.folder);
        if plan.steps.is_empty() {
            return DupePlan::Refuse {
                title: "Nothing chosen yet".into(),
                body: if plan.undecided > 0 {
                    "The extra copies here are your call: choose what happens to each one first."
                        .into()
                } else {
                    "Nothing in this group is set to change.".into()
                },
            };
        }
        DupePlan::Confirm {
            keeper: g.copies[0].path.clone(),
            total: plan.total(),
            warning: plan.warning(),
            steps: plan.steps,
        }
    }

    /// Carry out a group's chosen actions (worked out again here: the UI isn't
    /// trusted), each checked right before it runs.
    pub fn dupe_apply(
        &mut self,
        id: &str,
        trasher: &dyn Fn(&Path) -> Result<(), String>,
    ) -> TrashResult {
        let steps = match self.dupe_plan(id) {
            DupePlan::Confirm { steps, .. } => steps,
            DupePlan::Refuse { body, .. } => {
                return TrashResult {
                    moved: 0,
                    bytes: 0,
                    failures: vec![body],
                    message: "Nothing was changed.".into(),
                };
            }
        };
        let (mut trashed, mut stored, mut freed) = (0, 0, 0u64);
        let mut failures = Vec::new();
        for s in &steps {
            match act::apply(&self.snap, &self.protection, &s.target, s.how, trasher) {
                Ok(_) => {
                    act::record(&mut self.snap, &s.target, s.how);
                    if s.how == How::Trash {
                        trashed += 1;
                    } else {
                        stored += 1;
                    }
                    freed += s.bytes;
                }
                Err(e) => failures.push(e),
            }
        }
        self.reanalyze();
        // The group is done with (what's left is re-checked in the background).
        self.dupe_groups.retain(|g| g.id != id);
        let mut parts = Vec::new();
        if trashed > 0 {
            parts.push(format!("moved {trashed} to the Trash"));
        }
        if stored > 0 {
            parts.push(format!("stored {stored} once"));
        }
        let mut message = if parts.is_empty() {
            "Nothing was changed.".to_string()
        } else {
            let mut m = format!(
                "Copies: {}. {} freed",
                parts.join(" and "),
                human_bytes(freed)
            );
            m.push_str(if trashed > 0 {
                " once the Trash is emptied."
            } else {
                "."
            });
            m
        };
        if !failures.is_empty() {
            message.push_str(&format!(" {} failed.", failures.len()));
        }
        TrashResult {
            moved: trashed + stored,
            bytes: freed,
            failures,
            message,
        }
    }
}

fn group_view(g: &GroupState) -> GroupView {
    let keeper = &g.copies[0];
    let plan = act::plan(&g.copies, g.folder);
    GroupView {
        id: g.id.clone(),
        folder: g.folder,
        files: g.files,
        size: g.size,
        reclaimable: g.reclaimable,
        undecided: plan.undecided,
        steps: plan.steps.len(),
        copies: g
            .copies
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let (action, label, explain) = match c.action.filter(|_| i > 0) {
                    None => (
                        "keep".to_string(),
                        "Keep".to_string(),
                        "This copy stays as it is.".to_string(),
                    ),
                    Some(a) => (
                        serde_json::to_value(a)
                            .ok()
                            .and_then(|v| v.as_str().map(String::from))
                            .unwrap_or_default(),
                        a.label().to_string(),
                        a.explain().to_string(),
                    ),
                };
                CopyView {
                    path: c.path.clone(),
                    frees: c.frees,
                    action,
                    label,
                    explain,
                    choices: if i == 0 {
                        Vec::new()
                    } else {
                        dupes::choices(keeper, c)
                            .into_iter()
                            .map(|a| Choice {
                                action: a,
                                label: a.label(),
                            })
                            .collect()
                    },
                    synced: c.synced,
                    shared: c.shared.map(|s| match s {
                        Shared::HardLink => "hard_link",
                        Shared::Clone => "clone",
                    }),
                }
            })
            .collect(),
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
    use super::*;
    use diskwise_core::RuleSet;
    use std::io::Write;

    fn session_for(tag: &str) -> (Session, PathBuf) {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("app-dupes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let a: Vec<u8> = (0..(2u32 << 20)).map(|i| (i % 239) as u8).collect();
        for (p, bytes) in [
            ("Docs/a.bin", &a[..]),
            ("Docs/a copy.bin", &a[..]),
            ("Other/a.bin", &a[..]),
            ("Docs/notes.txt", b"only here"),
            ("Other/more.txt", b"different"),
        ] {
            let path = d.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(path)
                .unwrap()
                .write_all(bytes)
                .unwrap();
        }
        let d = std::fs::canonicalize(d).unwrap();
        let opts = diskwise_core::ScanOptions {
            min_node_size: 0,
            ..Default::default()
        };
        let snap = diskwise_core::snapshot(&d, opts, false).unwrap();
        let rules = RuleSet::builtin_for(snap.platform()).unwrap();
        (Session::new(snap, rules, 0, true), d)
    }

    fn check(s: &mut Session) {
        let job = s.begin_dupes().unwrap();
        let r = dupes::find(&job.snap, None, None, &job.prefs);
        s.finish_dupes(job.generation, r);
    }

    #[test]
    fn view_choices_and_apply() {
        let (mut s, d) = session_for("apply");
        check(&mut s);
        let v = s.dupes_view();
        assert_eq!(v.status, "ready");
        assert_eq!(v.groups.len(), 1);
        let g = &v.groups[0];
        assert!(g.copies[0].path.ends_with("/Docs/a.bin"));
        assert_eq!(g.copies[0].action, "keep");
        let named = g
            .copies
            .iter()
            .find(|c| c.path.ends_with("a copy.bin"))
            .unwrap();
        assert_eq!(named.action, "trash");
        let id = g.id.clone();
        let other = g
            .copies
            .iter()
            .find(|c| c.path.ends_with("Other/a.bin"))
            .unwrap()
            .path
            .clone();

        // Only offered choices are accepted; the keeper can't be given one.
        assert!(
            s.dupe_set_action(&id, &g.copies[0].path, Action::Trash)
                .is_err()
        );
        assert!(s.dupe_set_action(&id, &other, Action::Review).is_err());
        s.dupe_set_action(&id, &other, Action::Trash).unwrap();

        let DupePlan::Confirm { steps, .. } = s.dupe_plan(&id) else {
            panic!("expected a plan");
        };
        assert_eq!(steps.len(), 2);
        let fake = |p: &Path| std::fs::remove_file(p).map_err(|e| e.to_string());
        let result = s.dupe_apply(&id, &fake);
        assert_eq!(result.moved, 2, "{:?}", result.failures);
        assert!(d.join("Docs/a.bin").exists());
        assert!(!d.join("Other/a.bin").exists());
        // The candidate list follows.
        check(&mut s);
        assert!(s.dupes_view().groups.is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn stale_results_and_saved_scans() {
        let (mut s, d) = session_for("stale");
        let old = s.begin_dupes().unwrap();
        let newer = s.begin_dupes().unwrap();
        // The older check finishes last: ignored.
        let r = dupes::find(&newer.snap, None, None, &newer.prefs);
        s.finish_dupes(newer.generation, r);
        let n = s.dupes_view().groups.len();
        s.finish_dupes(old.generation, Report::default());
        assert_eq!(s.dupes_view().groups.len(), n);

        s.live = false;
        assert!(s.begin_dupes().is_none());
        assert_eq!(s.dupes_view().status, "unavailable");
        assert!(matches!(s.dupe_plan("x"), DupePlan::Refuse { .. }));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn keeper_swap_offers_folders() {
        let (mut s, d) = session_for("swap");
        check(&mut s);
        let g = &s.dupes_view().groups[0];
        let (id, other) = (
            g.id.clone(),
            g.copies
                .iter()
                .find(|c| c.path.ends_with("Other/a.bin"))
                .unwrap()
                .path
                .clone(),
        );
        let r = s.dupe_set_keeper(&id, &other).unwrap();
        let offer = r.offer.unwrap();
        assert!(offer.prefer.ends_with("/Other") && offer.over.ends_with("/Docs"));
        assert!(
            s.dupes_view().groups[0].copies[0]
                .path
                .ends_with("Other/a.bin")
        );
        std::fs::remove_dir_all(&d).unwrap();
    }
}
