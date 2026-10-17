//! The bidirectional sync engine.
//!
//! Three-way merge: a per-pair baseline snapshot records the last state the two
//! sides agreed on. Each run scans the current local and remote trees and, for
//! every path, classifies it against the baseline (created/modified/deleted)
//! independently per side. Combining the two verdicts decides the action and
//! which side wins - the sync logic the proton-drive CLI lacks.
//!
//! Safety: deletions never propagate unless enabled; a missing baseline unions
//! both sides (never mass-deletes); conflicts keep both versions by default.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;

use crate::events::{ConflictResolution, EventSink, SyncEvent};

use crate::config::{remote_join, strip_root, Config, ConflictPolicy, LocalDelete, Pair};
use crate::datefmt::{epoch_to_stamp, now_epoch};
use crate::logger::Logger;
use crate::models::{
    same_content, Action, Change, Compare, DownloadJob, Entry, LocalSeen, Op, Plan,
};
use crate::protoncli::{ListOutcome, ProtonCli, Remote};
use crate::staged::{self, Expect};
use crate::trash::trash_local;

/// `plan()`'s return: (the plan, the prospective new baseline, whether the
/// scan was incomplete, and the set of baseline rows to prune).
type PlanOutcome = (
    Plan,
    BTreeMap<String, Entry>,
    bool,
    HashSet<String>,
    Option<Held>,
);

/// Deletions the guard held back from one plan: how many files, of how many
/// the pair tracks.
#[derive(Clone, Copy, Debug)]
struct Held {
    remote: usize,
    local: usize,
    tracked: usize,
}

/// A pair root that comes back empty only holds deletions back when at least
/// this many files are on record for the pair. An unmounted drive or a folder
/// emptied by mistake is a folder full of files going missing at once; deleting
/// the only file of a small folder is just a deletion, and holding that back
/// would make the guard a nuisance rather than a safety net.
const EMPTY_ROOT_MIN_FILES: usize = 5;

/// The mass-deletion guard. With deletes propagating, a single run may delete at
/// most `allowed` files - the larger of `max_delete_files` and `max_delete_percent`
/// of the files the pair tracks - unless it was told the deletion is intended
/// (`Config::allow_mass_delete`). The first request that would go over trips the
/// budget: it and every later one in the run is refused (the deletions are held
/// back, the transfers carry on), and a run that tripped says so.
///
/// Every case the data-loss review found that makes a deletion look right when
/// it is not (an unmounted drive, an unreadable folder, a moved pair, ...) has
/// its own guard; this is the net under them for the one nobody thought of, and
/// what bounds the damage when it is the remote that suddenly looks emptied.
///
/// Shared by every worker of a run, so a walk that reconciles many folders at
/// once spends one budget, never one each.
pub struct DeleteBudget {
    allowed: usize,
    tracked: usize,
    used: AtomicUsize,
    /// The files whose deletion was refused, by side: `(local, remote)`. A set,
    /// because a folder that is held back is then descended into, and its files
    /// are refused again one by one - they are still the same files.
    held: Mutex<(HashSet<String>, HashSet<String>)>,
    tripped: AtomicBool,
}

impl DeleteBudget {
    /// The budget of one run for `pair`, whose baseline is `base`. `None` when the
    /// guard stands aside - the run was told the deletion is intended, or the pair
    /// tracks so few files that no run could go over (a run can never delete more
    /// than it tracks).
    pub fn new(cfg: &Config, pair: &Pair, base: &BTreeMap<String, Entry>) -> Option<DeleteBudget> {
        if cfg.allow_mass_delete || !cfg.propagate_deletes {
            return None;
        }
        let tracked = base
            .iter()
            .filter(|(k, e)| !e.is_dir && !pair.is_excluded(k))
            .count();
        let allowed = cfg
            .max_delete_files
            .max(tracked.saturating_mul(cfg.max_delete_percent.min(100) as usize) / 100);
        if allowed >= tracked {
            return None;
        }
        Some(DeleteBudget {
            allowed,
            tracked,
            used: AtomicUsize::new(0),
            held: Mutex::new((HashSet::new(), HashSet::new())),
            tripped: AtomicBool::new(false),
        })
    }

    /// How many files a run may delete.
    pub fn allowed(&self) -> usize {
        self.allowed
    }

    /// How many files the pair tracks.
    pub fn tracked(&self) -> usize {
        self.tracked
    }

    /// Ask to delete `files` files. All or nothing: `false` means hold them back
    /// (and say which, with [`DeleteBudget::hold`]); the budget is then tripped
    /// for the rest of the run.
    pub fn reserve(&self, files: usize) -> bool {
        if files == 0 {
            return true;
        }
        if self.tripped.load(Ordering::SeqCst) {
            return false;
        }
        let mut used = self.used.load(Ordering::SeqCst);
        loop {
            if used + files > self.allowed {
                self.tripped.store(true, Ordering::SeqCst);
                return false;
            }
            match self
                .used
                .compare_exchange(used, used + files, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
    }

    /// Refuse every deletion of the run, from the start (see [`census_missing`]).
    /// What is held back is counted as it is asked for, as ever.
    pub fn trip(&self) {
        self.tripped.store(true, Ordering::SeqCst);
    }

    pub fn tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }

    /// Record the files whose deletion was just refused - those on this computer
    /// and those on Proton Drive - so the user is told what was held and where.
    pub fn hold(
        &self,
        local: impl IntoIterator<Item = String>,
        remote: impl IntoIterator<Item = String>,
    ) {
        let mut held = self.held.lock().unwrap();
        held.0.extend(local);
        held.1.extend(remote);
    }

    /// How many files' deletions were held back, each counted once.
    pub fn held(&self) -> usize {
        let held = self.held.lock().unwrap();
        held.0.len() + held.1.len()
    }

    /// `(remote, local)`: the files held back on Proton Drive, and on this computer.
    pub fn held_sides(&self) -> (usize, usize) {
        let held = self.held.lock().unwrap();
        (held.1.len(), held.0.len())
    }
}

/// How many tracked files look to be missing from `pair`'s local folder, from a
/// bare count of the files there: cheap (one stat walk, no listing of the remote)
/// and exact enough for what it is for - telling, before a full walk deletes a
/// single file, that most of the tree has gone. New local files hide some missing
/// ones, which can only make it trip less.
pub fn census_missing(pair: &Pair, tracked: usize) -> usize {
    let root = &pair.local;
    let present = walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
                .is_none_or(|r| !pair.is_excluded(&r.replace('\\', "/")))
        })
        .flatten()
        .filter(|e| e.file_type().is_file())
        .count();
    tracked.saturating_sub(present)
}

/// The files the deletes in `plan` would remove, as `(local, remote)`: from this
/// computer and from Proton Drive. Each file once, and a folder as the files the
/// baseline holds under it: a whole-tree plan lists both a folder and every file
/// in it, so they are collected into one set per side, not summed.
fn planned_delete_sets(
    plan: &Plan,
    base: &BTreeMap<String, Entry>,
) -> (HashSet<String>, HashSet<String>) {
    let mut local: HashSet<String> = HashSet::new();
    let mut remote: HashSet<String> = HashSet::new();
    for op in plan
        .ops
        .iter()
        .filter(|o| matches!(o.action, Action::DeleteLocal | Action::DeleteRemote))
    {
        let set = if op.action == Action::DeleteLocal {
            &mut local
        } else {
            &mut remote
        };
        if !op.is_dir {
            set.insert(op.path.clone());
            continue;
        }
        let prefix = format!("{}/", op.path);
        for (k, e) in base
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
        {
            if !e.is_dir {
                set.insert(k.clone());
            }
        }
    }
    (local, remote)
}

/// One run's retry-backoff bookkeeping (M4-004), bundled so it travels
/// between `apply()` and `run_downloads()` as a single argument: `state` is
/// this pair's current backoff state going into the run (read-only);
/// `upserts`/`clears` accumulate this run's outcomes for the one persistence
/// call `apply()` makes at the end, alongside the baseline commit.
struct RetryBatch<'a> {
    state: &'a HashMap<String, (u32, i64)>,
    now: i64,
    upserts: &'a mut Vec<(String, u32, i64)>,
    clears: &'a mut Vec<String>,
}

pub struct SyncResult {
    pub pair: String,
    pub applied: usize,
    pub errors: Vec<String>,
    pub plan_summary: String,
    pub was_in_sync: bool,
}

/// Aggregate outcome of syncing a set of pairs. Shared by the CLI, GUI and
/// the watch daemon so they all drive the engine the same way.
pub struct RunSummary {
    pub applied: usize,
    pub errors: usize,
    pub pairs: usize,
}

/// One-shot reconcile of the given pairs with a fresh ProtonCli.
pub fn run_sync(
    cfg: &Config,
    pairs: &[Pair],
    dry_run: bool,
    resync: bool,
    log: &Logger,
) -> RunSummary {
    run_sync_with(cfg, pairs, dry_run, resync, log, None, None)
}

/// Like [`run_sync`], but with an optional structured-event sink and an optional
/// cancellation flag - used by the GUI/service for progress and stop control.
pub fn run_sync_with(
    cfg: &Config,
    pairs: &[Pair],
    dry_run: bool,
    resync: bool,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        if let Some(sink) = events {
            sink.emit(&SyncEvent::Error {
                pair: None,
                text: "proton-drive not found on PATH".into(),
            });
        }
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    let mut engine = Engine::new(cfg, proton, log, dry_run);
    engine.set_observer(events, cancel);
    let (mut applied, mut errors) = (0usize, 0usize);
    for pair in pairs {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        match engine.sync_pair(pair, resync) {
            Ok(r) => {
                applied += r.applied;
                errors += r.errors.len();
            }
            Err(e) => {
                errors += 1;
                log.error(&format!("pair {:?} failed: {e}", pair.name));
                if let Some(sink) = events {
                    sink.emit(&SyncEvent::Error {
                        pair: Some(pair.name.clone()),
                        text: e.to_string(),
                    });
                }
            }
        }
    }
    RunSummary {
        applied,
        errors,
        pairs: pairs.len(),
    }
}

/// Reconcile a single pair restricted to one sub-folder (`scope`, relative to
/// the pair root). Used by the watch daemon so a local change or a hot folder
/// syncs just that sub-tree instead of re-walking the whole pair.
pub fn run_sync_scoped(
    cfg: &Config,
    pair: &Pair,
    scope: &str,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    let mut engine = Engine::new(cfg, proton, log, false);
    engine.set_observer(events, cancel);
    match engine.sync_pair_scoped(pair, false, Some(scope)) {
        Ok(r) => RunSummary {
            applied: r.applied,
            errors: r.errors.len(),
            pairs: 1,
        },
        Err(e) => {
            log.error(&format!("pair {:?} [{scope}] failed: {e}", pair.name));
            if let Some(sink) = events {
                sink.emit(&SyncEvent::Error {
                    pair: Some(pair.name.clone()),
                    text: e.to_string(),
                });
            }
            RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            }
        }
    }
}

/// Folders (POSIX paths relative to the pair root) that hold a local file which
/// is new or changed versus the baseline. Local-only and fast (no network), so
/// the watcher can upload fresh local work first, before the slow remote walk.
/// Excluded sub-trees are skipped. Comparison is size+mtime (conservative: a
/// false positive just triggers a scoped sync that reconciles correctly).
pub fn local_change_folders(cfg: &Config, pair: &Pair) -> Vec<String> {
    let base = crate::state::load_baseline_for(&cfg.state_dir, pair)
        .map(|(b, _)| b)
        .unwrap_or_default();
    let root = &pair.local;
    if !root.exists() {
        return Vec::new();
    }
    let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(
            |e| match e.path().strip_prefix(root).ok().and_then(|p| p.to_str()) {
                Some(r) => !pair.is_excluded(&r.replace('\\', "/")),
                None => true,
            },
        )
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.to_str())
        {
            Some(r) => r.replace('\\', "/"),
            None => continue,
        };
        let changed = match base.get(&rel) {
            None => true, // new local file
            Some(b) => {
                let meta = entry.metadata().ok();
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let mtime = meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                b.size != size || b.mtime != mtime
            }
        };
        if changed {
            let folder = rel
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            folders.insert(folder);
        }
    }
    folders.into_iter().collect()
}

/// Reconcile only the direct children of one folder in a pair (non-recursive).
/// Used by the watch daemon for change-triggered and hot-folder syncs, so a
/// local change touches just that folder's listing rather than re-walking its
/// whole subtree (recursive inotify delivers deeper changes as their own events).
pub fn run_sync_shallow(
    cfg: &Config,
    pair: &Pair,
    folder: &str,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    // Show which folder is being checked (so the banner isn't a generic
    // "Scanning folder pairs" during a single-folder reconcile).
    if let Some(sink) = events {
        sink.emit(&SyncEvent::ScanProgress {
            pair: pair.name.clone(),
            folders: 0,
            current: remote_join(&pair.remote, folder),
        });
    }
    let mut engine = Engine::new(cfg, proton, log, false);
    engine.set_observer(events, cancel);
    match engine.sync_pair_shallow(pair, folder) {
        Ok((r, _children)) => RunSummary {
            applied: r.applied,
            errors: r.errors.len(),
            pairs: 1,
        },
        Err(e) => {
            log.error(&format!("pair {:?} <{folder}> failed: {e}", pair.name));
            if let Some(sink) = events {
                sink.emit(&SyncEvent::Error {
                    pair: Some(pair.name.clone()),
                    text: e.to_string(),
                });
            }
            RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            }
        }
    }
}

