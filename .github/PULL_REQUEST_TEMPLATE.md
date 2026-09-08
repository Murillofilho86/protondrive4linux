**What this changes and why**


**Checklist**
- [ ] `cargo test` passes locally
- [ ] `cargo build --features gui --bin protondrive4linux-gui` builds
- [ ] `cargo fmt --all -- --check` and `cargo clippy --all-targets --all-features -- -D warnings` are clean
- [ ] Updated `CHANGELOG.md` under `[Unreleased]` if this is a user-facing change
- [ ] Updated `docs/SYNC_MODEL.md` if this touches `engine.rs` or `watcher.rs`'s sync/safety behavior

`master` requires the `build-test` and `audit` checks to pass before merging - see
[`CONTRIBUTING.md`](../CONTRIBUTING.md#workflow).
