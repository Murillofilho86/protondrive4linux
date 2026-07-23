//! In-app update check (notify-only) against GitHub Releases.
//!
//! Behind the `gui` feature. Reads the configured channel's latest release,
//! compares its version to the running build, and reports whether a newer one
//! is available and where to get it. It never downloads or installs anything -
//! the user updates via their package manager or the release page.

use anyhow::{anyhow, Context, Result};

use crate::config::UpdateChannel;

/// owner/repo the releases are published under.
const REPO: &str = "WilhelmZA/protondrive_linux_sync";

/// The running build's version (from Cargo).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Outcome of an update check.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    /// True when `latest` is newer than the running version.
    pub newer: bool,
    /// The release's GitHub page (for an "Open release" action).
    pub html_url: String,
    /// Release notes / body (may be empty).
    pub notes: String,
    /// Whether the found release is flagged pre-release.
    pub prerelease: bool,
}

/// Check the given channel for the latest release.
///
/// Uses `GITHUB_TOKEN`/`GH_TOKEN` from the environment when set (needed while
/// the repo is private); works tokenless once the repo is public.
pub fn check(channel: UpdateChannel) -> Result<UpdateInfo> {
    let release = match channel {
        // The stable channel follows only promoted releases; `/releases/latest`
        // returns the newest non-draft, non-prerelease one.
        UpdateChannel::Stable => {
            let body = fetch(&format!(
                "https://api.github.com/repos/{REPO}/releases/latest"
            ))?;
            serde_json::from_str::<serde_json::Value>(&body).context("parsing release JSON")?
        }
        // The pre-release channel follows the newest published release of any
        // kind: take the first non-draft entry (the API lists newest first).
        UpdateChannel::Prerelease => {
            let body = fetch(&format!(
                "https://api.github.com/repos/{REPO}/releases?per_page=20"
            ))?;
            let arr: Vec<serde_json::Value> =
                serde_json::from_str(&body).context("parsing releases JSON")?;
            arr.into_iter()
                .find(|r| !r["draft"].as_bool().unwrap_or(false))
                .ok_or_else(|| anyhow!("no published releases yet"))?
        }
    };

    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("release has no tag_name"))?
        .to_string();
    let html_url = release["html_url"].as_str().unwrap_or("").to_string();
    let notes = release["body"].as_str().unwrap_or("").to_string();
    let prerelease = release["prerelease"].as_bool().unwrap_or(false);

    let current = current_version().to_string();
    let latest = tag.trim_start_matches('v').to_string();
    let newer = is_newer(&latest, &current);

    Ok(UpdateInfo {
        current,
        latest,
        newer,
        html_url,
        notes,
        prerelease,
    })
}

fn fetch(url: &str) -> Result<String> {
    let mut req = ureq::get(url)
        .set("User-Agent", "neutronsync-updater")
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tok) = token() {
        req = req.set("Authorization", &format!("Bearer {tok}"));
    }
    match req.call() {
        Ok(resp) => resp.into_string().context("reading the GitHub response"),
        Err(ureq::Error::Status(404, _)) => Err(anyhow!(
            "no matching release found yet (or the repo is private and GITHUB_TOKEN isn't set)"
        )),
        Err(ureq::Error::Status(code, _)) => Err(anyhow!("GitHub returned HTTP {code}")),
        Err(e) => Err(anyhow!("update check failed: {e}")),
    }
}

fn token() -> Option<String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var) {
            if !t.trim().is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// Whether `latest` is a newer version than `current`. Prefers a semver
/// comparison; falls back to "any difference counts as newer" if either string
/// doesn't parse as semver.
fn is_newer(latest: &str, current: &str) -> bool {
    match (
        semver::Version::parse(latest),
        semver::Version::parse(current),
    ) {
        (Ok(l), Ok(c)) => l > c,
        _ => !latest.is_empty() && latest != current,
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn semver_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // A final release outranks its own pre-releases, and vice versa.
        assert!(is_newer("0.2.0", "0.2.0-beta.1"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
    }
}
