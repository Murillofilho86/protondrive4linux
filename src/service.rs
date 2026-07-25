//! UI-agnostic controller for a rich frontend.
//!
//! A [`Controller`] owns the config and an observable [`AppState`]. The GUI (or
//! any frontend) reads a cheap `snapshot()` each frame and issues commands
//! (`sync`, `cancel`, `start_watch`, ...). All the slow work runs on background
//! threads; a [`StateSink`] turns engine [`SyncEvent`]s into the observable
//! state (per-pair phase, progress, activity feed), so the UI just renders it.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::config::{self, Config, Pair};
use crate::datefmt::now_epoch;
use crate::engine::run_sync_with;
use crate::events::{EventSink, SyncEvent};
use crate::logger::Logger;
use crate::protoncli::{ProtonCli, Remote};
use crate::stats::Stats;
use crate::watcher;

const ACTIVITY_CAP: usize = 500;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Scanning,
    Syncing,
    Synced,
    Error,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
}

impl Progress {
    /// 0.0..=1.0 for a progress bar (0 when nothing to do).
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairState {
    pub name: String,
    pub local: String,
    pub remote: String,
    pub phase: Phase,
    pub progress: Progress,
    pub scanned_folders: usize,
    pub current_op: Option<String>,
    pub last_error: Option<String>,
    pub last_synced: Option<i64>, // epoch seconds
    pub tracked: usize,
}

impl PairState {
    fn from_pair(p: &Pair) -> Self {
        PairState {
            name: p.name.clone(),
            local: p.local.display().to_string(),
            remote: p.remote.clone(),
            phase: Phase::Idle,
            progress: Progress::default(),
            scanned_folders: 0,
            current_op: None,
            last_error: None,
            last_synced: None,
            tracked: 0,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccountState {
    pub checked: bool,
    pub binary_found: bool,
    pub signed_in: bool,
    pub version: String,
    /// A login/version check is in flight (drives the Refresh spinner).
    pub checking: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ActivityKind {
    Info,
    Sync,
    Error,
}

/// A per-file operation, for rendering a rich activity table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityOp {
    pub action: String, // "upload" | "download" | "delete" | ...
    pub path: String,
    pub pair: String,
    pub ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityItem {
    pub ts: i64,
    pub kind: ActivityKind,
    pub text: String,
    /// Present for per-file operations; None for info/error/summary lines.
    pub op: Option<ActivityOp>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppState {
    pub pairs: Vec<PairState>,
    pub account: AccountState,
    pub activity: VecDeque<ActivityItem>,
    /// Operations currently in flight (rendered as live "uploading…" rows).
    pub active: Vec<ActivityOp>,
    pub busy: bool,
    pub watching: bool,
    /// The watch daemon has detected the proton-drive session is signed out.
    /// Surfaced as a distinct "sign in to resume" banner instead of error spam.
    #[serde(default)]
    pub signed_out: bool,
}

impl AppState {
    fn pair_mut(&mut self, name: &str) -> Option<&mut PairState> {
        self.pairs.iter_mut().find(|p| p.name == name)
    }

    fn push(&mut self, kind: ActivityKind, text: impl Into<String>) {
        self.activity.push_back(ActivityItem {
            ts: now_epoch(),
            kind,
            text: text.into(),
            op: None,
        });
        self.trim();
    }

    fn push_op(&mut self, op: ActivityOp) {
        let kind = if op.ok {
            ActivityKind::Sync
        } else {
            ActivityKind::Error
        };
        let text = format!("{} {}", op.action, op.path);
        self.activity.push_back(ActivityItem {
            ts: now_epoch(),
            kind,
            text,
            op: Some(op),
        });
        self.trim();
    }

    fn trim(&mut self) {
        while self.activity.len() > ACTIVITY_CAP {
            self.activity.pop_front();
        }
    }
}

/// Turns engine events into observable [`AppState`]. Thread-safe: `emit` may be
/// called concurrently (scan workers), so it just locks briefly.
pub struct StateSink {
    state: Arc<Mutex<AppState>>,
    stats: Option<Arc<Stats>>,
}

impl EventSink for StateSink {
    fn emit(&self, ev: &SyncEvent) {
        let mut s = self.state.lock().unwrap();
        match ev {
            SyncEvent::PairStarted { pair } => {
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = Phase::Scanning;
                    p.progress = Progress::default();
                    p.scanned_folders = 0;
                    p.current_op = None;
                    p.last_error = None;
                }
            }
            SyncEvent::ScanStarted { pair } => {
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = Phase::Scanning;
                }
            }
            SyncEvent::ScanProgress {
                pair,
                folders,
                current,
            } => {
                let cur = current.clone();
                if let Some(p) = s.pair_mut(pair) {
                    p.scanned_folders = *folders;
                    p.current_op = Some(format!("scanning {cur}"));
                }
            }
            SyncEvent::Planned {
                pair,
                total_ops,
                counts,
            } => {
                let summary = counts
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = if *total_ops == 0 {
                        Phase::Synced
                    } else {
                        Phase::Syncing
                    };
                    p.progress = Progress {
                        done: 0,
                        total: *total_ops,
                    };
                    p.current_op = None;
                }
                if *total_ops > 0 {
                    s.push(ActivityKind::Info, format!("{pair}: {summary}"));
                }
            }
            SyncEvent::OpStarted { pair, action, path } => {
                let label = format!("{action} {path}");
                if let Some(p) = s.pair_mut(pair) {
                    p.current_op = Some(label);
                }
                s.active.push(ActivityOp {
                    action: action.clone(),
                    path: path.clone(),
                    pair: pair.clone(),
                    ok: true,
                });
            }
            SyncEvent::OpFinished {
                pair,
                action,
                path,
                ok,
            } => {
                let ok = *ok;
                if let Some(p) = s.pair_mut(pair) {
                    p.progress.done += 1;
                    if !ok {
                        p.last_error = Some("an operation failed".into());
                    }
                }
                s.active
                    .retain(|a| !(a.pair == *pair && a.path == *path && a.action == *action));
                s.push_op(ActivityOp {
                    action: action.clone(),
                    path: path.clone(),
                    pair: pair.clone(),
                    ok,
                });
                drop(s);
                if let Some(st) = &self.stats {
                    let _ = st.record_op(pair, action, path, ok, now_epoch());
                }
            }
            SyncEvent::PairFinished {
                pair,
                applied,
                errors,
                tracked,
            } => {
                let (applied, errors, tracked) = (*applied, *errors, *tracked);
                if let Some(p) = s.pair_mut(pair) {
                    p.phase = if errors > 0 {
                        Phase::Error
                    } else {
                        Phase::Synced
                    };
                    p.progress.done = p.progress.total;
                    p.current_op = None;
                    p.tracked = tracked;
                    if errors == 0 {
                        p.last_synced = Some(now_epoch());
                        p.last_error = None;
                    }
                }
                // Only note completion when something actually happened — a
                // "0 applied, 0 errors" line is just noise.
                if applied > 0 || errors > 0 {
                    let kind = if errors > 0 {
                        ActivityKind::Error
                    } else {
                        ActivityKind::Sync
                    };
                    let msg = if errors > 0 {
                        format!("{pair}: {applied} applied, {errors} error(s)")
                    } else {
                        format!("{pair}: {applied} applied")
                    };
                    s.push(kind, msg);
                }
            }
            SyncEvent::Auth { signed_in } => {
                s.signed_out = !*signed_in;
                if *signed_in {
                    s.push(
                        ActivityKind::Info,
                        "Signed back in to Proton — resuming sync".to_string(),
                    );
                } else {
                    s.push(
                        ActivityKind::Error,
                        "Signed out of Proton — sign in on the Account tab to resume".to_string(),
                    );
                }
            }
            SyncEvent::Info { text } => s.push(ActivityKind::Info, text.clone()),
            SyncEvent::Error { pair, text } => {
                if let Some(name) = pair {
                    let t = text.clone();
                    if let Some(p) = s.pair_mut(name) {
                        p.phase = Phase::Error;
                        p.last_error = Some(t);
                    }
                }
                s.push(ActivityKind::Error, text.clone());
            }
        }
    }
}

/// The backend a rich frontend binds to. Cheap to clone-snapshot; commands are
/// non-blocking (they spawn background threads).
pub struct Controller {
    cfg: Mutex<Config>,
    state: Arc<Mutex<AppState>>,
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    watch_stop: Mutex<Option<Arc<AtomicBool>>>,
    stats: Option<Arc<Stats>>,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        let stats = Stats::open(&cfg.state_dir).ok().map(Arc::new);
        // Seed last_synced from the DB so a folder that has synced before isn't
        // shown as "Initializing…" on a fresh process start.
        let pairs: Vec<PairState> = cfg
            .pairs
            .iter()
            .map(|p| {
                let mut ps = PairState::from_pair(p);
                if let Some(st) = &stats {
                    if let Ok(ls) = st.last_synced(&p.name) {
                        ps.last_synced = ls;
                    }
                }
                ps
            })
            .collect();

        // restore the recent activity feed from the DB so it survives restarts
        let mut activity: VecDeque<ActivityItem> = VecDeque::new();
        if let Some(st) = &stats {
            let _ = st.prune_ops(1000);
            if let Ok(recs) = st.recent_ops(100) {
                for r in recs.into_iter().rev() {
                    let kind = if r.ok {
                        ActivityKind::Sync
                    } else {
                        ActivityKind::Error
                    };
                    let text = format!("{} {}", r.action, r.path);
                    activity.push_back(ActivityItem {
                        ts: r.ts,
                        kind,
                        text,
                        op: Some(ActivityOp {
                            action: r.action,
                            path: r.path,
                            pair: r.pair,
                            ok: r.ok,
                        }),
                    });
                }
            }
        }

        let c = Controller {
            cfg: Mutex::new(cfg),
            state: Arc::new(Mutex::new(AppState {
                pairs,
                activity,
                ..Default::default()
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            watch_stop: Mutex::new(None),
            stats,
        };
        c.refresh_account();
        c
    }

    /// A cheap clone of the current state - call this each frame.
    pub fn snapshot(&self) -> AppState {
        self.state.lock().unwrap().clone()
    }

    /// Publish a compact live snapshot to `<state_dir>/status.json` so a
    /// separate process (an open GUI window) can display what THIS process is
    /// doing. Used by the headless tray daemon, which owns the watcher/sync
    /// while a window is open: without this the window has no live view of the
    /// daemon's work. Best-effort and atomic (write-temp-then-rename); the
    /// activity feed is trimmed to keep the file small.
    pub fn publish_status(&self) {
        let mut snap = self.snapshot();
        const KEEP: usize = 100;
        if snap.activity.len() > KEEP {
            let drop = snap.activity.len() - KEEP;
            snap.activity.drain(0..drop);
        }
        let dir = self.cfg.lock().unwrap().state_dir.clone();
        let path = dir.join("status.json");
        let tmp = dir.join("status.json.tmp");
        if let Ok(json) = serde_json::to_vec(&snap) {
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// Read a live snapshot published by another process (see
    /// [`Controller::publish_status`]). `None` if absent or unparsable.
    pub fn read_status(state_dir: &Path) -> Option<AppState> {
        let data = std::fs::read(state_dir.join("status.json")).ok()?;
        serde_json::from_slice(&data).ok()
    }

    /// The current config (the frontend edits a copy, then `commit_config`).
    pub fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    /// Replace the config and refresh the observable pair list (preserving the
    /// phase/progress of pairs whose names are unchanged).
    pub fn commit_config(&self, cfg: Config) {
        *self.cfg.lock().unwrap() = cfg;
        self.rebuild_pairs();
    }

    fn rebuild_pairs(&self) {
        let cfg = self.cfg.lock().unwrap();
        let mut s = self.state.lock().unwrap();
        let old: HashMap<String, PairState> =
            s.pairs.drain(..).map(|p| (p.name.clone(), p)).collect();
        s.pairs = cfg
            .pairs
            .iter()
            .map(|p| {
                let mut ps = old
                    .get(&p.name)
                    .cloned()
                    .unwrap_or_else(|| PairState::from_pair(p));
                ps.local = p.local.display().to_string();
                ps.remote = p.remote.clone();
                ps
            })
            .collect();
    }

    /// Persist the config to `path`.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let cfg = self.cfg.lock().unwrap().clone();
        config::save(&cfg, path)
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn is_watching(&self) -> bool {
        self.watch_stop.lock().unwrap().is_some()
    }

    /// Reconcile the named pairs (empty = all) on a background thread.
    pub fn sync(&self, names: Vec<String>, dry_run: bool) {
        if self.busy.swap(true, Ordering::SeqCst) {
            return; // already running
        }
        self.cancel.store(false, Ordering::SeqCst);
        let cfg = self.cfg.lock().unwrap().clone();
        let pairs = select_pairs(&cfg, &names);
        {
            let mut s = self.state.lock().unwrap();
            s.busy = true;
            for p in &mut s.pairs {
                if names.is_empty() || names.iter().any(|n| n == &p.name) {
                    p.phase = Phase::Scanning;
                    p.progress = Progress::default();
                    p.current_op = None;
                    p.last_error = None;
                }
            }
        }
        let state = self.state.clone();
        let cancel = self.cancel.clone();
        let busy = self.busy.clone();
        let stats = self.stats.clone();
        thread::spawn(move || {
            let sink = StateSink {
                state: state.clone(),
                stats,
            };
            let log = Logger::new(&cfg.state_dir.join("logs"), false, false);
            run_sync_with(
                &cfg,
                &pairs,
                dry_run,
                false,
                &log,
                Some(&sink),
                Some(&cancel),
            );
            busy.store(false, Ordering::SeqCst);
            let mut g = state.lock().unwrap();
            g.busy = false;
            g.active.clear();
        });
    }

    /// Request cancellation of the current sync (stops before the next op).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Start the watch daemon for the named pairs (empty = all).
    pub fn start_watch(&self, names: Vec<String>) {
        let mut guard = self.watch_stop.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let cfg = self.cfg.lock().unwrap().clone();
        let pairs = select_pairs(&cfg, &names);
        let stop = Arc::new(AtomicBool::new(false));
        *guard = Some(stop.clone());
        self.state.lock().unwrap().watching = true;
        let state = self.state.clone();
        let stats = self.stats.clone();
        thread::spawn(move || {
            let sink = StateSink {
                state: state.clone(),
                stats,
            };
            let log = Logger::new(&cfg.state_dir.join("logs"), false, false);
            if let Err(e) = watcher::watch_with(&cfg, pairs, &log, &stop, Some(&sink)) {
                let mut s = state.lock().unwrap();
                s.push(ActivityKind::Error, format!("watch: {e}"));
            }
            let mut g = state.lock().unwrap();
            g.watching = false;
            g.active.clear();
        });
    }

    pub fn stop_watch(&self) {
        if let Some(stop) = self.watch_stop.lock().unwrap().take() {
            stop.store(true, Ordering::SeqCst);
        }
        self.state.lock().unwrap().watching = false;
    }

    /// Forget a folder's stored data (baseline, per-file state, stats, activity)
    /// so a later folder of the same name can't inherit stale state. The caller
    /// removes it from the config separately.
    pub fn forget_pair(&self, name: &str) {
        if let Some(st) = &self.stats {
            let _ = st.forget_pair(name);
        }
        let mut s = self.state.lock().unwrap();
        s.pairs.retain(|p| p.name != name);
    }

    /// Wipe all NeutronSync metadata — baseline, per-file state, hot-folder
    /// stats and the activity feed — and stop watching. Local files are NOT
    /// touched. The caller clears the folder list from the config.
    pub fn reset_data(&self) {
        self.stop_watch();
        if let Some(st) = &self.stats {
            let _ = st.wipe();
        }
        let mut s = self.state.lock().unwrap();
        s.pairs.clear();
        s.activity.clear();
        s.active.clear();
        s.watching = false;
    }

    /// Refresh the account panel (binary present? logged in? version) on a
    /// background thread.
    pub fn refresh_account(&self) {
        let cfg = self.cfg.lock().unwrap().clone();
        let state = self.state.clone();
        state.lock().unwrap().account.checking = true;
        thread::spawn(move || {
            let proton = ProtonCli::new(&cfg);
            let account = match proton.resolve_binary() {
                Some(p) => AccountState {
                    checked: true,
                    binary_found: true,
                    signed_in: proton.list_dir(&cfg.remote_root).is_ok(),
                    version: format!("{} ({})", proton.version(), p.display()),
                    checking: false,
                },
                None => AccountState {
                    checked: true,
                    binary_found: false,
                    signed_in: false,
                    version: "proton-drive NOT FOUND on PATH".into(),
                    checking: false,
                },
            };
            state.lock().unwrap().account = account;
        });
    }
}

fn select_pairs(cfg: &Config, names: &[String]) -> Vec<Pair> {
    if names.is_empty() {
        cfg.pairs.clone()
    } else {
        cfg.pairs
            .iter()
            .filter(|p| names.iter().any(|n| n == &p.name))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(names: &[&str]) -> Arc<Mutex<AppState>> {
        let pairs = names
            .iter()
            .map(|n| PairState {
                name: n.to_string(),
                local: String::new(),
                remote: String::new(),
                phase: Phase::Idle,
                progress: Progress::default(),
                scanned_folders: 0,
                current_op: None,
                last_error: None,
                last_synced: None,
                tracked: 0,
            })
            .collect();
        Arc::new(Mutex::new(AppState {
            pairs,
            ..Default::default()
        }))
    }

    #[test]
    fn sink_maps_events_to_state() {
        let st = state_with(&["docs"]);
        let sink = StateSink {
            state: st.clone(),
            stats: None,
        };
        sink.emit(&SyncEvent::PairStarted {
            pair: "docs".into(),
        });
        sink.emit(&SyncEvent::Planned {
            pair: "docs".into(),
            total_ops: 2,
            counts: vec![("upload".into(), 2)],
        });
        assert_eq!(st.lock().unwrap().pairs[0].phase, Phase::Syncing);
        sink.emit(&SyncEvent::OpFinished {
            pair: "docs".into(),
            action: "upload".into(),
            path: "a".into(),
            ok: true,
        });
        assert_eq!(st.lock().unwrap().pairs[0].progress.done, 1);
        sink.emit(&SyncEvent::PairFinished {
            pair: "docs".into(),
            applied: 2,
            errors: 0,
            tracked: 5,
        });
        let s = st.lock().unwrap();
        assert_eq!(s.pairs[0].phase, Phase::Synced);
        assert_eq!(s.pairs[0].tracked, 5);
        assert!(s.pairs[0].last_synced.is_some());
        assert!(s.activity.iter().any(|a| a.text.contains("2 applied")));
    }

    #[test]
    fn progress_fraction() {
        let p = Progress { done: 1, total: 4 };
        assert!((p.fraction() - 0.25).abs() < 1e-6);
        assert_eq!(Progress { done: 0, total: 0 }.fraction(), 0.0);
    }
}
