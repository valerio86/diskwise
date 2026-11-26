# diskwise

A macOS disk-space analyser: scan the whole drive, explain where the space went
(including the parts a normal folder walk can't see), and suggest safe ways to
reclaim it.

## Status

| Phase | What | State |
|---|---|---|
| 1 | Scanner CLI + JSON snapshot + macOS probes (APFS volumes, snapshots, swap) | ✅ |
| 2 | Rules engine: recognise caches, regenerable data, app-managed data, user data | ⏳ |
| 3 | Tauri UI: treemap, suggestions list, move-to-Trash actions with verification | ⏳ |
| 4 | Optional local LLM to explain unrecognised folders | ⏳ |

## Usage

```sh
cargo build --release
./target/release/diskwise scan                       # whole Data volume
./target/release/diskwise scan ~/Library -d 3 -t 10  # one folder, deeper
./target/release/diskwise scan --out disk.snapshot.json
./target/release/diskwise report disk.snapshot.json
```

For a complete scan, give your terminal **Full Disk Access**
(System Settings → Privacy & Security → Full Disk Access); without it, protected
folders are reported as unreadable rather than silently skipped.

## How it measures

- **Scans `/System/Volumes/Data`** by default — all user data, once, without
  walking the sealed System volume or double-counting firmlinks.
- **Allocated size** (`st_blocks × 512`), not apparent size, so sparse and
  compressed files count what they really cost. Hard links are counted once.
- **Stays on one volume** unless `--cross-mounts` is given.
- **Things a walk can't see** come from `diskutil apfs list` (per-volume usage,
  e.g. Preboot/Recovery/VM), `tmutil listlocalsnapshots` and `vm.swapusage`. The
  gap between the Data volume's usage and the scan total is reported explicitly.

Known limitation: APFS clones (copy-on-write copies) are counted at full size.

## Layout

- `crates/diskwise-core` — scanner, snapshot model, system probes (library; the
  Tauri app will reuse it)
- `crates/diskwise-cli` — `diskwise` command-line tool
