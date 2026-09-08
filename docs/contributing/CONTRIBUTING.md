# Contributing

## Layout

A single Cargo crate: a **library** (the reusable core) plus two binaries.

- `src/lib.rs` — core: `config`, `models`, `protoncli` (adapter over the
  `proton-drive` CLI), `engine` (three-way merge), `state`, `trash`, `datefmt`,
  `logger`, `events`, `service` (`Controller` for frontends), `watcher`.
- `src/main.rs` — the `protondrive4linux` CLI.
- `src/bin/gui.rs` — the `protondrive4linux-gui` GUI (behind the `gui` feature).

The GUI is intentionally thin over `service::Controller` (see `../GUI_API.md`);
the core has no UI or CLI assumptions.

## Build & test

```sh
cargo build                       # lib + CLI
cargo test                        # engine + unit tests
cargo build --features gui --bin protondrive4linux-gui
cargo run   --features gui --bin protondrive4linux-gui
```

GUI build needs system libs (Debian/Ubuntu):

```sh
sudo apt-get install -y libgtk-3-dev libxkbcommon-dev libwayland-dev \
  libx11-dev libxcb1-dev libgl1-mesa-dev
```

## Workflow

`master` is protected: no direct pushes (not even for admins), merges only
via pull request, and the `build-test` and `audit` jobs from
`.github/workflows/ci.yml` must pass first. In practice:

```sh
git checkout -b my-change
# ... commit ...
git push -u origin my-change
gh pr create --fill
# once CI is green: gh pr merge --squash (or merge in the GitHub UI)
```

This applies to every change, including ones made by Claude Code sessions
working in this repo - branch + PR, not a direct commit to `master`.

## Releasing

Releases are built by `.github/workflows/release.yml` on a `v*` tag:

```sh
git tag v0.1.0
git push origin v0.1.0     # builds .deb + .rpm + source tarball -> a pre-release,
                           # then publishes PKGBUILD to the AUR (protondrive4linux-git)
```

The AUR publish step re-runs `cargo test` first (belt-and-suspenders on top
of `master`'s branch protection) and is gated on the repo variable
`AUR_ACCOUNT_READY` being `"true"` - it no-ops (skips, doesn't fail) until
then, since it also needs three repo *secrets* set: `AUR_USERNAME`,
`AUR_EMAIL`, `AUR_SSH_PRIVATE_KEY` (an SSH key registered on the AUR account
that owns the package). As of 2026-09-08 the AUR isn't accepting new account
registrations (an orphaned-packages incident), so this is blocked until that
lifts; once the account exists and the three secrets are set, flip
`AUR_ACCOUNT_READY` to `"true"` (Settings → Secrets and variables → Actions
→ Variables) to turn the job on.

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
