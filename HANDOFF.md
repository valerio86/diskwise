# diskwise — handoff

State of the project as of 2026-11-26, written so a new session can pick up
without the conversation that built it. README.md covers what diskwise does and
how to use it; this file covers how to work on it and what's next.

## Where things stand

| Phase | What | State |
|---|---|---|
| 1 | Scanner + JSON snapshots + system probes | done |
| 2 | Rules engine + `diskwise suggest` (41 rules: common/macOS/Linux) | done |
| — | Linux support (CI on Ubuntu + macOS) | done |
| 3a | Terminal UI `diskwise tui` | done |
| 3b | Desktop app (Tauri 2), `crates/diskwise-app` | first version done |
| 4 | Duplicates, older versions, growth ("correlation" findings) | **in progress**: 4.1 core + CLI done |
| 5 | Optional local LLM to explain unrecognised folders | planned below |

Two full code reviews have been done and every finding fixed (commits 1ad8f48,
0b0d063). 68 tests; clippy clean on macOS, Linux and the app.

## Decisions already made (don't relitigate)

- **Rules first, LLM optional.** The value is the knowledge layer (what is this,
  is it safe, how to stop it coming back). A ~2 GB model inside a space-saving
  app is self-defeating, so the LLM is opt-in and never required.
- **macOS + Linux; no Windows.** Per-OS facts are plain data in
  `crates/diskwise-core/src/platform/` so any platform's rules test anywhere;
  snapshots record their OS (a Linux server snapshot can be read on a Mac).
- **Never delete, only move to the Trash**, always after a confirmation listing
  exact paths. One shared check (`actions::check_trash` + `Protection`) runs at
  plan time and again right before each trash. It refuses saved snapshots,
  the scan root, home folders (incl. relocated ones), system/package-manager
  trees, items changed since the scan, non-UTF-8 names, other-OS snapshots.
- **Installers are split by download source** (`kMDItemWhereFroms` /
  `user.xdg.origin.url`): store purchases → back up first; major vendors → safe;
  unknown → caution. Only host names are kept (URLs can hold purchase keys).
- **Archives are never "safe"** (they may be the only copy).
- **UI look follows Claude Code's own dialogs**: lavender accent, tabs inline,
  few boxes, bars as fill on a slate track. **Suggestions opens first**, Browse
  second (TUI and desktop alike).
- **Safety colours are blue / amber / pink**, validated for colour-vision
  deficiency in light and dark. Green next to amber fails for protanopia; don't
  reintroduce it. Colour is never the only signal (labels/headings name levels).
- **Commands in rule texts go in backticks** (a test enforces it): the desktop
  app shows them as code with a Copy button, the TUI highlights them.
- **Plain JS frontend, no Node build step** (Node is only used to run the
  palette validator).

## Working on it

```sh
cargo test --workspace              # everything (60 tests)
cargo clippy --all-targets          # core + CLI (default members)
cargo clippy -p diskwise-app --all-targets
cargo clippy --all-targets --target x86_64-unknown-linux-gnu   # Linux check from a Mac
cargo run -p diskwise-app           # desktop app (whole disk)
cargo run -p diskwise-app -- --from snap.json
target/release/diskwise tui         # TUI
```

- Rust comes from Homebrew's rustup: `PATH=/opt/homebrew/opt/rustup/bin:$PATH`
  (already in ~/.zshrc).
- The workspace's `default-members` exclude the app (Tauri needs WebKitGTK on
  Linux). CI builds the app on macOS only.
- **Tests that trash must use real folders under `target/test-tmp/`**, not the
  system temp dir: on macOS that lives under `/private`, which is protected, and
  the checks rightly refuse made-up paths. See `disk_app` (TUI) and
  `live_session` (app) test helpers.
- **Seeing the UI without clicking:**
  - Desktop: `crates/diskwise-app/dev/preview.sh name 'query' dark|light …`
    renders the real `ui/` in headless Chrome with fixtures from a snapshot
    (`DISKWISE_SNAPSHOT=snap.json`). Fixtures/screenshots hold real paths and
    are git-ignored. Theme is forced with Blink's `preferredColorScheme`; never
    use Chrome's WebContentsForceDark flag (it inverts the page).
  - TUI: `DISKWISE_SNAPSHOT=… DISKWISE_RENDER_DIR=… DISKWISE_COLOR=256 cargo test
    -p diskwise-cli render_real -- --ignored`, then `dev/cells2png.py` (needs
    Pillow; use a scratch venv) to see colours. In a pty, set a size first
    (`stty rows 40 cols 120`) or nothing draws.
- **Palette changes:** validate with the dataviz skill's
  `validate_palette.js` (`--pairs all` for treemaps; both modes).

## Open follow-ups (not blocking phase 4)

- **Real trashing hasn't been exercised by hand** in either app (tests use a
  fake trasher on scratch folders). Try once on something disposable.
- **The real desktop window** has only been smoke-launched; UI checks were via
  the preview harness. Needs a hands-on click-through.
