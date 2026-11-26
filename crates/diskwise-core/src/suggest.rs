//! Turn a snapshot into findings: rule matches on the tree plus system-level checks.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{Node, NodeKind, Snapshot};
use crate::provenance::download_hosts;
use crate::rules::{Action, Category, RuleSet, Safety};
use crate::{DEFAULT_ROOT, human_bytes};

const GB: u64 = 1_000_000_000;
/// Preboot above this is worth explaining.
const PREBOOT_NOTE_THRESHOLD: u64 = 10 * GB;
/// Unscanned Data-volume space above this is worth explaining.
const GAP_NOTE_THRESHOLD: u64 = 5 * GB;
const SWAP_NOTE_THRESHOLD: u64 = 4 * GB;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub path: String,
    pub bytes: u64,
    pub newest_mtime: i64,
    pub is_dir: bool,
    /// Host the file was downloaded from, if macOS recorded one. Never the full URL,
    /// which can contain purchase keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub category: Category,
    pub safety: Safety,
    pub action: Action,
    pub why: String,
    pub how: String,
    pub prevent: Option<String>,
    /// Total bytes involved. For `Action::Info` this is space explained, not reclaimable.
    pub bytes: u64,
    /// False when macOS doesn't report the size (e.g. snapshots).
    pub size_known: bool,
    /// Matched paths, largest first. Empty for system findings.
    pub items: Vec<Item>,
}

impl Finding {
    /// Whether acting on this finding frees space (as opposed to explaining it).
    pub fn reclaimable(&self) -> bool {
        self.action != Action::Info && self.size_known
    }
}

/// Analyse a snapshot. `now` is Unix seconds (passed in for testability).
pub fn analyze(snap: &Snapshot, rules: &RuleSet, now: i64) -> Vec<Finding> {
    let mut by_rule: HashMap<usize, Finding> = HashMap::new();
    walk(
        &snap.tree,
        &display_root(&snap.root),
        rules,
        now,
        &mut by_rule,
    );

    let mut findings: Vec<Finding> = by_rule.into_values().collect();
    for f in &mut findings {
        f.items.sort_by_key(|i| std::cmp::Reverse(i.bytes));
    }
    findings.extend(system_findings(snap));
    findings.sort_by(|a, b| {
        (a.safety, std::cmp::Reverse(a.bytes)).cmp(&(b.safety, std::cmp::Reverse(b.bytes)))
    });
    findings
}

