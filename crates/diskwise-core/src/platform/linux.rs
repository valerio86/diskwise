//! Linux probes: mounted filesystems (/proc/self/mounts + statvfs), swap
//! (/proc/meminfo), and download sources (user.xdg.origin.url).

use std::path::Path;

use crate::system::{MountInfo, SystemInfo, fs_usage};

/// Set by Chromium-based browsers, wget (--xattr) and curl (--xattr).
const ORIGIN_ATTRS: [&str; 2] = ["user.xdg.origin.url", "user.xdg.referrer.url"];

pub(crate) fn probe(_path: &str, info: &mut SystemInfo) {
    match std::fs::read_to_string("/proc/self/mounts") {
        Ok(text) => {
            for (device, mount_point, fs_type) in super::parse_mounts(&text) {
                if super::is_virtual_fs(&fs_type) {
                    info.virtual_mounts.push(mount_point);
                    continue;
                }
                if !super::is_storage_fs(&fs_type) {
                    continue;
                }
                match fs_usage(Path::new(&mount_point)) {
                    Ok(u) => info.mounts.push(MountInfo {
                        mount_point,
                        device,
                        fs_type,
                        capacity: u.capacity,
                        free: u.free,
                    }),
                    Err(e) => info.warnings.push(format!("{mount_point}: {e:#}")),
                }
            }
            info.mounts
                .sort_by_key(|m| std::cmp::Reverse(m.capacity - m.free.min(m.capacity)));
        }
        Err(e) => info.warnings.push(format!("mounts: {e}")),
    }
    match std::fs::read_to_string("/proc/meminfo") {
        Ok(text) => info.swap = super::parse_meminfo_swap(&text),
        Err(e) => info.warnings.push(format!("swap: {e}")),
    }
}

pub(crate) fn download_urls(path: &Path) -> Vec<String> {
    ORIGIN_ATTRS
        .iter()
        .filter_map(|a| xattr::get(path, a).ok().flatten())
        .filter_map(|v| String::from_utf8(v).ok())
        .collect()
}

#[cfg(test)]
pub(crate) fn set_download_urls(path: &Path, urls: &[&str]) -> bool {
    // tmpfs only supports user.* attributes on newer kernels.
    ORIGIN_ATTRS
        .iter()
        .zip(urls)
        .all(|(attr, url)| xattr::set(path, attr, url.as_bytes()).is_ok())
}

/// Bytes of a file that no other file shares. Linux reports shared extents
/// (reflinks on btrfs/XFS) but not how much of a file they cover, so a file with
/// any counts as `Some(0)`: an underestimate, never an overestimate. `None` if
/// the filesystem can't say.
pub(crate) fn private_size(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;

    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B; // _IOWR('f', 11, struct fiemap)
    const FIEMAP_FLAG_SYNC: u32 = 0x1;
    const FIEMAP_EXTENT_LAST: u32 = 0x1;
    const FIEMAP_EXTENT_SHARED: u32 = 0x2000;
    const BATCH: usize = 64;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    #[repr(C)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped_extents: u32,
        extent_count: u32,
        reserved: u32,
        extents: [Extent; BATCH],
    }

    let file = std::fs::File::open(path).ok()?;
    let allocated = file.metadata().ok()?.blocks() * 512;
    let mut start = 0u64;
    // Bounded: a pathological file with millions of extents isn't worth walking.
    // Not knowing then counts as shared, so the estimate stays low.
    for _ in 0..1024 {
        let mut map: Fiemap = unsafe { std::mem::zeroed() };
        map.start = start;
        map.length = u64::MAX - start;
        map.flags = FIEMAP_FLAG_SYNC;
        map.extent_count = BATCH as u32;
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_FIEMAP as _, &mut map) };
        if rc != 0 {
            return None;
        }
        let n = map.mapped_extents as usize;
        if n == 0 {
            return Some(allocated);
        }
        for e in &map.extents[..n.min(BATCH)] {
            if e.flags & FIEMAP_EXTENT_SHARED != 0 {
                return Some(0);
            }
            if e.flags & FIEMAP_EXTENT_LAST != 0 {
                return Some(allocated);
            }
        }
        let last = map.extents[n.min(BATCH) - 1];
        start = last.logical + last.length;
    }
    Some(0)
}

/// Where diskwise keeps caches: $XDG_CACHE_HOME/diskwise or ~/.cache/diskwise.
pub(crate) fn cache_dir() -> Option<std::path::PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|x| Path::new(x).is_absolute()) {
        return Some(Path::new(&x).join("diskwise"));
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".cache/diskwise"))
}

/// Whether the filesystem holding `path` can make reflink clones (btrfs, XFS).
/// XFS can be formatted without reflink support; acting on a clone checks again.
pub(crate) fn can_clone(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const BTRFS_SUPER_MAGIC: u32 = 0x9123_683E;
    const XFS_SUPER_MAGIC: u32 = 0x5846_5342;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    // f_type is signed and its width varies by architecture; the magic numbers are 32-bit.
    matches!(st.f_type as u32, BTRFS_SUPER_MAGIC | XFS_SUPER_MAGIC)
}
