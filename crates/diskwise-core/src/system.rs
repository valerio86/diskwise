//! Storage facts a file walk can't see. The data types are shared; the probing is
//! per platform (see `platform`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemInfo {
    /// Capacity and free space of the filesystem holding the scan root.
    #[serde(default)]
    pub filesystem: Option<FsUsage>,
    /// macOS: the APFS container and its volumes (they share free space).
    #[serde(default)]
    pub container: Option<ContainerInfo>,
    /// Linux: mounted storage filesystems, most used first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountInfo>,
    /// macOS: local APFS snapshots (Time Machine, OS updates); size not reported.
    #[serde(default)]
    pub snapshots: Vec<String>,
    #[serde(default)]
    pub swap: Option<SwapInfo>,
    /// Probes that failed, so missing data isn't silently treated as zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FsUsage {
    pub capacity: u64,
    /// Space available to unprivileged users.
    pub free: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub reference: String,
    pub capacity: u64,
    pub free: u64,
    pub volumes: Vec<VolumeInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub name: String,
    pub device: String,
    pub roles: Vec<String>,
    pub used: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    pub mount_point: String,
    pub device: String,
    pub fs_type: String,
    pub capacity: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapInfo {
    pub total: u64,
    pub used: u64,
}

/// Collect system storage info relevant to a scan of `path`.
pub fn probe(path: &str) -> SystemInfo {
    let mut info = SystemInfo::default();
    match fs_usage(Path::new(path)) {
        Ok(u) => info.filesystem = Some(u),
        Err(e) => info.warnings.push(format!("filesystem: {e:#}")),
    }
    crate::platform::probe(path, &mut info);
    info
}

/// Capacity and free space of the filesystem containing `path` (statvfs).
pub fn fs_usage(path: &Path) -> Result<FsUsage> {
    let c = CString::new(path.as_os_str().as_bytes()).context("path contains NUL")?;
    // SAFETY: `c` is a valid NUL-terminated string and `st` is a properly sized,
    // zero-initialised out-parameter that statvfs fills in.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return Err(std::io::Error::last_os_error()).context("statvfs");
        }
        st
    };
    #[allow(clippy::unnecessary_cast)] // field widths differ between platforms
    let (frsize, blocks, avail) = (st.f_frsize as u64, st.f_blocks as u64, st.f_bavail as u64);
    Ok(FsUsage {
        capacity: blocks * frsize,
        free: avail * frsize,
    })
}

/// Run a command and return its stdout, or an error including its stderr.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) fn run(cmd: &str, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_usage_of_root() {
        let u = fs_usage(Path::new("/")).unwrap();
        assert!(u.capacity > 0 && u.free <= u.capacity);
    }
}