- **Bundling**: no signed `.app` yet (`cargo tauri build` needs `tauri-cli`).
  A bundled app needs its own Full Disk Access entry; signing/notarising needs
  an Apple Developer ID.
- The **TUI and app keep separate copies of the plan/warning wording**
  (`refusal`, `warning_for`); the checks themselves are shared. Could move into
  core if they drift.
- **Server wishes** from earlier discussion, unscheduled: per-user breakdowns,
  scheduled scans with "what grew since last time", generic rules for shares
  outside home folders (e.g. `/srv`).

## Phase 4: duplicates, older versions, growth

Decided with the user 2026-11-26: do this **before** the LLM (facts beat
guesses, and it shrinks what's left to explain). Order: exact file dupes →
duplicate folders → older versions → growth between snapshots. **Suggest only**:
no archive action yet (an "archive to another volume" action may come later and
needs its own design, since it breaks the "only ever the Trash" model).
**Minimal user input**: runs over the whole home folder without being asked.

### 4.1 Exact duplicate files: done (`crates/diskwise-core/src/dupes.rs`)

- **The scan collects candidates** as it walks, so there's no second walk:
  `ScanOptions::large_file_min` (default 1 MiB) → `Snapshot::large_files`
  (snapshot version 5; older snapshots load with it empty). Only files in
  people's own places are collected: inside home folders when the root contains
  them (`/`, `/Users`), or anywhere under a root that doesn't (`/srv`, an
  external drive). Never ~/Library, home dot-folders, `.git`/`node_modules`/
  caches, or packages (`.app`, `.photoslibrary`, `.lrdata`, …): deleting one
  "duplicate" there breaks the owner. Only files whose size another candidate
  shares are kept (home: ~1.3k entries).
- **`dupes::find`**: checks that each file is unchanged since the scan, groups
  by size, then by BLAKE3 of the first and last 64 KiB (always, so cached and
  uncached copies stay comparable), then by full BLAKE3 (cached by path, size,
  mtime and inode in `~/Library/Caches/diskwise/hashes.json`, or
  `$XDG_CACHE_HOME/diskwise` on Linux).
- **Reclaimable = what deleting really frees.** Hard links (nlink > 1) free 0.
  macOS: APFS *private size* via `getattrlist(ATTR_CMNEXT_PRIVATESIZE)`, so
  clones free 0 (tested strictly with `cp -c`; the repo and CI are on APFS).
  Linux: FIEMAP, and any shared extent counts the file as 0. Always errs low.
  Groups that would free nothing are reported separately as `already_shared`.
