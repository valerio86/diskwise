//! Everything that differs between operating systems.
//!
//! The per-OS *facts* (default scan root, where home folders live, built-in rules)
//! are plain data available on every platform, so a snapshot taken on Linux can be
//! analysed on a Mac and Linux rules can be tested anywhere. Only the code that
//! queries the running system (`probe`, `download_urls`) is compiled per OS.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("diskwise supports macOS and Linux");

#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux::set_download_urls;
#[cfg(target_os = "linux")]
pub(crate) use linux::{cache_dir, can_clone, download_urls, private_size, probe};
#[cfg(all(test, target_os = "macos"))]
pub(crate) use macos::set_download_urls;
#[cfg(target_os = "macos")]
pub(crate) use macos::{cache_dir, can_clone, download_urls, private_size, probe};

/// Rules shared by every platform; tried after the platform's own rules.
pub const COMMON_RULES: &str = include_str!("../../rules/common.toml");

#[derive(Debug, PartialEq, Eq)]
pub struct Platform {
    /// Name stored in snapshots: "macos" or "linux".
    pub os: &'static str,
    /// What `diskwise scan` walks by default.
    pub default_root: &'static str,
    /// Parent of home folders; `~/` in rules expands to `<home_base>/*/`.
    pub home_base: &'static str,
    /// Built-in rules specific to this platform.
    pub rules: &'static str,
}

pub const MACOS: Platform = Platform {
    os: "macos",
    // The APFS Data volume: all user data once, without walking the sealed System
    // volume or double-counting the firmlinks that make its folders appear at `/`.
    default_root: "/System/Volumes/Data",
    home_base: "/Users",
    rules: include_str!("../../rules/macos.toml"),
};

pub const LINUX: Platform = Platform {
    os: "linux",
    default_root: "/",
    home_base: "/home",
    rules: include_str!("../../rules/linux.toml"),
};

#[cfg(target_os = "macos")]
pub const CURRENT: &Platform = &MACOS;
#[cfg(target_os = "linux")]
pub const CURRENT: &Platform = &LINUX;

impl Platform {
    /// Look up a platform by its snapshot name. Snapshots written before the `os`
    /// field existed came from macOS.
    pub fn by_name(os: &str) -> Option<&'static Platform> {
        match os {
            "macos" | "" => Some(&MACOS),
            "linux" => Some(&LINUX),
            _ => None,
        }
    }

    /// The path users know for a scanned path. On macOS the Data volume's folders
    /// appear at `/` through firmlinks, so show (and act on) those paths.
    pub fn display_path(&self, path: &str) -> String {
        if self.os == "macos" {
            match path.strip_prefix(MACOS.default_root) {
                Some("") => return "/".to_string(),
                Some(rest) if rest.starts_with('/') => return rest.to_string(),
                _ => {}
            }
        }
        path.to_string()
    }

    /// Built-in rule sources in priority order.
    pub fn builtin_rule_sources(&self) -> [(&'static str, &'static str); 2] {
        let name = match self.os {
            "macos" => "built-in macOS rules",
            _ => "built-in Linux rules",
        };
        [(name, self.rules), ("built-in common rules", COMMON_RULES)]
    }
}

/// Parses macOS `sysctl -n vm.swapusage`:
/// `total = 3072.00M  used = 1370.56M  free = 1701.44M  (encrypted)`.
#[cfg_attr(target_os = "linux", allow(dead_code))] // used on macOS; tested everywhere
pub(crate) fn parse_sysctl_swap(s: &str) -> Option<crate::system::SwapInfo> {
    let field = |key: &str| -> Option<u64> {
        let token = s
            .split(&format!("{key} = "))
            .nth(1)?
            .split_whitespace()
            .next()?;
        let (num, unit) = token.split_at(token.len().checked_sub(1)?);
        let mult: f64 = match unit {
            "K" => 1024.0,
            "M" => 1024.0 * 1024.0,
            "G" => 1024.0 * 1024.0 * 1024.0,
            _ => return None,
        };
        Some((num.parse::<f64>().ok()? * mult) as u64)
    };
    Some(crate::system::SwapInfo {
        total: field("total")?,
        used: field("used")?,
    })
}

/// Parses Linux `/proc/meminfo` swap lines (`SwapTotal:  2097148 kB`).
#[cfg_attr(target_os = "macos", allow(dead_code))] // used on Linux; tested everywhere
pub(crate) fn parse_meminfo_swap(s: &str) -> Option<crate::system::SwapInfo> {
    let kb = |key: &str| -> Option<u64> {
        let line = s.lines().find(|l| l.starts_with(key))?;
        line[key.len()..]
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse::<u64>()
            .ok()
            .map(|v| v * 1024)
    };
    let total = kb("SwapTotal:")?;
    Some(crate::system::SwapInfo {
        total,
        used: total.saturating_sub(kb("SwapFree:")?),
    })
}

