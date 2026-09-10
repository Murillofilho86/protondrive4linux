# Security Policy

protondrive4linux is an unofficial, community-maintained client — not affiliated with or
endorsed by Proton AG. All account authentication, encryption, and storage are handled entirely
by the official `proton-drive` CLI; this project only orchestrates it.

## Scope

In scope: the sync engine, the CLI, the GUI, the watcher, packaging (`PKGBUILD`, `.deb`, `.rpm`),
and the release/update pipeline in this repository.

Out of scope: the `proton-drive` CLI itself, and Proton's servers/infrastructure. Report those
to Proton directly at [security.protonmail.com](https://security.protonmail.com) or via their
own bug bounty program — a report filed here about the official CLI's internals will just be
redirected.

## Supported versions

Only the latest release is supported. This project is pre-1.0 and does not maintain security
backports to older versions — please upgrade before reporting an issue that may already be
fixed.

## Reporting a vulnerability

Please **do not** open a public issue for a suspected vulnerability.

Use GitHub's [private vulnerability reporting](https://github.com/Murillofilho86/protondrive4linux/security/advisories/new)
for this repository (Security tab → Report a vulnerability). This reaches the maintainer
directly and privately.

Include, where relevant:
- What you found and why it's a security issue (not just a bug).
- Steps to reproduce, or a minimal example.
- The version/commit you tested against.
- Whether you believe user data (synced files, credentials, local baseline state) is at risk.

## What to expect

This is a solo/community-maintained project without a dedicated security team or SLA. As a
best-effort target: an acknowledgment within a week, and a fix or mitigation plan communicated
before any public disclosure. Please give a reasonable amount of time to address the issue
before disclosing it publicly.

## Verifying a release

Every release artifact (`.deb`, `.rpm`, source and binary tarballs) is signed with a GPG signing
subkey dedicated to this project — separate from any personal key, so it can be rotated or
revoked without affecting anything else. See `docs/contributing/RELEASE_SIGNING.md` for the full
key-management model.

Master key fingerprint: `58D0231CA8A38EC4263500937F87E84394A6E101`
Signing subkey fingerprint: `72ACDDB4ED23A2F7CE7B768F7D57E42A1FDAB300` (expires 2027-09-10 —
renewed periodically per the runbook; verify against the current `docs/keys/*.asc` if this looks
stale)

```sh
gpg --import docs/keys/protondrive4linux-release.asc
gpg --verify protondrive4linux_X.Y.Z_amd64.deb.asc protondrive4linux_X.Y.Z_amd64.deb
```

A `Good signature from "protondrive4linux release signing ..."` confirms the artifact matches
what this project's CI actually built and published — not just that the download didn't get
corrupted in transit (which is all the GitHub-published SHA-256 checksum alone proves).

The key and CI wiring exist as of this commit; signing only actually runs once
`RELEASE_SIGNING_READY` is turned on (see `docs/contributing/RELEASE_SIGNING.md`). Anything
released before the first tag with a `.asc` attached is checksum-only.

## Known, already-documented risk areas

`SECURITY_AUDIT.md` records the due-diligence audit of the code this project forked from,
including what was found and fixed. `docs/roadmap/M2-supply-chain.md` tracks what's still open
(notably: releases aren't cryptographically signed yet, only checksummed — the in-app updater is
disabled until that's addressed). If your report overlaps with something already tracked there,
it's still worth reporting — confirmation that a known gap is actually exploitable is useful.
