//! macOS probes: APFS volumes (diskutil), local snapshots (tmutil), swap (sysctl),
//! and download sources (kMDItemWhereFroms).

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::system::{ContainerInfo, SystemInfo, VolumeInfo, run};

const WHERE_FROMS_ATTR: &str = "com.apple.metadata:kMDItemWhereFroms";

pub(crate) fn probe(path: &str, info: &mut SystemInfo) {
    match container_for(path) {
        Ok(c) => info.container = Some(c),
        Err(e) => info.warnings.push(format!("APFS volumes: {e:#}")),
    }
    match local_snapshots() {
        Ok(s) => info.snapshots = s,
        Err(e) => info.warnings.push(format!("snapshots: {e:#}")),
    }
    match run("sysctl", &["-n", "vm.swapusage"]) {
        Ok(out) => match super::parse_sysctl_swap(&String::from_utf8_lossy(&out)) {
            Some(s) => info.swap = Some(s),
            None => info.warnings.push("swap: unexpected sysctl output".into()),
        },
        Err(e) => info.warnings.push(format!("swap: {e:#}")),
    }
}

/// URLs recorded by browsers: a binary plist array (file URL, then referring page).
pub(crate) fn download_urls(path: &Path) -> Vec<String> {
    match xattr::get(path, WHERE_FROMS_ATTR) {
        Ok(Some(bytes)) => plist::from_bytes(&bytes).unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
pub(crate) fn set_download_urls(path: &Path, urls: &[&str]) -> bool {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &urls).unwrap();
    xattr::set(path, WHERE_FROMS_ATTR, &buf).is_ok()
}

fn container_for(path: &str) -> Result<ContainerInfo> {
    #[derive(Deserialize)]
    struct Info {
        #[serde(rename = "APFSContainerReference")]
        container: String,
    }
    #[derive(Deserialize)]
    struct List {
        #[serde(rename = "Containers")]
        containers: Vec<Container>,
    }
    #[derive(Deserialize)]
    struct Container {
        #[serde(rename = "ContainerReference")]
        reference: String,
        #[serde(rename = "CapacityCeiling")]
        capacity: u64,
        #[serde(rename = "CapacityFree")]
        free: u64,
        #[serde(rename = "Volumes")]
        volumes: Vec<Volume>,
    }
    #[derive(Deserialize)]
    struct Volume {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "DeviceIdentifier")]
        device: String,
        #[serde(rename = "Roles", default)]
        roles: Vec<String>,
        #[serde(rename = "CapacityInUse")]
        used: u64,
    }

    let info: Info = plist::from_bytes(&run("diskutil", &["info", "-plist", path])?)
        .context("parsing diskutil info")?;
    let list: List = plist::from_bytes(&run("diskutil", &["apfs", "list", "-plist"])?)
        .context("parsing diskutil apfs list")?;
    let c = list
        .containers
        .into_iter()
        .find(|c| c.reference == info.container)
        .ok_or_else(|| anyhow!("container {} not found", info.container))?;
    let mut volumes: Vec<VolumeInfo> = c
        .volumes
        .into_iter()
        .map(|v| VolumeInfo {
            name: v.name,
            device: v.device,
            roles: v.roles,
            used: v.used,
        })
        .collect();
    volumes.sort_by_key(|v| std::cmp::Reverse(v.used));
    Ok(ContainerInfo {
        reference: c.reference,
        capacity: c.capacity,
        free: c.free,
        volumes,
    })
}

fn local_snapshots() -> Result<Vec<String>> {
    let out = run("tmutil", &["listlocalsnapshots", "/"])?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("com.apple."))
        .map(String::from)
        .collect())
}

/// Bytes of a file that no other file shares (APFS "private size"): 0 for a full
/// clone, its whole allocation for an ordinary file. `None` if the filesystem
/// doesn't say (HFS+, exFAT, network volumes).
pub(crate) fn private_size(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut req: libc::attrlist = unsafe { std::mem::zeroed() };
    req.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    req.commonattr = libc::ATTR_CMN_RETURNED_ATTRS;
    // With FSOPT_ATTR_CMN_EXTENDED, `forkattr` selects extended common attributes.
    req.forkattr = libc::ATTR_CMNEXT_PRIVATESIZE;

    // Reply: u32 length, the attribute_set_t of what was returned, then the off_t.
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        returned: libc::attribute_set_t,
        private_size: libc::off_t,
    }
    let mut reply: Reply = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            (&mut req as *mut libc::attrlist).cast(),
            (&mut reply as *mut Reply).cast(),
            std::mem::size_of::<Reply>(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    let returned = reply.returned.forkattr;
    let size = reply.private_size;
    (rc == 0 && returned & libc::ATTR_CMNEXT_PRIVATESIZE != 0 && size >= 0).then_some(size as u64)
}

/// Where diskwise keeps caches: ~/Library/Caches/diskwise.
pub(crate) fn cache_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/Caches/diskwise"))
}

/// Whether the volume holding `path` can make clones (APFS).
pub(crate) fn can_clone(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) };
    name.to_bytes() == b"apfs"
}