- **Which copy to keep**: lowest "stray copy" score (Downloads/Desktop/tmp,
  `copy`, `(1)`, `backup`/`old`/`archive` folders), then by **creation time**
  (`LargeFile::created`; Finder copies keep the original's mtime, so mtime
  can't tell them apart) per `Prefer::Original` (default) or
  `Prefer::NewestCopy`, then shortest path. First copy in `Group::copies` is
  the keeper. The automatic pick is only a default: the user must be able to
  override it (see 4.2).
- **A suggested action per extra copy** (`Copy::action`), decided with the user:
  - `Trash`: a clear stray copy. Same folder as the keeper, with the keeper's
    name plus a copy marker (`copy`, `copy 2`, `(1)`, `_1`, `-2`, `Copy of …`).
    Judged *relative to the keeper*, so `IMG_1.jpg` alone never counts.
  - `StoreOnce`: in doubt (same contents elsewhere; something may refer to that
    path). Replace it with a clone of the keeper, so both paths keep working and
    the contents are stored once. Only on the keeper's volume, and only where
    clones work (`platform::can_clone`: APFS; btrfs/XFS).
  - `Review`: in doubt, but a clone isn't possible: Trash only, the user's call.
  - `Nothing`: already shares storage with the keeper.
  - On the user's home: 20 copies (145 MB) Trash, 499 (2.3 GB) StoreOnce.
- **`diskwise dupes [PATH]`** (default `$HOME`; `--from`, `--top`, `--json`,
  `--no-cache`, `--prefer original|newest`). On the user's home: 2.4 GB in 394 groups, ~5 s; reruns use the
  cache for hashing (the scan dominates).

### 4.2 next: duplicate folders, then the UIs

- **Duplicate folders**: fingerprint each folder bottom-up from its files'
  (name, size, hash) and report the *top* duplicated folder, not 4,000 files
  inside it (real case: `~/Downloads/New FLC` is a copy of
  `~/Documents/New FLC`). Then near-duplicates (≥ 90% same: "a copy that
  drifted"). Small files inside such folders matter here, so `large_files`
  may need to cover them; measure the snapshot size first.
- **TUI and desktop**: a "Duplicates" group under Suggestions, filled in the
  background after the scan (`DupeProgress` exists for a progress bar). Safety
  at most "caution".
- **Choosing the keeper**, least input first: (1) swap the keeper per group;
  (2) on a swap, offer "always prefer folder A over folder B", saved in a small
  preferences file and applied to every group with that pair of folders (copies
  come in batches); (3) the `Prefer` setting (original / newest copy). After a
  swap, actions are recomputed against the new keeper.
- **"Store once" action**: on the copy's volume, clone the keeper next to the
  copy (`clonefile` on macOS, `FICLONE` on Linux), carry over the copy's own
  metadata (permissions, owner, Finder tags and other xattrs, creation and
  modification dates), check the clone hashes the same, then `rename` it over
  the copy (atomic). Re-check the copy is unchanged right before. Never
  symlinks or hard links (the user finds them heavy-handed, and hard links
  make later edits show up in both).
- **References to a trashed copy** aren't tracked in general (too hard to do
  cleanly). Cheap safeguards only: it's the Trash (recoverable); warn if a
  symlink in the scan points at the copy; warn when project or library files
  sit nearby (e.g. a Lightroom catalog). "Store once" sidesteps the issue,
  which is why it's the in-doubt default.
- **Acting on duplicates needs a file-level trash check.** `check_trash` only
  accepts nodes in the tree, and files under `min_node_size` (10 MB) are folded
  into "(smaller items)". Add a check against the `LargeFile` record (same
  size, mtime, inode; never the last remaining copy; re-hash before trashing),
  keeping every other `Protection` rule.

### 4.3 later: older versions, growth

- Older versions: names that match after normalising (`_v2`, `-final`, dates,
  `(1)`), same type, different mtimes; installers of one product at several
  versions. Always "review" ("keep the newest" is a guess).
- Cold data (large, untouched for 2+ years) as an archive suggestion.
- Growth between snapshots ("what grew since last time"): also on the server
  wish list.

## Phase 5 plan: explain what no rule recognises

### Goal

For big folders no rule claims, answer "what is this, and what would clearing it
mean?" in plain language, so the user isn't left guessing at
`~/Library/Application Support/com.something.helper`. The LLM **explains and
proposes**; it never decides. Its output is labelled as an unverified guess,
can never make anything "safe", and can never move anything to the Trash by
itself.

### Recommended approach (decide before building)

1. **Runtime: talk to Ollama over localhost (recommended first).** The user
   installs Ollama and a small model (e.g. a 3–4B instruct model) themselves;
   diskwise calls `http://127.0.0.1:11434/api/chat` with JSON output. No
   model bundled, no native build deps, works on macOS and Linux, easy to
   swap models. Behind a small `Explainer` trait so other back ends fit later:
   - llama.cpp embedded (`llama-cpp-2` crate + a downloaded GGUF): no separate
     install, but heavy build and a ~2 GB download.
   - Apple's on-device Foundation Models (macOS 26+): no download, Mac only,
     needs a Swift bridge.
   *Decision needed:* confirm Ollama-first, and which default model.
2. **Privacy, strictly:** send only what's needed to recognise the folder — its
   path (home folder shortened to `~`), size, age, and the names of its largest
   children — never file contents. Only ever to localhost (refuse non-loopback
   URLs). Opt-in (`--explain` / a toggle), and the UI can show exactly what was
   sent. *Decision needed:* is sending child *names* acceptable?
3. **What gets explained:** unclaimed nodes above a threshold (e.g. ≥ 500 MB),
   largest first, capped per run (e.g. 15). Cached per path + size bucket in
   `~/.cache/diskwise/explanations.json` (Linux: XDG cache dir) so reruns are
   instant.
4. **Output contract (JSON, validated):**
   `{ what, owner, regenerable: yes|no|unknown, how_to_clear, confidence,
   suggested_rule }`. Safety is derived by diskwise, not taken from the model:
   at most "review" (or "caution" only if regenerable=yes *and* confidence is
   high); never "safe", never trash-on-confirm without the normal checks.
   `suggested_rule` is a TOML snippet the user can copy into `--rules`; that's
   how good explanations become real, tested rules.
5. **Where it shows:**
   - CLI: `diskwise explain [PATH]` and `suggest --explain`.
   - TUI and desktop: an "Unrecognised" group under Suggestions, visually
     distinct ("AI guess · unverified"), with the copyable rule snippet.

### Milestones

1. `Explainer` trait + Ollama client + prompt + JSON validation + cache, with a
   mock explainer for tests (no network in CI).
2. `diskwise explain` CLI; pick unclaimed large nodes from a snapshot.
3. **Offline evaluation before any UI:** run the explainer on paths the
   built-in rules *do* recognise and compare its category/regenerable answer
   with the rule's. That measures accuracy with no labelling work, and tells us
   whether the default model is good enough.
4. TUI + desktop "Unrecognised" group; copy rule snippet.
5. Docs: install Ollama, pick a model, privacy statement.

### Risks to watch

- Confident wrong answers → mitigated by capping safety, labelling, and never
  acting automatically; the eval in milestone 3 quantifies it.
- Slow first run on CPU-only machines → cap count, run in background, cache.
- Prompt injection via folder names (a folder named "ignore previous
  instructions…") → treat names as data in the prompt, validate the JSON
  strictly, and since output can't trigger actions, the blast radius is text.
