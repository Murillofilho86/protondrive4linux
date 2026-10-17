//! The in-memory fake `Remote` and the small helpers the engine-level tests
//! share (`tests/engine.rs`, `tests/data_loss.rs`): the real three-way-merge
//! logic end to end without the actual proton-drive binary.
//!
//! Include it with `#[path = "common/fake_remote.rs"] mod fake_remote;`.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use neutronsync::config::{Config, ConflictPolicy, LocalDelete, Pair, UpdateChannel};
use neutronsync::engine::Engine;
use neutronsync::logger::Logger;
use neutronsync::models::{Compare, Entry};
use neutronsync::protoncli::{ListOutcome, Remote};

/// Called at the start of a download with the remote path (see `Store::on_download`).
pub type DownloadHook = Rc<dyn Fn(&str)>;

// --- in-memory fake remote --------------------------------------------------
#[derive(Default)]
pub struct Store {
    pub files: BTreeMap<String, Vec<u8>>, // absolute proton path -> bytes
    pub dirs: BTreeSet<String>,
    // Remote mtime per file (epoch secs), for ConflictPolicy::Newer tests. A
    // file with no entry here is listed with mtime: None, matching how a real
    // remote entry can legitimately lack claimedModificationTime.
    pub mtimes: BTreeMap<String, i64>,
    pub moves: usize,                  // count of rename/move calls
    pub fail_dir: Option<String>,      // absolute path whose listing should error
    pub notfound_dir: Option<String>,  // absolute path whose probe reports NotFound
    pub fail_download: Option<String>, // absolute file path whose download should error
    pub fail_upload: Option<String>,   // absolute file path whose upload should error
    // Total connectivity loss (M1-007): every list/upload/download call fails,
    // like a real proton-drive CLI with no network. Distinct from fail_dir/
    // fail_download/fail_upload, which target one specific path.
    pub offline: bool,
    // Called at the start of every download with the remote path: lets a test
    // act as "the user, mid-run" (edit a local file, create one, ...).
    pub on_download: Option<DownloadHook>,
    // absolute remote path whose download "succeeds" but delivers only the first N bytes
    pub truncate_download: Option<(String, usize)>,
    // absolute remote path whose download writes the first N bytes then FAILS (disk full, killed CLI)
    pub partial_then_fail: Option<(String, usize)>,
    // List every file with the SHA-1 of its content, as the real CLI does when the
    // revision carries `claimedDigests` (off by default: a remote entry can
    // legitimately lack one).
    pub report_sha1: bool,
}

pub struct FakeRemote {
    pub store: Rc<RefCell<Store>>,
}

impl FakeRemote {
    pub fn new() -> (Self, Rc<RefCell<Store>>) {
        let mut s = Store::default();
        s.dirs.insert("/my-files".to_string());
        let rc = Rc::new(RefCell::new(s));
        (FakeRemote { store: rc.clone() }, rc)
    }
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

impl Remote for FakeRemote {
    fn list_dir_probe(&self, remote_path: &str) -> anyhow::Result<ListOutcome> {
        if self.store.borrow().offline {
            anyhow::bail!("simulated offline: no connection to Proton Drive");
        }
        if self.store.borrow().notfound_dir.as_deref() == Some(remote_path.trim_end_matches('/')) {
            return Ok(ListOutcome::NotFound);
        }
        Ok(ListOutcome::Listed(self.list_dir(remote_path)?))
    }

    fn list_dir(&self, remote_path: &str) -> anyhow::Result<Vec<Entry>> {
        let prefix = format!("{}/", remote_path.trim_end_matches('/'));
        let s = self.store.borrow();
        if s.offline {
            anyhow::bail!("simulated offline: no connection to Proton Drive");
        }
        if s.fail_dir.as_deref() == Some(remote_path.trim_end_matches('/')) {
            anyhow::bail!("simulated listing failure for {remote_path}");
        }
        let mut out = Vec::new();
        for d in &s.dirs {
            if let Some(rest) = d.strip_prefix(&prefix) {
                if !rest.is_empty() && !rest.contains('/') {
                    out.push(Entry {
                        path: rest.to_string(),
                        is_dir: true,
                        ..Default::default()
                    });
                }
            }
        }
        for (f, data) in &s.files {
            if let Some(rest) = f.strip_prefix(&prefix) {
                if !rest.contains('/') {
                    out.push(Entry {
                        path: rest.to_string(),
                        is_dir: false,
                        size: data.len() as u64,
                        mtime: s.mtimes.get(f).copied(),
                        sha1: s
                            .report_sha1
                            .then(|| sha1_smol::Sha1::from(data).digest().to_string()),
                        ..Default::default()
                    });
                }
            }
        }
        Ok(out)
    }

