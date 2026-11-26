//! Rule definitions (TOML) and matching against scanned paths.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::model::{Node, NodeKind};
use crate::parse_size;

/// Rules shipped with diskwise.
pub const BUILTIN_RULES: &str = include_str!("../rules/default.toml");

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
        f.write_str(match self {
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
        f.write_str(match self {
            Safety::Safe => "safe",
            Safety::Caution => "caution",
            Safety::Review => "review",
        })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
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
    /// The built-in rules only.
    pub fn builtin() -> Result<Self> {
        Self::from_sources(&[("built-in rules", BUILTIN_RULES)])
    }

    /// Compile rules from TOML sources. Earlier sources take priority, so put user
    /// rules before the built-in ones.
    pub fn from_sources(sources: &[(&str, &str)]) -> Result<Self> {
        let mut defs = Vec::new();
        for (name, text) in sources {
            let file: RuleFile = toml::from_str(text).with_context(|| format!("parsing {name}"))?;
            defs.extend(file.rules);
        }

        let mut include = GlobSetBuilder::new();
        let mut owner = Vec::new();
        let mut rules = Vec::with_capacity(defs.len());
        let mut ids = std::collections::HashSet::new();
        for def in defs {
            if !ids.insert(def.id.clone()) {
                // A user rule with a built-in's id replaces it: the earlier one wins.
                continue;
            }
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
    }
}

/// `~/x` means `x` in every home folder.
fn expand_home(pattern: &str) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        format!("/Users/*/{rest}")
    } else if pattern == "~" {
        "/Users/*".to_string()
    } else {
        pattern.to_string()
    }
}

fn glob(pattern: &str) -> Result<Glob> {
    Ok(GlobBuilder::new(&expand_home(pattern))
        .literal_separator(true)
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: NodeKind, size: u64, mtime: i64) -> Node {
        let mut n = Node::new("x".into(), kind);
        n.allocated = size;
        n.newest_mtime = mtime;
        n
    }

    #[test]
    fn builtin_rules_compile_with_unique_ids() {
        let rs = RuleSet::builtin().unwrap();
        assert!(rs.rules().len() > 30);
        let mut ids: Vec<_> = rs.rules().iter().map(|r| &r.def.id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn specific_rules_win_over_generic() {
        let rs = RuleSet::builtin().unwrap();
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
        let rs = RuleSet::builtin().unwrap();
        let now = 1_800_000_000;
        let dmg = "/Users/v/Downloads/Thing.dmg";
        let id = |n: &Node| rs.rule_for(dmg, n, now).map(|(_, r)| r.def.id.clone());

        // Fresh installer: too new for either Downloads rule.
        assert_eq!(id(&node(NodeKind::File, 300 << 20, now - DAY)), None);
        // Month-old installer.
        assert_eq!(
            id(&node(NodeKind::File, 300 << 20, now - 30 * DAY)).as_deref(),
            Some("downloaded-installers")
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
        let rs = RuleSet::from_sources(&[("test", src)]).unwrap();
        let target = dir.join("target").display().to_string();
        let n = node(NodeKind::Dir, 1, 0);
        assert!(rs.rule_for(&target, &n, 0).is_none());
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert!(rs.rule_for(&target, &n, 0).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
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
        let rs = RuleSet::from_sources(&[("user", user), ("built-in", BUILTIN_RULES)]).unwrap();
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
