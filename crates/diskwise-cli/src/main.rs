mod tui;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use diskwise_core::platform::{self, Platform};
use diskwise_core::rules::Safety;
use diskwise_core::suggest::Item;
use diskwise_core::{
    DEFAULT_ROOT, Finding, Node, NodeKind, RuleSet, ScanOptions, Snapshot, human_bytes, parse_size,
};

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
    /// Scan (or load a snapshot) and suggest what to clean up.
    Suggest {
        /// Directory to scan.
        #[arg(default_value = DEFAULT_ROOT)]
        path: PathBuf,
        /// Analyse a saved snapshot instead of scanning.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Your own rules file (TOML); its rules take priority over the built-in ones.
        #[arg(long)]
        rules: Option<PathBuf>,
        /// How many paths to list per suggestion.
        #[arg(long, default_value_t = 5)]
        items: usize,
        /// One line per suggestion.
        #[arg(short, long, conflicts_with = "json")]
        compact: bool,
        /// Print findings as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Browse the disk interactively and act on suggestions.
    Tui {
        /// Directory to scan.
        #[arg(default_value = DEFAULT_ROOT)]
        path: PathBuf,
        /// Start from a saved snapshot instead of scanning.
        #[arg(long)]
        from: Option<PathBuf>,
        /// Your own rules file (TOML); its rules take priority over the built-in ones.
        #[arg(long)]
        rules: Option<PathBuf>,
    },
    /// List the rules in effect.
    Rules {
        /// Your own rules file (TOML), listed first.
        #[arg(long)]
        rules: Option<PathBuf>,
        /// Show another platform's rules: macos or linux.
        #[arg(long)]
        os: Option<String>,
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
                progress: None,
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
            print_report(&load_snapshot(&file)?, &view);
        }
        Command::Suggest {
            path,
            from,
            rules,
            items,
            compact,
            json,
        } => {
            let snap = match from {
                Some(file) => load_snapshot(&file)?,
                None => {
                    eprintln!("Scanning {} …", path.display());
                    diskwise_core::snapshot(&path, ScanOptions::default(), true)
                        .with_context(|| format!("scanning {}", path.display()))?
                }
            };
            let now = now();
            // Use the rules of the platform the snapshot came from.
            let ruleset = load_rules(snap.platform(), rules.as_deref())?;
            let findings = diskwise_core::analyze(&snap, &ruleset, now);
            if json {
                println!("{}", serde_json::to_string_pretty(&findings)?);
            } else if compact {
                print_findings_compact(&findings);
            } else {
                print_findings(&findings, items);
            }
        }
        Command::Tui { path, from, rules } => {
            tui::run(tui::Options {
                root: path,
                from,
                rules,
            })?;
        }
        Command::Rules { rules, os } => {
            let p = match os.as_deref() {
                None => platform::CURRENT,
                Some(name) => Platform::by_name(name)
                    .with_context(|| format!("unknown platform {name:?} (try macos or linux)"))?,
            };
            for r in load_rules(p, rules.as_deref())?.rules() {
                let d = &r.def;
                println!(
                    "{:<28} {:<8} {:<11} {}",
                    d.id, d.safety, d.category, d.title
                );
            }
        }
    }
    Ok(())
}

/// Current time as Unix seconds.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn load_snapshot(file: &Path) -> Result<Snapshot> {
    let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_slice(&bytes).context("parsing snapshot")
}

/// Built-in rules for `platform`, with the user's file (if any) taking priority.
fn load_rules(platform: &Platform, user: Option<&Path>) -> Result<RuleSet> {
    match user {
        None => RuleSet::builtin_for(platform),
        Some(p) => {
            let text = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            RuleSet::with_user_rules(platform, &p.display().to_string(), &text)
        }
    }
}

const WRAP: usize = 92;

const GROUPS: [(Safety, &str); 3] = [
    (Safety::Safe, "SAFE: nothing is lost"),
    (Safety::Caution, "CAUTION: safe after a preparatory step"),
    (Safety::Review, "REVIEW: your data or a judgment call"),
];

fn print_totals(findings: &[Finding]) {
    let sum = |s: Safety| -> u64 {
        findings
            .iter()
            .filter(|f| f.safety == s && f.reclaimable())
            .map(|f| f.bytes)
            .sum()
    };
    println!(
        "\nReclaimable: {} safe, {} after a quick step, {} worth reviewing.",
        human_bytes(sum(Safety::Safe)),
        human_bytes(sum(Safety::Caution)),
        human_bytes(sum(Safety::Review))
    );
}

/// "  (from dl.humble.com)" when macOS recorded where the file was downloaded from.
fn from_note(item: &Item) -> String {
    item.source
        .as_ref()
        .map(|h| format!("  (from {h})"))
        .unwrap_or_default()
}

