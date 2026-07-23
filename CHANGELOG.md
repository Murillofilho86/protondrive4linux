# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Bidirectional folder sync on the official `proton-drive` CLI, with a
  three-way-merge engine (per-pair baseline snapshot).
- CLI (`neutronsync`): `init`, `login`, `logout`, `status`, `doctor`, `sync`
  (`--dry-run`/`--resync`/`--json`), and `watch`.
- Native GUI (`neutronsync-gui`, `gui` feature): egui/eframe app with a Folders /
  Activity / Settings / Account / About layout and a remote-folder browser.
- Concurrent remote tree scan (`scan_threads`) — the CLI has no recursive list.
- Local hash cache: in `sha1` mode only files whose size/mtime changed are
  re-hashed.
- Watch daemon: inotify events (debounced) as hints + a periodic full rescan as
  the safety net, single-instance locked.
- Rich-GUI backend (`service::Controller`): observable `AppState` + structured
  `SyncEvent` stream + cancellation.
- Recoverable deletes both ways: remote to Proton trash, local to the desktop
  trash (via `gio`).
- Packaging: `.deb` and `.rpm` via the release workflow; systemd user units.

### Fixed
- Data-safety: a failed upload/download no longer records post-transfer state in
  the baseline. Only files that fully synced this run are written; a failed
  transfer leaves that file's baseline row untouched so the next run retries it
  in the correct direction — a timeout on one file can no longer cause the stale
  side to overwrite a newer edit, and never invalidates the rest of the sync.
- Data-safety: a transient local stat failure (a file locked or renamed
  mid-scan) no longer makes a known file look deleted; its baseline entry is
  carried forward and the file is retried.
- Data-safety: a remote folder that reports "not found" while being walked is
  treated as a partial scan (deletions suppressed), not as an empty folder, so a
  transient listing failure can't trigger a mass local delete.
- Data-safety: a path that is a file on one side and a directory on the other
  is now surfaced and skipped instead of being silently left diverged (which
  also used to poison the baseline with the wrong type).
- Data-safety: a missing local root (e.g. an unmounted drive) is refused rather
  than mistaken for a mass deletion; the watch daemon no longer recreates a
  previously-synced root that has gone missing. A vanished remote base folder
  likewise suppresses deletions for that run.

### Security
- Path traversal: remote node names are validated to a single safe path
  component before use, and every local path built from a remote name is
  re-checked to stay within the pair's root, so a crafted or shared name (`..`,
  absolute, embedded separator) can no longer write outside the sync folder.
- Argument injection: every `proton-drive` invocation passes `--` before its
  positional paths, so a file or folder named like a flag (`-c`, `--help`) can
  no longer be interpreted as one.
- Log privacy: the sync log (which records file paths) is created in a `0700`
  directory as a `0600` file, and the per-run metadata cache dir is created
  `0700`, so neither is world-readable on a multi-user host.
- `credentials_store` is validated against its allowlist (`keychain`, `pass`,
  `unsafe_file`) before being exported to the CLI.

### Changed
- The config-path override environment variable is `NEUTRONSYNC_CONFIG` (was
  `PROTONSYNC_CONFIG`).
- SQLite baseline: `busy_timeout` is set so a concurrent writer waits briefly
  instead of failing immediately; rows left `pending` by an earlier build are
  cleared on open (they re-sync safely).

### Notes
- Requires the official `proton-drive` CLI on `PATH` (not bundled).
- `gio` (glib) is used for recoverable local deletes; a manual XDG-trash
  fallback is used if it is missing.
