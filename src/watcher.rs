//! Watch daemon. A local filesystem change (inotify via `notify`, recursive)
//! triggers a *shallow* reconcile of just the folder whose direct contents
//! changed; deeper changes arrive as their own events. On startup, folders with
//! fresh local changes sync first, then recently-active ("hot") folders, then a
//! full walk. Remote-side changes have no event feed on this CLI, so they are
//! caught by the hot pass and by the full walk, which is paced to how long a
//! walk actually takes (see `docs/SYNC_MODEL.md`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};

use crate::config::{Config, Pair};
use crate::datefmt::now_epoch;
use crate::engine::{run_sync_shallow_many, run_sync_streaming};
use crate::events::{EventSink, SyncEvent};
use crate::logger::Logger;
use crate::protoncli::{session_problem, ProtonCli, Remote, SessionProblem};
use crate::stats::Stats;

/// A sub-folder counts as "hot" if it saw >= this many changes in the window.
const HOT_WINDOW_SECS: i64 = 1800;
const HOT_THRESHOLD: i64 = 1;
/// Full-walk cadence: after a full reconcile, the next one is scheduled at
/// `walk_duration * FULL_WALK_MULTIPLIER`, floored at the configured
/// `poll_interval` and capped so it always runs eventually. So a big tree that
/// takes 15 min to walk is re-walked about every 90 min, while small/active
/// folders stay fresh via the change- and hot-folder-triggered scoped syncs.
const FULL_WALK_MULTIPLIER: u64 = 6;
const FULL_WALK_MAX_SECS: u64 = 6 * 3600;
/// While signed out, re-probe the proton-drive session no more often than this,
/// so a logged-out daemon idles cheaply instead of spawning a probe every tick.
const AUTH_REPROBE_SECS: i64 = 20;
/// While the CLI can't reach its keyring, look less often: the probe may be what
/// makes the keyring ask for its password, and one prompt a minute is already
/// plenty. Unlocking from the window checks (and resumes) at once.
const KEYRING_REPROBE_SECS: i64 = 60;
/// While signed in, still probe occasionally so an expired session is caught
/// before a full walk starts spamming per-folder errors.
const AUTH_KEEPALIVE_SECS: i64 = 300;

/// Outcome of a cheap auth probe. `Unknown` (a non-auth error, e.g. a network
/// blip) is deliberately distinct from `SignedOut` so a transient failure never
/// flips the "signed out" banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthProbe {
    SignedIn,
    SignedOut,
    /// The CLI could not reach the keyring its session lives in. Not a
    /// sign-out: the session may be intact, so nobody is asked to sign in.
    KeyringUnreachable,
    Unknown,
}

/// What a failed CLI call says about the session.
fn probe_from_error(msg: &str) -> AuthProbe {
    match session_problem(msg) {
        Some(SessionProblem::SignedOut) => AuthProbe::SignedOut,
        Some(SessionProblem::SecretStoreUnreachable) => AuthProbe::KeyringUnreachable,
        None => AuthProbe::Unknown,
    }
}

/// Probe whether the proton-drive session is authenticated by listing the remote
/// root — the same check the GUI's Account panel uses. Cheap (one CLI call).
fn probe_auth(cfg: &Config) -> AuthProbe {
    match ProtonCli::new(cfg).list_dir(&cfg.remote_root) {
        Ok(_) => AuthProbe::SignedIn,
        Err(e) => probe_from_error(&format!("{e:#}")),
    }
}

/// Whether the proton-drive session is usable, as far as the watcher knows.
/// Anything but `Open` pauses all sync work until a probe says it is back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Open,
    /// No session: it takes a sign-in to resume.
    SignedOut,
    /// The CLI can't reach its keyring (locked, no D-Bus session, headless).
    /// Resumes by itself once the keyring answers.
    KeyringUnreachable,
}