/// The Data volume is mounted at /System/Volumes/Data but its folders appear at `/`
/// through firmlinks; show (and act on) the familiar paths.
fn display_root(root: &str) -> String {
    match root.strip_prefix(DEFAULT_ROOT) {
        Some("") => "/".to_string(),
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        _ => root.to_string(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn walk(node: &Node, path: &str, rules: &RuleSet, now: i64, out: &mut HashMap<usize, Finding>) {
    for child in &node.children {
        if child.kind == NodeKind::Rest {
            continue;
        }
        let child_path = join(path, &child.name);
        match rules.rule_for(&child_path, child, now) {
            Some((idx, rule)) => {
                let source = if child.kind == NodeKind::File {
                    download_hosts(Path::new(&child_path)).into_iter().next()
                } else {
                    None
                };
                let d = &rule.def;
                let f = out.entry(idx).or_insert_with(|| Finding {
                    id: d.id.clone(),
                    title: d.title.clone(),
                    category: d.category,
                    safety: d.safety,
                    action: d.action,
                    why: d.why.clone(),
                    how: d.how.clone(),
                    prevent: d.prevent.clone(),
                    bytes: 0,
                    size_known: true,
                    items: Vec::new(),
                });
                f.bytes += child.allocated;
                f.items.push(Item {
                    path: child_path,
                    bytes: child.allocated,
                    newest_mtime: child.newest_mtime,
                    is_dir: child.kind == NodeKind::Dir,
                    source,
                });
            }
            None => walk(child, &child_path, rules, now, out),
        }
    }
}

fn info(id: &str, title: &str, bytes: u64, size_known: bool, why: String, how: &str) -> Finding {
    Finding {
        id: id.to_string(),
        title: title.to_string(),
        category: Category::System,
        safety: Safety::Review,
        action: Action::Info,
        why,
        how: how.to_string(),
        prevent: None,
        bytes,
        size_known,
        items: Vec::new(),
    }
}

fn system_findings(snap: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(sys) = &snap.system else { return out };

    if let Some(c) = &sys.container {
        let volume = |role: &str| c.volumes.iter().find(|v| v.roles.iter().any(|r| r == role));

        if let Some(pre) = volume("Preboot").filter(|v| v.used > PREBOOT_NOTE_THRESHOLD) {
            out.push(info(
                "preboot-large",
                "Large Preboot volume",
                pre.used,
                true,
                format!(
                    "Preboot holds the files macOS needs to start up, for each installed macOS \
                     version. It's using {}, more than usual, which typically means leftover \
                     data from OS updates.",
                    human_bytes(pre.used)
                ),
                "Don't change it by hand. Install any pending macOS update and restart; \
                 Preboot usually shrinks once updates finish.",
            ));
        }

        if let Some(data) = volume("Data")
            && snap.root == DEFAULT_ROOT
        {
            let gap = data.used.saturating_sub(snap.tree.allocated);
            if gap > GAP_NOTE_THRESHOLD {
                let fda = if snap.errors.count > 0 {
                    format!(
                        " The scan couldn't read {} folders; granting Full Disk Access \
                         would show some of this.",
                        snap.errors.count
                    )
                } else {
                    String::new()
                };
                out.push(info(
                    "unscanned-space",
                    "Space the scan can't see",
                    gap,
                    true,
                    format!(
                        "The Data volume uses {} but scanning its files found {}. The \
                         difference is local snapshots, purgeable space macOS frees on \
                         demand, and protected folders.{fda}",
                        human_bytes(data.used),
                        human_bytes(snap.tree.allocated)
                    ),
                    "Check the snapshot finding below. System Settings → General → Storage \
                     shows how much macOS considers purgeable.",
                ));
            }
        }
    }

    if !sys.snapshots.is_empty() {
        let os_updates = sys
            .snapshots
            .iter()
            .filter(|s| s.contains(".os.update"))
            .count();
        let time_machine = sys
            .snapshots
            .iter()
            .filter(|s| s.contains("TimeMachine"))
            .count();
        let mut how = Vec::new();
        if os_updates > 0 {
            how.push("OS-update snapshots go away once the pending macOS update is installed and you restart.");
        }
        if time_machine > 0 {
            how.push("Time Machine snapshots are removed automatically when space runs low; to remove them now: sudo tmutil deletelocalsnapshots /");
        }
        if how.is_empty() {
            how.push("List them with `tmutil listlocalsnapshots /`.");
        }
        out.push(info(
            "local-snapshots",
            "Local APFS snapshots",
            0,
            false,
            format!(
                "{} snapshot(s) ({os_updates} from OS updates, {time_machine} from Time Machine). \
                 They keep deleted and changed files on disk until removed. macOS doesn't \
                 report their size.",
                sys.snapshots.len()
            ),
            &how.join(" "),
        ));
    }

    if let Some(swap) = sys.swap.as_ref().filter(|s| s.used > SWAP_NOTE_THRESHOLD) {
        out.push(info(
            "swap",
            "Large swap usage",
            swap.total,
            true,
            format!(
                "macOS is using {} of swap, memory moved to disk because RAM is full.",
                human_bytes(swap.used)
            ),
            "Quit memory-heavy apps or restart to release it.",
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ErrorSummary, SNAPSHOT_VERSION};
    use crate::system::{ContainerInfo, SystemInfo, VolumeInfo};

    const NOW: i64 = 1_800_000_000;

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut n = Node::new(name.into(), NodeKind::Dir);
        n.allocated = children.iter().map(|c| c.allocated).sum();
        n.newest_mtime = children.iter().map(|c| c.newest_mtime).max().unwrap_or(NOW);
        n.children = children;
        n
    }

    fn file(name: &str, size: u64, age_days: i64) -> Node {
        let mut n = Node::new(name.into(), NodeKind::File);
        n.allocated = size;
        n.newest_mtime = NOW - age_days * 86_400;
        n
    }

    fn snapshot(tree: Node, data_used: u64) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            root: DEFAULT_ROOT.into(),
            scanned_at: NOW,
            duration_ms: 0,
            min_node_size: 0,
            tree,
            errors: ErrorSummary::default(),
            system: Some(SystemInfo {
                container: Some(ContainerInfo {
                    reference: "disk3".into(),
                    capacity: 250 * GB,
                    free: 50 * GB,
                    volumes: vec![
                        VolumeInfo {
                            name: "Data".into(),
                            device: "d".into(),
                            roles: vec!["Data".into()],
                            used: data_used,
                        },
                        VolumeInfo {
                            name: "Preboot".into(),
                            device: "p".into(),
                            roles: vec!["Preboot".into()],
                            used: 18 * GB,
                        },
                    ],
                }),
                snapshots: vec!["com.apple.os.update-ABC".into()],
                swap: None,
                warnings: vec![],
            }),
        }
    }

    #[test]
    fn rules_and_system_checks() {
        let tree = dir(
            DEFAULT_ROOT,
            vec![
                dir(".adobeTemp", vec![file("a.pimx", 3 * GB, 400)]),
                dir(
                    "Users",
                    vec![dir(
                        "v",
                        vec![
                            dir(
                                "Library",
                                vec![dir(
                                    "Caches",
                                    vec![
                                        dir("Google", vec![file("c", 2 * GB, 1)]),
                                        dir("pip", vec![file("c", 300_000_000, 1)]),
                                    ],
                                )],
                            ),
                            dir("Downloads", vec![file("Old.dmg", 300_000_000, 60)]),
                        ],
                    )],
                ),
            ],
        );
        let snap = snapshot(tree, 20 * GB);
        let findings = analyze(&snap, &RuleSet::builtin().unwrap(), NOW);
        let get = |id: &str| findings.iter().find(|f| f.id == id);

        let caches = get("user-caches").expect("caches");
        assert_eq!(caches.items.len(), 2);
        assert_eq!(caches.items[0].path, "/Users/v/Library/Caches/Google");
        assert_eq!(caches.bytes, 2 * GB + 300_000_000);

        assert_eq!(
            get("adobe-installer-leftovers").unwrap().items[0].path,
            "/.adobeTemp"
        );
        assert!(get("installer-unknown-source").is_some());
        assert!(get("preboot-large").is_some());
        assert!(get("local-snapshots").is_some_and(|f| !f.size_known));
        // Tree is ~5.6 GB, Data volume uses 20 GB → gap reported.
        assert!(get("unscanned-space").is_some());

        // Safe findings come first.
        assert_eq!(findings[0].safety, Safety::Safe);
    }

    #[test]
    fn display_root_maps_data_volume() {
        assert_eq!(display_root(DEFAULT_ROOT), "/");
        assert_eq!(display_root("/System/Volumes/Data/Users/v"), "/Users/v");
        assert_eq!(display_root("/Users/v"), "/Users/v");
        assert_eq!(
            display_root("/System/Volumes/DataX"),
            "/System/Volumes/DataX"
        );
    }
}
