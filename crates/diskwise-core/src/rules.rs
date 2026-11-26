//! Rule definitions (TOML) and matching against scanned paths.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::model::{Node, NodeKind};
use crate::parse_size;
use crate::platform::{self, Platform};
use crate::provenance::{download_hosts, host_matches};

const DEFAULT_MIN_SIZE: u64 = 50 << 20;
const DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Cache,
    Logs,
    Regenerable,
    AppManaged,
    Installer,
    Developer,
    UserData,
    System,
}

/// How risky acting on a finding is. Ordered from least to most risky.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Safety {
    /// Deleting loses nothing.
    Safe,
    /// Safe after a preparatory step (quit the app, check the project).
    Caution,
    /// The user's own data or a judgment call.
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Trash,
    InApp,
    Admin,
    Archive,
    Info,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindFilter {
    #[default]
    Any,
    Dir,
    File,
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` (not `write_str`) so width/alignment flags like `{:<8}` work.
        f.pad(match self {
            Category::Cache => "cache",
            Category::Logs => "logs",
            Category::Regenerable => "regenerable",
            Category::AppManaged => "app-managed",
            Category::Installer => "installer",
            Category::Developer => "developer",
            Category::UserData => "your data",
            Category::System => "system",
        })
    }
}

impl fmt::Display for Safety {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` (not `write_str`) so width/alignment flags like `{:<8}` work.
        f.pad(match self {
            Safety::Safe => "safe",
            Safety::Caution => "caution",
            Safety::Review => "review",
        })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` (not `write_str`) so width/alignment flags like `{:<8}` work.
        f.pad(match self {
            Action::Trash => "move to Trash",
            Action::InApp => "clear in the app",
            Action::Admin => "needs admin password",
            Action::Archive => "archive elsewhere",
            Action::Info => "nothing to delete",
        })
    }
}

/// A rule as written in TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDef {
    pub id: String,
    pub title: String,
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub kind: KindFilter,
    pub min_size: Option<String>,
    pub older_than_days: Option<u32>,
    pub sibling: Option<String>,
    /// Only files the OS recorded as downloaded from one of these domains (or subdomains).
    #[serde(default)]
    pub from: Vec<String>,
    /// A generic catch-all, tried only after every specific rule.
    #[serde(default)]
    pub fallback: bool,
    pub category: Category,
    pub safety: Safety,
    pub action: Action,
    pub why: String,
    pub how: String,
    pub prevent: Option<String>,
}

#[derive(Deserialize)]
struct RuleFile {
    #[serde(rename = "rule", default)]
    rules: Vec<RuleDef>,
}

/// A compiled rule.
#[derive(Debug)]
pub struct Rule {
    pub def: RuleDef,
    pub min_size: u64,
    exclude: GlobSet,
}

/// Compiled rules, tried in priority order.
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<Rule>,
    include: GlobSet,
    /// For each glob in `include`, the index of its rule.
    owner: Vec<usize>,
}

impl RuleSet {
    /// The built-in rules for this machine's platform.
    pub fn builtin() -> Result<Self> {
        Self::builtin_for(platform::CURRENT)
    }

    /// The built-in rules for `platform` (e.g. to analyse a snapshot from another OS).
    pub fn builtin_for(platform: &Platform) -> Result<Self> {
        Self::from_sources(platform, &platform.builtin_rule_sources())
    }

    /// The user's rules file followed by the built-in rules for `platform`.
    pub fn with_user_rules(platform: &Platform, name: &str, text: &str) -> Result<Self> {
        let [os, common] = platform.builtin_rule_sources();
        Self::from_sources(platform, &[(name, text), os, common])
    }

    /// Compile rules from TOML sources. Earlier sources take priority, so put user
    /// rules before the built-in ones. Fallback rules from every source are moved
    /// after all specific ones. `~/` expands to the platform's home folders.
    pub fn from_sources(platform: &Platform, sources: &[(&str, &str)]) -> Result<Self> {
        let mut defs = Vec::new();
        let mut ids = std::collections::HashSet::new();
        for (name, text) in sources {
            let file: RuleFile = toml::from_str(text).with_context(|| format!("parsing {name}"))?;
            // A user rule with a built-in's id replaces it: the earlier one wins.
            defs.extend(file.rules.into_iter().filter(|d| ids.insert(d.id.clone())));
        }
        defs.sort_by_key(|d| d.fallback); // stable: keeps order within each tier

        let glob = |p: &str| -> Result<Glob> {
            Ok(GlobBuilder::new(&expand_home(p, platform.home_base))
                .literal_separator(true)
                .build()?)
        };
        let mut include = GlobSetBuilder::new();
        let mut owner = Vec::new();
        let mut rules = Vec::with_capacity(defs.len());
        for def in defs {
            let idx = rules.len();
            for p in &def.patterns {
                include.add(glob(p).with_context(|| format!("rule {}: pattern {p:?}", def.id))?);
                owner.push(idx);
            }
            let mut exclude = GlobSetBuilder::new();
            for p in &def.exclude {
                exclude.add(glob(p).with_context(|| format!("rule {}: exclude {p:?}", def.id))?);
            }
            let min_size = match &def.min_size {
                Some(s) => parse_size(s)
                    .ok_or_else(|| anyhow!("rule {}: invalid min_size {s:?}", def.id))?,
                None => DEFAULT_MIN_SIZE,
            };
            rules.push(Rule {
                min_size,
                exclude: exclude.build()?,
                def,
            });
        }
        Ok(RuleSet {
            rules,
            include: include.build()?,
            owner,
        })
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The highest-priority rule that claims `node` at absolute `path`, if any.
    pub fn rule_for(&self, path: &str, node: &Node, now: i64) -> Option<(usize, &Rule)> {
        let mut candidates: Vec<usize> = self
            .include
            .matches(path)
            .into_iter()
            .map(|g| self.owner[g])
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        candidates
            .into_iter()
            .map(|i| (i, &self.rules[i]))
            .find(|(_, r)| r.accepts(path, node, now))
    }
}

impl Rule {
    fn accepts(&self, path: &str, node: &Node, now: i64) -> bool {
        let kind_ok = match self.def.kind {
            KindFilter::Any => true,
            KindFilter::Dir => node.kind == NodeKind::Dir,
            KindFilter::File => node.kind == NodeKind::File,
        };
        kind_ok
            && node.allocated >= self.min_size
            && !self.exclude.is_match(path)
            && self
                .def
                .older_than_days
                .is_none_or(|d| now - node.newest_mtime >= i64::from(d) * DAY)
            && self.def.sibling.as_ref().is_none_or(|s| {
                Path::new(path)
                    .parent()
                    .is_some_and(|dir| dir.join(s).exists())
            })
            && (self.def.from.is_empty() || self.downloaded_from_listed_domain(path))
    }

    fn downloaded_from_listed_domain(&self, path: &str) -> bool {
        download_hosts(Path::new(path))
            .iter()
            .any(|h| self.def.from.iter().any(|d| host_matches(h, d)))
    }
}

/// `~/x` means `x` in every home folder under `home_base`.
fn expand_home(pattern: &str, home_base: &str) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        format!("{home_base}/*/{rest}")
    } else if pattern == "~" {
        format!("{home_base}/*")
    } else {
        pattern.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{LINUX, MACOS};

    fn node(kind: NodeKind, size: u64, mtime: i64) -> Node {
        let mut n = Node::new("x".into(), kind);
        n.allocated = size;
        n.newest_mtime = mtime;
        n
    }

    #[test]
    fn builtin_rules_compile_with_unique_ids() {
        let rs = RuleSet::builtin_for(&MACOS).unwrap();
        assert!(rs.rules().len() > 30);
        let mut ids: Vec<_> = rs.rules().iter().map(|r| &r.def.id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn specific_rules_win_over_generic() {
        let rs = RuleSet::builtin_for(&MACOS).unwrap();
        let big = node(NodeKind::Dir, 5 << 30, 0);
        let id = |p: &str| rs.rule_for(p, &big, 0).map(|(_, r)| r.def.id.as_str());

        assert_eq!(
            id("/Users/v/Lightroom/Lightroom Catalog-v13-3 Smart Previews.lrdata"),
            Some("lightroom-smart-previews")
        );
        assert_eq!(
            id("/Users/v/Lightroom/Lightroom Catalog-v13-3 Previews.lrdata"),
            Some("lightroom-previews")
        );
        assert_eq!(
            id("/Users/v/Library/Application Support/Claude/vm_bundles/claudevm.bundle"),
            Some("claude-vm-bundle")
        );
        assert_eq!(id("/Users/v/Library/Caches/Google"), Some("user-caches"));
        assert_eq!(id("/.adobeTemp"), Some("adobe-installer-leftovers"));
        assert_eq!(
            id("/Users/v/Library/Photos/Libraries/Syndication.photoslibrary"),
            Some("messages-shared-photos")
        );
        assert_eq!(
            id("/Users/v/Pictures/Photos Library.photoslibrary"),
            Some("photos-library")
        );
        // The current catalog's helper data is not an "old catalog".
        assert_eq!(
            id("/Users/v/Lightroom/Lightroom Catalog-v13-3 Helper.lrdata"),
            None
        );
        // `*` must not cross directories.
        assert_eq!(id("/Users/v/Library/Caches/Google/Chrome"), None);
    }

    #[test]
    fn conditions_apply() {
        let rs = RuleSet::builtin_for(&MACOS).unwrap();
        let now = 1_800_000_000;
        let dmg = "/Users/v/Downloads/Thing.dmg";
        let id = |n: &Node| rs.rule_for(dmg, n, now).map(|(_, r)| r.def.id.clone());

        // Fresh installer: too new for either Downloads rule.
        assert_eq!(id(&node(NodeKind::File, 300 << 20, now - DAY)), None);
        // Month-old installer with no recorded source.
        assert_eq!(
            id(&node(NodeKind::File, 300 << 20, now - 30 * DAY)).as_deref(),
            Some("installer-unknown-source")
        );
        // Compressed data is not an "installer": it goes to review.
        let archive = node(NodeKind::File, 200 << 20, now - 60 * DAY);
        let (_, r) = rs
            .rule_for("/Users/v/Downloads/scan.tif.gz", &archive, now)
            .unwrap();
        assert_eq!(
            (r.def.id.as_str(), r.def.safety),
            ("downloaded-archives", Safety::Review)
        );
        // Too small for any rule.
        assert_eq!(id(&node(NodeKind::File, 1 << 20, now - 30 * DAY)), None);
        // Library is excluded from the generic large-file rule.
        let old_big = node(NodeKind::File, 4 << 30, now - 800 * DAY);
        assert!(
            rs.rule_for("/Users/v/Library/Foo/huge.bin", &old_big, now)
                .is_none()
        );
        assert_eq!(
            rs.rule_for("/Users/v/Movies/old.mov", &old_big, now)
                .map(|(_, r)| r.def.id.as_str()),
            Some("large-old-files")
        );
    }

    #[test]
    fn sibling_condition_checks_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("diskwise-sibling-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        let src = r#"
            [[rule]]
            id = "t"
            title = "t"
            match = ["**/target"]
            sibling = "Cargo.toml"
            min_size = "0"
            category = "developer"
            safety = "safe"
            action = "trash"
            why = ""
            how = ""
        "#;
        let rs = RuleSet::from_sources(&MACOS, &[("test", src)]).unwrap();
        let target = dir.join("target").display().to_string();
        let n = node(NodeKind::Dir, 1, 0);
        assert!(rs.rule_for(&target, &n, 0).is_none());
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert!(rs.rule_for(&target, &n, 0).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn installers_split_by_download_source() {
        use crate::provenance::set_download_urls;
        let now = 1_800_000_000;
        // The download source is read from the real file, so point the built-in
        // ~/Downloads rules at a temporary folder and create real files there.
        let downloads = std::env::temp_dir().join(format!("diskwise-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&downloads);
        std::fs::create_dir_all(&downloads).unwrap();
        let rerooted: Vec<String> = platform::CURRENT
            .builtin_rule_sources()
            .iter()
            .map(|(_, text)| text.replace("~/Downloads", &downloads.display().to_string()))
            .collect();
        let sources: Vec<(&str, &str)> =
            rerooted.iter().map(|t| ("rerooted", t.as_str())).collect();
        let rs = RuleSet::from_sources(platform::CURRENT, &sources).unwrap();

        let rule_for = |name: &str, urls: &[&str]| -> String {
            let f = downloads.join(name);
            std::fs::write(&f, b"x").unwrap();
            if !urls.is_empty() && !set_download_urls(&f, urls) {
                return "no-xattr-support".into();
            }
            let n = node(NodeKind::File, 300 << 20, now - 30 * DAY);
            rs.rule_for(&f.display().to_string(), &n, now)
                .map(|(_, r)| r.def.id.clone())
                .unwrap_or_default()
        };

        if rule_for("Probe.dmg", &["https://example.com/p.dmg"]) == "no-xattr-support" {
            eprintln!(
                "skipping: no extended attribute support in {}",
                downloads.display()
            );
            std::fs::remove_dir_all(&downloads).unwrap();
            return;
        }
        assert_eq!(
            rule_for(
                "Game.dmg",
                &[
                    "https://dl.humble.com/g/Game.dmg?gamekey=K",
                    "https://www.humblebundle.com/downloads"
                ]
            ),
            "installer-purchased"
        );
        assert_eq!(
            rule_for(
                "Chrome.dmg",
                &["https://dl.google.com/chrome/googlechrome.dmg"]
            ),
            "installer-vendor"
        );
        // A CDN host doesn't match, but the referring page does.
        assert_eq!(
            rule_for(
                "Epic.dmg",
                &[
                    "https://epicgames-download1.akamaized.net/x.dmg",
                    "https://www.unrealengine.com/"
                ]
            ),
            "installer-vendor"
        );
        assert_eq!(rule_for("Mystery.dmg", &[]), "installer-unknown-source");
        assert_eq!(
            rule_for(
                "Indie.dmg",
                &["https://downloads.tinystudio.example/indie.dmg"]
            ),
            "installer-unknown-source"
        );
        std::fs::remove_dir_all(&downloads).unwrap();
    }

    #[test]
    fn linux_rules() {
        let rs = RuleSet::builtin_for(&LINUX).unwrap();
        let now = 1_800_000_000;
        let big = node(NodeKind::Dir, 5 << 30, now);
        let id = |p: &str| rs.rule_for(p, &big, now).map(|(_, r)| r.def.id.clone());

        assert_eq!(id("/var/cache/apt/archives").as_deref(), Some("apt-cache"));
        assert_eq!(id("/var/log/journal").as_deref(), Some("systemd-journal"));
        assert_eq!(id("/var/log/nginx").as_deref(), Some("linux-logs"));
        assert_eq!(
            id("/home/v/.cache/pip").as_deref(),
            Some("package-manager-caches")
        );
        // Specific common rules beat the Linux catch-all `~/.cache/*`, although the
        // Linux file comes first: that's what `fallback` is for.
        assert_eq!(
            id("/home/v/.cache/huggingface").as_deref(),
            Some("ml-models")
        );
        assert_eq!(
            id("/home/v/.cache/thumbnails").as_deref(),
            Some("linux-user-cache")
        );
        // macOS paths mean nothing here.
        assert_eq!(id("/Users/v/Library/Caches/Google"), None);
        let deb = node(NodeKind::File, 300 << 20, now - 30 * 86_400);
        assert_eq!(
            rs.rule_for("/home/v/Downloads/tool.deb", &deb, now)
                .map(|(_, r)| r.def.id.as_str()),
            Some("installer-unknown-source")
        );
    }

    /// Commands must be in backticks so the apps can show them as code (with a
    /// Copy button in the desktop app). Catches e.g. "then run: sudo rm -rf …".
    #[test]
    fn commands_are_in_backticks() {
        const COMMANDS: [&str; 22] = [
            "sudo",
            "defaults",
            "docker",
            "journalctl",
            "flatpak",
            "apt",
            "dnf",
            "pacman",
            "paccache",
            "xcrun",
            "tmutil",
            "snap",
            "systemctl",
            "cargo",
            "npm",
            "uv",
            "ollama",
            "pip",
            "python",
            "brew",
            "coredumpctl",
            "rm",
        ];
        let mut bad = Vec::new();
        for p in [&MACOS, &LINUX] {
            for r in RuleSet::builtin_for(p).unwrap().rules() {
                for (field, text) in [
                    ("how", Some(&r.def.how)),
                    ("prevent", r.def.prevent.as_ref()),
                ] {
                    let Some(text) = text else { continue };
                    // Drop the backticked parts, then look for "<command> <argument>".
                    let outside: String = text.split('`').step_by(2).collect::<Vec<_>>().join(" ");
                    let words: Vec<&str> = outside.split_whitespace().collect();
                    for pair in words.windows(2) {
                        let next = pair[1].chars().next().unwrap_or(' ');
                        if COMMANDS.contains(&pair[0]) && (next.is_ascii_lowercase() || next == '-')
                        {
                            bad.push(format!("{} {field}: {:?}", r.def.id, text));
                        }
                    }
                }
            }
        }
        bad.dedup();
        assert!(
            bad.is_empty(),
            "commands outside backticks:\n{}",
            bad.join("\n")
        );
    }

    #[test]
    fn fallback_rules_sort_after_specific_ones() {
        for p in [&MACOS, &LINUX] {
            let rs = RuleSet::builtin_for(p).unwrap();
            let first_fallback = rs.rules().iter().position(|r| r.def.fallback).unwrap();
            assert!(
                rs.rules()[first_fallback..].iter().all(|r| r.def.fallback),
                "{}",
                p.os
            );
        }
    }

    #[test]
    fn user_rule_overrides_builtin_by_id() {
        let user = r#"
            [[rule]]
            id = "user-caches"
            title = "Mine"
            match = ["~/Library/Caches/*"]
            category = "cache"
            safety = "review"
            action = "info"
            why = ""
            how = ""
        "#;
        let rs = RuleSet::with_user_rules(&MACOS, "user", user).unwrap();
        let (_, r) = rs
            .rule_for(
                "/Users/v/Library/Caches/X",
                &node(NodeKind::Dir, 1 << 30, 0),
                0,
            )
            .unwrap();
        assert_eq!(r.def.title, "Mine");
    }
}
