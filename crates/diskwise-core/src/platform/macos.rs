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
