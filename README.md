# diskwise

A disk-space analyser for macOS and Linux: scan the whole drive, explain where
the space went (including the parts a normal folder walk can't see), and suggest
safe ways to reclaim it.

## Status

| Phase | What | State |
|---|---|---|
| 1 | Scanner CLI + JSON snapshot + macOS probes (APFS volumes, snapshots, swap) | ✅ |
| 2 | Rules engine + `suggest`: caches, regenerable data, app-managed data, your data, system space | ✅ |
| 3 | Tauri UI: treemap, suggestions list, move-to-Trash actions with verification | ⏳ |
| 4 | Optional local LLM to explain unrecognised folders | ⏳ |

## Usage

```sh
cargo build --release
./target/release/diskwise scan                       # whole Data volume
./target/release/diskwise scan ~/Library -d 3 -t 10  # one folder, deeper
./target/release/diskwise scan --out disk.snapshot.json
./target/release/diskwise report disk.snapshot.json
./target/release/diskwise suggest                    # scan + cleanup suggestions
./target/release/diskwise suggest -c                 # one line per suggestion
./target/release/diskwise suggest --from disk.snapshot.json --json
./target/release/diskwise rules                      # list rules in effect
```

For a complete scan on macOS, give your terminal **Full Disk Access**
(System Settings → Privacy & Security → Full Disk Access); on Linux, run as root
to include other users' files. Either way, unreadable folders are reported rather
than silently skipped.

## Platforms

| | macOS | Linux |
|---|---|---|
| Default scan root | `/System/Volumes/Data` (the Data volume) | `/` |
| System probes | APFS volumes (`diskutil`), local snapshots (`tmutil`), swap | mounted filesystems (`/proc/self/mounts` + `statvfs`), swap (`/proc/meminfo`) |
| Download source | `kMDItemWhereFroms` | `user.xdg.origin.url` / `user.xdg.referrer.url` |
| Platform rules | Library caches, Xcode, Lightroom, Photos, … | apt/dnf/pacman caches, journald, Docker, snap, Flatpak, core dumps, … |

Snapshots record the OS they were taken on, so a Linux server's snapshot can be
copied to a Mac and analysed with `suggest --from` (with Linux rules). Checks that
look at the live filesystem (`sibling`, `from`) don't match on the other machine,
so those items fall through to more cautious suggestions.

Other filesystems mounted inside the scan root are not entered (use
`--cross-mounts`) but are listed, with their size where known, so a separate
`/home` partition can't go missing unnoticed.

CI runs the tests on Ubuntu and macOS. Windows is not supported.

## How it measures

- **On macOS, scans `/System/Volumes/Data`** by default — all user data, once,
  without walking the sealed System volume or double-counting firmlinks.
- **Allocated size** (`st_blocks × 512`), not apparent size, so sparse and
  compressed files count what they really cost. Hard links are counted once.
- **Stays on one volume** unless `--cross-mounts` is given.
- **Things a walk can't see** come from the platform probes above. On macOS the
  gap between the Data volume's usage and the scan total is reported explicitly;
  on Linux, nearly full filesystems get a finding of their own.

Known limitation: APFS clones (copy-on-write copies) are counted at full size.

## Suggestions

`diskwise suggest` groups what it finds by how safe it is to act on:

- **safe**: nothing is lost (caches, logs, installers you can download again)
- **caution**: safe after a preparatory step (quit the app first, check the project)
- **review**: your data or a judgment call, plus explanations of space you can't
  simply delete (Preboot, snapshots, the gap a file walk can't see)

Each suggestion says what the item is, how to clear it, and, where it matters,
how to stop it coming back (e.g. Chrome re-downloads its AI model unless a policy is set).

### Rules

Rules live in [`crates/diskwise-core/rules/`](crates/diskwise-core/rules/):
`common.toml` (field reference, dev tooling, Downloads, your files) plus
`macos.toml` and `linux.toml`. They are compiled into the binary; `diskwise rules
[--os linux]` lists them. A rule matches absolute paths with globs (`~/`
= every home folder) plus optional conditions: `kind`, `min_size`,
`older_than_days`, `sibling` (e.g. only `target/` next to a `Cargo.toml`). The
first matching rule claims a path, so specific rules come before generic ones;
catch-alls are marked `fallback = true` and tried after every specific rule.

Rules can also use `from = ["domain", …]`, which matches files macOS recorded
as downloaded from those domains (the `kMDItemWhereFroms` attribute). This is
how installers are split: bought from a store (Humble, GOG, itch.io, …) →
back it up first, since it may not be downloadable later; from a major vendor →
safe to trash; unknown → check first. Only host names are read and shown: the
recorded URLs can contain purchase keys.

Add your own with `--rules my-rules.toml`: they take priority, and a rule with
the same `id` as a built-in one replaces it.

## Layout

- `crates/diskwise-core` — scanner, snapshot model, system probes, rules and
  analysis (library; the Tauri app will reuse it). OS differences live in
  `src/platform/`: per-OS facts as plain data (testable anywhere) plus the
  probe code compiled per OS.
- `crates/diskwise-cli` — `diskwise` command-line tool
