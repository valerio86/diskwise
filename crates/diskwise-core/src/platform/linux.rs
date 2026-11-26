//! Linux probes: mounted filesystems (/proc/self/mounts + statvfs), swap
//! (/proc/meminfo), and download sources (user.xdg.origin.url).

use std::collections::HashSet;
use std::path::Path;

use crate::system::{MountInfo, SystemInfo, fs_usage};

/// Set by Chromium-based browsers, wget (--xattr) and curl (--xattr).
const ORIGIN_ATTRS: [&str; 2] = ["user.xdg.origin.url", "user.xdg.referrer.url"];

pub(crate) fn probe(_path: &str, info: &mut SystemInfo) {
    match std::fs::read_to_string("/proc/self/mounts") {
        Ok(text) => {
            let mut seen_devices = HashSet::new();
            for (device, mount_point, fs_type) in super::parse_mounts(&text) {
                // btrfs subvolumes and bind mounts show one device several times.
                if !super::is_storage_fs(&fs_type) || !seen_devices.insert(device.clone()) {
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
