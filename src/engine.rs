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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;

use crate::events::{EventSink, SyncEvent};

use crate::config::{remote_join, strip_root, Config, ConflictPolicy, LocalDelete, Pair};
use crate::datefmt::{epoch_to_stamp, now_epoch};
use crate::logger::Logger;
use crate::models::{same_content, Action, Change, Compare, DownloadJob, Entry, Op, Plan};
use crate::protoncli::{ProtonCli, Remote};
use crate::trash::trash_local;

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
        if cancel.map_or(false, |c| c.load(Ordering::Relaxed)) {
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

pub struct Engine<'a, R: Remote> {
    cfg: &'a Config,
    remote: R,
    log: &'a Logger,
    dry_run: bool,
    gio: Option<PathBuf>,
    ensured: HashSet<String>,
    events: Option<&'a dyn EventSink>,
    cancel: Option<&'a AtomicBool>,
}

impl<'a, R: Remote> Engine<'a, R> {
    pub fn new(cfg: &'a Config, remote: R, log: &'a Logger, dry_run: bool) -> Self {
        Engine {
            cfg,
            remote,
            log,
            dry_run,
            gio: which_gio(),
            ensured: HashSet::new(),
            events: None,
            cancel: None,
        }
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
        self.cancel.map_or(false, |c| c.load(Ordering::Relaxed))
    }

