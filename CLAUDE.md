# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

protondrive4linux: bidirectional folder sync for Proton Drive on Linux, built on top of the official `proton-drive` CLI (which has no sync engine of its own — this crate adds a three-way-merge engine driven entirely by that CLI's one-shot commands). Single Cargo crate: a core library, a CLI binary, and an optional GUI binary.

Read `README.md` for user-facing behavior and config, `docs/SYNC_MODEL.md` for the sync design/invariants (required reading before touching `engine.rs` or `watcher.rs`), `docs/architecture/SYNC_STATE_MATRIX.md` for the exact LOCAL×REMOTE decision table `decide`/`decide_dir` implement, `docs/GUI_API.md` for the `service::Controller` API the GUI is built on, `docs/architecture/ARCHITECTURE_PRINCIPLES.md` for the module boundaries and non-negotiable design decisions, and `docs/roadmap/ROADMAP.md` for what's planned next and why (per-milestone detail in `docs/roadmap/M*.md`).

## Build & test

```sh
cargo build                                    # lib + CLI
cargo test                                     # engine + unit tests (the real spec for sync behavior)
cargo test <test_name>                         # run a single test, e.g. cargo test conflict_keep_both
cargo build --features gui --bin protondrive4linux-gui
cargo run   --features gui --bin protondrive4linux-gui
```

GUI build needs system libs (Debian/Ubuntu): `libgtk-3-dev libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libgl1-mesa-dev libxdo-dev libayatana-appindicator3-dev`.

`cargo test` must stay green; both binaries should build warning-free. No third-party runtime deps should be added to the core beyond what's already in `Cargo.toml`; keep the CLI dependency-light — anything GUI-only goes behind the `gui` feature (see `[features] gui = [...]` in `Cargo.toml`).

## Releasing

Not something to do casually — see `docs/contributing/CONTRIBUTING.md` for full detail. In short: `scripts/release.sh <version>` bumps version, closes the `[Unreleased]` CHANGELOG section, commits, and cuts a signed tag but stops before pushing; pushing a `v*` tag triggers `.github/workflows/release.yml` to build `.deb`/`.rpm`/tarball as a draft/pre-release. `scripts/promote.sh <version>` promotes a pre-release to stable (retitles it `(stable)` and gives it the "Latest" badge) or `--demote`s it back.

## Architecture

### Module layout (`src/lib.rs`)

- `config` — TOML config load/parse (`~/.config/protondrive4linux/protondrive4linux.toml`), `[[pair]]` definitions.
- `models` — shared data types (pair, file entry, etc.).
- `protoncli` — the **only** place that knows `proton-drive` CLI specifics (flags, invocation, output parsing). Keep all CLI-adapter logic contained here.
- `engine` — the three-way-merge sync engine: classifies each path per side against the baseline (Created/Modified/Deleted/Unchanged/Absent) and decides an action (`decide` / `decide_dir`). Also owns the streaming full-walk implementation (`run_sync_streaming`) and its tested sequential oracle (`Engine::sync_pair_streaming`).
- `state` — baseline persistence (SQLite, `stats.db`, table `baseline`, keyed by `(pair, rel)`). Baseline commits are additive/scoped — never a full-pair rewrite.
- `watcher` — inotify-based live sync (`notify-debouncer-full`): shallow, folder-scoped reconciliation on local events, hot-folder passes, and the adaptively-paced full walk.
- `trash` — recoverable local deletes (`gio`/freedesktop trash, with a manual XDG-trash fallback).
- `ignore` — exclude-path handling; excludes are pruned from scans entirely, never listed/downloaded/deleted.
- `events` — structured sync events emitted by the engine.
- `stats`, `datefmt`, `logger`, `auth_signal` — supporting utilities.
- `process_lock` — single-instance locks for the watcher, the tray daemon and the window: held with a kernel file lock (gone the moment the process ends) and described by pid + start time + boot id, so a reused pid or a lock left by a killed process is never taken for a live one. Anything that only wants to look (`holder`/`inspect`) never takes the lock.
- `tray_policy` — what keeps background sync alive when the tray dies (restart backoff and cap, the supervisor loop written against a `Supervised` trait) and what the window says about it (liveness banners, staleness). Pure: no process is spawned and no file is read; the real `--tray` supervisor is the `supervisor` module in `src/bin/gui.rs`.
- `doctor` — read-only installation health check behind `protondrive4linux doctor`: returns a `Report` (it never prints); everything it looks at outside the process goes through `Env`/`Host` so tests need no Proton account, systemd or network. It asks the CLI whether a session exists and never reads credentials itself.
- `service` — `Controller`: the backend API frontends (CLI and GUI) are built on. Non-blocking commands (`sync`, `cancel`, `start_watch`, `stop_watch`), cheap `snapshot()` for rendering, engine events surfaced as observable `AppState`. See `docs/GUI_API.md`.
- `updater` (gui feature only) — checks GitHub releases by channel (stable/prerelease), verifies checksum, installs via the system package manager.

### Binaries

- `src/main.rs` — `protondrive4linux` CLI (thin wrapper over the library: `login`, `init`, `sync`, `status`, `doctor`, `logout`, `watch`).
- `src/bin/gui.rs` — `protondrive4linux-gui` (egui, behind `gui` feature). Intentionally thin over `service::Controller`; has no sync logic of its own.
- `src/bin/gui/shots.rs` — `--screenshots <dir>`: draws each page from made-up example scenes and writes PNGs (`scripts/screenshots.sh`; the pictures in `docs/screenshots/` come from it). Self-contained: it must never read the user's config, state, themes or desktop entries, never call the CLI, and never print a real path (the Settings page prints the themes folder, hence the made-up `XDG_CONFIG_HOME`). When you add a page, add a scene; when a change alters how a page looks, regenerate and commit the pictures.

### Key design constraints (see `docs/SYNC_MODEL.md` for full detail)

- The `proton-drive` CLI has **no change feed** and its directory-metadata cache **serves stale listings** — every listing runs with a throwaway `PROTON_DRIVE_CACHE_DIR`, one per concurrent scan worker. This is why the engine leans on a baseline instead of querying "what changed."
- Local changes are cheap (inotify); remote changes are only discoverable by re-walking. The watcher exploits this asymmetry: shallow, folder-scoped reconciliation on local FS events, then hot-folder passes, then a full walk paced to `duration * 6` (`FULL_WALK_MULTIPLIER`), floored at `poll_interval`.
- Data-safety invariants that must never regress: positive-confirmation baseline commit (a file's baseline row advances only after its transfer actually completes — protects against a killed/cancelled run misreading a not-yet-downloaded file as a deletion), deletes are opt-in and recoverable, incomplete listings never delete, missing local root or vanished remote base is refused (not treated as mass-delete), a missing baseline unions both sides rather than mirroring, excludes are invisible to the scan, and conflicts always keep both copies.
- The GUI runs as up to **three processes** in tray mode: `--tray` is a thin supervisor that holds the tray lock and restarts the real tray (`--tray-child`) after a crash (policy in `tray_policy`); the tray owns the watcher/engine and syncs; the window process reads a `status.json` snapshot the tray publishes a few times a second (it has no direct access to its in-memory state) and checks the lock and the snapshot's age to say when background sync stopped or isn't responding. A CLI `sync` also writes into the same activity store the window reads.
- A keyring the CLI can't reach is a wait, not a sign-out. `protoncli::session_problem` tells the two apart from the CLI's message (the only place that knows its wording); the watcher's session gate pauses sync for either, but only a sign-out asks the user to sign in - for the keyring the UI says "waiting for your keyring", offers no sign-in (it would not help, and the session is usually intact) and resumes by itself. Don't fold it back into `signed_out`.
- `--tray-child` must exit 0 only for a Quit (or "no config, nothing to run") and 1 for anything else that ends it - the supervisor reads 0 as "the user quit" and stops, so a failure that exits 0 leaves background sync down.

### Tests

`tests/engine.rs` is the executable spec for sync semantics — it uses a `FakeRemote`/in-memory store to exercise the engine (initial upload/download, modify/delete propagation, conflict handling, idempotency, structured events) without needing the real `proton-drive` CLI. When changing merge/conflict/deletion logic, check this file first for the invariant being tested before changing behavior.
