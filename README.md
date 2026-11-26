# diskwise

A disk-space analyser for macOS and Linux: scan the whole drive, explain where
the space went (including the parts a normal folder walk can't see), and suggest
safe ways to reclaim it.

## Status

| Phase | What | State |
|---|---|---|
| 1 | Scanner CLI + JSON snapshot + macOS probes (APFS volumes, snapshots, swap) | ✅ |
| 2 | Rules engine + `suggest`: caches, regenerable data, app-managed data, your data, system space | ✅ |
| 3a | Terminal UI (`diskwise tui`): browse, suggestions, move to Trash with verification | ✅ |
| 3b | Desktop app (Tauri): treemap, suggestions, move to Trash | ✅ first version |
| 4 | Duplicates (`diskwise dupes`), older versions, growth over time | ⏳ exact duplicate files done |
| 5 | Optional local LLM to explain unrecognised folders | ⏳ |

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
./target/release/diskwise dupes                      # duplicate files in your home folder
./target/release/diskwise rules                      # list rules in effect
./target/release/diskwise tui                        # interactive browser
./target/release/diskwise tui --from disk.snapshot.json
```

## Terminal UI

`diskwise tui` scans (with live progress) and opens an `ncdu`-style browser that
works locally or over SSH:

- **Suggestions** (opens here; `1`): the `suggest` list; Enter jumps to the item in the
  browser.
- **Browse** (Tab or `2`): folders by size with bars. Items a suggestion covers are tagged
  and colour-coded (blue safe, amber caution, pink review: checked for colour-vision deficiency); folders show how
  much is cleanable inside them. The details pane says what an item is and how
  to clear it.
- **Move to Trash** (`d`): always asks first, showing the exact paths, total,
  and any caution. Never deletes outright. Items that should be cleared inside
  their app, need an admin password, or only explain space get instructions
  instead; top-level and home folders are refused. After moving, diskwise checks
  each path is really gone before updating the tree. Space comes back when the
  Trash is emptied. On macOS it uses NSFileManager (no Finder permission prompt,
  works over SSH), so Finder's "Put Back" isn't offered; drag items back instead.
- **Links** to folders are always listed, in italics as `name → target`: what
  they point to is counted where it lives. ⏎ follows a link when its target was
  scanned. Moving a link to the Trash moves only the link (the dialog says so),
  and rules never match links.
- `s` sorts by size, name or oldest first; `r` rescans; `?` lists all keys.
- **Colours** follow what the terminal supports: exact colours where
  `COLORTERM=truecolor`, the 256-colour palette for `*-256color` terminals (and
  macOS Terminal, iTerm, VS Code, …), the 16 standard colours for basic ones, and
  none with `NO_COLOR` or `TERM=dumb` (inverse video and `█░` bars instead).
  Override with `--color auto|truecolor|256|16|none`. The selected row and text
  are readable on light and dark terminal themes alike.

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

## Desktop app

```sh
cargo run -p diskwise-app                       # scan the whole disk
cargo run -p diskwise-app -- ~/Library          # one folder
cargo run -p diskwise-app -- --from disk.snapshot.json
```

A treemap of the disk where only space a suggestion covers is coloured: blue
safe to clean, amber safe after a step, pink your call. Untouched space stays
grey, so the map lights up where you can reclaim space. Large folders show their
contents in miniature, and miniatures fill from the bottom with the cleanable
share deeper inside.

Two sections, as in the TUI (⌘1 / ⌘2):
- **Suggestions** (opens first): totals, then a list grouped by safety with bars
  on one scale (only for space you can free), and the whole selected suggestion
  beside it: what it is, what to do, how to stop it coming back, every path.
- **Browse**: the map (or List view) with a narrow panel explaining the
  selection. "Show on map" jumps here with that suggestion's items highlighted;
  "Back to suggestion" in the chip jumps back.
Moving to the Trash works as in the TUI (confirmation, refusals, verification),
using the same core code. There's a List view with the same data as a table.

The safety colours were checked with a colour-vision-deficiency validator in both
light and dark mode (the obvious green/amber pair fails for protanopia, so safe
is blue). The app follows the system light/dark setting.

Run from a terminal, the app inherits the terminal's Full Disk Access. A bundled
`.app` needs its own entry in System Settings → Privacy & Security → Full Disk Access.

To check the UI without running the app, `crates/diskwise-app/dev/preview.sh`
screenshots the real `ui/` in headless Chrome against fixtures made from a
snapshot (the fixtures hold real paths and are git-ignored).

## Moving things to the Trash, safely

Both the TUI and the desktop app go through one check before anything moves:

- Never deleted outright, only moved to the Trash, and only after a confirmation
  that shows the exact paths.
- Refused: the scanned folder itself; top-level folders; every home folder
  (including relocated ones, e.g. `/var/home` on Fedora Silverblue, found via
  `$HOME` and the user database); system and package-manager trees (`/System`,
  `/Library`, `/private`, `/usr`, `/opt/homebrew` on macOS; `/usr`, `/etc`,
  `/var/lib`, `/boot`, … on Linux).
- Refused if the item has changed since the scan (gone, or a different kind or
  size), if its name isn't valid UTF-8 (diskwise can't name it exactly), or if
  the scan came from another OS.
- Nothing is moved from a saved snapshot (`--from`): it may be stale or from
  another machine. Scan again first.
- Items outside home folders that no suggestion covers get a stronger warning.
- After moving, each path is checked to be really gone.

## Suggestions

`diskwise suggest` groups what it finds by how safe it is to act on:

- **safe**: nothing is lost (caches, logs, installers you can download again)
- **caution**: safe after a preparatory step (quit the app first, check the project)
- **review**: your data or a judgment call, plus explanations of space you can't
  simply delete (Preboot, snapshots, the gap a file walk can't see)

Each suggestion says what the item is, how to clear it, and, where it matters,
how to stop it coming back (e.g. Chrome re-downloads its AI model unless a policy is set).

## Duplicates

`diskwise dupes [FOLDER]` (default: your home folder) finds files of 1 MB or
more with identical contents and suggests which copy to keep (the one outside
Downloads/Desktop, without "copy" or "(1)" in its name, then the oldest). It
deletes nothing.

- Only your own files are compared: not ~/Library, tools' dot-folders, app
  packages (`.app`, Photos/Lightroom libraries) or `.git`/`node_modules`.
- The space shown is what deleting the extra copies would **really** free.
  Hard links and APFS clones (Finder's Duplicate) share storage, so they count
  as nothing; on Linux, files with reflinked extents count as nothing too.
- Files are compared by size, then their first and last 64 KB, then a full
  BLAKE3 hash. Full hashes are cached (`~/Library/Caches/diskwise`,
  `~/.cache/diskwise` on Linux), so reruns only read files that changed.

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
- `crates/diskwise-cli` — `diskwise` command-line tool and TUI (`src/tui/`:
  `app.rs` state and behaviour, unit-tested without a terminal; `ui.rs` drawing,
  tested by rendering to a text buffer; `theme.rs` colour detection)
- `crates/diskwise-app` — Tauri desktop app: `src/session.rs` (everything the
  UI asks for, plain Rust and unit-tested), `src/main.rs` (Tauri commands and
  events), `ui/` (HTML/CSS/JS, no build step), `dev/` (preview harness, icon
  generator)
- `dev/cells2png.py` — renders a TUI screen dump to PNG to check colours