impl Gate {
    /// Where the gate stands after `probe`. An inconclusive probe (a network
    /// blip) leaves it where it was: a flaky connection must neither pause
    /// sync nor resume it.
    fn after(self, probe: AuthProbe) -> Gate {
        match probe {
            AuthProbe::SignedIn => Gate::Open,
            AuthProbe::SignedOut => Gate::SignedOut,
            AuthProbe::KeyringUnreachable => Gate::KeyringUnreachable,
            AuthProbe::Unknown => self,
        }
    }

    /// How long to leave the session alone before probing it again.
    fn probe_every_secs(self) -> i64 {
        match self {
            Gate::Open => AUTH_KEEPALIVE_SECS,
            Gate::SignedOut => AUTH_REPROBE_SECS,
            Gate::KeyringUnreachable => KEYRING_REPROBE_SECS,
        }
    }
}

/// Announce a change of the gate: emit a structured event (so the GUI banner
/// flips) and log it once. Nothing happens if it did not change.
fn note_gate(events: Option<&dyn EventSink>, log: &Logger, from: Gate, to: Gate) {
    if from == to {
        return;
    }
    if let Some(sink) = events {
        sink.emit(&match to {
            Gate::Open => SyncEvent::Auth { signed_in: true },
            Gate::SignedOut => SyncEvent::Auth { signed_in: false },
            Gate::KeyringUnreachable => SyncEvent::KeyringUnreachable,
        });
    }
    match (from, to) {
        (Gate::KeyringUnreachable, Gate::Open) => {
            log.info("watch: the keyring answers again — resuming")
        }
        (_, Gate::Open) => log.info("watch: signed back in to Proton — resuming"),
        (_, Gate::SignedOut) => {
            log.warn("watch: proton-drive session signed out — pausing sync until you sign in")
        }
        (_, Gate::KeyringUnreachable) => log.warn(
            "watch: the proton-drive CLI can't reach its keyring (locked, or none running in \
             this session) — pausing sync; it resumes by itself once the keyring answers",
        ),
    }
}

/// Move `gate` to where `probe` says the session is. A change is announced, and
/// when it shuts the gate any running walk is told to stop (it would only fail
/// folder by folder). Returns whether the gate changed.
fn apply_probe(
    gate: &mut Gate,
    probe: AuthProbe,
    events: Option<&dyn EventSink>,
    log: &Logger,
    walk_cancel: &AtomicBool,
) -> bool {
    let next = gate.after(probe);
    if next == *gate {
        return false;
    }
    note_gate(events, log, *gate, next);
    if next != Gate::Open {
        walk_cancel.store(true, Ordering::Relaxed);
    }
    *gate = next;
    true
}

enum Msg {
    /// A debounced local change under pair index `usize`, in sub-folder `sub`
    /// (POSIX path relative to the pair root; "" means the root). Synced scoped
    /// to just that folder, not the whole pair.
    Sub { pair: usize, sub: String },
    /// The short timer fired: sync only the recently-active ("hot") sub-folders.
    PollHot,
}

/// The sub-folder of a pair-relative file path (everything before the last
/// separator; "" for a top-level file).
fn folder_of(rel: &str) -> String {
    match rel.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

/// Whether a filesystem event is a real content/structure change worth syncing.
/// We IGNORE access (reads) and metadata-only (attribute/mtime) events: our own
/// reconcile READS folders and sets mtimes on downloads, and if those counted as
/// changes they would mark the just-touched folders "hot" and drive an endless
/// self-feeding re-scan loop. Everything else (create, write, delete, rename,
/// or an unspecified modify) is treated as a real change, conservatively.
fn is_content_change(kind: &notify_debouncer_full::notify::EventKind) -> bool {
    use notify_debouncer_full::notify::event::{AccessKind, AccessMode, ModifyKind};
    use notify_debouncer_full::notify::EventKind;
    match kind {
        // A completed WRITE (file closed after being written, IN_CLOSE_WRITE) is a
        // real change — many tools only signal a save this way, so we must keep it.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        // Other access (reads, opens, read-closes) and metadata/mtime touches are
        // our own reconcile's footprint; counting them self-triggers a re-scan.
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_)) => false,
        // Create / delete / data-modify / rename / unspecified modify = real.
        _ => true,
    }
}

