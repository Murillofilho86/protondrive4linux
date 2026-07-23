//! Watch daemon: filesystem events (inotify via `notify`) as *hints*, plus a
//! periodic full rescan as the *safety net* - the pattern every serious sync
//! client uses, because events can be missed (queue overflow, watch limits,
//! downtime). Events only mark a pair dirty; the engine still computes the real
//! diff (cheap now, thanks to the hash cache). Remote changes have no event
//! feed on this CLI, so they are caught by the periodic rescan.

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
use crate::engine::run_sync_with;
use crate::events::EventSink;
use crate::logger::Logger;
use crate::stats::Stats;

/// A pair counts as "hot" if it saw >= this many changes in the window.
const HOT_WINDOW_SECS: i64 = 1800;
const HOT_THRESHOLD: i64 = 1;

enum Msg {
    /// A debounced local change was seen under pair index `usize`.
    Pair(usize),
    /// The short timer fired: rescan only "hot" (recently active) pairs.
    PollHot,
    /// The long timer fired: rescan everything (safety net + remote pull).
    PollAll,
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

    // 1) Catch-up reconcile on startup (also seeds baselines).
    log.info("watch: initial reconcile of all pairs");
    let s = run_sync_with(cfg, &pairs, false, false, log, events, Some(stop));
    log.info(&format!(
        "watch: initial done ({} applied, {} error(s))",
        s.applied, s.errors
    ));

    // 2) Debounced FS watcher -> mark the owning pair dirty and record hotness.
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
            let mut hit = BTreeSet::new();
            for ev in events {
                for path in &ev.paths {
                    if let Some(i) = handler_roots.iter().position(|r| path.starts_with(r)) {
                        hit.insert(i);
                        // record which subfolder changed, for hot-folder scans
                        if let Some(st) = &handler_stats {
                            if let Ok(rel) = path.strip_prefix(&handler_roots[i]) {
                                let rel = rel.to_string_lossy().replace('\\', "/");
                                if !rel.is_empty() {
                                    let _ = st.record_change(&handler_names[i], &rel, now_epoch());
                                }
                            }
                        }
                    }
                }
            }
            for i in hit {
                let _ = handler_tx.send(Msg::Pair(i));
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
        "watch: watching {} folder(s); hot scan every {}s, full rescan every {}s",
        roots.len(),
        cfg.scan_interval_secs,
        cfg.poll_interval_secs
    ));

    // 3a) Long timer: full rescan (safety net + catches remote changes anywhere).
    let poll_tx = tx.clone();
    let poll = cfg.poll_interval_secs.max(30);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(poll));
        if poll_tx.send(Msg::PollAll).is_err() {
            break;
        }
    });

    // 3b) Short timer: rescan only hot (recently active) pairs, so busy folders
    //     pick up remote changes far sooner than the full sweep.
    let hot_tx = tx.clone();
    let scan = cfg.scan_interval_secs.max(15);
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(scan));
        if hot_tx.send(Msg::PollHot).is_err() {
            break;
        }
    });

    // 4) Main loop: coalesce a burst, then reconcile the affected pairs. Single
    //    threaded, so no two syncs of the same pair ever overlap. Our own writes
    //    (downloads) re-fire events, but the follow-up reconcile is an idempotent
    //    no-op, so it converges rather than looping.
    loop {
        if stop.load(Ordering::Relaxed) {
            log.info("watch: stopped");
            break;
        }
        // Timed recv so the stop flag is checked promptly.
        let first = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => m,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut dirty: BTreeSet<usize> = BTreeSet::new();
        let mut poll_all = false;
        let mut poll_hot = false;
        let mut take = |m: Msg| match m {
            Msg::Pair(i) => {
                dirty.insert(i);
            }
            Msg::PollHot => poll_hot = true,
            Msg::PollAll => poll_all = true,
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

        let to_sync: Vec<Pair> = if poll_all {
            pairs.clone()
        } else {
            let mut idxs = dirty.clone();
            // add hot pairs on the short tick
            if poll_hot {
                if let Some(st) = &stats {
                    let now = now_epoch();
                    for (i, p) in pairs.iter().enumerate() {
                        let hot = st
                            .hot_folders(&p.name, HOT_WINDOW_SECS, HOT_THRESHOLD, now)
                            .map(|v| !v.is_empty())
                            .unwrap_or(false);
                        if hot {
                            idxs.insert(i);
                        }
                    }
                }
            }
            idxs.iter().filter_map(|&i| pairs.get(i).cloned()).collect()
        };
        if to_sync.is_empty() {
            continue;
        }
        let names: Vec<&str> = to_sync.iter().map(|p| p.name.as_str()).collect();
        let reason = if poll_all {
            "full rescan"
        } else if !dirty.is_empty() {
            "local change"
        } else {
            "hot rescan"
        };
        log.info(&format!("watch: {reason} -> reconciling {names:?}"));
        let s = run_sync_with(cfg, &to_sync, false, false, log, events, Some(stop));
        log.info(&format!(
            "watch: done ({} applied, {} error(s))",
            s.applied, s.errors
        ));
    }
    Ok(())
}