/// Parses `/proc/self/mounts`, undoing the octal escapes used for spaces etc.
/// Returns (device, mount point, filesystem type).
#[cfg_attr(target_os = "macos", allow(dead_code))] // used on Linux; tested everywhere
pub(crate) fn parse_mounts(s: &str) -> Vec<(String, String, String)> {
    fn unescape(f: &str) -> String {
        let b = f.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
            {
                out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    s.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((
                unescape(f.next()?),
                unescape(f.next()?),
                f.next()?.to_string(),
            ))
        })
        .collect()
}

/// Kernel, in-memory and image filesystems: never "data the scan missed". Overlay
/// and squashfs are here because their data is already counted where it's stored
/// (/var/lib/docker, /var/lib/snapd/snaps).
#[cfg_attr(target_os = "macos", allow(dead_code))] // used on Linux; tested everywhere
pub(crate) fn is_virtual_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "proc"
            | "sysfs"
            | "tmpfs"
            | "devtmpfs"
            | "devpts"
            | "cgroup"
            | "cgroup2"
            | "securityfs"
            | "pstore"
            | "efivarfs"
            | "bpf"
            | "tracefs"
            | "debugfs"
            | "configfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "autofs"
            | "binfmt_misc"
            | "ramfs"
            | "nsfs"
            | "rpc_pipefs"
            | "squashfs"
            | "overlay"
            | "nfsd"
            | "selinuxfs"
    )
}

/// Filesystem types listed with their usage (as opposed to proc, tmpfs, cgroup, …).
#[cfg_attr(target_os = "macos", allow(dead_code))] // used on Linux; tested everywhere
pub(crate) fn is_storage_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "ext2"
            | "ext3"
            | "ext4"
            | "xfs"
            | "btrfs"
            | "zfs"
            | "f2fs"
            | "bcachefs"
            | "jfs"
            | "reiserfs"
            | "vfat"
            | "exfat"
            | "ntfs"
            | "ntfs3"
            | "fuseblk"
            | "hfsplus"
            | "apfs"
            | "nfs"
            | "nfs4"
            | "cifs"
            | "smb3"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_paths() {
        assert_eq!(MACOS.display_path("/System/Volumes/Data"), "/");
        assert_eq!(
            MACOS.display_path("/System/Volumes/Data/Users/v"),
            "/Users/v"
        );
        assert_eq!(
            MACOS.display_path("/System/Volumes/DataX"),
            "/System/Volumes/DataX"
        );
        assert_eq!(MACOS.display_path("/Users/v"), "/Users/v");
        assert_eq!(
            LINUX.display_path("/System/Volumes/Data/x"),
            "/System/Volumes/Data/x"
        );
        assert_eq!(Platform::by_name(""), Some(&MACOS));
        assert_eq!(Platform::by_name("linux"), Some(&LINUX));
    }

    #[test]
    fn sysctl_swap() {
        let s =
            parse_sysctl_swap("total = 3072.00M  used = 1370.56M  free = 1701.44M  (encrypted)")
                .unwrap();
        assert_eq!(s.total, 3072 * 1024 * 1024);
        assert!(s.used > 1370 * 1024 * 1024 && s.used < 1371 * 1024 * 1024);
        assert!(parse_sysctl_swap("garbage").is_none());
    }

    #[test]
    fn meminfo_swap() {
        let s = parse_meminfo_swap(
            "MemTotal: 1 kB\nSwapTotal:     2097148 kB\nSwapFree:      1048574 kB\n",
        )
        .unwrap();
        assert_eq!(s.total, 2097148 * 1024);
        assert_eq!(s.used, (2097148 - 1048574) * 1024);
    }

    #[test]
    fn mounts() {
        let m = parse_mounts(
            "/dev/sda1 / ext4 rw 0 0\nproc /proc proc rw 0 0\n//nas/My\\040Share /mnt/my\\040share cifs rw 0 0\n",
        );
        assert_eq!(m[0], ("/dev/sda1".into(), "/".into(), "ext4".into()));
        assert_eq!(m[2].1, "/mnt/my share");
        assert!(is_storage_fs("cifs") && !is_storage_fs("proc"));
        assert!(is_virtual_fs("tmpfs") && is_virtual_fs("overlay"));
        // Unknown types (sshfs, rclone, virtiofs…) are not virtual: still reported.
        assert!(!is_virtual_fs("fuse.sshfs") && !is_virtual_fs("btrfs"));
    }
}
