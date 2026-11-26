//! The user's choices about duplicates, remembered between runs: which copy
//! to keep when nothing else decides, "always prefer folder A over folder B",
//! and copies or whole folders to keep as they are.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Prefer, inside};

/// One "keep copies here rather than there" choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderOrder {
    pub prefer: String,
    pub over: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default)]
    pub prefer: Prefer,
    /// Applied to every group with copies under both folders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folder_order: Vec<FolderOrder>,
    /// Folders whose duplicates are intentional: copies inside are kept as they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intentional: Vec<String>,
    /// Particular copies (files or folders) to keep as they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<String>,
}

impl Prefs {
    /// The saved preferences; the defaults if there's no file yet. An error
    /// if the file exists but can't be read or understood: callers then use
    /// the defaults *without saving*, so a hand-edit mistake never wipes the
    /// user's choices.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| {
                format!(
                    "Your saved duplicate choices in {} couldn't be read ({e}). They're left as they are and nothing will be saved over them; fix or remove the file.",
                    path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("Couldn't read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, path)
    }

    /// Where preferences live by default.
    pub fn default_path() -> Option<PathBuf> {
        crate::platform::config_dir().map(|d| d.join("duplicates.json"))
    }

    /// Whether `path` (an extra copy) should be kept as it is: it, or a folder
    /// it's in, was kept, or it's in a folder of intentional duplicates.
    pub fn is_kept(&self, path: &str) -> bool {
        self.keep.iter().any(|k| k == path || inside(path, k))
            || self
                .intentional
                .iter()
                .any(|f| f == path || inside(path, f))
    }

    /// How strongly the folder choices favour keeping `path` over the `others`:
    /// +1 for each choice that prefers a folder holding it over one holding
    /// another copy, -1 the other way round.
    pub(super) fn order_score(&self, path: &str, others: &[&str]) -> i32 {
        let under = |p: &str, f: &str| p == f || inside(p, f);
        self.folder_order
            .iter()
            .map(|o| {
                if under(path, &o.prefer) && others.iter().any(|x| under(x, &o.over)) {
                    1
                } else if under(path, &o.over) && others.iter().any(|x| under(x, &o.prefer)) {
                    -1
                } else {
                    0
                }
            })
            .sum()
    }

    /// Remember to prefer `prefer` over `over`, replacing any opposite choice.
    pub fn add_folder_order(&mut self, prefer: &str, over: &str) {
        self.folder_order.retain(|o| {
            !(o.prefer == over && o.over == prefer) && !(o.prefer == prefer && o.over == over)
        });
        self.folder_order.push(FolderOrder {
            prefer: prefer.to_string(),
            over: over.to_string(),
        });
    }

    /// Keep `path` as it is from now on, or stop doing so.
    pub fn set_kept(&mut self, path: &str, kept: bool) {
        self.keep.retain(|k| k != path);
        if kept {
            self.keep.push(path.to_string());
        }
    }

    /// Mark `folder`'s duplicates as intentional, or not.
    pub fn set_intentional(&mut self, folder: &str, on: bool) {
        self.intentional.retain(|f| f != folder);
        if on {
            self.intentional.push(folder.to_string());
        }
    }
}

/// The two folders to offer for "always prefer A over B" after the user picks
/// `chosen` over `previous`: the innermost folders where their paths differ
/// (`~/Projects/x/a.jpg` over `~/Downloads/a.jpg` → `~/Projects/x` over `~/Downloads`).
pub fn folder_pair(chosen: &str, previous: &str) -> Option<(String, String)> {
    let a: Vec<&str> = chosen.split('/').collect();
    let b: Vec<&str> = previous.split('/').collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    // Both must have a folder below the common part (not just the file name).
    if common == 0 || a.len() < common + 2 || b.len() < common + 2 {
        return None;
    }
    let parent = |v: &[&str]| v[..v.len() - 1].join("/");
    Some((parent(&a), parent(&b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_and_intentional() {
        let mut p = Prefs::default();
        p.set_intentional("/Users/v/Development/stilson-road", true);
        p.set_kept("/Users/v/Downloads/x.pdf", true);
        p.set_kept("/Users/v/Downloads/P", true);
        assert!(
            p.is_kept("/Users/v/Downloads/P/sub/x.mov"),
            "inside a kept folder copy"
        );
        assert!(p.is_kept("/Users/v/Development/stilson-road/anim/web/a.mp4"));
        assert!(p.is_kept("/Users/v/Downloads/x.pdf"));
        assert!(!p.is_kept("/Users/v/Downloads/y.pdf"));
        assert!(!p.is_kept("/Users/v/Development/stilson-road2/a"));
        p.set_kept("/Users/v/Downloads/x.pdf", false);
        assert!(!p.is_kept("/Users/v/Downloads/x.pdf"));
    }

    #[test]
    fn folder_order_scores() {
        let mut p = Prefs::default();
        p.add_folder_order("/Users/v/Projects", "/Users/v/Downloads");
        let a = "/Users/v/Projects/x/a.jpg";
        let b = "/Users/v/Downloads/a.jpg";
        assert_eq!(p.order_score(a, &[b]), 1);
        assert_eq!(p.order_score(b, &[a]), -1);
        assert_eq!(p.order_score("/Users/v/Other/a.jpg", &[b]), 0);
        // The opposite choice replaces the first.
        p.add_folder_order("/Users/v/Downloads", "/Users/v/Projects");
        assert_eq!(p.folder_order.len(), 1);
        assert_eq!(p.order_score(b, &[a]), 1);
    }

    #[test]
    fn pairs_offered_after_a_swap() {
        assert_eq!(
            folder_pair("/Users/v/Projects/x/a.jpg", "/Users/v/Downloads/a.jpg"),
            Some(("/Users/v/Projects/x".into(), "/Users/v/Downloads".into()))
        );
        assert_eq!(folder_pair("/Users/v/a.jpg", "/Users/v/a (1).jpg"), None);
    }

    #[test]
    fn round_trip() {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("prefs-{}", std::process::id()));
        let path = d.join("duplicates.json");
        let mut p = Prefs {
            prefer: Prefer::NewestCopy,
            ..Default::default()
        };
        p.set_intentional("/Users/v/x", true);
        p.save(&path).unwrap();
        assert_eq!(Prefs::load(&path).unwrap(), p);
        assert_eq!(
            Prefs::load(&d.join("missing.json")).unwrap(),
            Prefs::default()
        );
        // A broken file is an error, not silently the defaults.
        std::fs::write(&path, b"{ \"keep\": [\"/x\",] }").unwrap();
        assert!(Prefs::load(&path).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
