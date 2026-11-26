//! macOS storage facts a file walk can't see: APFS volumes, local snapshots, swap.

use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub container: Option<ContainerInfo>,
    /// Local APFS snapshots (Time Machine, OS updates). Their size isn't reported by macOS.
    pub snapshots: Vec<String>,
    pub swap: Option<SwapInfo>,
    /// Probes that failed, so missing data isn't silently treated as zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
pub struct SwapInfo {
    pub total: u64,
    pub used: u64,
}

/// Collect system storage info for the APFS container holding `path`.
pub fn probe(path: &str) -> SystemInfo {
    let mut warnings = Vec::new();
    let container = container_for(path)
        .map_err(|e| warnings.push(format!("APFS volumes: {e:#}")))
        .ok();
    let snapshots = local_snapshots()
        .map_err(|e| warnings.push(format!("snapshots: {e:#}")))
        .unwrap_or_default();
    let swap = swap_usage()
        .map_err(|e| warnings.push(format!("swap: {e:#}")))
        .ok();
    SystemInfo {
        container,
        snapshots,
        swap,
        warnings,
    }
}

fn run(cmd: &str, args: &[&str]) -> Result<Vec<u8>> {
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

fn swap_usage() -> Result<SwapInfo> {
    let out = run("sysctl", &["-n", "vm.swapusage"])?;
    parse_swap(&String::from_utf8_lossy(&out))
}

/// Parses `total = 3072.00M  used = 1370.56M  free = 1701.44M  (encrypted)`.
fn parse_swap(s: &str) -> Result<SwapInfo> {
    let field = |key: &str| -> Result<u64> {
        let rest = s
            .split(&format!("{key} = "))
            .nth(1)
            .ok_or_else(|| anyhow!("no {key} in {s:?}"))?;
        let token = rest.split_whitespace().next().unwrap_or("");
        let (num, unit) = token.split_at(token.len().saturating_sub(1));
        let mult: f64 = match unit {
            "K" => 1024.0,
            "M" => 1024.0 * 1024.0,
            "G" => 1024.0 * 1024.0 * 1024.0,
            _ => return Err(anyhow!("unknown unit in {token:?}")),
        };
        Ok((num.parse::<f64>()? * mult) as u64)
    };
    Ok(SwapInfo {
        total: field("total")?,
        used: field("used")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_swapusage() {
        let s =
            parse_swap("total = 3072.00M  used = 1370.56M  free = 1701.44M  (encrypted)").unwrap();
        assert_eq!(s.total, 3072 * 1024 * 1024);
        assert!(s.used > 1370 * 1024 * 1024 && s.used < 1371 * 1024 * 1024);
    }
}
