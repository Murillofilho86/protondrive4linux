# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
[Semantic Versioning](https://semver.org/). New work lands under
`[Unreleased]` and moves to a dated version when it ships.

## [Unreleased]

## [0.2.1] - 2026-07-25

### Added
- Streaming full walk. The full reconcile now walks the tree folder-by-folder
  and transfers as it goes (a concurrent breadth-first walk of shallow,
  direct-children reconciles), instead of scanning the whole tree and only then
  applying. Uploads and downloads overlap the walk, so a large tree starts
  syncing immediately rather than after a long scan. Each folder is reconciled by
  the same scoped, positive-confirmation primitive, so containment and the
  data-safety invariants hold per folder.

### Fixed
- Self-feeding watch loop. The daemon's own reconcile reads were firing
  filesystem access/attribute events, which marked the just-read folders "hot"
  and triggered endless re-scanning, so the watcher never settled. It now only
  treats real content changes (create, write, delete, rename) as changes and
  ignores access/metadata events.
- Deletion safety (from an adversarial review of the folder-scoped reconcile): a
  failed local directory read no longer reads as "everything deleted"; a
  sub-folder's baseline rows are pruned only when its delete actually completed
  (not on a cancelled or failed one); and a remote "not found" is treated as a
  missing listing (deletes suppressed) rather than an empty one.
- Packaging shipped no app icon, so the `.deb`/`.rpm` launcher fell back to a
  generic system icon and a second, differently-named entry. The packages now
  ship the NeutronSync icon, the desktop entry uses it (with a matching
  `StartupWMClass`), and the app writes its user-level desktop file under the
  same name so it shadows the packaged one instead of duplicating it.

### Changed
- Startup and change-event bursts reconcile their folders concurrently (a worker
  pool), instead of one slow CLI cold-start at a time, so a batch of folders
  syncs about as fast as the full walk rather than sequentially.
- Change-triggered syncs process the newest change first, so a fresh edit is
  never starved behind a backlog of older or no-op folder checks.
- The Activity banner shows the folder currently being checked during a
  single-folder (shallow) reconcile, instead of a generic "Scanning folder
  pairs".

## [0.1.1] - 2026-07-24

### Fixed
- Data loss on an interrupted sync. A run that was cancelled (or killed) before
  its transfers ran used to record the not-yet-transferred files in the baseline
  as if they were synced. The next run then saw those files missing locally and
  propagated a deletion of the remote copy. The baseline is now committed by
  positive confirmation: a file is recorded only after its transfer actually
  completes, so a failed or cancelled operation leaves the previous state intact
  and is simply retried next run, never mistaken for a deletion.
- Excluded folders were still walked during the scan (and only skipped later).
  They are now pruned from the scan itself, on both the remote tree walk and the
  local walk, so an excluded sub-tree is never listed, descended, or stat-walked.

### Added
- Sub-folder-scoped sync. A local change now syncs only the folder it happened
  in, not the whole pair; the engine can scan/reconcile/commit a single sub-tree
  (never seeing anything outside it, so it can't be mistaken for a deletion).
- Live activity across processes. The background tray daemon publishes its state,
  so an open GUI window shows what the daemon is doing (scanning, syncing,
  per-file operations) in real time instead of appearing idle.
- CLI syncs are recorded to the activity feed. Any `neutronsync sync` now writes
  its operations to the same store the GUI reads, so a sync run from the command
  line shows up in the GUI's Activity view (previously only GUI- and
  tray-daemon-initiated syncs appeared there).

### Changed
- Watch scheduling, prioritized and cheap. A local change triggers a **shallow**
  reconcile of just the folder whose direct contents changed (files merged,
  immediate sub-folders created/removed but not descended into) — deeper changes
  arrive as their own watch events, so touching a file never re-walks a subtree.
  On startup, folders with fresh local changes sync first, then recently-active
  ("hot") folders, then the full walk. The full walk is paced to how long a walk
  actually takes (about six times its duration, floored at `poll_interval`), so a
  large tree is not re-walked constantly while small/active folders stay fresh.
- The watch daemon reloads the config file at the start of each reconcile, so
  exclusion and setting changes take effect without restarting it.
- Tray "Quit" now closes the open window as well as the background daemon, giving
  one clean exit. Closing the window still leaves the tray syncing in the
  background.

## [0.1.0] - 2026-07-24

### Added
- Bidirectional folder sync on the official `proton-drive` CLI, with a
  three-way-merge engine and a per-pair baseline snapshot (stored in SQLite).
- CLI (`neutronsync`): `init`, `login`, `logout`, `status`, `doctor`, `sync`
  (`--dry-run` / `--resync` / `--json`), and `watch`.
- Native GUI (`neutronsync-gui`, `gui` feature): egui/eframe app with a Folders,
  Activity, Settings, Account, and About layout, a remote-folder browser, and an
  optional system tray. Original "neutron" atom logo.
- Per-pair selective sync: exclude sub-folders (in the GUI or via `exclude` in
  the config). Excluded paths are ignored by the engine and never touched on
  Proton. Excluding an already-synced folder freezes both copies; NeutronSync
  offers to remove the local copy (to the desktop trash) to free space while the
  cloud copy stays. Re-including re-downloads from the cloud, so excluding can
  never delete remote data.
- Watch daemon: filesystem events (debounced) plus a "hot" cache of recently
  active folders and a periodic full rescan as the safety net. Single-instance
  locked.
- Concurrent remote tree scan (`scan_threads`) and concurrent, retrying
  downloads, since the CLI has no recursive listing.
- Local hash cache: in `sha1` mode only files whose size or mtime changed are
  re-hashed.
- In-app update check (GUI): follows a `stable` or `prerelease` channel from
  GitHub releases and notifies when a newer version exists. It never downloads
  or installs anything itself.
- GUI backend (`service::Controller`): observable `AppState`, a structured
  `SyncEvent` stream, and cancellation.
- Recoverable deletes both ways: remote to Proton's online trash, local to the
  desktop trash (via `gio`, with an XDG-trash fallback).
- Trust model made explicit in the GUI: the sign-in and About screens state that
  logging in signs the CLI into Proton and that NeutronSync never sees your
  credentials, plus a note on how change detection works and a not-affiliated
  disclaimer.
- Packaging: `.deb`, `.rpm`, and a portable binary tarball via the release
  workflow; systemd user units. Commits and tags are GPG-signed.

### Fixed
- Data-safety: a failed upload or download no longer records post-transfer state
  in the baseline. Only files that fully synced this run are written; a failed
  transfer leaves that file's baseline row untouched so the next run retries it
  in the correct direction. A timeout on one file can no longer cause the stale
  side to overwrite a newer edit, and never invalidates the rest of the sync.
- Data-safety: a transient local stat failure (a file locked or renamed
  mid-scan) no longer makes a known file look deleted; its baseline entry is
  carried forward and the file is retried.
- Data-safety: a remote folder that reports "not found" while being walked is
  treated as a partial scan (deletions suppressed), not as an empty folder, so a
  transient listing failure can't trigger a mass local delete.
- Data-safety: a path that is a file on one side and a directory on the other is
  surfaced and skipped instead of being silently left diverged.
- Data-safety: a missing local root (an unmounted drive) is refused rather than
  mistaken for a mass deletion; the watch daemon no longer recreates a
  previously-synced root that has gone missing. A vanished remote base folder
  likewise suppresses deletions for that run.

### Security
- Path traversal: remote node names are validated to a single safe path
  component, and every local path built from a remote name is re-checked to stay
  within the pair's root, so a crafted or shared name (`..`, absolute, or with an
  embedded separator) cannot write outside the sync folder.
- Argument injection: every `proton-drive` invocation passes `--` before its
  positional paths, so a file or folder named like a flag (`-c`, `--help`) is not
  interpreted as one.
- Log privacy: the sync log (which records file paths) is created in a `0700`
  directory as a `0600` file, and the per-run metadata cache dir is created
  `0700`, so neither is world-readable on a multi-user host.
- `credentials_store` is validated against its allowlist (`keychain`, `pass`,
  `unsafe_file`) before being exported to the CLI.

### Changed
- `init` writes an empty config (no sample "documents" pair); add your own
  `[[pair]]` entries.
- The config-path override environment variable is `NEUTRONSYNC_CONFIG`.
- SQLite baseline: `busy_timeout` is set so a concurrent writer waits briefly
  instead of failing immediately.

### Notes
- Requires the official `proton-drive` CLI on `PATH` (not bundled).
- `gio` (glib) is used for recoverable local deletes; an XDG-trash fallback is
  used if it is missing.
