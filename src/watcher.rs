//! Watch daemon. A local filesystem change (inotify via `notify`, recursive)
//! triggers a *shallow* reconcile of just the folder whose direct contents
//! changed; deeper changes arrive as their own events. On startup, folders with
//! fresh local changes sync first, then recently-active ("hot") folders, then a
//! full walk. Remote-side changes have no event feed on this CLI, so they are
//! caught by the hot pass and by the full walk, which is paced to how long a
//! walk actually takes (see `docs/SYNC_MODEL.md`).

use std::collections::{BTreeMap, BTreeSet};
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
use crate::engine::{run_sync_shallow, run_sync_with};
use crate::events::EventSink;
use crate::logger::Logger;
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

/// Single-instance lock so a watch daemon and, say, a systemd timer don't run
/// the same pairs at once. Advisory: a PID file, checked against /proc.
struct DaemonLock {
    path: PathBuf,
}

impl DaemonLock {
    fn acquire(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("watch.lock");
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(pid) = s.trim().parse::<u32>() {
                if Path::new(&format!("/proc/{pid}")).exists() {
                    bail!(
                        "a neutronsync watch is already running (pid {pid}); \
                         lock file {}",
                        path.display()
                    );
                }
            }
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(DaemonLock { path })
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Reload the config from its source file so edits made while the daemon runs
/// (exclusions, delete policy, compare mode, …) take effect on the next
/// reconcile — no restart needed. Falls back to the config the daemon started
/// with when there's no source path or the reload fails (e.g. a half-written
/// file mid-save), so a bad read never disrupts syncing.
fn reload_cfg(base: &Config, log: &Logger) -> Config {
    match base.source_path.as_deref() {
        Some(p) => match crate::config::load(p.to_str()) {
            Ok(c) => c,
            Err(e) => {
                log.warn(&format!(
                    "watch: keeping running config (reload failed: {e})"
                ));
                base.clone()
            }
        },
        None => base.clone(),
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
    // so the next full walk can be paced to how long a walk actually takes.
    let full_walk = |stop: &AtomicBool| -> u64 {
        let started = now_epoch();
        let live = reload_cfg(cfg, log);
        let all = pick_pairs(&live, &watched);
        let s = run_sync_with(&live, &all, false, false, log, events, Some(stop));
        let secs = (now_epoch() - started).max(0) as u64;
        log.info(&format!(
            "watch: full walk done in {secs}s ({} applied, {} error(s))",
            s.applied, s.errors
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

    // Prioritised startup (the watcher is already attached, so live changes
    // queue meanwhile): 1) sync folders with fresh LOCAL changes first — a quick
    // local scan finds them and uploads within seconds; 2) sync recently-active
    // HOT folders; 3) then the full walk for everything else. Each step is a SAFE
    // scoped sync that checks the remote for that folder, so an upload never
    // blindly clobbers a remote copy.
    {
        let live = reload_cfg(cfg, log);
        let mut primed: BTreeSet<(String, String)> = BTreeSet::new();
        let scoped = |p: &Pair, folder: &str, why: &str| {
            let label = if folder.is_empty() { "(root)" } else { folder };
            log.info(&format!("watch: startup {why} sync {:?} [{label}]", p.name));
            let _ = run_sync_shallow(&live, p, folder, log, events, Some(stop));
        };
        // 1) folders with fresh local changes
        for p in pick_pairs(&live, &watched) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            for folder in crate::engine::local_change_folders(&live, &p) {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // Root-level ("") changes are left to the full walk (step 3) —
                // an empty scope is a whole-pair sync, pointless to do twice.
                if folder.is_empty() {
                    continue;
                }
                scoped(&p, &folder, "local-change");
                primed.insert((p.name.clone(), folder));
            }
        }
        // 2) recently-active hot folders (skip any already synced above)
        if let Some(st) = &stats {
            let now = now_epoch();
            for p in pick_pairs(&live, &watched) {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let hot = st
                    .hot_folders(&p.name, HOT_WINDOW_SECS, HOT_THRESHOLD, now)
                    .unwrap_or_default();
                for folder in hot {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if folder.is_empty() || primed.contains(&(p.name.clone(), folder.clone())) {
                        continue;
                    }
                    scoped(&p, &folder, "hot");
                    primed.insert((p.name.clone(), folder));
                }
            }
        }
    }

    // 3) Catch-up FULL reconcile (seeds baselines, catches remote-only changes).
    //    The next full walk is paced off this walk's measured duration.
    log.info("watch: initial full reconcile of all pairs");
    let last = full_walk(stop);
    let mut next_full_at = now_epoch() + next_full_delay(last) as i64;
    log.info(&format!(
        "watch: next full walk in ~{}s",
        (next_full_at - now_epoch()).max(0)
    ));

    // 4) Main loop: coalesce a burst of change events into a set of (pair,
    //    sub-folder) SCOPED syncs; on the hot tick, add recently-active
    //    sub-folders; and run a FULL walk when the adaptive timer is due. Single
    //    threaded, so syncs never overlap. Our own writes re-fire events, but the
    //    follow-up scoped sync is an idempotent no-op, so it converges.
    loop {
        if stop.load(Ordering::Relaxed) {
            log.info("watch: stopped");
            break;
        }

        // Adaptive full walk: due when the paced timer elapses.
        if now_epoch() >= next_full_at {
            let last = full_walk(stop);
            next_full_at = now_epoch() + next_full_delay(last) as i64;
            log.info(&format!(
                "watch: next full walk in ~{}s",
                (next_full_at - now_epoch()).max(0)
            ));
            continue;
        }

        // Timed recv so the stop flag and the full-walk timer are checked often.
        let first = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Accumulate changed sub-folders per pair index, plus the hot tick.
        let mut subs: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        let mut poll_hot = false;
        let mut take = |m: Msg| match m {
            Msg::Sub { pair, sub } => {
                subs.entry(pair).or_default().insert(sub);
            }
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

        // On the hot tick, add each pair's recently-active sub-folders so busy
        // areas pick up remote changes between full walks.
        if poll_hot {
            if let Some(st) = &stats {
                let now = now_epoch();
                for (i, p) in pairs.iter().enumerate() {
                    if let Ok(folders) =
                        st.hot_folders(&p.name, HOT_WINDOW_SECS, HOT_THRESHOLD, now)
                    {
                        if !folders.is_empty() {
                            let set = subs.entry(i).or_default();
                            set.extend(folders);
                        }
                    }
                }
            }
        }

        if subs.is_empty() {
            continue;
        }

        // Reload config so live exclusion/setting edits apply, then run one
        // SCOPED sync per (pair, sub-folder) — never the whole pair.
        let live = reload_cfg(cfg, log);
        for (i, folders) in &subs {
            let name = match pairs.get(*i) {
                Some(p) => p.name.clone(),
                None => continue,
            };
            let pair = match live.pairs.iter().find(|p| p.name == name) {
                Some(p) => p.clone(),
                None => continue, // pair was removed from the config
            };
            for sub in folders {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let label = if sub.is_empty() {
                    "(root)"
                } else {
                    sub.as_str()
                };
                log.info(&format!(
                    "watch: change -> shallow sync {:?} [{label}]",
                    pair.name
                ));
                let s = run_sync_shallow(&live, &pair, sub, log, events, Some(stop));
                log.info(&format!(
                    "watch: shallow done ({} applied, {} error(s))",
                    s.applied, s.errors
                ));
            }
        }
    }
    Ok(())
}