/// Streaming full walk: BFS the folder tree and shallow-reconcile each folder as
/// it is discovered, transferring immediately, a bounded number of folders in
/// parallel. Unlike the batch walk (`run_sync`) it does not scan the whole tree
/// before acting, so uploads and downloads overlap the walk. Each folder is
/// reconciled by the same scoped, positive-confirmation shallow primitive, so
/// containment and every data-safety invariant hold per folder. Concurrent
/// workers commit disjoint baseline rows (SQLite serialises the writes).
pub fn run_sync_streaming(
    cfg: &Config,
    pair: &Pair,
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        if let Some(sink) = events {
            sink.emit(&SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "proton-drive not found on PATH".into(),
            });
        }
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    // Refuse a vanished local root (unmounted drive) rather than walk into an
    // empty mountpoint and read every remote file as a deletion.
    if !pair.local.exists() {
        log.error(&format!(
            "local folder {} does not exist — refusing streaming walk",
            pair.local.display()
        ));
        if let Some(sink) = events {
            sink.emit(&SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "local folder missing — refused".into(),
            });
        }
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 1,
        };
    }

    // One shared baseline snapshot for the whole walk. Each folder reads only its
    // own (disjoint) rows from it, and only its own reconcile writes them, so a
    // snapshot taken now stays correct throughout.
    let base = match crate::state::load_baseline_for(&cfg.state_dir, pair) {
        Ok((b, moved)) => {
            note_pair_moved(log, events, pair, moved);
            std::sync::Arc::new(b)
        }
        Err(e) => {
            log.error(&format!("baseline load failed for {:?}: {e}", pair.name));
            return RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            };
        }
    };

    let threads = if cfg.scan_threads > 0 {
        cfg.scan_threads
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8)
    };

    // One deletion allowance for the whole walk. Before it deletes a single file,
    // a count of the files still on disk tells it whether most of the tree has
    // gone (an unmounted drive, an emptied folder): then it deletes nothing.
    let budget = DeleteBudget::new(cfg, pair, &base).map(Arc::new);
    if let Some(b) = &budget {
        census_trip(log, pair, b);
    }

    let applied = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let counter = AtomicUsize::new(0);
    let is_cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));

    // One pair-level scan for the whole walk (the per-folder reconciles are
    // silent at the pair level), so the GUI shows a single stable "scanning …"
    // with a climbing folder count instead of flapping Scanning<->Synced.
    if let Some(sink) = events {
        sink.emit(&SyncEvent::ScanStarted {
            pair: pair.name.clone(),
        });
    }

    let mut frontier: Vec<String> = vec![String::new()]; // start at the pair root
    while !frontier.is_empty() && !is_cancelled() {
        let queue: Mutex<std::collections::VecDeque<String>> =
            Mutex::new(frontier.drain(..).collect());
        let next: Mutex<Vec<String>> = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    // One Engine (and CLI cache) per worker, reused across the
                    // folders it handles this level to amortise CLI startup.
                    let proton = ProtonCli::new(cfg);
                    let mut eng = Engine::new(cfg, proton, log, false);
                    eng.set_observer(events, cancel);
                    eng.set_budget(budget.clone());
                    loop {
                        if is_cancelled() {
                            break;
                        }
                        let folder = {
                            let mut q = queue.lock().unwrap();
                            q.pop_front()
                        };
                        let Some(folder) = folder else { break };
                        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                        if let Some(sink) = events {
                            sink.emit(&SyncEvent::ScanProgress {
                                pair: pair.name.clone(),
                                folders: n,
                                current: remote_join(&pair.remote, &folder),
                            });
                        }
                        match eng.sync_pair_shallow_with_base(pair, &folder, &base) {
                            Ok((r, children)) => {
                                applied.fetch_add(r.applied, Ordering::Relaxed);
                                errors.fetch_add(r.errors.len(), Ordering::Relaxed);
                                next.lock().unwrap().extend(children);
                            }
                            Err(e) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                                log.error(&format!("pair {:?} <{folder}> failed: {e}", pair.name));
                            }
                        }
                    }
                });
            }
        });
        frontier = next.into_inner().unwrap();
        frontier.sort();
        frontier.dedup();
    }

    if !is_cancelled() {
        let _ = crate::state::set_last_synced(&cfg.state_dir, &pair.name, now_epoch());
    }
    let applied = applied.load(Ordering::Relaxed);
    let mut errors = errors.load(Ordering::Relaxed);
    errors += report_if_tripped(log, events, pair, budget.as_deref());
    if let Some(sink) = events {
        sink.emit(&SyncEvent::PairFinished {
            pair: pair.name.clone(),
            applied,
            errors,
            tracked: counter.load(Ordering::Relaxed),
        });
    }
    RunSummary {
        applied,
        errors,
        pairs: 1,
    }
}

/// Reconcile a specific SET of folders in a pair concurrently (shallow, no
/// descent). Used by the watch daemon for the startup local-change / hot phases
/// and change-event bursts, so a batch of folders runs through a worker pool
/// (like the streaming walk) instead of one slow CLI cold-start at a time. Each
/// folder is the same scoped, positive-confirmation shallow reconcile; workers
/// share one read-only baseline snapshot and commit disjoint rows.
pub fn run_sync_shallow_many(
    cfg: &Config,
    pair: &Pair,
    folders: &[String],
    log: &Logger,
    events: Option<&dyn EventSink>,
    cancel: Option<&AtomicBool>,
) -> RunSummary {
    if folders.is_empty() {
        return RunSummary {
            applied: 0,
            errors: 0,
            pairs: 0,
        };
    }
    let proton = ProtonCli::new(cfg);
    if proton.resolve_binary().is_none() {
        log.error("proton-drive not found on PATH");
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 0,
        };
    }
    if !pair.local.exists() {
        log.error(&format!(
            "local folder {} does not exist — refusing shallow batch",
            pair.local.display()
        ));
        return RunSummary {
            applied: 0,
            errors: 1,
            pairs: 1,
        };
    }
    let base = match crate::state::load_baseline_for(&cfg.state_dir, pair) {
        Ok((b, moved)) => {
            note_pair_moved(log, events, pair, moved);
            std::sync::Arc::new(b)
        }
        Err(e) => {
            log.error(&format!("baseline load failed for {:?}: {e}", pair.name));
            return RunSummary {
                applied: 0,
                errors: 1,
                pairs: 1,
            };
        }
    };
    let threads = if cfg.scan_threads > 0 {
        cfg.scan_threads
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8)
    };
    let queue: Mutex<std::collections::VecDeque<String>> =
        Mutex::new(folders.iter().cloned().collect());
    // One deletion allowance for the whole batch of folders.
    let budget = DeleteBudget::new(cfg, pair, &base).map(Arc::new);
    let applied = AtomicUsize::new(0);
    let errors = AtomicUsize::new(0);
    let counter = AtomicUsize::new(0);
    let is_cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    // One pair-level scan for the whole batch (per-folder reconciles are silent
    // at the pair level), so the GUI shows one stable "scanning …" with a
    // climbing folder count rather than flapping per folder.
    if let Some(sink) = events {
        sink.emit(&SyncEvent::ScanStarted {
            pair: pair.name.clone(),
        });
    }
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let proton = ProtonCli::new(cfg);
                let mut eng = Engine::new(cfg, proton, log, false);
                eng.set_observer(events, cancel);
                eng.set_budget(budget.clone());
                loop {
                    if is_cancelled() {
                        break;
                    }
                    let folder = {
                        let mut q = queue.lock().unwrap();
                        q.pop_front()
                    };
                    let Some(folder) = folder else { break };
                    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(sink) = events {
                        sink.emit(&SyncEvent::ScanProgress {
                            pair: pair.name.clone(),
                            folders: n,
                            current: remote_join(&pair.remote, &folder),
                        });
                    }
                    match eng.sync_pair_shallow_with_base(pair, &folder, &base) {
                        Ok((r, _children)) => {
                            applied.fetch_add(r.applied, Ordering::Relaxed);
                            errors.fetch_add(r.errors.len(), Ordering::Relaxed);
                        }
                        Err(e) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                            log.error(&format!("pair {:?} <{folder}> failed: {e}", pair.name));
                        }
                    }
                }
            });
        }
    });
    let applied = applied.load(Ordering::Relaxed);
    let mut errors = errors.load(Ordering::Relaxed);
    errors += report_if_tripped(log, events, pair, budget.as_deref());
    if let Some(sink) = events {
        sink.emit(&SyncEvent::PairFinished {
            pair: pair.name.clone(),
            applied,
            errors,
            tracked: counter.load(Ordering::Relaxed),
        });
    }
    RunSummary {
        applied,
        errors,
        pairs: 1,
    }
}

/// Before a full walk deletes anything: if far more of what the pair tracks is
/// missing from the local folder than a run may delete, refuse every deletion of
/// the run now rather than after the first `allowed` files are gone.
fn census_trip(log: &Logger, pair: &Pair, budget: &DeleteBudget) {
    let missing = census_missing(pair, budget.tracked());
    if missing > budget.allowed() {
        log.warn(&format!(
            "{:?}: {} of the {} files it tracks are not on disk; holding every deletion back",
            pair.name,
            missing,
            budget.tracked()
        ));
        budget.trip();
    }
}

/// If the run's budget tripped, tell the user (once, for the run) and count it
/// as the one problem it is; returns the number of errors to add.
fn report_if_tripped(
    log: &Logger,
    events: Option<&dyn EventSink>,
    pair: &Pair,
    budget: Option<&DeleteBudget>,
) -> usize {
    // Tripped by the census but nothing was asked to be deleted: nothing to say.
    let Some(b) = budget.filter(|b| b.tripped() && b.held() > 0) else {
        return 0;
    };
    let (remote, local) = b.held_sides();
    report_held(
        log,
        events,
        pair,
        Held {
            remote,
            local,
            tracked: b.tracked(),
        },
    );
    1
}

pub struct Engine<'a, R: Remote> {
    cfg: &'a Config,
    remote: R,
    log: &'a Logger,
    dry_run: bool,
    gio: Option<PathBuf>,
    ensured: HashSet<String>,
    events: Option<&'a dyn EventSink>,
    cancel: Option<&'a AtomicBool>,
    /// The mass-deletion guard of the run this engine is part of, shared with
    /// the other workers of a walk. `None`: each reconcile makes its own.
    budget: Option<Arc<DeleteBudget>>,
}

impl<'a, R: Remote> Engine<'a, R> {
    pub fn new(cfg: &'a Config, remote: R, log: &'a Logger, dry_run: bool) -> Self {
        Engine {
            cfg,
            remote,
            log,
            dry_run,
            gio: crate::trash::which_gio(),
            ensured: HashSet::new(),
            events: None,
            cancel: None,
            budget: None,
        }
    }

    /// Share the run's deletion budget with this engine (see [`DeleteBudget`]).
    pub fn set_budget(&mut self, budget: Option<Arc<DeleteBudget>>) {
        self.budget = budget;
    }

    /// Attach a structured-event sink and/or a cancellation flag (used by the
    /// GUI/service). Optional; the CLI leaves both unset.
    pub fn set_observer(
        &mut self,
        events: Option<&'a dyn EventSink>,
        cancel: Option<&'a AtomicBool>,
    ) {
        self.events = events;
        self.cancel = cancel;
    }