fn size_label(f: &Finding) -> String {
    if f.size_known {
        human_bytes(f.bytes)
    } else {
        "size ?".to_string()
    }
}

/// One line per finding: size, title, action, and the largest path.
fn print_findings_compact(findings: &[Finding]) {
    print_totals(findings);
    let home = std::env::var("HOME").unwrap_or_default();
    let tilde = |p: &str| match p.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => p.to_string(),
    };
    let title_w = findings
        .iter()
        .map(|f| f.title.chars().count())
        .max()
        .unwrap_or(0);
    let action_w = findings
        .iter()
        .map(|f| f.action.to_string().chars().count())
        .max()
        .unwrap_or(0);

    for (safety, heading) in GROUPS {
        let group: Vec<_> = findings.iter().filter(|f| f.safety == safety).collect();
        if group.is_empty() {
            continue;
        }
        println!("\n{heading}");
        for f in group {
            let location = match f.items.as_slice() {
                [] => String::new(),
                [one] => format!("{}{}", tilde(&one.path), from_note(one)),
                [first, rest @ ..] => format!(
                    "{}{} (+{} more)",
                    tilde(&first.path),
                    from_note(first),
                    rest.len()
                ),
            };
            let line = format!(
                "{:>9}  {:<title_w$}  {:<action_w$}  {location}",
                size_label(f),
                f.title,
                f.action.to_string(),
            );
            println!("{}", line.trim_end());
        }
    }
}

fn print_findings(findings: &[Finding], max_items: usize) {
    print_totals(findings);

    for (safety, heading) in GROUPS {
        let group: Vec<_> = findings.iter().filter(|f| f.safety == safety).collect();
        if group.is_empty() {
            continue;
        }
        println!(
            "\n── {heading} {}",
            "─".repeat(WRAP.saturating_sub(heading.len() + 4))
        );
        for f in group {
            let size = size_label(f);
            println!("\n{size:>9}  {}   [{} · {}]", f.title, f.category, f.action);
            print_wrapped("", &f.why);
            print_wrapped("→ ", &f.how);
            if let Some(p) = &f.prevent {
                print_wrapped("Prevent: ", p);
            }
            for item in f.items.iter().take(max_items) {
                println!(
                    "           {:>9}  {}{}",
                    human_bytes(item.bytes),
                    item.path,
                    from_note(item)
                );
            }
            if f.items.len() > max_items {
                let rest: u64 = f.items[max_items..].iter().map(|i| i.bytes).sum();
                println!(
                    "           {:>9}  … and {} more",
                    human_bytes(rest),
                    f.items.len() - max_items
                );
            }
        }
    }
}

/// Print `text` word-wrapped under the finding title, with `lead` on the first line.
fn print_wrapped(lead: &str, text: &str) {
    let indent = " ".repeat(11);
    let mut line = format!("{indent}{lead}");
    let mut empty = true;
    for word in text.split_whitespace() {
        if !empty && line.chars().count() + 1 + word.chars().count() > WRAP {
            println!("{line}");
            line = format!("{indent}{}", " ".repeat(lead.chars().count()));
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(word);
        empty = false;
    }
    println!("{line}");
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
        if sys.container.is_none()
            && let Some(fs) = &sys.filesystem
        {
            let used = fs.capacity.saturating_sub(fs.free);
            println!(
                "\nFilesystem: {} used of {}, {} free ({:.0}% full)",
                human_bytes(used),
                human_bytes(fs.capacity),
                human_bytes(fs.free),
                used as f64 / fs.capacity.max(1) as f64 * 100.0
            );
        }
        if !sys.mounts.is_empty() {
            println!("\nMounted filesystems:");
            for m in &sys.mounts {
                let used = m.capacity.saturating_sub(m.free);
                println!(
                    "  {:>10} used of {:>10} ({:>3.0}%)  {} [{}]",
                    human_bytes(used),
                    human_bytes(m.capacity),
                    used as f64 / m.capacity.max(1) as f64 * 100.0,
                    m.mount_point,
                    m.fs_type
                );
            }
        }
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
                && snap.root == platform::MACOS.default_root
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

    if !snap.skipped_mounts.is_empty() {
        println!("\nNot scanned (separate filesystems; scan them directly or use --cross-mounts):");
        for m in &snap.skipped_mounts {
            println!("  {}", snap.platform().display_path(m));
        }
    }

    println!("\nLargest items:");
    print_tree(t, 0, view, t.allocated);

    if snap.errors.count > 0 {
        let hint = if snap.platform().os == "macos" {
            "grant your terminal Full Disk Access\n(System Settings → Privacy & Security → Full Disk Access)"
        } else {
            "run as root (sudo) to include other users' files"
        };
        println!(
            "\n{} paths could not be read. If this is high, {hint}. First few:",
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