/// Name, inside the state dir, of the watcher's lock file (`doctor` reads it to
/// tell whether a watcher is running).
pub const LOCK_FILE: &str = "watch.lock";

/// Single-instance lock so a watch daemon and, say, a systemd timer don't run
/// the same pairs at once. Held with a kernel file lock and described by a pid +
/// start time + boot id record - see [`crate::process_lock`] for why a bare PID
/// file wasn't enough (a pid outlives its process in the file, and gets reused).
struct DaemonLock {
    _lock: crate::process_lock::ProcessLock,
}

impl DaemonLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        use crate::process_lock::{acquire, AcquireError};
        let path = state_dir.join(LOCK_FILE);
        match acquire(&path) {
            Ok(lock) => Ok(DaemonLock { _lock: lock }),
            Err(AcquireError::Held { pid }) => bail!(
                "a protondrive4linux watch is already running{}; lock file {}",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default(),
                path.display()
            ),
            Err(AcquireError::Io(e)) => {
                Err(anyhow::Error::new(e).context(format!("taking {}", path.display())))
            }
        }
    }
}

/// Reload the config from its source file so edits made while the daemon runs
/// (exclusions, delete policy, compare mode, …) take effect on the next
/// reconcile — no restart needed. Falls back to the config the daemon started
/// with when there's no source path or the reload fails (e.g. a half-written
/// file mid-save), so a bad read never disrupts syncing.
fn reload_cfg(base: &Config, log: &Logger) -> Config {
    let Some(p) = base.source_path.as_deref() else {
        return base.clone();
    };
    // `p.to_str()` returning None (non-UTF-8 path) must not be treated the
    // same as "no explicit path was given" - config::load(None) would then
    // search the default config locations instead of reloading the actual
    // running config, and could load an entirely different file.
    let Some(p_str) = p.to_str() else {
        log.warn(&format!(
            "watch: keeping running config ({} is not valid UTF-8)",
            p.display()
        ));
        return base.clone();
    };
    match crate::config::load(Some(p_str)) {
        Ok(c) => c,
        Err(e) => {
            log.warn(&format!(
                "watch: keeping running config (reload failed: {e})"
            ));
            base.clone()
        }
    }
}

/// Pick the pairs named in `names` out of a (freshly reloaded) config.
fn pick_pairs(cfg: &Config, names: &BTreeSet<String>) -> Vec<Pair> {
    cfg.pairs
        .iter()
        .filter(|p| names.contains(&p.name))
        .cloned()
        .collect()
}

/// Run the watch loop until `stop` is set (GUI toggle) or the process is killed.
pub fn watch(cfg: &Config, pairs: Vec<Pair>, log: &Logger, stop: &AtomicBool) -> Result<()> {
    watch_with(cfg, pairs, log, stop, None)
}