    // --- scanning -----------------------------------------------------------
    /// Walk the local tree. In sha1 mode, files whose size+mtime match the
    /// baseline reuse the stored hash instead of being re-read (a
    /// (size,mtime)->sha1 cache), so only changed files are hashed.
    fn scan_local(
        &self,
        root: &Path,
        baseline: &BTreeMap<String, Entry>,
    ) -> BTreeMap<String, Entry> {
        let mut out = BTreeMap::new();
        if !root.exists() {
            return out;
        }
        let want_sha1 = self.cfg.compare == Compare::Sha1;
        for entry in walkdir::WalkDir::new(root)
            .min_depth(1)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
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
            }
        }
        out
    }

    /// Returns the remote tree, the relative folders that could not be listed
    /// (empty = a complete scan), and whether the pair's remote base folder
    /// itself was not found (a vanished/moved base, distinct from an empty one).
    fn scan_remote(
        &self,
        base: &str,
        pair_name: &str,
    ) -> Result<(BTreeMap<String, Entry>, Vec<String>, bool)> {
        // The concurrent tree walk lives in the Remote impl; here we supply a
        // progress sink (log line + structured ScanProgress) and flatten the
        // (relpath, entry) pairs into a map.
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
        let scan = self.remote.list_tree(base, &progress)?;
        let mut out = BTreeMap::new();
        for (rel, e) in scan.entries {
            out.insert(rel, e);
        }
        Ok((out, scan.failed, scan.root_missing))
    }

    // --- planning -----------------------------------------------------------
    /// Returns the plan, the prospective new baseline, and whether the scan was
    /// INCOMPLETE (some folders unreadable, or a side came back suspiciously
    /// empty). On an incomplete scan the caller must suppress deletions.
    fn plan(&self, pair: &Pair, resync: bool) -> Result<(Plan, BTreeMap<String, Entry>, bool)> {
        // Load the baseline first so the local scan can reuse cached hashes.
        let base: BTreeMap<String, Entry> = if resync {
            BTreeMap::new()
        } else {
            crate::state::load_baseline(&self.cfg.state_dir, &pair.name)?
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

        let local = self.scan_local(&pair.local, &base);
        let (remote, remote_failed, remote_root_missing) =
            self.scan_remote(&pair.remote, &pair.name)?;
        let mut incomplete = !remote_failed.is_empty();
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
                    "{} folder(s) couldn't be listed — synced without deletions, will retry",
                    remote_failed.len()
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

        let cmp = self.cfg.compare;
        let ls = classify(&local, &base, cmp);
        let rs = classify(&remote, &base, cmp);

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

        // Collapse delete+create pairs of identical content into a single
        // rename/move (keeps large files from being re-uploaded on rename).
        // Skip on an incomplete scan: a "missing" source may just be in an
        // unlisted folder, so a rename could wrongly move a remote file.
        if !incomplete {
            detect_renames(&mut plan, &base, &local, &remote);
        }
        Ok((plan, new_base, incomplete))
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
            self.emit_download(path, re, plan, new_base, rc.label());
            return;
        }
        // one side created, other absent
        if lc == Created && rc == Absent {
            self.emit_upload(path, le, plan, new_base, "new local");
            return;
        }
        if rc == Created && lc == Absent {
            self.emit_download(path, re, plan, new_base, "new remote");
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

    fn emit_download(
        &self,
        path: &str,
        e: Option<&Entry>,
        plan: &mut Plan,
        nb: &mut BTreeMap<String, Entry>,
        reason: &str,
    ) {
        plan.add(Op::new(Action::Download, path, false, reason));
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
                plan.add(Op::new(
                    Action::Noop,
                    path,
                    false,
                    "conflict skipped (both changed)",
                ));
            }
            ConflictPolicy::Newer => match (le.and_then(|e| e.mtime), re.and_then(|e| e.mtime)) {
                (Some(lm), Some(rm)) if lm >= rm => {
                    self.emit_upload(path, le, plan, nb, "conflict: local newer")
                }
                (Some(_), Some(_)) => {
                    self.emit_download(path, re, plan, nb, "conflict: remote newer")
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
    fn apply(
        &mut self,
        pair: &Pair,
        plan: Plan,
        mut new_base: BTreeMap<String, Entry>,
    ) -> SyncResult {
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
                return result;
            }
        }

        // Per-file bookkeeping for the additive commit. Files whose transfer
        // FAILED this run are dropped from `new_base` below, so their baseline
        // row is left exactly as it was (or stays absent for a brand-new file)
        // and the file is simply retried next run — never recorded with
        // half-finished state. Genuine deletes/renames drop their old rows.
        let mut failed_up: HashSet<String> = HashSet::new();
        let mut failed_down: HashSet<String> = HashSet::new();
        let mut deleted_ok: HashSet<String> = HashSet::new();

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
                continue;
            }
            if self.dry_run {
                continue;
            }
            self.emit(SyncEvent::OpStarted {
                pair: pair.name.clone(),
                action: op.action.label().to_string(),
                path: op.path.clone(),
            });
            let outcome = self.apply_op(pair, op, &mut new_base);
            let ok = outcome.is_ok();
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
                }
                Err(e) => {
                    let msg = format!("{} {}: {e}", op.action.label(), op.path);
                    self.log.error(&msg);
                    result.errors.push(msg);
                    if op.action == Action::Upload {
                        failed_up.insert(op.path.clone());
                    }
                }
            }
            self.emit(SyncEvent::OpFinished {
                pair: pair.name.clone(),
                action: op.action.label().to_string(),
                path: op.path.clone(),
                ok,
            });
        }

        // Concurrent download phase.
        if !downloads.is_empty() && !self.cancelled() {
            if self.dry_run {
                for op in &downloads {
                    self.log.info(&op.describe());
                }
            } else {
                self.run_downloads(pair, &downloads, &new_base, &mut result, &mut failed_down);
            }
        }

        // A failed transfer must not alter that file's recorded state: drop it
        // from the prospective baseline so its previous row survives untouched
        // (or stays absent for a brand-new file). Next run re-detects the change
        // and retries it in the correct direction — a timeout on one file never
        // invalidates the rest of the sync.
        for rel in failed_up.iter().chain(failed_down.iter()) {
            new_base.remove(rel);
        }

        // Additive per-file commit: only files that fully synced this run are
        // written (as 'synced'); unseen rows are left intact, so a partial or
        // interrupted run still persists exactly the files that did complete.
        if !self.dry_run {
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
            let retrying = failed_down.len() + failed_up.len();
            if retrying > 0 {
                self.log.warn(&format!(
                    "{retrying} file(s) failed for {:?}; they retry next run.",
                    pair.name
                ));
            }
            let _ = crate::state::set_last_synced(
                &self.cfg.state_dir,
                &pair.name,
                crate::datefmt::now_epoch(),
            );
        }
        result
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
        failed_down: &mut HashSet<String>,
    ) {
        let mut jobs: Vec<DownloadJob> = Vec::new();
        for op in downloads {
            // Guard the destination against a remote-controlled name escaping
            // the sync root; skip (don't abort the batch) anything unsafe.
            let local_full = match safe_join(&pair.local, &op.path) {
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
            jobs.push(DownloadJob {
                rel: op.path.clone(),
                remote_path: remote_join(&pair.remote, &op.path),
                dest_dir: dest_dir.to_string_lossy().into_owned(),
                mtime: new_base.get(&op.path).and_then(|e| e.mtime),
            });
        }

        let events = self.events;
        let log = self.log;
        let pair_name = pair.name.as_str();
        let local_root = &pair.local;
        let applied = AtomicUsize::new(0);
        let errs: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let failed_rels: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let fallback = AtomicBool::new(false);
        let cancel_ref: &AtomicBool = self.cancel.unwrap_or(&fallback);

        let on_start = |j: &DownloadJob| {
            log.info(&format!("download    file {} (new remote)", j.rel));
            if let Some(s) = events {
                s.emit(&SyncEvent::OpStarted {
                    pair: pair_name.to_string(),
                    action: "download".into(),
                    path: j.rel.clone(),
                });
            }
        };
        let on_done = |j: &DownloadJob, r: std::result::Result<(), String>| {
            let ok = r.is_ok();
            match r {
                Ok(()) => {
                    applied.fetch_add(1, Ordering::Relaxed);
                    match_mtime(&local_root.join(&j.rel), j.mtime);
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
        failed_down.extend(failed_rels.into_inner().unwrap());
    }

    fn apply_op(
        &mut self,
        pair: &Pair,
        op: &Op,
        new_base: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        let local_full = safe_join(&pair.local, &op.path)?;
        let remote_full = remote_join(&pair.remote, &op.path);
        match op.action {
            Action::MkdirRemote => self.ensure_remote_dir(&pair.remote, &op.path)?,
            Action::MkdirLocal => {
                std::fs::create_dir_all(&local_full)?;
            }
            Action::Upload => {
                let parent = parent_rel(&op.path);
                self.ensure_remote_dir(&pair.remote, parent)?;
                self.remote.upload(
                    &local_full.to_string_lossy(),
                    &remote_join(&pair.remote, parent),
                )?;
            }
            Action::Download => {
                let dest_dir = local_full
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(pair.local.clone());
                std::fs::create_dir_all(&dest_dir)?;
                self.remote
                    .download(&remote_full, &dest_dir.to_string_lossy())?;
                if let Some(e) = new_base.get(&op.path) {
                    match_mtime(&local_full, e.mtime);
                }
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

    fn resolve_conflict(
        &mut self,
        pair: &Pair,
        rel: &str,
        new_base: &mut BTreeMap<String, Entry>,
    ) -> Result<()> {
        let local_full = pair.local.join(rel);
        let stamp = epoch_to_stamp(now_epoch());
        let conflict_rel = conflict_name(rel, &stamp);
        let conflict_full = pair.local.join(&conflict_rel);
        self.log
            .info(&format!("  conflict: keeping local copy as {conflict_rel}"));
        if local_full.exists() {
            std::fs::rename(&local_full, &conflict_full)?;
        }
        // bring the remote version down to the original name
        let dest_dir = local_full
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(pair.local.clone());
        std::fs::create_dir_all(&dest_dir)?;
        self.remote
            .download(&remote_join(&pair.remote, rel), &dest_dir.to_string_lossy())?;
        // push the renamed local copy up so both versions exist on both sides
        let parent = parent_rel(&conflict_rel);
        self.ensure_remote_dir(&pair.remote, parent)?;
        self.remote.upload(
            &conflict_full.to_string_lossy(),
            &remote_join(&pair.remote, parent),
        )?;
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
    pub fn sync_pair(&mut self, pair: &Pair, resync: bool) -> Result<SyncResult> {
        self.log.info(&format!(
            "=== pair {:?} : {} <-> {} ===",
            pair.name,
            pair.local.display(),
            pair.remote
        ));
        self.emit(SyncEvent::PairStarted {
            pair: pair.name.clone(),
        });
        let (mut plan, new_base, incomplete) = self.plan(pair, resync)?;
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
        let result = self.apply(pair, plan, new_base);
        self.emit(SyncEvent::PairFinished {
            pair: pair.name.clone(),
            applied: result.applied,
            errors: result.errors.len(),
            tracked,
        });
        Ok(result)
    }
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
fn rewrite_renames<'a>(
    plan: &mut Plan,
    del: Action,
    create: Action,
    rename: Action,
    from_entry: impl Fn(&str) -> Option<&'a Entry>,
    to_entry: impl Fn(&str) -> Option<&'a Entry>,
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
            if is_rename_match(fe, te) {
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
fn detect_renames(
    plan: &mut Plan,
    base: &BTreeMap<String, Entry>,
    local: &BTreeMap<String, Entry>,
    remote: &BTreeMap<String, Entry>,
) {
    rewrite_renames(
        plan,
        Action::DeleteRemote,
        Action::Upload,
        Action::RenameRemote,
        |p| base.get(p),
        |p| local.get(p),
    );
    rewrite_renames(
        plan,
        Action::DeleteLocal,
        Action::Download,
        Action::RenameLocal,
        |p| base.get(p),
        |p| remote.get(p),
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

fn which_gio() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("gio");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::safe_join;
    use std::path::Path;

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
}
