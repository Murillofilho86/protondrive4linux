# neutronsync

Bidirectional folder sync for Proton Drive on Linux, built on Proton's official
`proton-drive` CLI. Written in Rust: a small dependency-light CLI plus an
optional native GUI, both on top of one core library.

> Not affiliated with, endorsed by, or sponsored by Proton AG. "Proton" and
> "Proton Drive" are trademarks of Proton AG; this is an independent, unofficial
> tool that drives Proton's own `proton-drive` CLI.

The official CLI can upload, download, list, and manage sharing, but it has no
sync engine of its own. `neutronsync` adds that: a three-way-merge engine that
keeps one or more local folders and their Proton Drive counterparts in sync in
both directions, driven entirely by the CLI's one-shot commands.

> Branches: this `rust` branch is the Rust implementation. The original Python
> implementation lives on the `python` branch.

## How it works

For each folder pair, `neutronsync` keeps a baseline snapshot of the last state
the two sides agreed on. On every run it scans the current local and remote
trees and classifies each path against the baseline (created, modified, or
deleted) independently on each side. Combining the two tells it exactly what to
do and which side wins, so it can tell "new file on the remote" apart from "file
deleted locally", which a plain mirror cannot.

Safety model:

- **Deletions propagate recoverably.** With `propagate_deletes = true`, deleting
  a file on one side removes it on the other, but the remote copy goes to
  Proton's online trash and the local copy goes to your desktop trash (via `gio`;
  set `local_delete = "remove"` to unlink permanently). `propagate_deletes =
  false` leaves the other side untouched.
- **A missing baseline never deletes.** If the sync state is absent (first run,
  or state dir wiped) the two sides are unioned, never mirrored, so a lost
  baseline cannot trigger a mass delete.
- **Conflicts never lose data.** If a file changed on both sides since the last
  sync, the default `keep-both` policy keeps both versions (the local copy is
  renamed `name (conflict <timestamp>).ext`).

## Requirements

- A recent Rust toolchain (edition 2021).
- The official `proton-drive` CLI on your `PATH`
  (<https://proton.me/blog/proton-drive-cli>). Verified against `cli-drive 0.6.0`.
- `gio` (from glib, present on most desktops) for recoverable local deletes; a
  manual XDG-trash fallback is used if it is missing.

## Build

```sh
# CLI
cargo build --release                 # -> target/release/neutronsync
cargo install --path .                 # -> ~/.cargo/bin/neutronsync
cargo test                             # engine + unit tests

# GUI (opt-in feature, keeps the CLI dependency-light)
cargo run --features gui --bin neutronsync-gui
cargo build --release --features gui --bin neutronsync-gui
```

The GUI build needs a few system libraries (Debian/Ubuntu):

```sh
sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcb1-dev libgl1-mesa-dev
```

## Setup (CLI)

```sh
neutronsync login                       # proton-drive auth login (browser)
neutronsync init                        # write ~/.config/neutronsync/neutronsync.toml
$EDITOR ~/.config/neutronsync/neutronsync.toml
neutronsync sync --dry-run
neutronsync sync
```

## Commands

```
neutronsync init [--force]
neutronsync login | logout
neutronsync status
neutronsync doctor [REMOTE_PATH]        # probe the CLI + show list parsing
neutronsync sync [PAIR...] [--dry-run] [--resync] [--json]
neutronsync watch [PAIR...]             # live sync: FS events + periodic rescan
```

- `--dry-run` prints the plan and changes nothing.
- `--resync` rebuilds the baseline from the union of both sides.
- Passing pair names syncs only those; no names syncs all.

## Watch mode (live sync)

`neutronsync watch` runs a foreground daemon that reconciles on local change,
following the pattern real sync clients use: **filesystem events are hints, a
periodic full rescan is the safety net.**

- Local edits are caught immediately via inotify (debounced by `debounce`
  seconds so a burst of writes collapses into one reconcile).
- A full rescan runs every `poll_interval` seconds — it catches anything the
  events missed and is the only way remote-side changes are noticed (the CLI has
  no remote event feed).
- A single-instance lock (a PID file under the state dir) stops a `watch` daemon
  and a scheduled `sync` from running the same pairs at once.

The GUI exposes the same thing as a **Watch** toggle. Very large trees can hit
the kernel's `fs.inotify.max_user_watches` limit; raise it if you watch huge
folders. Remote changes still appear only at the next rescan interval.

`neutronsync doctor` runs a real `filesystem list -j` and prints both the raw
JSON and how `neutronsync` parsed it. The parser is verified against `cli-drive
0.6.0`; if a future CLI version renames fields, `doctor` shows what changed and
the fix lives in `src/protoncli.rs` alone.

## Configuration

TOML at `~/.config/neutronsync/neutronsync.toml` (override with `-c PATH` or
`$NEUTRONSYNC_CONFIG`). See `neutronsync.example.toml`.

```toml
[cli]
binary = "proton-drive"
upload_flags = ["--conflict-strategy", "replace"]
download_flags = ["--conflict-strategy", "replace"]
fresh_cache = true            # throwaway metadata cache per run (avoids stale listings)

[options]
remote_root = "/my-files"     # Proton's per-user root
propagate_deletes = true      # deletes cross over (recoverably); false = never delete
local_delete = "trash"        # trash | remove
conflict = "keep-both"        # keep-both | newer | skip
compare = "size+mtime"        # size | size+mtime | sha1

[[pair]]
name = "documents"
local = "~/Documents"
remote = "Documents"          # -> /my-files/Documents
```

`fresh_cache` matters: the CLI caches directory metadata and serves it stale, so
without a fresh cache per run it would miss changes made on another device.

## GUI

A native egui/eframe app (pure Rust, single binary) that mirrors the Proton
Drive desktop client: a dark navigation rail with a sync-status line, and a
detail pane. Pages: Folders (add/remove pairs with a native picker), Activity
(live sync log), Settings, Account, About. It reads and writes the same config
as the CLI. The backend API it sits on is documented in `docs/GUI_API.md`.

## Running it in the background (systemd --user)

Two options; use one, not both (they'd contend for the same pairs — the lock
prevents damage but it's wasteful):

**Live sync (recommended)** — the watch daemon:

```sh
mkdir -p ~/.config/systemd/user
cp systemd/neutronsync-watch.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now neutronsync-watch.service
```

**Periodic sync** — a timer instead:

```sh
cp systemd/neutronsync.service systemd/neutronsync.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now neutronsync.timer
```

The shipped units target `/usr/bin/neutronsync` (where the `.deb`/`.rpm` install
it). If you built with `cargo install` instead, edit `ExecStart` to
`%h/.cargo/bin/neutronsync`. Proton Drive is rate-limited; keep the sync set
modest and the rescan interval sane.

## Known limitations

- Change detection defaults to `size+mtime` (Proton preserves the original
  mtime). `compare = "sha1"` compares content exactly, at the cost of hashing
  every local file each run.
- Renames look like a delete + create.

## License

See `LICENSE`.
