//! diskwise core: scan a disk into a size tree, collect storage facts the walk
//! can't see, and turn both into cleanup suggestions. Supports macOS and Linux.

pub mod model;
pub mod platform;
pub mod provenance;
pub mod rules;
pub mod scan;
pub mod suggest;
pub mod system;

use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub use model::{Node, NodeKind, Snapshot};
pub use platform::Platform;
pub use rules::RuleSet;
pub use scan::ScanOptions;
pub use suggest::{Finding, analyze};

/// What a scan walks by default on this platform.
pub const DEFAULT_ROOT: &str = platform::CURRENT.default_root;

/// Run a full scan and, optionally, the system probes.
pub fn snapshot(root: &Path, opts: ScanOptions, with_system: bool) -> std::io::Result<Snapshot> {
    // Rules match absolute paths, and paths are compared as strings throughout,
    // so resolve "." / "Downloads/" / symlinked roots to one canonical form.
    let root = &std::fs::canonicalize(root)?;
    let started = Instant::now();
    let min_node_size = opts.min_node_size;
    let scan::ScanOutput {
        tree,
        errors,
        skipped_mounts,
    } = scan::scan(root, opts)?;
    let system = with_system.then(|| system::probe(&root.display().to_string()));
    // Kernel and in-memory filesystems (/proc, /dev, /run, …) don't count as "not
    // scanned". Everything else does, including network and FUSE mounts.
    let mut skipped_mounts = skipped_mounts;
    if let Some(sys) = &system {
        skipped_mounts.retain(|m| !sys.virtual_mounts.contains(m));
    }
    Ok(Snapshot {
        version: model::SNAPSHOT_VERSION,
        os: platform::CURRENT.os.to_string(),
        root: root.display().to_string(),
        scanned_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        duration_ms: started.elapsed().as_millis() as u64,
        min_node_size,
        tree,
        errors,
        skipped_mounts,
        system,
    })
}

/// Format a byte count the way Finder does (decimal units).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Parse sizes like `10M`, `500K`, `1G`, `2048` (binary multiples).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_root_is_canonical() {
        // "." must become an absolute path, or no rule (all absolute) can match.
        let snap = snapshot(Path::new("."), ScanOptions::default(), false).unwrap();
        assert!(snap.root.starts_with('/'));
        assert_eq!(Path::new(&snap.root), std::fs::canonicalize(".").unwrap());
        assert!(!snap.root.ends_with("/.") && !snap.root.ends_with("/"));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10M"), Some(10 << 20));
        assert_eq!(parse_size("1.5k"), Some(1536));
        assert_eq!(parse_size("42"), Some(42));
        assert_eq!(parse_size("x"), None);
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(153_789_448_192), "153.8 GB");
    }
}