    fn create_folder(&self, parent: &str, name: &str) -> anyhow::Result<()> {
        self.store
            .borrow_mut()
            .dirs
            .insert(format!("{}/{}", parent.trim_end_matches('/'), name));
        Ok(())
    }

    fn upload(&self, local_path: &str, remote_parent: &str) -> anyhow::Result<()> {
        if self.store.borrow().offline {
            anyhow::bail!("simulated offline: no connection to Proton Drive");
        }
        let data = std::fs::read(local_path)?;
        let key = format!(
            "{}/{}",
            remote_parent.trim_end_matches('/'),
            basename(local_path)
        );
        if self.store.borrow().fail_upload.as_deref() == Some(key.as_str()) {
            anyhow::bail!("simulated upload failure for {key}");
        }
        self.store.borrow_mut().files.insert(key, data);
        Ok(())
    }

    fn download(&self, remote_path: &str, local_dest: &str) -> anyhow::Result<()> {
        let hook = self.store.borrow().on_download.clone();
        if let Some(h) = hook {
            h(remote_path);
        }
        if self.store.borrow().offline {
            anyhow::bail!("simulated offline: no connection to Proton Drive");
        }
        if self.store.borrow().fail_download.as_deref() == Some(remote_path) {
            anyhow::bail!("simulated download failure for {remote_path}");
        }
        let data = self
            .store
            .borrow()
            .files
            .get(remote_path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such remote file: {remote_path}"))?;
        std::fs::create_dir_all(local_dest)?;
        let target = Path::new(local_dest).join(basename(remote_path));
        let tr = self.store.borrow().truncate_download.clone();
        if let Some((p, n)) = tr {
            if p == remote_path {
                std::fs::write(&target, &data[..n.min(data.len())])?;
                return Ok(());
            }
        }
        let pf = self.store.borrow().partial_then_fail.clone();
        if let Some((p, n)) = pf {
            if p == remote_path {
                std::fs::write(&target, &data[..n.min(data.len())])?;
                anyhow::bail!("simulated: no space left on device");
            }
        }
        std::fs::write(&target, data)?;
        Ok(())
    }

    fn trash(&self, remote_path: &str) -> anyhow::Result<()> {
        let p = remote_path.trim_end_matches('/').to_string();
        let mut s = self.store.borrow_mut();
        s.files.remove(&p);
        s.dirs.remove(&p);
        let pref = format!("{p}/");
        s.files.retain(|k, _| !k.starts_with(&pref));
        s.dirs.retain(|k| !k.starts_with(&pref));
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> anyhow::Result<()> {
        let mut s = self.store.borrow_mut();
        let data = s
            .files
            .remove(from)
            .ok_or_else(|| anyhow::anyhow!("no such remote file: {from}"))?;
        s.files.insert(to.to_string(), data);
        s.moves += 1;
        Ok(())
    }

    // Mirrors ProtonCli::list_tree's root-vs-subfolder NotFound distinction
    // (protoncli.rs) via this fake's own `list_dir_probe`, so whole-pair tests
    // (which go through `Engine::sync_pair` -> `list_tree`, not the trait's
    // default that hard-codes `root_missing: false`) can exercise the
    // `remote_root_missing` guard in engine.rs.
    fn list_tree(
        &self,
        base: &str,
        exclude: &(dyn Fn(&str) -> bool + Sync),
        progress: &(dyn Fn(&str) + Sync),
    ) -> anyhow::Result<neutronsync::models::TreeScan> {
        let mut out = Vec::new();
        let mut failed = Vec::new();
        let mut root_missing = false;
        let mut stack = vec![String::new()];
        while let Some(rel) = stack.pop() {
            let here = neutronsync::config::remote_join(base, &rel);
            progress(&here);
            let entries = match self.list_dir_probe(&here) {
                Ok(ListOutcome::Listed(entries)) => entries,
                Ok(ListOutcome::NotFound) if rel.is_empty() => {
                    root_missing = true;
                    Vec::new()
                }
                // A subfolder reporting NotFound is a race/transient, not an
                // authoritative "base is gone" - treat as a failed listing so
                // deletions are suppressed this run without flagging root_missing.
                Ok(ListOutcome::NotFound) => {
                    failed.push(rel);
                    continue;
                }
                Err(_) => {
                    failed.push(rel);
                    continue;
                }
            };
            for e in entries {
                let child_rel = if rel.is_empty() {
                    e.path.clone()
                } else {
                    format!("{rel}/{}", e.path)
                };
                if exclude(&child_rel) {
                    continue;
                }
                let is_dir = e.is_dir;
                out.push((
                    child_rel.clone(),
                    Entry {
                        path: child_rel.clone(),
                        ..e
                    },
                ));
                if is_dir {
                    stack.push(child_rel);
                }
            }
        }
        Ok(neutronsync::models::TreeScan {
            entries: out,
            failed,
            root_missing,
        })
    }
}

// --- helpers ----------------------------------------------------------------
pub fn cfg(root: &Path) -> Config {
    Config {
        binary: "proton-drive".into(),
        upload_flags: vec![],
        download_flags: vec![],
        credentials_store: None,
        fresh_cache: false,
        scan_threads: 0,
        download_threads: 0,
        cli_timeout_secs: 30,
        retry_base_delay_secs: 30,
        retry_max_delay_secs: 1800,
        retry_max_attempts: 6,
        upload_limit_kbps: 0,
        download_limit_kbps: 0,
        upload_limiter: neutronsync::ratelimit::RateLimiter::new(),
        download_limiter: neutronsync::ratelimit::RateLimiter::new(),
        remote_root: "/my-files".into(),
        propagate_deletes: false,
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
        theme: neutronsync::config::ThemePref::System,
        file_manager: false,
        state_dir: root.join("state"),
        pairs: vec![Pair {
            name: "docs".into(),
            local: root.join("local"),
            remote: "/my-files/docs".into(),
            auto: true,
            exclude: vec![],
        }],
        source_path: None,
    }
}

pub fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

pub fn set_local_mtime(p: &Path, secs: u64) {
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

pub fn run(cfg: &Config, remote: FakeRemote, resync: bool) {
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.sync_pair(&cfg.pairs[0], resync).unwrap();
}

// Like `run`, but surfaces the pair result instead of unwrapping — used to
// assert that a pair is REFUSED (e.g. a vanished local root).
pub fn try_run(cfg: &Config, remote: FakeRemote, resync: bool) -> anyhow::Result<()> {
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.sync_pair(&cfg.pairs[0], resync).map(|_| ())
}

// A fresh failure puts a path in retry backoff (M4-004), so it's skipped -
// not re-attempted - on the very next run. Tests below that simulate "the
// underlying problem clears, then retry" (not M4-004's backoff timing itself)
// call this between the failing run and the retry run to simulate the
// backoff window having elapsed, by forcing every pending path's `next_at`
// into the past while keeping its recorded attempt count.
pub fn clear_retry_backoff(cfg: &Config, pair: &str) {
    let state = neutronsync::state::retry_state(&cfg.state_dir, pair).unwrap();
    let upserts: Vec<(String, u32, i64)> = state
        .into_iter()
        .map(|(rel, (attempts, _))| (rel, attempts, 0))
        .collect();
    neutronsync::state::apply_retry_updates(&cfg.state_dir, pair, &upserts, &[]).unwrap();
}

pub fn remote_files(store: &Rc<RefCell<Store>>) -> BTreeMap<String, String> {
    store
        .borrow()
        .files
        .iter()
        .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
        .collect()
}

// A fresh temp dir under the system temp, cleaned at drop.
pub struct Tmp(pub PathBuf);
impl Tmp {
    pub fn new(tag: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("protondrive4linux-test-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Rebuild a `FakeRemote` sharing an existing store (a run consumes the remote).
pub fn reuse(store: &Rc<RefCell<Store>>) -> (FakeRemote, Rc<RefCell<Store>>) {
    (
        FakeRemote {
            store: store.clone(),
        },
        store.clone(),
    )
}

/// chmod-based tests mean nothing to root (it reads and writes anything).
pub fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// The content of every file that still exists anywhere: the remote store and
/// the local tree. For "this edit must survive somewhere" assertions.
pub fn everything(t: &Tmp, store: &Rc<RefCell<Store>>) -> Vec<String> {
    let mut out: Vec<String> = store
        .borrow()
        .files
        .values()
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .collect();
    for e in walkdir::WalkDir::new(t.path().join("local"))
        .into_iter()
        .flatten()
    {
        if e.file_type().is_file() {
            out.push(std::fs::read_to_string(e.path()).unwrap_or_default());
        }
    }
    out
}

/// Names (not paths) directly inside the local pair root.
pub fn local_names(t: &Tmp) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(t.path().join("local"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}
