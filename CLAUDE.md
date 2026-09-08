# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

protondrive4linux: bidirectional folder sync for Proton Drive on Linux, built on top of the official `proton-drive` CLI (which has no sync engine of its own — this crate adds a three-way-merge engine driven entirely by that CLI's one-shot commands). Single Cargo crate: a core library, a CLI binary, and an optional GUI binary.

Read `README.md` for user-facing behavior and config, `docs/SYNC_MODEL.md` for the sync design/invariants (required reading before touching `engine.rs` or `watcher.rs`), `docs/GUI_API.md` for the `service::Controller` API the GUI is built on, `docs/architecture/ARCHITECTURE_PRINCIPLES.md` for the module boundaries and non-negotiable design decisions, and `docs/roadmap/ROADMAP.md` for what's planned next and why (per-milestone detail in `docs/roadmap/M*.md`).

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
- `service` — `Controller`: the backend API frontends (CLI and GUI) are built on. Non-blocking commands (`sync`, `cancel`, `start_watch`, `stop_watch`), cheap `snapshot()` for rendering, engine events surfaced as observable `AppState`. See `docs/GUI_API.md`.
- `updater` (gui feature only) — checks GitHub releases by channel (stable/prerelease), verifies checksum, installs via the system package manager.

### Binaries

- `src/main.rs` — `protondrive4linux` CLI (thin wrapper over the library: `login`, `init`, `sync`, `status`, `doctor`, `logout`, `watch`).
- `src/bin/gui.rs` — `protondrive4linux-gui` (egui, behind `gui` feature). Intentionally thin over `service::Controller`; has no sync logic of its own.

### Key design constraints (see `docs/SYNC_MODEL.md` for full detail)

- The `proton-drive` CLI has **no change feed** and its directory-metadata cache **serves stale listings** — every listing runs with a throwaway `PROTON_DRIVE_CACHE_DIR`, one per concurrent scan worker. This is why the engine leans on a baseline instead of querying "what changed."
- Local changes are cheap (inotify); remote changes are only discoverable by re-walking. The watcher exploits this asymmetry: shallow, folder-scoped reconciliation on local FS events, then hot-folder passes, then a full walk paced to `duration * 6` (`FULL_WALK_MULTIPLIER`), floored at `poll_interval`.
- Data-safety invariants that must never regress: positive-confirmation baseline commit (a file's baseline row advances only after its transfer actually completes — protects against a killed/cancelled run misreading a not-yet-downloaded file as a deletion), deletes are opt-in and recoverable, incomplete listings never delete, missing local root or vanished remote base is refused (not treated as mass-delete), a missing baseline unions both sides rather than mirroring, excludes are invisible to the scan, and conflicts always keep both copies.
- The GUI runs as **two processes** in tray mode: the tray daemon owns the watcher/engine and syncs; the window process reads a `status.json` snapshot the daemon publishes a few times a second (it has no direct access to the daemon's in-memory state). A CLI `sync` also writes into the same activity store the window reads.

### Tests

`tests/engine.rs` is the executable spec for sync semantics — it uses a `FakeRemote`/in-memory store to exercise the engine (initial upload/download, modify/delete propagation, conflict handling, idempotency, structured events) without needing the real `proton-drive` CLI. When changing merge/conflict/deletion logic, check this file first for the invariant being tested before changing behavior.
