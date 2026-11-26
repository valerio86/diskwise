use serde::{Deserialize, Serialize};

/// Version of the JSON snapshot format. Bump when the shape changes.
pub const SNAPSHOT_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Dir,
    File,
    /// Aggregate of children too small to list individually.
    Rest,
    /// A symbolic link (not followed). Its own size is tiny; what it points to
    /// is counted where it lives, if inside the scan.
    Link,
}

/// One entry in the size tree. Directory sizes include everything beneath them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub kind: NodeKind,
    /// Bytes actually allocated on disk (st_blocks * 512); hard links counted once.
    pub allocated: u64,
    /// Apparent size (st_size) of regular files.
    pub logical: u64,
    /// Number of non-directory entries beneath (or represented by) this node.
    pub files: u64,
    /// Number of directories beneath this node (excluding itself).
    pub dirs: u64,
    /// Most recent modification time in this subtree, Unix seconds.
    pub newest_mtime: i64,
    /// True if this directory (or something under it) could not be read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Node>,
    /// The name on disk isn't valid UTF-8, so `name` is an approximation that
    /// mustn't be used to act on the item.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lossy_name: bool,
    /// For links: the target as written in the link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl Node {
    pub fn new(name: String, kind: NodeKind) -> Self {
        Node {
            name,
            kind,
            allocated: 0,
            logical: 0,
            files: 0,
            dirs: 0,
            newest_mtime: 0,
            incomplete: false,
            children: Vec::new(),
            lossy_name: false,
            target: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorSummary {
    pub count: u64,
    /// First few unreadable paths, for diagnosis (usually missing Full Disk Access).
    pub samples: Vec<String>,
}

/// Everything one scan produces; serialised as the snapshot JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    /// Platform the scan ran on ("macos", "linux"); empty in v1 snapshots (macOS).
    #[serde(default)]
    pub os: String,
    pub root: String,
    pub scanned_at: i64,
    pub duration_ms: u64,
    pub min_node_size: u64,
    pub tree: Node,
    pub errors: ErrorSummary,
    /// Mount points of other filesystems inside the root that were not scanned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_mounts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<crate::system::SystemInfo>,
}

impl Snapshot {
    /// The platform this snapshot was taken on (falls back to this machine's for
    /// an unknown name).
    pub fn platform(&self) -> &'static crate::Platform {
        crate::Platform::by_name(&self.os).unwrap_or(crate::platform::CURRENT)
    }
}
