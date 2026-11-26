use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use diskwise_core::{DEFAULT_ROOT, Node, NodeKind, ScanOptions, Snapshot, human_bytes, parse_size};

#[derive(Parser)]
#[command(name = "diskwise", version, about = "See where your disk space went")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a directory (default: the whole Data volume) and print a summary.
    Scan {
        /// Directory to scan.
        #[arg(default_value = DEFAULT_ROOT)]
        path: PathBuf,
        /// Write the full snapshot as JSON to this file.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Fold items smaller than this into "(smaller items)" (e.g. 10M, 1G).
        #[arg(long, default_value = "10M", value_parser = parse_size_arg)]
        min_size: u64,
        /// Also descend into other mounted volumes.
        #[arg(long)]
        cross_mounts: bool,
        /// Skip the macOS volume/snapshot/swap probes.
        #[arg(long)]
        no_system: bool,
        #[command(flatten)]
        view: ViewArgs,
    },
    /// Print the summary of a previously saved snapshot.
    Report {
        file: PathBuf,
        #[command(flatten)]
        view: ViewArgs,
    },
}

#[derive(clap::Args)]
struct ViewArgs {
    /// How many levels of the tree to print.
    #[arg(short, long, default_value_t = 2)]
    depth: usize,
    /// How many entries to show per level.
    #[arg(short, long, default_value_t = 8)]
    top: usize,
}

fn parse_size_arg(s: &str) -> Result<u64, String> {
    parse_size(s).ok_or_else(|| format!("invalid size {s:?}"))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Scan {
            path,
            out,
            min_size,
            cross_mounts,
            no_system,
            view,
        } => {
            eprintln!("Scanning {} …", path.display());
            let opts = ScanOptions {
                min_node_size: min_size,
                one_filesystem: !cross_mounts,
            };
            let snap = diskwise_core::snapshot(&path, opts, !no_system)
                .with_context(|| format!("scanning {}", path.display()))?;
            if let Some(out) = out {
                fs::write(&out, serde_json::to_vec(&snap)?)
                    .with_context(|| format!("writing {}", out.display()))?;
                eprintln!("Snapshot saved to {}", out.display());
            }
            print_report(&snap, &view);
        }
        Command::Report { file, view } => {
            let bytes = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let snap: Snapshot = serde_json::from_slice(&bytes).context("parsing snapshot")?;
            print_report(&snap, &view);
        }
    }
    Ok(())
}

fn print_report(snap: &Snapshot, view: &ViewArgs) {
    let t = &snap.tree;
    println!(
        "\n{}  —  {} on disk, {} files, {} folders, scanned in {:.1}s",
        snap.root,
        human_bytes(t.allocated),
        t.files,
        t.dirs,
        snap.duration_ms as f64 / 1000.0
    );

    if let Some(sys) = &snap.system {
        if let Some(c) = &sys.container {
            let used = c.capacity.saturating_sub(c.free);
            println!(
                "\nDisk ({}): {} used of {}, {} free ({:.0}% full)",
                c.reference,
                human_bytes(used),
                human_bytes(c.capacity),
                human_bytes(c.free),
                used as f64 / c.capacity as f64 * 100.0
            );
            println!("\nAPFS volumes (shared space):");
            for v in &c.volumes {
                println!(
                    "  {:>10}  {} [{}]",
                    human_bytes(v.used),
                    v.name,
                    v.roles.join(", ")
                );
            }
            if let Some(data) = c
                .volumes
                .iter()
                .find(|v| v.roles.iter().any(|r| r == "Data"))
                && snap.root == DEFAULT_ROOT
                && data.used > t.allocated
            {
                println!(
                    "\n  Data volume uses {} but the scan found {}: {} is in snapshots,\n  \
                         purgeable space, or folders the scan could not read.",
                    human_bytes(data.used),
                    human_bytes(t.allocated),
                    human_bytes(data.used - t.allocated)
                );
            }
        }
        if let Some(s) = &sys.swap {
            println!(
                "\nSwap: {} used of {} allocated",
                human_bytes(s.used),
                human_bytes(s.total)
            );
        }
        if !sys.snapshots.is_empty() {
            println!(
                "\nLocal snapshots ({}), size not reported by macOS:",
                sys.snapshots.len()
            );
            for s in &sys.snapshots {
                println!("  {s}");
            }
        }
        for w in &sys.warnings {
            println!("\n  warning: {w}");
        }
    }

    println!("\nLargest items:");
    print_tree(t, 0, view, t.allocated);

    if snap.errors.count > 0 {
        println!(
            "\n{} paths could not be read. If this is high, grant your terminal Full Disk Access\n\
             (System Settings → Privacy & Security → Full Disk Access). First few:",
            snap.errors.count
        );
        for s in snap.errors.samples.iter().take(5) {
            println!("  {s}");
        }
    }
}

fn print_tree(node: &Node, level: usize, view: &ViewArgs, total: u64) {
    if level >= view.depth {
        return;
    }
    for child in node.children.iter().take(view.top) {
        let pct = if total > 0 {
            child.allocated as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        let marker = match child.kind {
            NodeKind::Dir => "/",
            _ => "",
        };
        let note = if child.incomplete {
            "  (partly unreadable)"
        } else {
            ""
        };
        println!(
            "{}{:>10} {:>5.1}%  {}{}{}",
            "  ".repeat(level + 1),
            human_bytes(child.allocated),
            pct,
            child.name,
            marker,
            note
        );
        print_tree(child, level + 1, view, total);
    }
}