    fn emit(&self, ev: SyncEvent) {
        if let Some(sink) = self.events {
            sink.emit(&ev);
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
    }

    /// For every path both sides changed to the same size at different times, hash
    /// the local file when the remote listed a digest, so `decide` can tell a
    /// touch (same bytes) from two different edits (see there). Rare, and the one
    /// case where reading a local file for a comparison is worth it.
    fn digest_equal_sizes(
        &self,
        root: &Path,
        local: &mut BTreeMap<String, Entry>,
        remote: &BTreeMap<String, Entry>,
        ls: &HashMap<String, Change>,
        rs: &HashMap<String, Change>,
    ) {
        let both_changed = |p: &String| {
            matches!(ls.get(p), Some(Change::Created | Change::Modified))
                && matches!(rs.get(p), Some(Change::Created | Change::Modified))
        };
        for (path, le) in local.iter_mut() {
            let Some(re) = remote.get(path) else { continue };
            if le.is_dir
                || re.is_dir
                || le.sha1.is_some()
                || re.sha1.is_none()
                || le.size != re.size
                || le.size == 0
                || !both_changed(path)
            {
                continue;
            }
            if let Ok(rel_path) = safe_join(root, path) {
                le.sha1 = sha1_file(&rel_path);
            }
        }
    }

    /// How long a staging folder must have sat idle to be taken for the
    /// leftovers of a download whose process died: longer than the CLI is allowed
    /// to run (it is killed at `cli_timeout_secs`), so a live download never is.
    fn stale_after(&self) -> Duration {
        Duration::from_secs(self.cfg.cli_timeout_secs).saturating_add(staged::STALE_MARGIN)
    }

    // --- scanning -----------------------------------------------------------
    /// Walk the local tree. In sha1 mode, files whose size+mtime match the
    /// baseline reuse the stored hash instead of being re-read (a
    /// (size,mtime)->sha1 cache), so only changed files are hashed.
    ///
    /// Also returns how many places it could not see into - a folder it was not
    /// allowed to open, a tracked folder that is now a symlink. Whatever is in
    /// them is simply absent from the result, which reads as "deleted"; a caller
    /// that sees a non-zero count must not delete on that basis.
    fn scan_local(
        &self,
        root: &Path,
        baseline: &BTreeMap<String, Entry>,
        pair: &Pair,
        scope: Option<&str>,
    ) -> (BTreeMap<String, Entry>, usize) {
        let mut out = BTreeMap::new();
        let mut blind = 0usize;
        if !root.exists() {
            return (out, blind);
        }
        // A scoped sync walks only `root/<scope>` but still yields paths relative
        // to `root`, so keys line up with the baseline. A missing scoped folder
        // returns empty (the union vs the scoped baseline then handles it).
        let start = match scope {
            Some(s) => root.join(s),
            None => root.to_path_buf(),
        };
        if !start.exists() {
            return (out, blind);
        }
        let want_sha1 = self.cfg.compare == Compare::Sha1;
        // Prune excluded subtrees from the walk itself (`filter_entry` stops
        // descent), so an excluded folder is never even stat-walked locally —
        // matching the remote scan and keeping excludes truly invisible. A
        // staging folder (a download in progress) is one of them; one left by a
        // dead process is cleaned up on the way past.
        let stale = self.stale_after();
        for entry in walkdir::WalkDir::new(&start)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.file_type().is_dir()
                    && e.file_name().to_str().is_some_and(staged::is_staging_name)
                {
                    staged::sweep_if_stale(e.path(), stale);
                    return false;
                }
                match e.path().strip_prefix(root).ok().and_then(|p| p.to_str()) {
                    Some(r) => !pair.is_excluded(&r.replace('\\', "/")),
                    None => true,
                }
            })
        {
            // A folder the walk could not open (permissions, an I/O error) is
            // not an empty folder: say so instead of dropping the error.
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    blind += 1;
                    self.log.warn(&format!(
                        "local scan: cannot read {}: {err}",
                        err.path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ));
                    continue;
                }
            };
            let rel = match entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.to_str())
            {
                Some(r) => r.replace('\\', "/"),
                None => continue,
            };
            let ft = entry.file_type();
            if ft.is_dir() {
                out.insert(
                    rel.clone(),
                    Entry {
                        path: rel,
                        is_dir: true,
                        ..Default::default()
                    },
                );
            } else if ft.is_file() {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    // A transient stat failure (a file locked or renamed
                    // mid-walk, an I/O blip) must NOT make a known file look
                    // deleted. If we synced it before, carry its baseline entry
                    // forward so it stays classified Unchanged and is retried,
                    // not trashed.
                    Err(_) => {
                        if let Some(b) = baseline.get(&rel) {
                            out.insert(rel.clone(), b.clone());
                        }
                        continue;
                    }
                };
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);
                let sha1 = if want_sha1 {
                    // Reuse the cached hash when size+mtime are unchanged.
                    match baseline.get(&rel) {
                        Some(b)
                            if !b.is_dir
                                && b.size == size
                                && b.mtime == mtime
                                && b.sha1.is_some() =>
                        {
                            b.sha1.clone()
                        }
                        _ => sha1_file(entry.path()),
                    }
                } else {
                    None
                };
                out.insert(
                    rel.clone(),
                    Entry {
                        path: rel,
                        is_dir: false,
                        size,
                        mtime,
                        sha1,
                        remote_id: None,
                    },
                );
            } else if let Some(b) = baseline.get(&rel) {
                // A symlink, pipe, socket or device where a tracked file or
                // folder used to be. It is not a deletion: carry a file forward
                // untouched (it is neither uploaded nor trashed), and for a folder
                // - whose contents can no longer be seen - hold back deletions.
                self.log.warn(&format!(
                    "local scan: {rel} is a symlink or special file now, not a regular \
                     {}; left alone",
                    if b.is_dir { "folder" } else { "file" }
                ));
                if b.is_dir {
                    blind += 1;
                } else {
                    out.insert(rel.clone(), b.clone());
                }
            }
        }
        (out, blind)
    }

    /// Returns the remote tree, the relative folders that could not be listed
    /// (empty = a complete scan), and whether the pair's remote base folder
    /// itself was not found (a vanished/moved base, distinct from an empty one).
    fn scan_remote(
        &self,
        pair: &Pair,
        scope: Option<&str>,
    ) -> Result<(BTreeMap<String, Entry>, Vec<String>, bool)> {
        // The concurrent tree walk lives in the Remote impl; here we supply a
        // progress sink (log line + structured ScanProgress), an exclude
        // predicate (so excluded subtrees are never walked), and flatten the
        // (relpath, entry) pairs into a map. When `scope` is set we walk only
        // that remote sub-folder, but re-prefix every result with the scope so
        // keys stay relative to the pair root and line up with the baseline.
        let scoped_base = scope.map(|s| remote_join(&pair.remote, s));
        let base = scoped_base.as_deref().unwrap_or(pair.remote.as_str());
        let pair_name = pair.name.as_str();
        let log = self.log;
        let events = self.events;
        let counter = AtomicUsize::new(0);
        let progress = |p: &str| {
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            log.info(&format!("scanning  {p}"));
            if let Some(sink) = events {
                sink.emit(&SyncEvent::ScanProgress {
                    pair: pair_name.to_string(),
                    folders: n,
                    current: p.to_string(),
                });
            }
        };
        // `list_tree` yields paths relative to `base`; map them back to
        // pair-root-relative before checking excludes and keying the map.
        let full_rel = |under_base: &str| -> String {
            match scope {
                Some(s) => format!("{s}/{under_base}"),
                None => under_base.to_string(),
            }
        };
        let exclude = |rel: &str| pair.is_excluded(&full_rel(rel));
        let scan = self.remote.list_tree(base, &exclude, &progress)?;
        let mut out = BTreeMap::new();
        for (rel, mut e) in scan.entries {
            let key = full_rel(&rel);
            e.path = key.clone();
            out.insert(key, e);
        }
        Ok((out, scan.failed, scan.root_missing))
    }

    // --- planning -----------------------------------------------------------
    /// Builds the plan and the prospective new baseline. Also reports whether
    /// the scan was INCOMPLETE (some folders unreadable, or a side came back
    /// suspiciously empty) and the set of baseline rows to PRUNE because they
    /// now fall under an excluded sub-path. On an incomplete scan the caller
    /// must suppress deletions.
    fn plan(&self, pair: &Pair, resync: bool, scope: Option<&str>) -> Result<PlanOutcome> {
        // Load the baseline first so the local scan can reuse cached hashes. For
        // a scoped sync, restrict the baseline to rows strictly UNDER the scope
        // so nothing outside it is ever seen as missing (and thus deleted) — the
        // scan, reconcile and additive commit all stay within the sub-tree.
        let (full, moved) = crate::state::load_baseline_for(&self.cfg.state_dir, pair)?;
        note_pair_moved(self.log, self.events, pair, moved);
        // The guard counts against everything the pair tracks, however narrow
        // this run's scope: a scoped run is not a licence to delete more.
        let budget = if resync {
            None
        } else {
            DeleteBudget::new(self.cfg, pair, &full)
        };
        let base: BTreeMap<String, Entry> = if resync {
            BTreeMap::new()
        } else {
            match scope {
                Some(s) => {
                    let prefix = format!("{s}/");
                    full.into_iter()
                        .filter(|(k, _)| k.starts_with(&prefix))
                        .collect()
                }
                None => full,
            }
        };
        if base.is_empty() && !resync {
            self.log.info(&format!(
                "No baseline for {:?} yet; establishing one (union of both sides).",
                pair.name
            ));
        }
        self.emit(SyncEvent::ScanStarted {
            pair: pair.name.clone(),
        });

        // A missing local root (typically an unmounted drive) makes every known
        // file look deleted. Refuse the pair outright rather than trash the
        // remote copy or re-download the whole tree into the empty mountpoint.
        if !resync && !base.is_empty() && !pair.local.exists() {
            anyhow::bail!(
                "local folder {} does not exist — refusing to sync {:?} so a missing \
                 mount can't be mistaken for a mass deletion. Restore/remount it and \
                 re-run (use --resync to rebuild the baseline from scratch).",
                pair.local.display(),
                pair.name
            );
        }

        let (local, local_blind) = self.scan_local(&pair.local, &base, pair, scope);
        let (remote, remote_failed, remote_root_missing) = self.scan_remote(pair, scope)?;
        let mut incomplete = !remote_failed.is_empty();
        if local_blind > 0 {
            incomplete = true;
            self.log.warn(&format!(
                "local scan incomplete for {:?}: {local_blind} place(s) could not be read; \
                 syncing without deletions this run.",
                pair.name,
            ));
            self.emit(SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: format!(
                    "{} couldn't be read here — synced without deletions, will retry",
                    crate::datefmt::counted(local_blind, "folder", "folders")
                ),
            });
        }
        // The pair root itself came back with nothing in it while the baseline
        // knows files: an unmounted drive's mount point, a folder emptied by
        // mistake, a restore that has not finished. Whatever it is, "every file
        // was deleted" is the least likely reading, and the cost of being wrong
        // is the remote copy: hold deletions back, and say why.
        let tracked_files = base
            .iter()
            .filter(|(k, e)| !e.is_dir && !pair.is_excluded(k))
            .count();
        if scope.is_none() && !resync && local.is_empty() && tracked_files >= EMPTY_ROOT_MIN_FILES {
            incomplete = true;
            self.log.warn(&format!(
                "the local folder of {:?} is empty but {tracked_files} file(s) are on record; \
                 syncing without deletions. If the drive is unmounted, mount it; if you \
                 emptied the folder on purpose, delete those files on Proton Drive too.",
                pair.name,
            ));
            self.emit(SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "local folder is empty — synced without deletions (unmounted drive?)".into(),
            });
        }
        if incomplete {
            self.log.warn(&format!(
                "remote scan incomplete for {:?}: {} folder(s) could not be listed; \
                 syncing without deletions this run. Unreadable: {:?}",
                pair.name,
                remote_failed.len(),
                remote_failed,
            ));
            self.emit(SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: format!(
                    "{} couldn't be listed — synced without deletions, will retry",
                    crate::datefmt::counted(remote_failed.len(), "folder", "folders")
                ),
            });
        }

        // The pair's remote base folder vanishing while a baseline exists is a
        // catastrophe signature (base deleted or moved on another device). An
        // empty-but-present base is a legitimate "all remote files deleted" and
        // is left to propagate; only a genuinely missing base is guarded. Sync
        // additively but suppress deletions so it can't be mirrored as a mass
        // local delete — local files simply re-upload and rebuild the base.
        if !resync && !base.is_empty() && remote_root_missing {
            incomplete = true;
            self.log.warn(&format!(
                "remote base folder for {:?} was not found but {} path(s) are on \
                 record; syncing without deletions this run to avoid a mass delete. \
                 If you moved or removed it deliberately, re-run with --resync.",
                pair.name,
                base.len()
            ));
            self.emit(SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "remote base folder missing — synced without deletions to avoid data loss"
                    .into(),
            });
        }

        if !pair.exclude.is_empty() {
            self.log.info(&format!(
                "excluding {} sub-path(s) from {:?}: {:?}",
                pair.exclude.len(),
                pair.name,
                pair.exclude
            ));
        }

        let cmp = self.cfg.compare;
        let mut local = local;
        let ls = classify(&local, &base, cmp);
        let rs = classify(&remote, &base, cmp);
        self.digest_equal_sizes(&pair.local, &mut local, &remote, &ls, &rs);

        let mut plan = Plan::default();
        let mut new_base: BTreeMap<String, Entry> = BTreeMap::new();

        let mut keys: Vec<&String> = local
            .keys()
            .chain(remote.keys())
            .chain(base.keys())
            .collect();
        keys.sort();
        keys.dedup();

        for path in keys {
            // Excluded sub-paths are invisible to the engine: no upload, no
            // download, and crucially no delete on either side. They are not
            // added to new_base, and their old baseline rows are pruned (below)
            // so a later re-include can only ever re-download, never delete.
            if pair.is_excluded(path) {
                continue;
            }
            let lc = *ls.get(path).unwrap_or(&Change::Absent);
            let rc = *rs.get(path).unwrap_or(&Change::Absent);
            self.decide(
                path,
                lc,
                rc,
                local.get(path),
                remote.get(path),
                base.get(path),
                &mut plan,
                &mut new_base,
            );
        }

        // Baseline rows now under an excluded sub-path are forgotten. This is a
        // pure DB cleanup (no filesystem or remote effect) that makes re-include
        // behave like a fresh folder — union/redownload, never a delete.
        let prune_excluded: HashSet<String> = base
            .keys()
            .filter(|k| pair.is_excluded(k))
            .cloned()
            .collect();

        // Collapse delete+create pairs of identical content into a single
        // rename/move (keeps large files from being re-uploaded on rename).
        // Skip on an incomplete scan: a "missing" source may just be in an
        // unlisted folder, so a rename could wrongly move a remote file.
        if !incomplete {
            detect_renames(&mut plan, &base, &local, &remote, &pair.local);
        }
        // The mass-deletion guard: the whole plan is known here, so it is judged
        // as a whole - over budget, none of its deletions happen.
        let mut held = None;
        if let Some(b) = &budget {
            let (local_files, remote_files) = planned_delete_sets(&plan, &base);
            if !b.reserve(local_files.len() + remote_files.len()) {
                plan.ops
                    .retain(|op| !matches!(op.action, Action::DeleteRemote | Action::DeleteLocal));
                held = Some(Held {
                    remote: remote_files.len(),
                    local: local_files.len(),
                    tracked: b.tracked(),
                });
            }
        }
        Ok((plan, new_base, incomplete, prune_excluded, held))
    }

    #[allow(clippy::too_many_arguments)]
    fn decide(
        &self,
        path: &str,
        lc: Change,
        rc: Change,
        le: Option<&Entry>,
        re: Option<&Entry>,
        be: Option<&Entry>,
        plan: &mut Plan,
        new_base: &mut BTreeMap<String, Entry>,
    ) {
        let is_dir = le.or(re).or(be).map(|e| e.is_dir).unwrap_or(false);

        // Both unchanged -> keep as-is.
        if lc == Change::Unchanged && rc == Change::Unchanged {
            if let Some(e) = le.or(re) {
                new_base.insert(path.to_string(), e.clone());
            }
            return;
        }

        // Type clash: the path is a file on one side and a directory on the
        // other. The file/dir merge logic can't reconcile that, so surface it
        // and skip — never silently leave the two sides diverged or advance the
        // baseline to the wrong type. Leaving the baseline row untouched means
        // it keeps being flagged until the user removes or renames one side.
        if let (Some(a), Some(b)) = (le, re) {
            if a.is_dir != b.is_dir {
                let msg = format!(
                    "{path}: type mismatch — file on one side, directory on the \
                     other; skipped. Remove or rename one side to resolve."
                );
                self.log.warn(&msg);
                self.emit(SyncEvent::Error {
                    pair: None,
                    text: msg,
                });
                return;
            }
        }

        if is_dir {
            self.decide_dir(path, lc, rc, le, re, plan, new_base);
            return;
        }

        let same_now =
            matches!((le, re), (Some(a), Some(b)) if same_content(a, b, self.cfg.compare));

        use Change::*;
        // one side changed, the other did not
        if matches!(lc, Created | Modified) && rc == Unchanged {
            self.emit_upload(path, le, plan, new_base, lc.label());
            return;
        }
        if matches!(rc, Created | Modified) && lc == Unchanged {
            self.emit_download(path, re, le, plan, new_base, rc.label());
            return;
        }
        // one side created, other absent
        if lc == Created && rc == Absent {
            self.emit_upload(path, le, plan, new_base, "new local");
            return;
        }
        if rc == Created && lc == Absent {
            self.emit_download(path, re, le, plan, new_base, "new remote");
            return;
        }
        // both created / both modified
        if matches!(lc, Created | Modified) && matches!(rc, Created | Modified) {
            if same_now {
                if let Some(e) = le {
                    new_base.insert(path.to_string(), e.clone());
                }
                return;
            }
            // Same byte size but different mtimes. A touch on both sides (or Proton's
            // claimedModificationTime against a local clock) changes the time and
            // not the bytes - but so does editing a number, a flag or a digit on
            // both sides, and the two cannot be told apart by size. Taking it for
            // drift (as this once did, to stop conflict copies multiplying) left
            // the two files different with a baseline saying they agreed: the next
            // run saw the remote's time differ from the baseline's, downloaded it
            // over the local file, and the local edit was gone from both sides.
            // Only a digest settles it: when both sides have one and they match,
            // it is drift and the baseline is refreshed; in every other case
            // (different digests, or none to compare) both versions are kept.
            // The local digest of such a file is filled in before this runs
            // (`digest_equal_sizes`), so it is there whenever the remote listed one.
            if let (Some(a), Some(b)) = (le, re) {
                if a.size == b.size && a.size > 0 {
                    let proven_same = matches!(
                        (&a.sha1, &b.sha1),
                        (Some(x), Some(y)) if x == y
                    );
                    if proven_same {
                        if let Some(e) = le {
                            new_base.insert(path.to_string(), e.clone());
                        }
                        return;
                    }
                }
            }
            self.emit_conflict(path, le, re, plan, new_base);
            return;
        }
        // deletions
        if lc == Deleted && rc == Unchanged {
            self.emit_delete_remote(path, re, plan, new_base);
            return;
        }
        if rc == Deleted && lc == Unchanged {
            self.emit_delete_local(path, le, plan, new_base);
            return;
        }
        if lc == Deleted && rc == Deleted {
            return; // gone on both sides
        }
        // delete-vs-change: never lose the surviving edit
        if lc == Deleted && rc == Modified {
            self.emit_download(
                path,
                re,
                le,
                plan,
                new_base,
                "deleted locally but modified remotely; keeping remote",
            );
            return;
        }
        if rc == Deleted && lc == Modified {
            self.emit_upload(
                path,
                le,
                plan,
                new_base,
                "deleted remotely but modified locally; keeping local",
            );
            return;
        }
        // fallback: keep whatever exists, nothing destructive
        if let Some(e) = le.or(re) {
            new_base.insert(path.to_string(), e.clone());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn decide_dir(
        &self,
        path: &str,
        lc: Change,
        rc: Change,
        le: Option<&Entry>,
        re: Option<&Entry>,
        plan: &mut Plan,
        new_base: &mut BTreeMap<String, Entry>,
    ) {
        use Change::*;
        if lc == Created && re.is_none() {
            plan.add(Op::new(Action::MkdirRemote, path, true, "new local dir"));
            if let Some(e) = le {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if rc == Created && le.is_none() {
            plan.add(Op::new(Action::MkdirLocal, path, true, "new remote dir"));
            if let Some(e) = re {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if lc == Deleted && rc == Unchanged {
            if self.cfg.propagate_deletes {
                plan.add(Op::new(
                    Action::DeleteRemote,
                    path,
                    true,
                    "dir removed locally",
                ));
            } else if let Some(e) = re {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if rc == Deleted && lc == Unchanged {
            if self.cfg.propagate_deletes {
                plan.add(Op::new(
                    Action::DeleteLocal,
                    path,
                    true,
                    "dir removed remotely",
                ));
            } else if let Some(e) = le {
                new_base.insert(path.to_string(), e.clone());
            }
        } else if let Some(e) = le.or(re) {
            new_base.insert(path.to_string(), e.clone());
        }
    }

    fn emit_upload(
        &self,
        path: &str,
        e: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
        reason: &str,
    ) {
        plan.add(Op::new(Action::Upload, path, false, reason));
        if let Some(e) = e {
            nb.insert(path.to_string(), e.clone());
        }
    }

    /// Plan a download of `e` (the remote entry). `seen` is the local entry the
    /// scan found at the path, so the file is only replaced if the user has not
    /// touched it by the time the download lands.
    fn emit_download(
        &self,
        path: &str,
        e: Option<&Entry>,
        seen: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
        reason: &str,
    ) {
        plan.add(Op::new(Action::Download, path, false, reason).seeing(LocalSeen::of(seen)));
        if let Some(e) = e {
            nb.insert(path.to_string(), e.clone());
        }
    }

    fn emit_conflict(
        &self,
        path: &str,
        le: Option<&Entry>,
        re: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        match self.cfg.conflict {
            ConflictPolicy::Skip => {
                plan.add(
                    Op::new(Action::Noop, path, false, "conflict skipped (both changed)")
                        .settling(ConflictResolution::Skipped),
                );
            }
            ConflictPolicy::Newer => match (le.and_then(|e| e.mtime), re.and_then(|e| e.mtime)) {
                (Some(lm), Some(rm)) if lm >= rm => {
                    self.emit_upload(path, le, plan, nb, "conflict: local newer");
                    settle_last(plan, ConflictResolution::KeptLocal);
                }
                (Some(_), Some(_)) => {
                    self.emit_download(path, re, le, plan, nb, "conflict: remote newer");
                    settle_last(plan, ConflictResolution::KeptRemote);
                }
                _ => self.emit_conflict_keep_both(path, re, plan, nb),
            },
            ConflictPolicy::KeepBoth => self.emit_conflict_keep_both(path, re, plan, nb),
        }
    }

    fn emit_conflict_keep_both(
        &self,
        path: &str,
        re: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        plan.add(Op::new(
            Action::Conflict,
            path,
            false,
            "both sides changed; keeping both",
        ));
        if let Some(e) = re {
            nb.insert(path.to_string(), e.clone()); // original name holds remote version
        }
    }

    fn emit_delete_remote(
        &self,
        path: &str,
        re: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        if self.cfg.propagate_deletes {
            plan.add(Op::new(
                Action::DeleteRemote,
                path,
                false,
                "removed locally",
            ));
        } else {
            plan.add(Op::new(
                Action::Noop,
                path,
                false,
                "removed locally; delete not propagated",
            ));
            if let Some(e) = re {
                nb.insert(path.to_string(), e.clone());
            }
        }
    }

    fn emit_delete_local(
        &self,
        path: &str,
        le: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
    ) {
        if self.cfg.propagate_deletes {
            plan.add(Op::new(
                Action::DeleteLocal,
                path,
                false,
                "removed remotely",
            ));
        } else {
            plan.add(Op::new(
                Action::Noop,
                path,
                false,
                "removed remotely; delete not propagated",
            ));
            if let Some(e) = le {
                nb.insert(path.to_string(), e.clone());
            }
        }
    }

    // --- execution ----------------------------------------------------------
    /// Returns the result and the set of paths whose delete/rename actually
    /// COMPLETED this run (a subset of what was planned — cancelled or failed
    /// ops are absent), so callers can act only on confirmed deletions.
    fn apply(
        &mut self,
        pair: &Pair,
        plan: Plan,
        mut new_base: BTreeMap<String, Entry>,
        prune_excluded: HashSet<String>,
    ) -> (SyncResult, HashSet<String>) {
        let mut result = SyncResult {
            pair: pair.name.clone(),
            applied: 0,
            errors: Vec::new(),
            plan_summary: plan.summary(),
            was_in_sync: plan.is_empty(),
        };

        let mut ops: Vec<Op> = plan.ops;
        ops.sort_by_key(order_key);

        // Downloads are independent (each self-creates its parent dir and only
        // reads the baseline), so they run through a concurrent pool. Everything
        // else — mkdirs, uploads, conflicts, renames, deletes — stays on this
        // thread in order (conflicts/renames mutate the new baseline).
        let (downloads, rest): (Vec<Op>, Vec<Op>) =
            ops.into_iter().partition(|o| o.action == Action::Download);

        let needs_base = rest.iter().any(|o| {
            matches!(
                o.action,
                Action::Upload | Action::MkdirRemote | Action::Conflict
            )
        });
        if needs_base && !self.dry_run {
            if let Err(e) = self.ensure_remote_base(pair) {
                let msg = format!("remote base {}: {e}", pair.remote);
                self.log.error(&msg);
                result.errors.push(msg);
                return (result, HashSet::new());
            }
        }

        // Per-file bookkeeping for the additive commit, by POSITIVE
        // confirmation: a row that `decide()` added to `new_base` at plan time
        // is provisional until its op actually completes. `pending` is every
        // such op's path (deletes/no-ops add no row — deletes drop theirs via
        // `deleted_ok` — so they're excluded); `done` collects the ones that
        // succeed. Any pending row NOT in `done` at commit time — a FAILED
        // transfer OR an op the run never reached because it was CANCELLED — is
        // dropped, so its previous baseline row is left exactly as it was (or
        // stays absent for a brand-new file) and it is re-detected and retried
        // next run. This stops a partial/cancelled run from recording a
        // never-transferred file as 'synced', which the next run would otherwise
        // read as a deletion and propagate.
        let pending: HashSet<String> = downloads
            .iter()
            .chain(rest.iter())
            .filter(|o| {
                !matches!(
                    o.action,
                    Action::DeleteRemote | Action::DeleteLocal | Action::Noop
                )
            })
            .map(|o| o.path.clone())
            .collect();
        let mut done: HashSet<String> = HashSet::new();
        let mut deleted_ok: HashSet<String> = HashSet::new();

        // Retry backoff (M4-004): a path that failed recently is skipped this
        // run rather than retried immediately - it stays `pending` (untouched
        // by this run) and is picked up again once its backoff window passes.
        // `retry_upserts`/`retry_clears` collect this run's outcomes for the
        // one persistence call at the end, alongside the baseline commit.
        let now = now_epoch();
        let retry_state =
            crate::state::retry_state(&self.cfg.state_dir, &pair.name).unwrap_or_default();
        let in_backoff = |path: &str| {
            retry_state
                .get(path)
                .is_some_and(|&(_, next_at)| next_at > now)
        };
        let mut retry_upserts: Vec<(String, u32, i64)> = Vec::new();
        let mut retry_clears: Vec<String> = Vec::new();

        for op in &rest {
            if self.cancelled() {
                self.log
                    .warn("cancelled; stopping before the next operation");
                self.emit(SyncEvent::Info {
                    text: "cancelled".into(),
                });
                break;
            }
            self.log.info(&op.describe());
            if op.action == Action::Noop {
                // `conflict = "skip"` leaves both sides alone, but the user has
                // to hear about it: it never sorts itself out.
                if let (Some(res), false) = (op.conflict, self.dry_run) {
                    self.report_conflict(pair, &op.path, res, None);
                }
                continue;
            }
            if self.dry_run {
                continue;
            }
            if in_backoff(&op.path) {
                self.log.info(&format!(
                    "skipping {} {} — in retry backoff",
                    op.action.label(),
                    op.path
                ));
                continue;
            }
            // A folder goes with everything in it. The files inside were judged
            // one by one when the plan was made - or, in a shallow reconcile, not
            // at all, since only the folder's own listing is looked at. So before
            // removing one, check that nothing in it is a file the sync has not
            // seen: a new file, an edit, something from another device. If there
            // is, the folder stays; its files reconcile on their own and the
            // folder is removed on a later run if it is then empty of news.
            if op.is_dir && matches!(op.action, Action::DeleteLocal | Action::DeleteRemote) {
                if let Some(why) = self.folder_delete_hazard(pair, op) {
                    let (where_, gone_on) = if op.action == Action::DeleteLocal {
                        ("here", "Proton Drive")
                    } else {
                        ("on Proton Drive", "this computer")
                    };
                    let msg = format!(
                        "kept the folder {} {where_}: it was removed on {gone_on}, but {why}",
                        op.path
                    );
                    self.log.warn(&msg);
                    self.emit(SyncEvent::Info { text: msg });
                    continue;
                }
            }
            self.emit(SyncEvent::OpStarted {
                pair: pair.name.clone(),
                action: op.action.label().to_string(),
                path: op.path.clone(),
            });
            let outcome = self.apply_op(pair, op, &mut new_base);
            let ok = outcome.is_ok();
            // The bare cause, without the "<action> <path>: " prefix the log
            // line carries: the event already names the action and the path.
            let err_text = outcome.as_ref().err().map(|e| e.to_string());
            match outcome {
                Ok(()) => {
                    result.applied += 1;
                    match op.action {
                        Action::DeleteRemote | Action::DeleteLocal => {
                            deleted_ok.insert(op.path.clone());
                        }
                        Action::RenameRemote | Action::RenameLocal => {
                            if let Some(from) = &op.from {
                                deleted_ok.insert(from.clone());
                            }
                        }
                        _ => {}
                    }
                    // Confirm this op's provisional baseline row (harmless for
                    // deletes/no-ops, which aren't in `pending`).
                    done.insert(op.path.clone());
                    retry_clears.push(op.path.clone());
                    if let Some(res) = op.conflict {
                        self.report_conflict(pair, &op.path, res, None);
                    }
                }
                Err(e) => {
                    let msg = format!("{} {}: {e}", op.action.label(), op.path);
                    self.log.error(&msg);
                    result.errors.push(msg);
                    let attempts = retry_state.get(&op.path).map(|&(a, _)| a).unwrap_or(0) + 1;
                    let next_at = backoff_next_at(self.cfg, &op.path, attempts, now);
                    retry_upserts.push((op.path.clone(), attempts, next_at));
                }
            }
            self.emit(SyncEvent::OpFinished {
                pair: pair.name.clone(),
                action: op.action.label().to_string(),
                path: op.path.clone(),
                ok,
                error: err_text,
            });
        }

        // Concurrent download phase.
        if !downloads.is_empty() && !self.cancelled() {
            if self.dry_run {
                for op in &downloads {
                    self.log.info(&op.describe());
                }
            } else {
                let mut retry = RetryBatch {
                    state: &retry_state,
                    now,
                    upserts: &mut retry_upserts,
                    clears: &mut retry_clears,
                };
                self.run_downloads(
                    pair,
                    &downloads,
                    &new_base,
                    &mut result,
                    &mut done,
                    &mut retry,
                );
                for op in &downloads {
                    if let Some(res) = op.conflict {
                        if done.contains(&op.path) {
                            self.report_conflict(pair, &op.path, res, None);
                        }
                    }
                }
            }
        }

        // Positive-confirmation commit: a provisional row survives ONLY if its
        // op completed this run. Drop every pending row not in `done` — a failed
        // transfer OR an op skipped because the run was cancelled before reaching
        // it — so its previous baseline row survives untouched (or stays absent
        // for a brand-new file). Next run re-detects the change and retries it in
        // the correct direction — a timeout or a cancel never invalidates the
        // rest of the sync, and a never-transferred file is never recorded as
        // synced (which the next run would read as a deletion).
        for rel in &pending {
            if !done.contains(rel) {
                new_base.remove(rel);
            }
        }

        // Additive per-file commit: only files that fully synced this run are
        // written (as 'synced'); unseen rows are left intact, so a partial or
        // interrupted run still persists exactly the files that did complete.
        if !self.dry_run {
            // Forget baseline rows now under an excluded sub-path (pure DB
            // cleanup, no filesystem or remote effect) so a later re-include
            // re-downloads rather than propagating a delete.
            if !prune_excluded.is_empty() {
                self.log.info(&format!(
                    "pruning {} baseline row(s) now under an excluded path for {:?}",
                    prune_excluded.len(),
                    pair.name
                ));
                deleted_ok.extend(prune_excluded);
            }
            if let Err(e) = crate::state::commit_baseline(
                &self.cfg.state_dir,
                &pair.name,
                &new_base,
                &deleted_ok,
            ) {
                self.log.warn(&format!(
                    "could not commit baseline for {:?}: {e}",
                    pair.name
                ));
            }
            if let Err(e) = crate::state::apply_retry_updates(
                &self.cfg.state_dir,
                &pair.name,
                &retry_upserts,
                &retry_clears,
            ) {
                self.log.warn(&format!(
                    "could not update retry backoff state for {:?}: {e}",
                    pair.name
                ));
            }
            let retrying = pending.iter().filter(|r| !done.contains(*r)).count();
            if retrying > 0 {
                self.log.warn(&format!(
                    "{retrying} file(s) not completed for {:?} (failed or cancelled); \
                     they retry next run.",
                    pair.name
                ));
            }
            let _ = crate::state::set_last_synced(
                &self.cfg.state_dir,
                &pair.name,
                crate::datefmt::now_epoch(),
            );
        }
        (result, deleted_ok)
    }

    /// Run the download ops through the backend's concurrent pool, emitting the
    /// same per-op events as the sequential path so the GUI shows every file in
    /// flight. Reads `new_base` for target mtimes only (no mutation), so it's
    /// safe to share across the worker threads.
    fn run_downloads(
        &self,
        pair: &Pair,
        downloads: &[Op],
        new_base: &BTreeMap<String, Entry>,
        result: &mut SyncResult,
        done_down: &mut HashSet<String>,
        retry: &mut RetryBatch,
    ) {
        let mut jobs: Vec<DownloadJob> = Vec::new();
        for op in downloads {
            // Retry backoff (M4-004): a path that failed recently is skipped
            // this run; it stays `pending` and is picked up again once its
            // backoff window passes.
            if retry
                .state
                .get(&op.path)
                .is_some_and(|&(_, next_at)| next_at > retry.now)
            {
                self.log
                    .info(&format!("skipping download {} — in retry backoff", op.path));
                continue;
            }
            // Guard the destination against a remote-controlled name escaping
            // the sync root; skip (don't abort the batch) anything unsafe.
            let local_full = match safe_join_to_write(&pair.local, &op.path) {
                Ok(p) => p,
                Err(e) => {
                    let msg = e.to_string();
                    self.log.error(&msg);
                    result.errors.push(msg);
                    continue;
                }
            };
            let dest_dir = local_full
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| pair.local.clone());
            let Some(name) = local_full
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
            else {
                let msg = format!("download {}: no file name", op.path);
                self.log.error(&msg);
                result.errors.push(msg);
                continue;
            };
            // The CLI writes into a private staging folder next to the final
            // location, never into it (see `staged`); the folder is created when
            // the job starts, so only the running ones exist at any moment.
            jobs.push(DownloadJob {
                rel: op.path.clone(),
                remote_path: remote_join(&pair.remote, &op.path),
                dest_dir: staged::plan_dir(&dest_dir).to_string_lossy().into_owned(),
                final_dir: dest_dir.to_string_lossy().into_owned(),
                name,
                mtime: new_base.get(&op.path).and_then(|e| e.mtime),
                size: new_base.get(&op.path).map(|e| e.size).unwrap_or(0),
                local_seen: op.local_seen.unwrap_or(LocalSeen::Absent),
            });
        }

        let events = self.events;
        let log = self.log;
        let cfg = self.cfg;
        let pair_name = pair.name.as_str();
        let applied = AtomicUsize::new(0);
        let errs: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let done_rels: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let failed_rels: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let fallback = AtomicBool::new(false);
        let cancel_ref: &AtomicBool = self.cancel.unwrap_or(&fallback);

        let on_start = |j: &DownloadJob| {
            // Bandwidth limit (M4-005): blocks until it's this job's turn.
            cfg.download_limiter
                .acquire(j.size, cfg.download_limit_kbps);
            log.info(&format!("download    file {} (new remote)", j.rel));
            if let Err(e) = std::fs::create_dir_all(&j.final_dir)
                .and_then(|()| std::fs::create_dir(&j.dest_dir))
            {
                // The download will fail on its own and say so; this says why.
                log.error(&format!(
                    "download {}: cannot create a staging folder next to it: {e}",
                    j.rel
                ));
            }
            if let Some(s) = events {
                s.emit(&SyncEvent::OpStarted {
                    pair: pair_name.to_string(),
                    action: "download".into(),
                    path: j.rel.clone(),
                });
            }
        };
        let on_done = |j: &DownloadJob, r: std::result::Result<(), String>| {
            // Whatever the CLI wrote is in the staging folder. Only a download
            // that arrived whole, and found the local file as the plan left it,
            // is moved into place; the folder goes either way.
            let staging = staged::Staging::adopt(Path::new(&j.dest_dir));
            let r = r.and_then(|()| {
                staging
                    .install(
                        &j.name,
                        &Expect {
                            size: j.size,
                            mtime: j.mtime,
                            local: j.local_seen,
                        },
                    )
                    .map_err(|e| e.to_string())
            });
            drop(staging);
            let ok = r.is_ok();
            let err_text = r.as_ref().err().cloned();
            match r {
                Ok(()) => {
                    applied.fetch_add(1, Ordering::Relaxed);
                    done_rels.lock().unwrap().push(j.rel.clone());
                }
                Err(e) => {
                    let msg = format!("download {}: {e}", j.rel);
                    log.error(&msg);
                    errs.lock().unwrap().push(msg);
                    failed_rels.lock().unwrap().push(j.rel.clone());
                }
            }
            if let Some(s) = events {
                s.emit(&SyncEvent::OpFinished {
                    pair: pair_name.to_string(),
                    action: "download".into(),
                    path: j.rel.clone(),
                    ok,
                    error: err_text,
                });
            }
        };

        self.remote.download_many(
            &jobs,
            self.cfg.download_threads,
            cancel_ref,
            &on_start,
            &on_done,
        );

        result.applied += applied.load(Ordering::Relaxed);
        result.errors.extend(errs.into_inner().unwrap());
        let done_rels = done_rels.into_inner().unwrap();
        retry.clears.extend(done_rels.iter().cloned());
        done_down.extend(done_rels);
        for rel in failed_rels.into_inner().unwrap() {
            let attempts = retry.state.get(&rel).map(|&(a, _)| a).unwrap_or(0) + 1;
            let next_at = backoff_next_at(self.cfg, &rel, attempts, retry.now);
            retry.upserts.push((rel, attempts, next_at));
        }
    }

    fn apply_op(
        &mut self,
        pair: &Pair,
        op: &Op,
        new_base: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        let local_full = safe_join_to_write(&pair.local, &op.path)?;
        let remote_full = remote_join(&pair.remote, &op.path);
        match op.action {
            Action::MkdirRemote => self.ensure_remote_dir(&pair.remote, &op.path)?,
            Action::MkdirLocal => {
                std::fs::create_dir_all(&local_full)?;
            }
            Action::Upload => {
                let parent = parent_rel(&op.path);
                self.ensure_remote_dir(&pair.remote, parent)?;
                // Bandwidth limit (M4-005): pace against the real on-disk
                // size, not a possibly-stale scanned value.
                let size = std::fs::metadata(&local_full).map(|m| m.len()).unwrap_or(0);
                self.cfg
                    .upload_limiter
                    .acquire(size, self.cfg.upload_limit_kbps);
                self.remote.upload(
                    &local_full.to_string_lossy(),
                    &remote_join(&pair.remote, parent),
                )?;
            }
            Action::Download => {
                let want = new_base.get(&op.path);
                self.fetch_into_place(
                    pair,
                    &op.path,
                    &Expect {
                        size: want.map(|e| e.size).unwrap_or(0),
                        mtime: want.and_then(|e| e.mtime),
                        local: op.local_seen.unwrap_or(LocalSeen::Absent),
                    },
                )?;
            }
            Action::DeleteRemote => self.remote.trash(&remote_full)?,
            Action::DeleteLocal => self.delete_local(&local_full, op.is_dir)?,
            Action::RenameRemote => {
                let from = op.from.as_deref().unwrap_or(&op.path);
                self.remote
                    .rename(&remote_join(&pair.remote, from), &remote_full)?;
            }
            Action::RenameLocal => {
                let from = op.from.as_deref().unwrap_or(&op.path);
                let from_full = safe_join(&pair.local, from)?;
                if let Some(parent) = local_full.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&from_full, &local_full)?;
                if let Some(e) = new_base.get(&op.path) {
                    match_mtime(&local_full, e.mtime);
                }
            }
            Action::Conflict => self.resolve_conflict(pair, &op.path, new_base)?,
            Action::Noop => {}
        }
        Ok(())
    }

    /// Why removing the folder `op.path` - which the other side no longer has -
    /// would take something the sync has not seen, or `None` if everything in it
    /// is a file the baseline knows and that is unchanged since. Checked against
    /// the stored baseline, which only ever holds what completed, so it errs on
    /// the side of keeping the folder. Anything it cannot establish counts as
    /// unsafe.
    fn folder_delete_hazard(&self, pair: &Pair, op: &Op) -> Option<String> {
        let base = match crate::state::load_baseline(&self.cfg.state_dir, &pair.name) {
            Ok(b) => b,
            Err(_) => return Some("this pair's record can't be read".to_string()),
        };
        let prefix = format!("{}/", op.path);
        let cmp = self.cfg.compare;
        // An exclude inside the folder is invisible to the sync and must never be
        // deleted; removing the folder would take it.
        if let Some(ex) = pair.exclude.iter().find(|e| e.starts_with(&prefix)) {
            return Some(format!("{ex} inside it is excluded from sync"));
        }
        // What `rel`, found with this size and time, says about being safe to remove.
        let judge = |rel: &str, found: &Entry, side: &str| -> Option<String> {
            match base.get(rel) {
                Some(b) if !b.is_dir && same_content(found, b, cmp) => None,
                Some(_) => Some(format!("{rel} was changed {side} since the last sync")),
                None => Some(format!("{rel} is new {side} and has not synced")),
            }
        };
        match op.action {
            Action::DeleteLocal => {
                let root = match safe_join(&pair.local, &op.path) {
                    Ok(p) => p,
                    Err(e) => return Some(e.to_string()),
                };
                for entry in walkdir::WalkDir::new(&root)
                    .min_depth(1)
                    .follow_links(false)
                {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => return Some(format!("part of it can't be read ({e})")),
                    };
                    let Some(under) = entry
                        .path()
                        .strip_prefix(&root)
                        .ok()
                        .and_then(|p| p.to_str())
                    else {
                        return Some(
                            "it holds a file with a name that isn't valid text".to_string(),
                        );
                    };
                    let rel = format!("{prefix}{}", under.replace('\\', "/"));
                    if pair.is_excluded(&rel) {
                        continue;
                    }
                    let ft = entry.file_type();
                    if ft.is_dir() {
                        continue;
                    }
                    if !ft.is_file() {
                        return Some(format!("{rel} is a symlink or special file"));
                    }
                    let Ok(meta) = entry.metadata() else {
                        return Some(format!("{rel} can't be read"));
                    };
                    let found = Entry {
                        path: rel.clone(),
                        is_dir: false,
                        size: meta.len(),
                        mtime: meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64),
                        sha1: None,
                        remote_id: None,
                    };
                    if let Some(why) = judge(&rel, &found, "here") {
                        return Some(why);
                    }
                }
                None
            }
            Action::DeleteRemote => {
                let scan = match self.remote.list_tree(
                    &remote_join(&pair.remote, &op.path),
                    &|_| false,
                    &|_| {},
                ) {
                    Ok(s) => s,
                    Err(e) => return Some(format!("it can't be listed ({e})")),
                };
                if !scan.failed.is_empty() {
                    return Some("part of it can't be listed".to_string());
                }
                for (under, found) in &scan.entries {
                    let rel = format!("{prefix}{under}");
                    if found.is_dir || pair.is_excluded(&rel) {
                        continue;
                    }
                    if let Some(why) = judge(&rel, found, "on Proton Drive") {
                        return Some(why);
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Report how a conflict was settled: a text-log line (the resolution
    /// history for CLI users) and a structured event for frontends.
    fn report_conflict(
        &self,
        pair: &Pair,
        path: &str,
        resolution: ConflictResolution,
        copy: Option<&str>,
    ) {
        self.log.info(&format!(
            "  conflict on {path}: {}{}",
            resolution.describe(),
            copy.map(|c| format!(" (copy: {c})")).unwrap_or_default()
        ));
        self.emit(SyncEvent::Conflict {
            pair: pair.name.clone(),
            path: path.to_string(),
            resolution,
            copy: copy.map(str::to_string),
        });
    }

    /// Download `rel` from the remote into place: into a private staging folder
    /// first, then moved over the destination only once it has arrived whole and
    /// the destination is still what `expect.local` says (see [`staged`]).
    fn fetch_into_place(&self, pair: &Pair, rel: &str, expect: &Expect) -> Result<()> {
        let local_full = safe_join_to_write(&pair.local, rel)?;
        let dest_dir = local_full
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(pair.local.clone());
        let name = local_full
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow::anyhow!("{rel:?} has no file name"))?;
        std::fs::create_dir_all(&dest_dir)?;
        let staging = staged::Staging::create(&dest_dir)?;
        self.remote.download(
            &remote_join(&pair.remote, rel),
            &staging.dir().to_string_lossy(),
        )?;
        staging.install(&name, expect)?;
        Ok(())
    }

    fn resolve_conflict(
        &mut self,
        pair: &Pair,
        rel: &str,
        new_base: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        let local_full = safe_join_to_write(&pair.local, rel)?;
        let stamp = epoch_to_stamp(now_epoch());
        // A name nothing is called yet: setting the local version aside must
        // never overwrite another file (an earlier conflict copy, say).
        let conflict_rel = unique_conflict_name(&pair.local, rel, &stamp)?;
        let conflict_full = pair.local.join(&conflict_rel);
        self.log
            .info(&format!("  conflict: keeping local copy as {conflict_rel}"));
        if local_full.exists() {
            move_aside(&local_full, &conflict_full)?;
        }
        // bring the remote version down to the original name
        let want = new_base.get(rel);
        self.fetch_into_place(
            pair,
            rel,
            &Expect {
                size: want.map(|e| e.size).unwrap_or(0),
                mtime: want.and_then(|e| e.mtime),
                // The local file was just set aside: anything there now is new.
                local: LocalSeen::Absent,
            },
        )?;
        // push the renamed local copy up so both versions exist on both sides
        let parent = parent_rel(&conflict_rel);
        self.ensure_remote_dir(&pair.remote, parent)?;
        self.remote.upload(
            &conflict_full.to_string_lossy(),
            &remote_join(&pair.remote, parent),
        )?;
        self.report_conflict(pair, rel, ConflictResolution::KeptBoth, Some(&conflict_rel));
        let size = std::fs::metadata(&conflict_full)
            .map(|m| m.len())
            .unwrap_or(0);
        new_base.insert(
            conflict_rel.clone(),
            Entry {
                path: conflict_rel,
                is_dir: false,
                size,
                ..Default::default()
            },
        );
        Ok(())
    }

    fn ensure_remote_base(&mut self, pair: &Pair) -> Result<()> {
        if let Some(rel) = strip_root(&self.cfg.remote_root, &pair.remote) {
            let rel = rel.to_string();
            self.ensure_remote_dir(&self.cfg.remote_root.clone(), &rel)?;
        }
        Ok(())
    }

    fn ensure_remote_dir(&mut self, base: &str, rel: &str) -> Result<()> {
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return Ok(());
        }
        let mut acc = String::new();
        for part in rel.split('/') {
            let parent = remote_join(base, &acc);
            let full = if acc.is_empty() {
                part.to_string()
            } else {
                format!("{acc}/{part}")
            };
            if self.ensured.contains(&full) {
                acc = full;
                continue;
            }
            self.remote.create_folder(&parent, part)?;
            self.ensured.insert(full.clone());
            acc = full;
        }
        Ok(())
    }

    fn delete_local(&self, path: &Path, is_dir: bool) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        match self.cfg.local_delete {
            LocalDelete::Trash => trash_local(path, self.gio.as_deref()),
            LocalDelete::Remove => {
                if is_dir {
                    std::fs::remove_dir_all(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
                Ok(())
            }
        }
    }

    // --- top-level ----------------------------------------------------------
    /// Reconcile a whole pair (local tree <-> remote tree).
    pub fn sync_pair(&mut self, pair: &Pair, resync: bool) -> Result<SyncResult> {
        self.sync_pair_scoped(pair, resync, None)
    }

    /// Reconcile a pair, optionally restricted to a single sub-folder (`scope`,
    /// a POSIX path relative to the pair root). A scoped run scans, reconciles,
    /// and commits only within that sub-tree, so a local change can be synced
    /// for just its folder instead of re-walking the whole pair. Nothing outside
    /// the scope is looked at, so it can never be seen as deleted.
    pub fn sync_pair_scoped(
        &mut self,
        pair: &Pair,
        resync: bool,
        scope: Option<&str>,
    ) -> Result<SyncResult> {
        // An empty scope means the pair ROOT — i.e. the whole pair, not a
        // sub-folder called "". Normalise it to None so paths aren't prefixed
        // with a stray leading "/" (which would misclassify every entry and
        // mass-recreate folders). A root-level file change thus does a full sync.
        let scope = scope.filter(|s| !s.is_empty());
        self.log.info(&format!(
            "=== pair {:?}{} : {} <-> {} ===",
            pair.name,
            scope.map(|s| format!(" [{s}]")).unwrap_or_default(),
            pair.local.display(),
            pair.remote
        ));
        self.emit(SyncEvent::PairStarted {
            pair: pair.name.clone(),
        });
        let (mut plan, new_base, incomplete, prune_excluded, held) =
            self.plan(pair, resync, scope)?;
        if incomplete {
            // Safety: never delete or move based on a partial view of the remote.
            plan.ops.retain(|op| {
                !matches!(
                    op.action,
                    Action::DeleteRemote
                        | Action::DeleteLocal
                        | Action::RenameRemote
                        | Action::RenameLocal
                )
            });
        }
        if plan.is_empty() {
            self.log.info(&format!(
                "already in sync ({} files tracked).",
                new_base.len()
            ));
        } else {
            self.log.info(&format!("plan: {}", plan.summary()));
        }
        self.emit(SyncEvent::Planned {
            pair: pair.name.clone(),
            total_ops: plan.actionable().count(),
            counts: plan
                .counts()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        });
        let tracked = new_base.len();
        let (mut result, _deleted_ok) = self.apply(pair, plan, new_base, prune_excluded);
        if let Some(h) = held {
            // A run that held deletions back is not a clean one: it counts as a
            // problem (exit status, the pair's state) until the user settles it.
            report_held(self.log, self.events, pair, h);
            result.errors.push(held_error(pair, h));
        }
        self.emit(SyncEvent::PairFinished {
            pair: pair.name.clone(),
            applied: result.applied,
            errors: result.errors.len(),
            tracked,
        });
        Ok(result)
    }

    /// Reconcile ONLY the direct children of `folder` (POSIX path relative to
    /// the pair root; "" = the root). Non-recursive: files in `folder` merge
    /// normally, and immediate sub-folders are created/removed but NOT descended
    /// into. Deeper changes arrive as their own watch events (recursive inotify),
    /// and remote-only changes are caught by the hot pass and the full walk — so
    /// a local change touches just its folder's listing, never a whole subtree.
    pub fn sync_pair_shallow(
        &mut self,
        pair: &Pair,
        folder: &str,
    ) -> Result<(SyncResult, Vec<String>)> {
        let (base_all, moved) = crate::state::load_baseline_for(&self.cfg.state_dir, pair)?;
        note_pair_moved(self.log, self.events, pair, moved);
        self.sync_pair_shallow_with_base(pair, folder, &base_all)
    }

    /// Sequential streaming walk (BFS): shallow-reconcile each folder as it is
    /// discovered, descending into the children it reports, transferring as it
    /// goes. Same per-folder primitive and one shared baseline snapshot as the
    /// concurrent [`run_sync_streaming`]; kept single-threaded here so it works
    /// with any `Remote` (including the test fake) and is the tested reference
    /// for the walk's semantics.
    pub fn sync_pair_streaming(&mut self, pair: &Pair) -> Result<SyncResult> {
        let (base, moved) = crate::state::load_baseline_for(&self.cfg.state_dir, pair)?;
        note_pair_moved(self.log, self.events, pair, moved);
        // The walk's one deletion allowance, as `run_sync_streaming` has it.
        let budget = DeleteBudget::new(self.cfg, pair, &base).map(Arc::new);
        if let Some(b) = &budget {
            census_trip(self.log, pair, b);
        }
        self.budget = budget.clone();
        let mut applied = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut frontier: Vec<String> = vec![String::new()];
        while !frontier.is_empty() {
            if self.cancelled() {
                break;
            }
            let mut next: Vec<String> = Vec::new();
            for folder in frontier.drain(..) {
                if self.cancelled() {
                    break;
                }
                let (r, children) = self.sync_pair_shallow_with_base(pair, &folder, &base)?;
                applied += r.applied;
                errors.extend(r.errors);
                next.extend(children);
            }
            next.sort();
            next.dedup();
            frontier = next;
        }
        self.budget = None;
        if let Some(b) = budget.as_deref().filter(|b| b.tripped() && b.held() > 0) {
            let (remote, local) = b.held_sides();
            let held = Held {
                remote,
                local,
                tracked: b.tracked(),
            };
            report_held(self.log, self.events, pair, held);
            errors.push(held_error(pair, held));
        }
        if !self.dry_run && !self.cancelled() {
            let _ = crate::state::set_last_synced(
                &self.cfg.state_dir,
                &pair.name,
                crate::datefmt::now_epoch(),
            );
        }
        Ok(SyncResult {
            pair: pair.name.clone(),
            applied,
            was_in_sync: applied == 0 && errors.is_empty(),
            errors,
            plan_summary: String::new(),
        })
    }

    /// Like [`sync_pair_shallow`] but with a pre-loaded baseline snapshot, so a
    /// streaming walk can share one snapshot across many folders instead of
    /// reloading it per folder. Returns the result plus the immediate sub-folders
    /// to descend into (present on either side after the reconcile; a folder
    /// deleted this run is not returned).
    pub fn sync_pair_shallow_with_base(
        &mut self,
        pair: &Pair,
        folder: &str,
        base_all: &BTreeMap<String, Entry>,
    ) -> Result<(SyncResult, Vec<String>)> {
        let folder = folder.trim_matches('/');
        let label = if folder.is_empty() {
            "(root)".to_string()
        } else {
            folder.to_string()
        };
        self.log
            .info(&format!("=== pair {:?} <{label}> shallow ===", pair.name));
        // NOTE: no PairStarted/Planned/PairFinished here. This runs per-folder,
        // often hundreds of times inside one streaming walk; emitting pair-level
        // events per folder would flap the GUI (Scanning<->Synced, path blanked)
        // and never show a stable progress line. The callers (run_sync_shallow,
        // run_sync_shallow_many, run_sync_streaming) emit ONE ScanStarted, a
        // per-folder ScanProgress, and ONE PairFinished around the whole batch.
        if !pair.local.exists() {
            anyhow::bail!(
                "local folder {} does not exist — refusing shallow sync",
                pair.local.display()
            );
        }
        // A folder that is, or sits below, a symlink is not followed: listing it
        // would read the link's target as if it were part of the tree, and a
        // download into it would be written wherever the link points.
        if let Some(link) = symlink_in_path(&pair.local, folder) {
            self.log.warn(&format!(
                "shallow: {} is a symlink; not following it ({label})",
                link.display()
            ));
            return Ok((
                SyncResult {
                    pair: pair.name.clone(),
                    applied: 0,
                    errors: Vec::new(),
                    plan_summary: String::new(),
                    was_in_sync: true,
                },
                Vec::new(),
            ));
        }
        // An excluded folder (a user exclude, a junk name, a download's staging
        // folder) is invisible to the engine: nothing to list on either side. A
        // watcher event inside one must not cost a CLI call to find that out.
        if !folder.is_empty() && pair.is_excluded(folder) {
            return Ok((
                SyncResult {
                    pair: pair.name.clone(),
                    applied: 0,
                    errors: Vec::new(),
                    plan_summary: String::new(),
                    was_in_sync: true,
                },
                Vec::new(),
            ));
        }

        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        // Direct children only: a key is "<prefix><name>" with no further '/'.
        let is_direct = |k: &str| match k.strip_prefix(&prefix) {
            Some(rest) => !rest.is_empty() && !rest.contains('/'),
            None => false,
        };
        let base_direct: BTreeMap<String, Entry> = base_all
            .iter()
            .filter(|(k, _)| is_direct(k) && !pair.is_excluded(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        // LOCAL direct children (depth 1, no recursion).
        let want_sha1 = self.cfg.compare == Compare::Sha1;
        let dir_path = if folder.is_empty() {
            pair.local.clone()
        } else {
            pair.local.join(folder)
        };
        let mut local_direct: BTreeMap<String, Entry> = BTreeMap::new();
        // A local `read_dir` failure (a permissions blip, an I/O error, or fd
        // exhaustion under the concurrent walk) must NOT be read as "everything
        // here was deleted" — that would trash still-present remote files. Flag
        // it and suppress deletes this run, exactly like a failed remote listing.
        // The same goes for an error half way through the listing (a network
        // filesystem dropping out): what came after it is unknown, not gone.
        let mut local_failed = false;
        match std::fs::read_dir(&dir_path) {
            Err(e) => {
                self.log.warn(&format!(
                    "shallow: local listing failed for {}: {e}; syncing without deletions",
                    dir_path.display()
                ));
                local_failed = true;
            }
            Ok(rd) => {
                for e in rd {
                    let e = match e {
                        Ok(e) => e,
                        Err(err) => {
                            self.log.warn(&format!(
                                "shallow: local listing of {} broke off: {err}; \
                                 syncing without deletions",
                                dir_path.display()
                            ));
                            local_failed = true;
                            continue;
                        }
                    };
                    let name = e.file_name().to_string_lossy().replace('\\', "/");
                    if staged::is_staging_name(&name) {
                        // A download in progress, or its leftovers if the process
                        // died: never part of the listing either way.
                        staged::sweep_if_stale(&e.path(), self.stale_after());
                        continue;
                    }
                    let rel = format!("{prefix}{name}");
                    if pair.is_excluded(&rel) {
                        continue;
                    }
                    let ft = match e.file_type() {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    if ft.is_dir() {
                        local_direct.insert(
                            rel.clone(),
                            Entry {
                                path: rel,
                                is_dir: true,
                                ..Default::default()
                            },
                        );
                    } else if ft.is_file() {
                        match e.metadata() {
                            Ok(m) => {
                                let size = m.len();
                                let mtime = m
                                    .modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs() as i64);
                                let sha1 = if want_sha1 {
                                    match base_direct.get(&rel) {
                                        Some(b)
                                            if !b.is_dir
                                                && b.size == size
                                                && b.mtime == mtime
                                                && b.sha1.is_some() =>
                                        {
                                            b.sha1.clone()
                                        }
                                        _ => sha1_file(&e.path()),
                                    }
                                } else {
                                    None
                                };
                                local_direct.insert(
                                    rel.clone(),
                                    Entry {
                                        path: rel,
                                        is_dir: false,
                                        size,
                                        mtime,
                                        sha1,
                                        remote_id: None,
                                    },
                                );
                            }
                            // A transient stat failure must not look like a
                            // delete: carry the baseline entry forward so the
                            // file stays Unchanged and is retried, never trashed.
                            Err(_) => {
                                if let Some(b) = base_direct.get(&rel) {
                                    local_direct.insert(rel.clone(), b.clone());
                                }
                            }
                        }
                    } else if let Some(b) = base_direct.get(&rel) {
                        // A symlink, pipe, socket or device where a tracked file
                        // or folder used to be is not a deletion: a file is
                        // carried forward untouched, and a folder (whose contents
                        // can no longer be seen) holds deletions back.
                        self.log.warn(&format!(
                            "shallow: {rel} is a symlink or special file now, not a \
                             regular {}; left alone",
                            if b.is_dir { "folder" } else { "file" }
                        ));
                        if b.is_dir {
                            local_failed = true;
                        } else {
                            local_direct.insert(rel.clone(), b.clone());
                        }
                    }
                }
            }
        }
        // The pair root came back empty while the baseline knows items in it: an
        // unmounted drive's mount point, a folder emptied by mistake. Hold
        // deletions back (see `plan`).
        let tracked_files = if folder.is_empty() && local_direct.is_empty() {
            base_all
                .iter()
                .filter(|(k, e)| !e.is_dir && !pair.is_excluded(k))
                .count()
        } else {
            0
        };
        if folder.is_empty() && !local_failed && tracked_files >= EMPTY_ROOT_MIN_FILES {
            local_failed = true;
            self.log.warn(&format!(
                "shallow: the local folder of {:?} is empty but {tracked_files} file(s) are on \
                 record; syncing without deletions (unmounted drive?)",
                pair.name,
            ));
            self.emit(SyncEvent::Error {
                pair: Some(pair.name.clone()),
                text: "local folder is empty — synced without deletions (unmounted drive?)".into(),
            });
        }

        // REMOTE direct children (single non-recursive listing). A transport
        // error means we can't see the folder — suppress deletes this run rather
        // than act on a blind listing. (A genuinely absent folder lists empty.)
        let remote_path = remote_join(&pair.remote, folder);
        let (remote_direct, remote_failed) = match self.remote.list_dir_probe(&remote_path) {
            Ok(ListOutcome::Listed(entries)) => {
                let mut m = BTreeMap::new();
                for mut e in entries {
                    let rel = format!("{prefix}{}", e.path);
                    if pair.is_excluded(&rel) {
                        continue;
                    }
                    e.path = rel.clone();
                    m.insert(rel, e);
                }
                (m, false)
            }
            // Not found: a folder the parent just listed as present now reports
            // missing — a race (trashed elsewhere) or a transient error misread
            // as not-found. Either way, don't propagate deletes on that basis;
            // treat it as a failed listing (empty, deletes suppressed).
            Ok(ListOutcome::NotFound) => {
                self.log.warn(&format!(
                    "shallow: remote folder {remote_path:?} not found; \
                     syncing without deletions"
                ));
                (BTreeMap::new(), true)
            }
            Err(err) => {
                self.log.warn(&format!(
                    "shallow: remote listing failed for {remote_path:?}: {err}; \
                     syncing without deletions"
                ));
                (BTreeMap::new(), true)
            }
        };

        let cmp = self.cfg.compare;
        let ls = classify(&local_direct, &base_direct, cmp);
        let rs = classify(&remote_direct, &base_direct, cmp);
        self.digest_equal_sizes(&pair.local, &mut local_direct, &remote_direct, &ls, &rs);
        let mut plan = Plan::default();
        let mut new_base: BTreeMap<String, Entry> = BTreeMap::new();
        let mut keys: Vec<&String> = local_direct
            .keys()
            .chain(remote_direct.keys())
            .chain(base_direct.keys())
            .collect();
        keys.sort();
        keys.dedup();
        for path in keys {
            let lc = *ls.get(path).unwrap_or(&Change::Absent);
            let rc = *rs.get(path).unwrap_or(&Change::Absent);
            self.decide(
                path,
                lc,
                rc,
                local_direct.get(path),
                remote_direct.get(path),
                base_direct.get(path),
                &mut plan,
                &mut new_base,
            );
        }
        // Never delete or move on a blind view of either side: if the local
        // read_dir OR the remote listing failed, strip all destructive ops.
        if remote_failed || local_failed {
            plan.ops.retain(|op| {
                !matches!(
                    op.action,
                    Action::DeleteRemote
                        | Action::DeleteLocal
                        | Action::RenameRemote
                        | Action::RenameLocal
                )
            });
        }
        // The mass-deletion guard. Judged on what this folder would delete (a
        // folder counts as the files the baseline holds under it) against the
        // run's shared budget, so a walk over many folders spends one allowance.
        // Without a shared budget (a single-folder run) it makes its own.
        let mut held_alone: Option<Held> = None;
        let (planned_local, planned_remote) = planned_delete_sets(&plan, base_all);
        let planned = planned_local.len() + planned_remote.len();
        if planned > 0 {
            let own;
            let budget: Option<&DeleteBudget> = match &self.budget {
                Some(b) => Some(b.as_ref()),
                None => {
                    own = DeleteBudget::new(self.cfg, pair, base_all);
                    own.as_ref()
                }
            };
            if let Some(b) = budget {
                if !b.reserve(planned) {
                    plan.ops.retain(|op| {
                        !matches!(op.action, Action::DeleteRemote | Action::DeleteLocal)
                    });
                    if self.budget.is_none() {
                        held_alone = Some(Held {
                            remote: planned_remote.len(),
                            local: planned_local.len(),
                            tracked: b.tracked(),
                        });
                    }
                    b.hold(planned_local, planned_remote);
                }
            }
        }
        // Baseline rows for direct children that are now EXCLUDED. (`base_direct`
        // already filters excludes out, so scan the full snapshot instead.) These
        // are pruned so a later re-include re-downloads rather than deleting.
        let prune_excluded: HashSet<String> = base_all
            .keys()
            .filter(|k| is_direct(k) && pair.is_excluded(k))
            .cloned()
            .collect();

        if plan.is_empty() {
            self.log
                .info(&format!("shallow <{label}>: already in sync."));
        } else {
            self.log
                .info(&format!("shallow <{label}> plan: {}", plan.summary()));
        }

        // A deleted direct sub-folder has descendants in the baseline that the
        // apply's per-key commit won't touch; collect them for cleanup after.
        let deleted_dirs: Vec<String> = plan
            .ops
            .iter()
            .filter(|o| matches!(o.action, Action::DeleteRemote | Action::DeleteLocal) && o.is_dir)
            .map(|o| o.path.clone())
            .collect();

        let tracked = new_base.len();
        let (mut result, deleted_ok) = self.apply(pair, plan, new_base, prune_excluded);
        if let Some(h) = held_alone {
            report_held(self.log, self.events, pair, h);
            result.errors.push(held_error(pair, h));
        }

        // Purge descendant baseline rows ONLY for sub-folders whose delete
        // actually COMPLETED (in `deleted_ok`). A delete that was cancelled or
        // failed leaves the folder in place, so its rows must survive — otherwise
        // the baseline would diverge from reality and cause spurious re-work.
        if !self.dry_run {
            let mut descendants: HashSet<String> = HashSet::new();
            for d in deleted_dirs.iter().filter(|d| deleted_ok.contains(*d)) {
                let pfx = format!("{d}/");
                for k in base_all.keys() {
                    if k.starts_with(&pfx) {
                        descendants.insert(k.clone());
                    }
                }
            }
            if !descendants.is_empty() {
                let _ = crate::state::commit_baseline(
                    &self.cfg.state_dir,
                    &pair.name,
                    &BTreeMap::new(),
                    &descendants,
                );
            }
        }

        let _ = tracked; // pair-level events are emitted by the caller, not here

        // Immediate sub-folders to descend into on a streaming walk: dirs present
        // on either side after the reconcile, minus any deleted this run.
        // Only a folder whose removal completed is gone: one that was kept (it held
        // something that had not synced) must still be descended into, or what is
        // in it would never be reconciled.
        let deleted: HashSet<&String> = deleted_dirs
            .iter()
            .filter(|d| self.dry_run || deleted_ok.contains(*d))
            .collect();
        let mut children: Vec<String> = local_direct
            .iter()
            .chain(remote_direct.iter())
            .filter(|(k, e)| e.is_dir && !deleted.contains(k))
            .map(|(k, _)| k.clone())
            .collect();
        children.sort();
        children.dedup();
        Ok((result, children))
    }
}

/// Say, in the log and to the frontends, that the guard held deletions back.
fn report_held(log: &Logger, events: Option<&dyn EventSink>, pair: &Pair, held: Held) {
    log.warn(&format!("{:?}: {}", pair.name, held_error(pair, held)));
    if let Some(sink) = events {
        sink.emit(&SyncEvent::DeletesHeld {
            pair: pair.name.clone(),
            remote: held.remote,
            local: held.local,
            tracked: held.tracked,
        });
    }
}

/// The text of a held-back run, as an entry of a result's errors.
fn held_error(pair: &Pair, held: Held) -> String {
    crate::events::held_deletes_text(&pair.name, held.remote, held.local, held.tracked)
}

/// Say, in the log and to the frontends, that a pair was pointed at other
/// folders and its old record discarded (a no-op when it was not).
fn note_pair_moved(
    log: &Logger,
    events: Option<&dyn EventSink>,
    pair: &Pair,
    moved: Option<crate::state::PairMoved>,
) {
    let Some(m) = moved else { return };
    let text = m.describe(&pair.name);
    log.warn(&text);
    if let Some(sink) = events {
        sink.emit(&SyncEvent::Info { text });
    }
}

/// Mark the op just added to `plan` as the one that settles a conflict.
fn settle_last(plan: &mut Plan, resolution: ConflictResolution) {
    if let Some(op) = plan.ops.last_mut() {
        op.conflict = Some(resolution);
    }
}

/// Next epoch-second `path` is eligible to be retried after its `attempts`-th
/// consecutive failure (M4-004): exponential backoff off
/// `retry_base_delay_secs`, capped by `retry_max_delay_secs`, with growth
/// plateauing once `attempts` reaches `retry_max_attempts` - a transient
/// problem (rate limit, outage) is expected to eventually clear, so this
/// never gives up for good, it just stops escalating. Jitter is mixed in via
/// a hash of `(path, attempts)`, so it's deterministic (no `rand` dependency)
/// but differs between paths - spreading out retries so many files failing
/// together don't all retry in lockstep.
fn backoff_next_at(cfg: &Config, path: &str, attempts: u32, now: i64) -> i64 {
    let max_attempts = cfg.retry_max_attempts.max(1);
    let exp = attempts.min(max_attempts).saturating_sub(1).min(20);
    let base = cfg.retry_base_delay_secs.max(1);
    let delay = base
        .saturating_mul(1u64 << exp)
        .min(cfg.retry_max_delay_secs.max(base));
    let mut hasher = DefaultHasher::new();
    (path, attempts).hash(&mut hasher);
    let half = delay / 2;
    let jitter = if half == 0 {
        0
    } else {
        hasher.finish() % (half + 1)
    };
    now + (half + jitter) as i64
}

/// Join `rel` onto `root`, guaranteeing the result stays within `root`. Only
/// normal path components are allowed; any `..`, absolute, or prefix component
/// (which could escape the sync root) is rejected. Every local filesystem path
/// built from a remote-controlled name must go through this.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => anyhow::bail!("refusing unsafe path {rel:?}: it escapes the sync root"),
        }
    }
    Ok(out)
}

// --- free helpers -----------------------------------------------------------
/// The first symlink among the folders `rel` passes through below `root` (the
/// last component included), if any. `root` itself may be a symlink - a user can
/// keep their sync folder anywhere - but nothing inside it is followed.
fn symlink_in_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    for comp in Path::new(rel).components() {
        cur.push(comp);
        if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return Some(cur);
        }
    }
    None
}

/// [`safe_join`] for a path something is about to be written to: also refuses
/// one whose parent folders include a symlink, which would land the write
/// wherever the link points - outside the sync root. (The last component may be
/// a link: moving a file over it replaces the link, not what it points at.)
fn safe_join_to_write(root: &Path, rel: &str) -> Result<PathBuf> {
    let full = safe_join(root, rel)?;
    if let Some((parent, _)) = rel.rsplit_once('/') {
        if let Some(link) = symlink_in_path(root, parent) {
            anyhow::bail!(
                "refusing to write {rel:?}: {} is a symlink and may lead outside the sync root",
                link.display()
            );
        }
    }
    Ok(full)
}

fn classify(
    current: &BTreeMap<String, Entry>,
    base: &BTreeMap<String, Entry>,
    cmp: Compare,
) -> HashMap<String, Change> {
    let mut status = HashMap::new();
    let mut keys: Vec<&String> = current.keys().chain(base.keys()).collect();
    keys.sort();
    keys.dedup();
    for path in keys {
        let inc = current.get(path);
        let inb = base.get(path);
        let change = match (inc, inb) {
            (Some(_), None) => Change::Created,
            (None, Some(_)) => Change::Deleted,
            (None, None) => Change::Absent,
            (Some(c), Some(b)) => {
                if same_content(c, b, cmp) {
                    Change::Unchanged
                } else {
                    Change::Modified
                }
            }
        };
        status.insert(path.clone(), change);
    }
    status
}

fn order_key(op: &Op) -> (u8, i64) {
    let rank = match op.action {
        Action::MkdirRemote | Action::MkdirLocal => 0,
        Action::Upload | Action::Download | Action::Conflict => 1,
        Action::RenameRemote | Action::RenameLocal => 1,
        Action::DeleteRemote | Action::DeleteLocal => 2,
        Action::Noop => 1,
    };
    let depth = op.path.matches('/').count() as i64;
    match op.action {
        // mkdir shallow-first; deletes deep-first; transfers shallow-first
        Action::DeleteRemote | Action::DeleteLocal => (rank, -depth),
        _ => (rank, depth),
    }
}

fn parent_rel(rel: &str) -> &str {
    match rel.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Two entries are the same file moved/renamed: identical size AND a matching
/// strong signal (sha1 if both have it, else mtime). Size alone is too weak.
fn is_rename_match(a: &Entry, b: &Entry) -> bool {
    if a.is_dir || b.is_dir || a.size != b.size {
        return false;
    }
    if let (Some(x), Some(y)) = (&a.sha1, &b.sha1) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.mtime, b.mtime) {
        return x == y;
    }
    false
}

/// Rewrite delete+create pairs with identical content into a single rename op.
/// `from_entry` gives the baseline content of a deleted path; `to_entry` gives
/// the new content of a created path.
///
/// `confirm(from, to)` has the last word on a pair that matched by size and time:
/// size and (whole-second) time are a heuristic, and a build output or a batch
/// of files extracted together can share both with a file that has other bytes in
/// it. It returns `false` only when it can show they differ, in which case the
/// two stay a delete and a create - never a move that would put the old content
/// under the new name and leave the new content unsent.
fn rewrite_renames<'a>(
    plan: &mut Plan,
    del: Action,
    create: Action,
    rename: Action,
    from_entry: impl Fn(&str) -> Option<&'a Entry>,
    to_entry: impl Fn(&str) -> Option<&'a Entry>,
    confirm: impl Fn(&str, &str) -> bool,
) {
    let creates: Vec<usize> = plan
        .ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.action == create && !o.is_dir)
        .map(|(i, _)| i)
        .collect();
    let dels: Vec<usize> = plan
        .ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.action == del && !o.is_dir)
        .map(|(i, _)| i)
        .collect();

    let mut used_del: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut renamed: Vec<(usize, String)> = Vec::new(); // (create index, from path)
    for &c in &creates {
        let to = plan.ops[c].path.clone();
        let Some(te) = to_entry(&to) else { continue };
        for &d in &dels {
            if used_del.contains(&d) {
                continue;
            }
            let from = plan.ops[d].path.clone();
            let Some(fe) = from_entry(&from) else {
                continue;
            };
            if is_rename_match(fe, te) && confirm(&from, &to) {
                used_del.insert(d);
                renamed.push((c, from));
                break;
            }
        }
    }
    if renamed.is_empty() {
        return;
    }
    for (c, from) in renamed {
        let to = plan.ops[c].path.clone();
        plan.ops[c] = Op::rename(rename, from, to, "renamed");
    }
    let mut i = 0usize;
    plan.ops.retain(|_| {
        let keep = !used_del.contains(&i);
        i += 1;
        keep
    });
}

/// Detect local renames (delete-remote + upload) and remote renames
/// (delete-local + download) by matching content, collapsing each into a move.
///
/// A match by size and time is checked against the remote's digest when it listed
/// one: the file about to be moved is hashed and compared. With no digest there
/// is nothing to check against and the match stands.
fn detect_renames(
    plan: &mut Plan,
    base: &BTreeMap<String, Entry>,
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
    local_root: &Path,
) {
    // Does `local_rel`'s content match the digest `remote_rel` was listed with?
    let matches_remote_digest = |remote_rel: &str, local_rel: &str| -> bool {
        match remote.get(remote_rel).and_then(|e| e.sha1.as_deref()) {
            Some(want) => safe_join(local_root, local_rel)
                .ok()
                .and_then(|p| sha1_file(&p))
                .is_some_and(|got| got == want),
            None => true,
        }
    };
    // A local rename: the remote file at `from` is to be moved to `to`, which
    // has to be the same bytes as `from`'s remote digest.
    rewrite_renames(
        plan,
        Action::DeleteRemote,
        Action::Upload,
        Action::RenameRemote,
        |p| base.get(p),
        |p| local.get(p),
        |from, to| matches_remote_digest(from, to),
    );
    // A remote rename: the local file at `from` is to be moved to `to`, whose
    // remote digest has to be `from`'s bytes.
    rewrite_renames(
        plan,
        Action::DeleteLocal,
        Action::Download,
        Action::RenameLocal,
        |p| base.get(p),
        |p| remote.get(p),
        |from, to| matches_remote_digest(to, from),
    );
}

fn conflict_name(rel: &str, stamp: &str) -> String {
    let p = Path::new(rel);
    let dir = p.parent().and_then(|d| d.to_str()).unwrap_or("");
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = p.extension().and_then(|s| s.to_str());
    let name = match ext {
        Some(e) => format!("{stem} (conflict {stamp}).{e}"),
        None => format!("{stem} (conflict {stamp})"),
    };
    if dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    }
}

/// A conflict-copy name for `rel` that no existing file has. The timestamp has
/// whole-second resolution, so two conflicts on one file within a second - or a
/// file the user already named that way - would otherwise collide, and the later
/// rename would silently overwrite the earlier copy.
fn unique_conflict_name(root: &Path, rel: &str, stamp: &str) -> Result<String> {
    let taken = |r: &str| std::fs::symlink_metadata(root.join(r)).is_ok();
    let first = conflict_name(rel, stamp);
    if !taken(&first) {
        return Ok(first);
    }
    for n in 2..1000 {
        let candidate = conflict_name(rel, &format!("{stamp} {n}"));
        if !taken(&candidate) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("no free name for a conflict copy of {rel:?}")
}

/// Move `from` to `to` without ever replacing an existing `to`: `rename(2)`
/// would. A hard link fails if `to` exists, and unlinking `from` afterwards
/// completes the move; a filesystem without hard links falls back to a rename
/// guarded by an existence check.
fn move_aside(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            std::fs::rename(from, to)
        }
    }
}