/// Like [`watch`], but forwards structured events to `events` (used by the GUI
/// service so the watch daemon feeds the same progress state).
pub fn watch_with(
    cfg: &Config,
    pairs: Vec<Pair>,
    log: &Logger,
    stop: &AtomicBool,
    events: Option<&dyn EventSink>,
) -> Result<()> {
    if pairs.is_empty() {
        bail!("no pairs to watch");
    }
    let _lock = DaemonLock::acquire(&cfg.state_dir)?;
    let stats = Stats::open(&cfg.state_dir).ok().map(Arc::new);

    // Canonical local roots. Only create a missing root for a brand-new pair
    // (so inotify can attach). If a pair has synced before and its root is now
    // missing, it's most likely an unmounted drive — do NOT recreate it as an
    // empty directory, which would look like a mass deletion. Leave it be; the
    // reconcile refuses that pair and warns until it's restored.
    let roots: Vec<PathBuf> = pairs
        .iter()
        .map(|p| {
            let has_base = stats
                .as_ref()
                .and_then(|s| s.has_baseline(&p.name).ok())
                .unwrap_or(false);
            if !p.local.exists() && !has_base {
                let _ = std::fs::create_dir_all(&p.local);
            }
            std::fs::canonicalize(&p.local).unwrap_or_else(|_| p.local.clone())
        })
        .collect();

    let (tx, rx) = mpsc::channel::<Msg>();

    // Names we watch; used to pick the matching pairs out of a freshly reloaded
    // config each cycle so live exclusion/setting edits apply without a restart.
    let watched: BTreeSet<String> = pairs.iter().map(|p| p.name.clone()).collect();

    // A full reconcile of every watched pair, reloading config first and timed
    // so the next full walk can be paced to how long a walk actually takes. Uses
    // the STREAMING walk: it reconciles and transfers folder-by-folder as it
    // discovers them, so uploads/downloads overlap the walk instead of waiting
    // for the whole tree to be scanned first.
    let full_walk = |cancel: &AtomicBool| -> u64 {
        let started = now_epoch();
        let live = reload_cfg(cfg, log);
        let (mut applied, mut errs) = (0usize, 0usize);
        for p in pick_pairs(&live, &watched) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let r = run_sync_streaming(&live, &p, log, events, Some(cancel));
            applied += r.applied;
            errs += r.errors;
        }
        let secs = (now_epoch() - started).max(0) as u64;
        log.info(&format!(
            "watch: full walk done in {secs}s ({applied} applied, {errs} error(s))"
        ));
        secs
    };
    // Delay to the next full walk: walk_duration * multiplier, floored at the
    // configured poll_interval and capped so it always eventually runs.
    let next_full_delay = |secs: u64| -> u64 {
        secs.saturating_mul(FULL_WALK_MULTIPLIER)
            .max(cfg.poll_interval_secs.max(30))
            .min(FULL_WALK_MAX_SECS)
    };

    // 1) Start watching BEFORE the (possibly long) initial full walk, so local
    //    changes made while it runs are captured and processed right after,
    //    rather than lost during a blind startup window.
    // 2) Debounced FS watcher -> record hotness + queue a scoped sync per folder.
    let handler_roots = roots.clone();
    let handler_names: Vec<String> = pairs.iter().map(|p| p.name.clone()).collect();
    let handler_stats = stats.clone();
    let handler_tx = tx.clone();
    let mut debouncer = new_debouncer(
        Duration::from_secs(cfg.debounce_secs.max(1)),
        None,
        move |res: DebounceEventResult| {
            let events = match res {
                Ok(ev) => ev,
                Err(_) => return,
            };
            // Collect the distinct (pair, sub-folder) pairs that changed, record
            // hotness, and queue a SCOPED sync for each — just the folder the
            // change happened in, never the whole pair.
            let mut hit: BTreeSet<(usize, String)> = BTreeSet::new();
            for ev in events {
                // Skip our own reads / mtime-touches so they can't mark folders
                // hot and cause an endless re-scan loop.
                if !is_content_change(&ev.kind) {
                    continue;
                }
                for path in &ev.paths {
                    if let Some(i) = handler_roots.iter().position(|r| path.starts_with(r)) {
                        if let Ok(rel) = path.strip_prefix(&handler_roots[i]) {
                            let rel = rel.to_string_lossy().replace('\\', "/");
                            if rel.is_empty() {
                                continue;
                            }
                            if let Some(st) = &handler_stats {
                                let _ = st.record_change(&handler_names[i], &rel, now_epoch());
                            }
                            // Reconcile the folder whose direct contents changed:
                            // the path itself if it's a directory, else its
                            // parent (also correct for a just-deleted path).
                            let folder = if path.is_dir() {
                                rel.clone()
                            } else {
                                folder_of(&rel)
                            };
                            hit.insert((i, folder));
                        }
                    }
                }
            }
            for (i, folder) in hit {
                let _ = handler_tx.send(Msg::Sub {
                    pair: i,
                    sub: folder,
                });
            }
        },
    )?;
    for r in &roots {
        if let Err(e) = debouncer.watch(r, RecursiveMode::Recursive) {
            // A missing root (e.g. an unmounted drive) must not kill the whole
            // daemon; that pair still reconciles on the periodic rescan (and is
            // refused there until the folder is back).
            log.warn(&format!(
                "watch: cannot watch {} ({e}); it will still reconcile periodically",
                r.display()
            ));
        }
    }
    log.info(&format!(
        "watch: watching {} folder(s); hot re-check every {}s; full walk adaptive (>= {}s)",
        roots.len(),
        cfg.scan_interval_secs,
        cfg.poll_interval_secs
    ));

    // 3) Short timer: periodically re-check hot (recently active) sub-folders so
    //    a busy folder picks up remote changes without waiting for a full walk.
    let hot_tx = tx.clone();
    let scan = cfg.scan_interval_secs.max(15);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(scan));
        if hot_tx.send(Msg::PollHot).is_err() {
            break;
        }
    });

    // Auth state. When the proton-drive session is signed out, or the keyring it
    // lives in can't be reached, every remote call fails; rather than hammer
    // every folder and bury it as per-folder errors, we detect it, surface ONE
    // clear event (a GUI banner that says which), and pause real work —
    // re-probing on a throttle until the session is back.
    let mut gate = Gate::Open.after(probe_auth(&reload_cfg(cfg, log)));
    let mut last_auth_probe = now_epoch();
    let state_dir = cfg.state_dir.clone();
    note_gate(events, log, Gate::Open, gate);

    // Prioritised startup (the watcher is already attached, so live changes
    // queue meanwhile): 1) sync folders with fresh LOCAL changes first; 2) sync
    // recently-active HOT folders; 3) then the streaming full walk for the rest.
    // Steps 1 and 2 reconcile their folders CONCURRENTLY (a worker pool) so a
    // large set isn't a slow one-folder-at-a-time slog. Root ("") folders are
    // left to the full walk. Each folder is a safe shallow reconcile. Skipped
    // while the session is unusable; the main loop resumes them once it is back.
    if gate == Gate::Open {
        let live = reload_cfg(cfg, log);
        let mut primed: BTreeSet<(String, String)> = BTreeSet::new();
        // 1) folders with fresh local changes
        for p in pick_pairs(&live, &watched) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let folders: Vec<String> = crate::engine::local_change_folders(&live, &p)
                .into_iter()
                .filter(|f| !f.is_empty())
                .collect();
            if !folders.is_empty() {
                log.info(&format!(
                    "watch: startup local-change sync {:?} ({} folder(s))",
                    p.name,
                    folders.len()
                ));
                let _ = run_sync_shallow_many(&live, &p, &folders, log, events, Some(stop));
                for f in folders {
                    primed.insert((p.name.clone(), f));
                }
            }
        }
        // 2) recently-active hot folders (skip root and any already synced above)
        if let Some(st) = &stats {
            let now = now_epoch();
            for p in pick_pairs(&live, &watched) {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let folders: Vec<String> = st
                    .hot_folders(&p.name, HOT_WINDOW_SECS, HOT_THRESHOLD, now)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|f| !f.is_empty() && !primed.contains(&(p.name.clone(), f.clone())))
                    .collect();
                if !folders.is_empty() {
                    log.info(&format!(
                        "watch: startup hot sync {:?} ({} folder(s))",
                        p.name,
                        folders.len()
                    ));
                    let _ = run_sync_shallow_many(&live, &p, &folders, log, events, Some(stop));
                }
            }
        }
    }

    // 3) The initial full reconcile runs as a BACKGROUND walk (see the loop
    //    below), so local changes made while it runs sync immediately instead of
    //    waiting the whole walk out. Schedule it to start right away.
    log.info("watch: initial full reconcile of all pairs (background)");
    let mut next_full_at = now_epoch();

    // A cancel flag for the BACKGROUND full walk, distinct from `stop` (the
    // global shutdown / GUI-toggle flag). We trip it on shutdown or when we
    // detect a sign-out, so a walk in flight winds down promptly instead of
    // churning through a whole tree of failing calls.
    let walk_cancel = Arc::new(AtomicBool::new(false));

    // 4) Main loop. The full walk runs on a BACKGROUND thread so change-triggered
    //    shallow syncs are serviced CONCURRENTLY with it — a fresh save no longer
    //    waits out a long walk. Only one walk runs at a time. Overlapping a
    //    change sync with the walk is safe: concurrent per-folder reconciles
    //    already happen inside a single walk, and each is the same scoped,
    //    positive-confirmation primitive (SQLite serialises the baseline writes).
    thread::scope(|s| {
        let mut walk: Option<thread::ScopedJoinHandle<'_, u64>> = None;
        loop {
            if stop.load(Ordering::Relaxed) {
                walk_cancel.store(true, Ordering::Relaxed);
                log.info("watch: stopped");
                break;
            }

            // Reap a finished background walk; pace the next off its duration.
            if walk.as_ref().is_some_and(|h| h.is_finished()) {
                let secs = walk.take().unwrap().join().unwrap_or(0);
                next_full_at = now_epoch() + next_full_delay(secs) as i64;
                log.info(&format!(
                    "watch: next full walk in ~{}s",
                    (next_full_at - now_epoch()).max(0)
                ));
            }

            // Auth gate: while the session is unusable (signed out, or its keyring
            // out of reach) do no sync work; re-probe on a throttle (or at once
            // when the GUI signals a fresh login or unlock) and resume the moment
            // it is back. While it is usable, still probe now and then so an
            // expiry is caught before a walk starts failing folder by folder.
            if gate != Gate::Open {
                walk_cancel.store(true, Ordering::Relaxed);
            }
            let auth_due = crate::auth_signal::take(&state_dir)
                || now_epoch() - last_auth_probe >= gate.probe_every_secs();
            if auth_due {
                last_auth_probe = now_epoch();
                let probe = probe_auth(&reload_cfg(cfg, log));
                if apply_probe(&mut gate, probe, events, log, &walk_cancel) && gate == Gate::Open {
                    // Re-walk soon so anything missed while out is reconciled.
                    next_full_at = now_epoch();
                }
            }
            if gate != Gate::Open {
                thread::sleep(Duration::from_millis(500));
                continue;
            }

            // Start a background full walk when due and none is running.
            if walk.is_none() && now_epoch() >= next_full_at {
                walk_cancel.store(false, Ordering::Relaxed);
                walk = Some(s.spawn(|| full_walk(&walk_cancel)));
            }

            // Timed recv so the stop flag and timers are checked often.
            let first = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // Accumulate change events in arrival order (newest last), plus the
            // hot tick. We process NEWEST changes first so a fresh edit is never
            // starved behind a backlog of older / no-op folder checks.
            let mut order: Vec<(usize, String)> = Vec::new();
            let mut poll_hot = false;
            let mut take = |m: Msg| match m {
                Msg::Sub { pair, sub } => order.push((pair, sub)),
                Msg::PollHot => poll_hot = true,
            };
            take(first);
            while let Ok(m) = rx.try_recv() {
                take(m);
            }
            // brief settle to catch stragglers from the same burst
            thread::sleep(Duration::from_millis(300));
            while let Ok(m) = rx.try_recv() {
                take(m);
            }

            // Hot sub-folders (lower priority than live changes).
            let mut hot: Vec<(usize, String)> = Vec::new();
            if poll_hot {
                if let Some(st) = &stats {
                    let now = now_epoch();
                    for (i, p) in pairs.iter().enumerate() {
                        if let Ok(folders) =
                            st.hot_folders(&p.name, HOT_WINDOW_SECS, HOT_THRESHOLD, now)
                        {
                            for f in folders {
                                hot.push((i, f));
                            }
                        }
                    }
                }
            }

            // Processing list: newest change events first (deduped), then hot
            // folders, skipping anything already queued.
            let mut seen: BTreeSet<(usize, String)> = BTreeSet::new();
            let mut todo: Vec<(usize, String)> = Vec::new();
            for item in order.into_iter().rev().chain(hot) {
                if seen.insert(item.clone()) {
                    todo.push(item);
                }
            }
            if todo.is_empty() {
                continue;
            }

            // Reload config so live exclusion/setting edits apply, then run the
            // SHALLOW (direct-children-only) syncs, grouped by pair and
            // reconciled CONCURRENTLY per pair so a burst isn't a one-at-a-time
            // slog.
            let live = reload_cfg(cfg, log);
            let mut by_pair: Vec<(usize, Vec<String>)> = Vec::new();
            for (i, sub) in todo {
                match by_pair.iter_mut().find(|(pi, _)| *pi == i) {
                    Some((_, v)) => v.push(sub),
                    None => by_pair.push((i, vec![sub])),
                }
            }
            let mut cycle_errors = 0usize;
            for (i, folders) in by_pair {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let name = match pairs.get(i) {
                    Some(p) => p.name.clone(),
                    None => continue,
                };
                let pair = match live.pairs.iter().find(|p| p.name == name) {
                    Some(p) => p.clone(),
                    None => continue, // pair removed from the config
                };
                log.info(&format!(
                    "watch: change -> shallow sync {:?} ({} folder(s))",
                    pair.name,
                    folders.len()
                ));
                let r = run_sync_shallow_many(&live, &pair, &folders, log, events, Some(stop));
                cycle_errors += r.errors;
                log.info(&format!(
                    "watch: shallow done ({} applied, {} error(s))",
                    r.applied, r.errors
                ));
            }

            // A cycle that errored may mean the session expired, or its keyring
            // went out of reach. Probe once (cheap); only a genuine session
            // problem shuts the gate — a transient/other error is left to retry
            // normally.
            if cycle_errors > 0 && gate == Gate::Open {
                last_auth_probe = now_epoch();
                let probe = probe_auth(&reload_cfg(cfg, log));
                apply_probe(&mut gate, probe, events, log, &walk_cancel);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_content_change;
    use notify_debouncer_full::notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify_debouncer_full::notify::EventKind;

    #[test]
    fn ignores_reads_and_metadata_keeps_real_changes() {
        // Reads and attribute/mtime touches must NOT count (they self-trigger).
        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Any)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        // But a completed write (IN_CLOSE_WRITE) IS a real change.
        assert!(is_content_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!is_content_change(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Any)
        )));
        assert!(!is_content_change(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        )));
        // Real content/structure changes must count.
        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Any
        ))));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Any)));
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use std::sync::Mutex;

    /// Remembers the events it is given, as their `Debug` text.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl EventSink for Recorder {
        fn emit(&self, ev: &SyncEvent) {
            self.0.lock().unwrap().push(format!("{ev:?}"));
        }
    }

    impl Recorder {
        fn all(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    const KEYRING: &str = "the CLI can't reach its keyring: Failed to load session from \
                           secrets (ensure you have secrets available): Could not connect: \
                           No such file or directory (code: 1)";

    #[test]
    fn a_failed_call_is_read_for_what_it_says_about_the_session() {
        assert_eq!(probe_from_error(KEYRING), AuthProbe::KeyringUnreachable);
        assert_eq!(
            probe_from_error("not signed in: you need to login first"),
            AuthProbe::SignedOut
        );
        // A network blip or a rate limit says nothing about the session.
        for other in ["connection reset", "rate limited, try again", ""] {
            assert_eq!(probe_from_error(other), AuthProbe::Unknown, "{other:?}");
        }
    }

    #[test]
    fn a_probe_moves_the_gate_and_an_inconclusive_one_leaves_it_alone() {
        use AuthProbe::*;
        for from in [Gate::Open, Gate::SignedOut, Gate::KeyringUnreachable] {
            assert_eq!(from.after(SignedIn), Gate::Open);
            assert_eq!(from.after(SignedOut), Gate::SignedOut);
            assert_eq!(from.after(KeyringUnreachable), Gate::KeyringUnreachable);
            // A flaky connection neither pauses sync nor resumes it.
            assert_eq!(from.after(Unknown), from);
        }
    }

    #[test]
    fn a_keyring_is_probed_less_often_than_a_sign_out() {
        // A probe of a locked keyring can be what makes it ask for a password.
        assert!(Gate::KeyringUnreachable.probe_every_secs() > Gate::SignedOut.probe_every_secs());
        assert!(Gate::Open.probe_every_secs() > Gate::KeyringUnreachable.probe_every_secs());
    }

    #[test]
    fn each_change_of_the_gate_is_announced_once_and_in_its_own_words() {
        let log = Logger::silent();
        let events = Recorder::default();
        let note = |from, to| note_gate(Some(&events), &log, from, to);

        note(Gate::Open, Gate::Open);
        note(Gate::KeyringUnreachable, Gate::KeyringUnreachable);
        assert!(events.all().is_empty(), "no change, nothing said");

        note(Gate::Open, Gate::KeyringUnreachable);
        note(Gate::KeyringUnreachable, Gate::Open);
        note(Gate::Open, Gate::SignedOut);
        note(Gate::SignedOut, Gate::KeyringUnreachable);
        note(Gate::KeyringUnreachable, Gate::SignedOut);
        note(Gate::SignedOut, Gate::Open);
        assert_eq!(
            events.all(),
            [
                "KeyringUnreachable",
                "Auth { signed_in: true }",
                "Auth { signed_in: false }",
                "KeyringUnreachable",
                "Auth { signed_in: false }",
                "Auth { signed_in: true }",
            ]
        );
    }

    #[test]
    fn shutting_the_gate_stops_a_running_walk_and_opening_it_does_not() {
        let log = Logger::silent();
        let walk = AtomicBool::new(false);
        let mut gate = Gate::Open;

        // Inconclusive: nothing changes, nothing is cancelled.
        assert!(!apply_probe(
            &mut gate,
            AuthProbe::Unknown,
            None,
            &log,
            &walk
        ));
        assert_eq!((gate, walk.load(Ordering::Relaxed)), (Gate::Open, false));

        // The keyring goes out of reach: the walk (which would only fail folder
        // by folder) is told to stop.
        assert!(apply_probe(
            &mut gate,
            AuthProbe::KeyringUnreachable,
            None,
            &log,
            &walk
        ));
        assert_eq!(
            (gate, walk.load(Ordering::Relaxed)),
            (Gate::KeyringUnreachable, true)
        );

        // Still out of reach: no change to report.
        assert!(!apply_probe(
            &mut gate,
            AuthProbe::KeyringUnreachable,
            None,
            &log,
            &walk
        ));

        // It answers: the gate opens. Starting the next walk is the loop's job
        // (it clears the flag itself), so the flag is left as it was.
        assert!(apply_probe(
            &mut gate,
            AuthProbe::SignedIn,
            None,
            &log,
            &walk
        ));
        assert_eq!(gate, Gate::Open);
    }
}
