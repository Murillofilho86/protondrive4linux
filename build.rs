//! Embeds the commit the binary was built from into `PROTONDRIVE_BUILD_SHA`
//! (read via `env!` in `src/main.rs`), so `--version` shows exactly which
//! commit produced the binary, independent of the changelog or the tag.
//!
//! `.deb`/`.rpm`/binary-tarball CI builds run from a real `actions/checkout`,
//! so `.git` is present and this picks up the real commit automatically with
//! no workflow changes needed. The *source* tarball is built via `git
//! archive`, which deliberately excludes `.git` - building from it falls
//! back to "unknown" rather than failing.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=PROTONDRIVE_BUILD_SHA");

    let sha = std::env::var("PROTONDRIVE_BUILD_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(git_sha)
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=PROTONDRIVE_BUILD_SHA={sha}");
}

/// Short commit hash, with a `-dirty` suffix if the working tree has
/// uncommitted changes (so local dev builds are visibly distinguishable from
/// a clean release build of the same commit).
fn git_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        sha.push_str("-dirty");
    }
    Some(sha)
}
