# Contributing

## Layout

A single Cargo crate: a **library** (the reusable core) plus two binaries.

- `src/lib.rs` — core: `config`, `models`, `protoncli` (adapter over the
  `proton-drive` CLI), `engine` (three-way merge), `state`, `trash`, `datefmt`,
  `logger`, `events`, `service` (`Controller` for frontends), `watcher`.
- `src/main.rs` — the `neutronsync` CLI.
- `src/bin/gui.rs` — the `neutronsync-gui` GUI (behind the `gui` feature).

The GUI is intentionally thin over `service::Controller` (see `docs/GUI_API.md`);
the core has no UI or CLI assumptions.

## Build & test

```sh
cargo build                       # lib + CLI
cargo test                        # engine + unit tests
cargo build --features gui --bin neutronsync-gui
cargo run   --features gui --bin neutronsync-gui
```

GUI build needs system libs (Debian/Ubuntu):

```sh
sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcb1-dev libgl1-mesa-dev
```

## Releasing

Releases are built by `.github/workflows/release.yml` on a `v*` tag:

```sh
git tag v0.1.0
git push origin v0.1.0     # builds .deb + .rpm + source tarball -> draft Release
```

Packaging metadata lives in `Cargo.toml` (`[package.metadata.deb]` and
`[package.metadata.generate-rpm]`). Both packages ship the CLI, the GUI, the
`.desktop` entry, and the systemd user units. Packaging requires the `gui`
feature (the GUI binary is one of the shipped assets):

```sh
cargo deb --features gui
cargo generate-rpm            # after a `cargo build --release --features gui`
```

## Conventions

- No third-party runtime deps in the core beyond what's in `Cargo.toml`; keep
  the CLI light (GUI-only deps go behind the `gui` feature).
- Prefer keeping all `proton-drive` CLI specifics inside `protoncli.rs`.
- `cargo test` must stay green; the GUI and CLI should build warning-free.
