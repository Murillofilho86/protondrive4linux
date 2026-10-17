//! Structured events emitted by the engine, for rich frontends.
//!
//! The CLI is happy with text logs, but a GUI wants typed progress: which pair
//! is scanning, how many folders seen, the plan totals, per-operation
//! start/finish (to drive a progress bar), and per-pair completion. The engine
//! emits these to an optional [`EventSink`]; `service::Controller` turns them
//! into observable [`crate::service::AppState`].

/// How a both-sides-changed conflict was settled (or, for `Skipped`, not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Both versions were kept: the local one was renamed to a
    /// "(conflict ...)" copy and the remote one now holds the original name.
    KeptBoth,
    /// The local version won (`conflict = "newer"`, local was newer); the
    /// remote copy was replaced.
    KeptLocal,
    /// The remote version won (`conflict = "newer"`, remote was newer); the
    /// local copy was replaced.
    KeptRemote,
    /// Left untouched on both sides (`conflict = "skip"`): it needs a manual
    /// decision, and is re-detected on every run until the two sides agree.
    Skipped,
}

impl ConflictResolution {
    /// Stable identifier, as stored in the conflicts table.
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictResolution::KeptBoth => "kept-both",
            ConflictResolution::KeptLocal => "kept-local",
            ConflictResolution::KeptRemote => "kept-remote",
            ConflictResolution::Skipped => "skipped",
        }
    }

    /// Inverse of [`ConflictResolution::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "kept-both" => ConflictResolution::KeptBoth,
            "kept-local" => ConflictResolution::KeptLocal,
            "kept-remote" => ConflictResolution::KeptRemote,
            "skipped" => ConflictResolution::Skipped,
            _ => return None,
        })
    }

    /// A short phrase for logs: what happened to the two versions.
    pub fn describe(self) -> &'static str {
        match self {
            ConflictResolution::KeptBoth => "kept both versions",
            ConflictResolution::KeptLocal => "kept the local version (it was newer)",
            ConflictResolution::KeptRemote => "kept the remote version (it was newer)",
            ConflictResolution::Skipped => "left untouched (conflict = skip)",
        }
    }
}

/// A single structured event about sync progress.
#[derive(Clone, Debug)]
pub enum SyncEvent {
    /// A pair's reconcile is starting.
    PairStarted { pair: String },
    /// The remote tree scan has begun for a pair.
    ScanStarted { pair: String },
    /// Progress while walking the remote tree (folders listed so far).
    ScanProgress {
        pair: String,
        folders: usize,
        current: String,
    },
    /// The plan has been computed: total actionable ops + a per-action breakdown.
    Planned {
        pair: String,
        total_ops: usize,
        counts: Vec<(String, usize)>,
    },
    /// An operation is about to run.
    OpStarted {
        pair: String,
        action: String,
        path: String,
    },
    /// An operation finished (ok=false means it errored). `error` carries the
    /// reason when it failed, so a frontend and the ops history can show WHY
    /// rather than a bare "an operation failed".
    OpFinished {
        pair: String,
        action: String,
        path: String,
        ok: bool,
        error: Option<String>,
    },
    /// A pair finished reconciling.
    PairFinished {
        pair: String,
        applied: usize,
        errors: usize,
        tracked: usize,
    },
    /// A file changed on both sides since the last sync and was settled per
    /// the conflict policy (or, for `Skipped`, left for the user). Emitted
    /// once the resolution has actually completed - never for a transfer that
    /// failed, which is simply re-detected next run. `copy` is the preserved
    /// local copy's path for `KeptBoth`.
    Conflict {
        pair: String,
        path: String,
        resolution: ConflictResolution,
        copy: Option<String>,
    },
    /// Free-form informational line (mirrors a log line).
    Info { text: String },
    /// An error, optionally attributed to a pair.
    Error { pair: Option<String>, text: String },
    /// The proton-drive session's sign-in state changed. Emitted by the watch
    /// daemon when it detects the session has expired (signed out) or come back,
    /// so a logged-out state surfaces as one clear banner instead of a pile of
    /// per-folder transfer errors. Either value also means the keyring answered,
    /// so it ends a [`SyncEvent::KeyringUnreachable`] wait.
    Auth { signed_in: bool },
    /// The CLI cannot reach the keyring its session is stored in (locked, no
    /// D-Bus session, a headless box). Not a sign-out: the session may be
    /// intact, signing in again won't help, and sync resumes by itself once the
    /// keyring answers - which is announced with an [`SyncEvent::Auth`].
    KeyringUnreachable,
    /// The mass-deletion guard held a run's deletions back: they would have
    /// removed `files` files, too many of the `tracked` the pair holds, which is
    /// far more often a mistake (an unmounted drive, a folder emptied by
    /// accident) than intended. Nothing was deleted; the run still transferred.
    /// It is re-reported by every run until the user confirms or restores.
    DeletesHeld {
        pair: String,
        files: usize,
        tracked: usize,
    },
}

/// What to tell the user when the guard held `files` deletions of `tracked`
/// back: what it saw, that nothing was deleted, and both ways out - deleting for
/// real, or getting the files back.
pub fn held_deletes_text(pair: &str, files: usize, tracked: usize) -> String {
    let percent = if tracked == 0 {
        0
    } else {
        (files * 100).div_ceil(tracked).min(100)
    };
    format!(
        "{pair}: held back {} ({percent}% of the {} this folder pair tracks) - that looks like a \
         mistake, such as an unmounted drive or a folder emptied by accident, so nothing was \
         deleted. If you meant it: `protondrive4linux sync --allow-mass-delete {pair}`. To get \
         the files back instead: `protondrive4linux sync --resync {pair}`.",
        crate::datefmt::counted(files, "deletion", "deletions"),
        crate::datefmt::counted(tracked, "file", "files"),
    )
}

/// Receiver of [`SyncEvent`]s. Implementations must be cheap and thread-safe:
/// `emit` is called from the engine, including from concurrent scan workers.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &SyncEvent);
}

/// A no-op sink (used when no frontend is observing).
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _ev: &SyncEvent) {}
}