fn match_mtime(path: &Path, mtime: Option<i64>) {
    let secs = match mtime {
        Some(s) if s >= 0 => s as u64,
        _ => return,
    };
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(secs));
    }
}

fn sha1_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = sha1_smol::Sha1::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.digest().to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        backoff_next_at, planned_delete_sets, safe_join, safe_join_to_write, symlink_in_path,
        DeleteBudget,
    };
    use crate::config::{Config, ConflictPolicy, LocalDelete, UpdateChannel};
    use crate::models::Compare;
    use std::path::Path;

    fn entry(path: &str, is_dir: bool) -> crate::models::Entry {
        crate::models::Entry {
            path: path.into(),
            is_dir,
            size: 1,
            ..Default::default()
        }
    }

    fn pair() -> crate::config::Pair {
        crate::config::Pair {
            name: "docs".into(),
            local: "/l".into(),
            remote: "/r".into(),
            auto: true,
            exclude: vec!["skip".into()],
        }
    }

    fn base_of(files: usize) -> std::collections::BTreeMap<String, crate::models::Entry> {
        (0..files)
            .map(|i| (format!("f{i:04}"), entry(&format!("f{i:04}"), false)))
            .collect()
    }

    #[test]
    fn the_budget_is_the_larger_of_the_floor_and_the_percentage_and_off_when_it_cannot_matter() {
        let mut c = retry_cfg(30, 1800, 6);
        c.propagate_deletes = true;
        // 1000 files: 20% = 200 > 100
        let b = DeleteBudget::new(&c, &pair(), &base_of(1000)).unwrap();
        assert_eq!((b.allowed(), b.tracked()), (200, 1000));
        // 300 files: 20% = 60 < 100
        assert_eq!(
            DeleteBudget::new(&c, &pair(), &base_of(300))
                .unwrap()
                .allowed(),
            100
        );
        // 100 files or fewer: no run can delete more than it tracks
        assert!(DeleteBudget::new(&c, &pair(), &base_of(100)).is_none());
        assert!(DeleteBudget::new(&c, &pair(), &base_of(0)).is_none());
        // told it is intended / nothing propagates: it stands aside
        let mut sure = c.clone();
        sure.allow_mass_delete = true;
        assert!(DeleteBudget::new(&sure, &pair(), &base_of(1000)).is_none());
        let mut keep = c.clone();
        keep.propagate_deletes = false;
        assert!(DeleteBudget::new(&keep, &pair(), &base_of(1000)).is_none());
        // 100% turns it off
        let mut off = c.clone();
        off.max_delete_files = 0;
        off.max_delete_percent = 100;
        assert!(DeleteBudget::new(&off, &pair(), &base_of(1000)).is_none());
        // excluded paths are not tracked
        let mut base = base_of(150);
        for i in 0..100 {
            base.insert(format!("skip/x{i}"), entry(&format!("skip/x{i}"), false));
        }
        assert_eq!(
            DeleteBudget::new(&c, &pair(), &base).unwrap().tracked(),
            150
        );
    }

    #[test]
    fn the_budget_lets_exactly_its_allowance_through_however_many_workers_ask() {
        let mut c = retry_cfg(30, 1800, 6);
        c.propagate_deletes = true;
        let b = DeleteBudget::new(&c, &pair(), &base_of(1000)).unwrap(); // allowed 200
        let granted = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..100 {
                        if b.reserve(1) {
                            granted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        assert_eq!(granted.load(std::sync::atomic::Ordering::SeqCst), 200);
        assert!(b.tripped());
        // once tripped, even a small request is refused for the rest of the run
        assert!(!b.reserve(1));
        // all or nothing: a request that does not fit is refused whole
        let fresh = DeleteBudget::new(&c, &pair(), &base_of(1000)).unwrap();
        assert!(fresh.reserve(150));
        assert!(!fresh.reserve(100), "150 + 100 > 200");
        // what was refused is remembered by file and side, each file once:
        // a folder held back and then descended into is refused again file by file
        fresh.hold((0..100).map(|i| format!("a/f{i}")), std::iter::empty());
        fresh.hold(
            (0..100).map(|i| format!("a/f{i}")),
            (0..5).map(|i| format!("r{i}")),
        );
        assert_eq!(fresh.held(), 105);
        assert_eq!(fresh.held_sides(), (5, 100));
    }

    #[test]
    fn a_folder_and_the_files_in_it_are_one_deletion_not_two() {
        use crate::models::{Action, Op, Plan};
        let mut base = base_of(0);
        for p in ["d/a", "d/b", "d/sub/c", "other/x"] {
            base.insert(p.to_string(), entry(p, false));
        }
        base.insert("d".into(), entry("d", true));
        base.insert("d/sub".into(), entry("d/sub", true));
        let mut plan = Plan::default();
        // a whole-tree plan lists the folder and each file in it
        for (path, dir) in [
            ("d", true),
            ("d/sub", true),
            ("d/a", false),
            ("d/b", false),
            ("d/sub/c", false),
        ] {
            plan.add(Op::new(Action::DeleteRemote, path, dir, ""));
        }
        let counts = |plan: &Plan| {
            let (l, r) = planned_delete_sets(plan, &base);
            (l.len(), r.len())
        };
        assert_eq!(counts(&plan), (0, 3));
        // a shallow plan lists only the folder
        let mut shallow = Plan::default();
        shallow.add(Op::new(Action::DeleteLocal, "d", true, ""));
        assert_eq!(counts(&shallow), (3, 0));
        // anything that is not a delete does not count
        shallow.add(Op::new(Action::Upload, "other/x", false, ""));
        assert_eq!(counts(&shallow), (3, 0));
        // and the two sides are told apart
        shallow.add(Op::new(Action::DeleteRemote, "other/x", false, ""));
        assert_eq!(counts(&shallow), (3, 1));
    }

    #[test]
    fn writes_are_refused_through_a_symlink_but_not_onto_one_or_under_a_linked_root() {
        let base = std::env::temp_dir().join(format!(
            "protondrive4linux-links-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let real = base.join("real");
        let outside = base.join("outside");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, real.join("link")).unwrap();
        std::os::unix::fs::symlink(real.join("sub"), real.join("sublink")).unwrap();

        // Through a link: refused, however deep.
        assert!(safe_join_to_write(&real, "link/a.txt").is_err());
        assert!(safe_join_to_write(&real, "link/x/y/a.txt").is_err());
        assert_eq!(symlink_in_path(&real, "link/x"), Some(real.join("link")));
        // Onto a link (the last component) is how a file replaces one: allowed.
        assert!(safe_join_to_write(&real, "link").is_ok());
        assert!(safe_join_to_write(&real, "sublink").is_ok());
        // Ordinary paths, including ones that do not exist yet.
        assert!(safe_join_to_write(&real, "sub/a.txt").is_ok());
        assert!(safe_join_to_write(&real, "new/dir/a.txt").is_ok());
        assert_eq!(symlink_in_path(&real, "sub/a.txt"), None);
        // The root itself may be a link (the sync folder can live anywhere).
        let linked_root = base.join("linked-root");
        std::os::unix::fs::symlink(&real, &linked_root).unwrap();
        assert!(safe_join_to_write(&linked_root, "sub/a.txt").is_ok());
        assert!(safe_join_to_write(&linked_root, "link/a.txt").is_err());
        // The escapes `safe_join` already refuses still are.
        assert!(safe_join_to_write(&real, "../etc/passwd").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn safe_join_allows_nested_but_rejects_escapes() {
        let root = Path::new("/sync/root");
        assert_eq!(
            safe_join(root, "a/b.txt").unwrap(),
            Path::new("/sync/root/a/b.txt")
        );
        assert_eq!(safe_join(root, "./a").unwrap(), Path::new("/sync/root/a"));
        // A remote-controlled name that tries to climb out is refused.
        assert!(safe_join(root, "../escape").is_err());
        assert!(safe_join(root, "a/../../escape").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
    }

    fn retry_cfg(base: u64, max_delay: u64, max_attempts: u32) -> Config {
        Config {
            binary: "proton-drive".into(),
            upload_flags: vec![],
            download_flags: vec![],
            credentials_store: None,
            fresh_cache: false,
            scan_threads: 0,
            download_threads: 0,
            cli_timeout_secs: 30,
            retry_base_delay_secs: base,
            retry_max_delay_secs: max_delay,
            retry_max_attempts: max_attempts,
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            upload_limiter: crate::ratelimit::RateLimiter::new(),
            download_limiter: crate::ratelimit::RateLimiter::new(),
            remote_root: "/my-files".into(),
            propagate_deletes: false,
            max_delete_files: crate::config::DEFAULT_MAX_DELETE_FILES,
            max_delete_percent: crate::config::DEFAULT_MAX_DELETE_PERCENT,
            allow_mass_delete: false,
            auto_sync: false,
            run_in_tray: false,
            x11_compat: false,
            local_delete: LocalDelete::Trash,
            conflict: ConflictPolicy::KeepBoth,
            compare: Compare::SizeMtime,
            poll_interval_secs: 900,
            scan_interval_secs: 120,
            debounce_secs: 2,
            update_channel: UpdateChannel::Stable,
            check_on_launch: false,
            notifications: true,
            theme: crate::config::ThemePref::System,
            file_manager: false,
            state_dir: std::env::temp_dir(),
            pairs: vec![],
            source_path: None,
        }
    }

    #[test]
    fn backoff_grows_exponentially_then_plateaus_at_max_attempts() {
        let cfg = retry_cfg(10, 10_000, 4);
        let now = 1_000_000;
        // attempts 1..4: 10, 20, 40, 80 (± jitter, which is in [delay/2, delay]).
        let d1 = backoff_next_at(&cfg, "a.txt", 1, now) - now;
        let d2 = backoff_next_at(&cfg, "a.txt", 2, now) - now;
        let d3 = backoff_next_at(&cfg, "a.txt", 3, now) - now;
        let d4 = backoff_next_at(&cfg, "a.txt", 4, now) - now;
        let d5 = backoff_next_at(&cfg, "a.txt", 5, now) - now; // past max_attempts: plateaus
        assert!((5..=10).contains(&d1), "attempt 1 delay was {d1}");
        assert!((10..=20).contains(&d2), "attempt 2 delay was {d2}");
        assert!((20..=40).contains(&d3), "attempt 3 delay was {d3}");
        assert!((40..=80).contains(&d4), "attempt 4 delay was {d4}");
        // Same exponent (and thus the same bounds) as attempt 4 - growth
        // stopped, it never gives up retrying for good.
        assert!((40..=80).contains(&d5), "attempt 5 delay was {d5}");
    }

    #[test]
    fn backoff_is_capped_at_max_delay() {
        let cfg = retry_cfg(1000, 1500, 10);
        let now = 0;
        let d = backoff_next_at(&cfg, "a.txt", 10, now) - now;
        assert!(d <= 1500, "delay {d} exceeded the configured ceiling");
    }

    #[test]
    fn backoff_jitter_differs_between_paths_at_the_same_attempt() {
        let cfg = retry_cfg(10_000, 10_000_000, 6);
        let now = 0;
        // Full jitter is [delay/2, delay] - different paths should (almost
        // certainly) land on different points in that range.
        let a = backoff_next_at(&cfg, "a.txt", 3, now);
        let b = backoff_next_at(&cfg, "b.txt", 3, now);
        assert_ne!(a, b, "two different paths landed on identical jitter");
    }

    #[test]
    fn backoff_next_at_is_deterministic() {
        let cfg = retry_cfg(30, 1800, 6);
        assert_eq!(
            backoff_next_at(&cfg, "same.txt", 3, 500),
            backoff_next_at(&cfg, "same.txt", 3, 500),
            "same inputs must always produce the same next_at"
        );
    }
}
