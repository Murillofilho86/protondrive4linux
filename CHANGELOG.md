# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
[Semantic Versioning](https://semver.org/). New work lands under
`[Unreleased]` and moves to a dated version when it ships.

## [Unreleased]

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
