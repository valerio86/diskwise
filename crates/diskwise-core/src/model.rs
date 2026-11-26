use serde::{Deserialize, Serialize};

/// Version of the JSON snapshot format. Bump when the shape changes.
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Dir,
    File,
    /// Aggregate of children too small to list individually.
    Rest,
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
    pub root: String,
    pub scanned_at: i64,
    pub duration_ms: u64,
    pub min_node_size: u64,
    pub tree: Node,
    pub errors: ErrorSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<crate::system::SystemInfo>,
}
