//! Engine tests using an in-memory fake `Remote` - the real three-way-merge
//! logic end to end without the actual proton-drive binary. Mirrors the Python
//! tests/test_engine.py so the two ports can be diffed for behaviour parity.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use std::sync::atomic::AtomicBool;

use neutronsync::config::{Config, ConflictPolicy, LocalDelete, Pair, UpdateChannel};
use neutronsync::engine::Engine;
use neutronsync::events::{EventSink, SyncEvent};
use neutronsync::logger::Logger;
use neutronsync::models::{Compare, Entry};
use neutronsync::protoncli::Remote;

// --- in-memory fake remote --------------------------------------------------
#[derive(Default)]
struct Store {
    files: BTreeMap<String, Vec<u8>>, // absolute proton path -> bytes
    dirs: BTreeSet<String>,
    moves: usize,                  // count of rename/move calls
    fail_dir: Option<String>,      // absolute path whose listing should error
    fail_download: Option<String>, // absolute file path whose download should error
    fail_upload: Option<String>,   // absolute file path whose upload should error
}

struct FakeRemote {
    store: Rc<RefCell<Store>>,
}

impl FakeRemote {
    fn new() -> (Self, Rc<RefCell<Store>>) {
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
    fn list_dir(&self, remote_path: &str) -> anyhow::Result<Vec<Entry>> {
        let prefix = format!("{}/", remote_path.trim_end_matches('/'));
        let s = self.store.borrow();
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
        std::fs::write(Path::new(local_dest).join(basename(remote_path)), data)?;
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
}

// --- helpers ----------------------------------------------------------------
fn cfg(root: &Path) -> Config {
    Config {
        binary: "proton-drive".into(),
        upload_flags: vec![],
        download_flags: vec![],
        credentials_store: None,
        fresh_cache: false,
        scan_threads: 0,
        download_threads: 0,
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

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn run(cfg: &Config, remote: FakeRemote, resync: bool) {
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.sync_pair(&cfg.pairs[0], resync).unwrap();
}

// Like `run`, but surfaces the pair result instead of unwrapping — used to
// assert that a pair is REFUSED (e.g. a vanished local root).
fn try_run(cfg: &Config, remote: FakeRemote, resync: bool) -> anyhow::Result<()> {
    let log = Logger::silent();
    let mut eng = Engine::new(cfg, remote, &log, false);
    eng.sync_pair(&cfg.pairs[0], resync).map(|_| ())
}

fn remote_files(store: &Rc<RefCell<Store>>) -> BTreeMap<String, String> {
    store
        .borrow()
        .files
        .iter()
        .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
        .collect()
}

// A fresh temp dir under the system temp, cleaned at drop.
struct Tmp(PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("neutronsync-test-{tag}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- tests ------------------------------------------------------------------
#[test]
fn initial_upload() {
    let t = Tmp::new("init");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "hello");
    write(&t.path().join("local/sub/b.txt"), "world");
    run(&c, fake, false);
    let rf = remote_files(&store);
    assert_eq!(
        rf.get("/my-files/docs/a.txt").map(String::as_str),
        Some("hello")
    );
    assert_eq!(
        rf.get("/my-files/docs/sub/b.txt").map(String::as_str),
        Some("world")
    );
}

#[test]
fn download_new_remote() {
    let t = Tmp::new("dl");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false); // establish empty baseline
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/remote.txt".into(), b"from-cloud".to_vec());
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    let got = std::fs::read_to_string(t.path().join("local/remote.txt")).unwrap();
    assert_eq!(got, "from-cloud");
}

#[test]
fn modify_local_uploads() {
    let t = Tmp::new("mod");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "v1");
    run(&c, fake, false);
    write(&t.path().join("local/a.txt"), "v2-longer");
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/a.txt")
            .map(String::as_str),
        Some("v2-longer")
    );
}

#[test]
fn delete_local_no_propagate() {
    let t = Tmp::new("delnop");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "keep");
    run(&c, fake, false);
    std::fs::remove_file(t.path().join("local/a.txt")).unwrap();
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        store.borrow().files.contains_key("/my-files/docs/a.txt"),
        "remote copy preserved"
    );
    assert!(
        !t.path().join("local/a.txt").exists(),
        "not resurrected locally"
    );
}

#[test]
fn delete_local_propagate_trashes_remote() {
    let t = Tmp::new("delprop");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "bye");
    run(&c, fake, false);
    std::fs::remove_file(t.path().join("local/a.txt")).unwrap();
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        !store.borrow().files.contains_key("/my-files/docs/a.txt"),
        "remote copy trashed"
    );
}

#[test]
fn delete_remote_propagate_removes_local() {
    let t = Tmp::new("delrem");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    c.local_delete = LocalDelete::Remove; // deterministic (no real trash)
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/gone.txt"), "data");
    run(&c, fake, false);
    store.borrow_mut().files.remove("/my-files/docs/gone.txt");
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        !t.path().join("local/gone.txt").exists(),
        "local copy removed"
    );
}

#[test]
fn conflict_keep_both() {
    let t = Tmp::new("conf");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "base");
    run(&c, fake, false);
    write(&t.path().join("local/a.txt"), "local-change");
    store.borrow_mut().files.insert(
        "/my-files/docs/a.txt".into(),
        b"remote-change-different".to_vec(),
    );
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    let names: Vec<String> = std::fs::read_dir(t.path().join("local"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("conflict")),
        "conflict copy created: {names:?}"
    );
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "remote-change-different"
    );
    let contents: BTreeSet<String> = remote_files(&store).into_values().collect();
    assert!(
        contents.contains("remote-change-different") && contents.contains("local-change"),
        "both versions on remote"
    );
}

#[test]
fn idempotent_second_run() {
    let t = Tmp::new("idem");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "x");
    write(&t.path().join("local/d/e.txt"), "y");
    run(&c, fake, false);
    // second run: nothing should change
    let (fake2, _s2) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake2, &log, false);
    let res = eng.sync_pair(&c.pairs[0], false).unwrap();
    assert_eq!(res.applied, 0, "second run is a no-op");
}

#[test]
fn manual_xdg_trash_fallback() {
    // trash_local with gio=None must move the file into $XDG_DATA_HOME/Trash.
    let t = Tmp::new("trash");
    let data_home = t.path().join("xdgdata");
    std::env::set_var("XDG_DATA_HOME", &data_home);
    let victim = t.path().join("victim.txt");
    std::fs::write(&victim, "bye").unwrap();
    neutronsync::trash::trash_local(&victim, None).unwrap();
    std::env::remove_var("XDG_DATA_HOME");
    assert!(!victim.exists(), "victim moved out");
    let files = std::fs::read_dir(data_home.join("Trash/files"))
        .unwrap()
        .count();
    assert!(files >= 1, "file landed in XDG trash");
}

#[test]
fn sha1_mode_idempotent() {
    // Exercises the (size,mtime)->sha1 cache path: a second run in sha1 mode,
    // with the file unchanged, must reuse the cached hash and do nothing.
    let t = Tmp::new("sha1");
    let mut c = cfg(t.path());
    c.compare = Compare::Sha1;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "content");
    run(&c, fake, false); // upload; baseline stores the local sha1
    let (fake2, _s2) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake2, &log, false);
    let res = eng.sync_pair(&c.pairs[0], false).unwrap();
    assert_eq!(res.applied, 0, "sha1 second run is a no-op");
}

struct CapSink(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
impl EventSink for CapSink {
    fn emit(&self, ev: &SyncEvent) {
        self.0.lock().unwrap().push(format!("{ev:?}"));
    }
}

#[test]
fn emits_structured_events() {
    let t = Tmp::new("events");
    let c = cfg(t.path());
    let (fake, _store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "hi");
    let evs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = CapSink(evs.clone());
    let sink_ref: &dyn EventSink = &sink;
    let cancel = AtomicBool::new(false);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, fake, &log, false);
    eng.set_observer(Some(sink_ref), Some(&cancel));
    eng.sync_pair(&c.pairs[0], false).unwrap();
    let joined = evs.lock().unwrap().join("\n");
    for want in [
        "PairStarted",
        "ScanStarted",
        "Planned",
        "OpFinished",
        "PairFinished",
    ] {
        assert!(joined.contains(want), "missing {want} in:\n{joined}");
    }
}

// Rebuild a FakeRemote sharing an existing store (a run consumes the remote).
fn reuse(store: &Rc<RefCell<Store>>) -> (FakeRemote, Rc<RefCell<Store>>) {
    (
        FakeRemote {
            store: store.clone(),
        },
        store.clone(),
    )
}

#[test]
fn local_rename_becomes_remote_move() {
    let t = Tmp::new("ren");
    let mut c = cfg(t.path());
    c.propagate_deletes = true; // a rename surfaces as delete-old + create-new
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "hello world");
    run(&c, fake, false); // initial upload + baseline
    std::fs::rename(t.path().join("local/a.txt"), t.path().join("local/b.txt")).unwrap();
    let (fake2, _s2) = reuse(&store);
    run(&c, fake2, false);
    let rf = remote_files(&store);
    assert_eq!(
        rf.get("/my-files/docs/b.txt").map(String::as_str),
        Some("hello world")
    );
    assert!(
        !rf.contains_key("/my-files/docs/a.txt"),
        "old name should be gone"
    );
    assert_eq!(
        store.borrow().moves,
        1,
        "rename should move, not delete + re-upload"
    );
}

#[test]
fn incomplete_remote_scan_suppresses_delete() {
    // A folder that can't be listed must NOT be read as "remote deleted" —
    // deletions are suppressed and the baseline is left untouched, so a
    // transient listing failure can never destroy local data.
    let t = Tmp::new("incomplete");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    {
        let mut s = store.borrow_mut();
        s.dirs.insert("/my-files/docs".into());
        s.dirs.insert("/my-files/docs/sub".into());
        s.files
            .insert("/my-files/docs/sub/a.txt".into(), b"hi".to_vec());
    }
    // First sync: pulls the file down and records a baseline.
    run(&c, fake, false);
    let local_file = t.path().join("local/sub/a.txt");
    assert!(local_file.exists(), "file should download on first sync");

    // Now the subfolder listing fails; a.txt vanishes from the remote view.
    store.borrow_mut().fail_dir = Some("/my-files/docs/sub".into());
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);

    // The local copy must survive (no delete propagated), and the remote file
    // is untouched too.
    assert!(
        local_file.exists(),
        "local file must survive an incomplete remote scan"
    );
    assert!(
        store
            .borrow()
            .files
            .contains_key("/my-files/docs/sub/a.txt"),
        "remote file must be untouched"
    );
}

#[test]
fn downloads_many_new_remote_files() {
    // Several new remote files should all be pulled down (exercises the batch
    // download path; FakeRemote uses the sequential default of download_many).
    let t = Tmp::new("dlmany");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false); // establish empty baseline
    {
        let mut s = store.borrow_mut();
        s.dirs.insert("/my-files/docs/sub".into());
        for i in 0..5 {
            s.files.insert(
                format!("/my-files/docs/f{i}.txt"),
                format!("body {i}").into_bytes(),
            );
        }
        s.files
            .insert("/my-files/docs/sub/deep.txt".into(), b"deep".to_vec());
    }
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    for i in 0..5 {
        let p = t.path().join(format!("local/f{i}.txt"));
        assert!(p.exists(), "f{i}.txt should have downloaded");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("body {i}"));
    }
    assert!(
        t.path().join("local/sub/deep.txt").exists(),
        "nested file should download too"
    );
}

#[test]
fn failed_download_is_pending_not_deleted() {
    // A download that fails is recorded pending (not synced). On the next run
    // the still-missing local file must be RE-DOWNLOADED, never mistaken for a
    // local deletion and trashed remotely — even with propagate_deletes on.
    let t = Tmp::new("pending");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false); // empty baseline
    {
        let mut s = store.borrow_mut();
        s.files
            .insert("/my-files/docs/keep.txt".into(), b"payload".to_vec());
        s.fail_download = Some("/my-files/docs/keep.txt".into());
    }
    // Run 1: the download fails -> file recorded pending_down, not synced.
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        !t.path().join("local/keep.txt").exists(),
        "download was supposed to fail"
    );
    assert!(
        store.borrow().files.contains_key("/my-files/docs/keep.txt"),
        "remote file must NOT be trashed after a failed download"
    );

    // Run 2: downloads work again -> the pending file resumes, not deleted.
    store.borrow_mut().fail_download = None;
    let (fake3, _s) = reuse(&store);
    run(&c, fake3, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/keep.txt")).unwrap(),
        "payload",
        "pending file should download on the next run"
    );
    assert!(
        store.borrow().files.contains_key("/my-files/docs/keep.txt"),
        "remote file still present"
    );
}

#[test]
fn failed_upload_keeps_local_edit() {
    // Regression: a file present on BOTH sides, edited locally, whose upload
    // then fails must NOT be recorded as synced. On the next run it must
    // re-upload the LOCAL edit — never pull the stale remote copy back over it.
    let t = Tmp::new("failup");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    let local = t.path().join("local/x.txt");
    write(&local, "v1");
    run(&c, fake, false); // both sides now hold "v1"

    // Edit locally, then make the upload fail.
    write(&local, "v2-local-edit");
    store.borrow_mut().fail_upload = Some("/my-files/docs/x.txt".into());
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("v1"),
        "upload was supposed to fail; remote keeps the old copy"
    );
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "v2-local-edit",
        "local edit must be untouched after a failed upload"
    );

    // Uploads work again: the local edit must win (re-uploaded), and the stale
    // remote copy must never be pulled back over it.
    store.borrow_mut().fail_upload = None;
    let (fake3, _s) = reuse(&store);
    run(&c, fake3, false);
    assert_eq!(
        std::fs::read_to_string(&local).unwrap(),
        "v2-local-edit",
        "local edit must survive — never overwritten by the old remote copy"
    );
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("v2-local-edit"),
        "the local edit should have uploaded on the retry"
    );
}

#[test]
fn file_replaced_by_dir_is_surfaced_not_merged() {
    // A path that is a file on one side and a directory on the other is a type
    // clash: it must be surfaced and skipped, never silently reconciled or
    // allowed to advance the baseline to the wrong type (which used to leave
    // the two sides permanently diverged).
    let t = Tmp::new("typeswap");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/x.txt"), "file");
    run(&c, fake, false); // both sides hold the file x.txt

    // Locally replace the file with a directory of the same name.
    std::fs::remove_file(t.path().join("local/x.txt")).unwrap();
    std::fs::create_dir(t.path().join("local/x.txt")).unwrap();
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("file"),
        "remote file must survive a local file->dir type clash"
    );
    assert!(t.path().join("local/x.txt").is_dir(), "local dir stays");

    // It must not silently converge: a second run still leaves the remote file
    // intact (the baseline was never advanced to the wrong type).
    let (fake3, _s) = reuse(&store);
    run(&c, fake3, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/x.txt")
            .map(String::as_str),
        Some("file"),
    );
}

#[test]
fn missing_local_root_refuses_delete() {
    // A vanished local root (e.g. an unmounted drive) must NOT be read as a
    // mass deletion: the pair is refused and the remote copy is left intact.
    let t = Tmp::new("missroot");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "keep");
    write(&t.path().join("local/b.txt"), "keep too");
    run(&c, fake, false);

    // The whole local root disappears (unmounted); the baseline is still populated.
    std::fs::remove_dir_all(t.path().join("local")).unwrap();
    let (fake2, _s) = reuse(&store);
    let res = try_run(&c, fake2, false);
    assert!(
        res.is_err(),
        "a missing local root must be refused, not synced"
    );
    let rf = remote_files(&store);
    assert!(
        rf.contains_key("/my-files/docs/a.txt") && rf.contains_key("/my-files/docs/b.txt"),
        "remote files must survive a vanished local root"
    );
}

#[test]
fn exclude_skips_subtree() {
    // An excluded sub-path is never uploaded/downloaded; everything else syncs.
    let t = Tmp::new("excl");
    let mut c = cfg(t.path());
    c.pairs[0].exclude = vec!["APPS".into()];
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/keep.txt"), "keep");
    write(&t.path().join("local/APPS/app.bin"), "app");
    run(&c, fake, false);
    let rf = remote_files(&store);
    assert!(
        rf.contains_key("/my-files/docs/keep.txt"),
        "non-excluded file should upload"
    );
    assert!(
        !rf.keys().any(|k| k.contains("/APPS")),
        "excluded APPS must never reach the remote"
    );
}

#[test]
fn excluding_after_sync_touches_neither_side() {
    // Exclude = freeze: an already-synced subtree stays put on BOTH sides, with
    // deletes on — the feature must never issue a delete for excluded paths.
    let t = Tmp::new("exclafter");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/keep.txt"), "keep");
    write(&t.path().join("local/APPS/app.bin"), "app");
    run(&c, fake, false);
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "APPS should be synced before we exclude it"
    );

    // Now exclude APPS and sync again.
    c.pairs[0].exclude = vec!["APPS".into()];
    let (fake2, _s) = reuse(&store);
    run(&c, fake2, false);
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "remote APPS must survive being excluded (never a Proton-side op)"
    );
    assert!(
        t.path().join("local/APPS/app.bin").exists(),
        "local APPS must survive being excluded (freeze, not delete)"
    );
}

#[test]
fn reinclude_after_local_removal_redownloads_never_deletes_remote() {
    // The Dropbox "free up space" lifecycle: exclude, remove the local copy,
    // then re-include -> it re-downloads from the cloud and NEVER deletes remote.
    let t = Tmp::new("reincl");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/keep.txt"), "keep");
    write(&t.path().join("local/APPS/app.bin"), "app");
    run(&c, fake, false);

    // Exclude APPS (prunes its baseline), then the user frees local space.
    c.pairs[0].exclude = vec!["APPS".into()];
    let (f2, _s) = reuse(&store);
    run(&c, f2, false);
    std::fs::remove_dir_all(t.path().join("local/APPS")).unwrap();
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "remote still holds APPS while it's excluded"
    );

    // Re-include: must re-download from the cloud, remote untouched.
    c.pairs[0].exclude = vec![];
    let (f3, _s) = reuse(&store);
    run(&c, f3, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/APPS/app.bin")).unwrap(),
        "app",
        "re-include should restore the local copy from the cloud"
    );
    assert!(
        remote_files(&store).contains_key("/my-files/docs/APPS/app.bin"),
        "re-include must never delete the remote copy"
    );
}

#[test]
fn cancelled_run_does_not_record_untransferred_as_synced() {
    // Data-loss regression. A run cancelled before its transfers run must NOT
    // record the never-transferred files in the baseline. Otherwise the next
    // run sees them in the baseline but absent locally and "propagates" a bogus
    // remote deletion. With the positive-confirmation commit, a cancelled
    // download leaves the baseline untouched, so the next run RE-DOWNLOADS it
    // and the remote copy survives — even with propagate_deletes on.
    let t = Tmp::new("cancel-dl");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    std::fs::create_dir_all(t.path().join("local")).unwrap();

    let (fake, store) = FakeRemote::new();
    run(&c, fake, false); // establish an empty baseline

    // A new remote file appears after the baseline was taken.
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/report.txt".into(), b"payload".to_vec());

    // Run 1: cancelled up front, so the planned download never executes.
    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let cancel = AtomicBool::new(true);
        let mut eng = Engine::new(&c, fake2, &log, false);
        eng.set_observer(None, Some(&cancel));
        let _ = eng.sync_pair(&c.pairs[0], false);
    }
    assert!(
        !t.path().join("local/report.txt").exists(),
        "cancelled run should not have downloaded anything"
    );
    assert!(
        store
            .borrow()
            .files
            .contains_key("/my-files/docs/report.txt"),
        "cancelled run must never trash the remote file"
    );

    // Run 2: not cancelled. The file must DOWNLOAD (be treated as new-remote),
    // never mistaken for a local deletion and trashed.
    {
        let (fake3, _s) = reuse(&store);
        run(&c, fake3, false);
    }
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/report.txt")).unwrap(),
        "payload",
        "file must download on the recovery run, not be deleted"
    );
    assert!(
        store
            .borrow()
            .files
            .contains_key("/my-files/docs/report.txt"),
        "remote file must still exist after the recovery run"
    );
}

#[test]
fn excluded_subtree_is_not_synced() {
    // An excluded sub-path must be invisible to the engine: never downloaded,
    // never created locally, never recorded — while its siblings sync normally.
    let t = Tmp::new("excl-walk");
    let mut c = cfg(t.path());
    c.pairs[0].exclude = vec!["sub".into()];
    std::fs::create_dir_all(t.path().join("local")).unwrap();

    let (fake, store) = FakeRemote::new();
    {
        let mut s = store.borrow_mut();
        s.dirs.insert("/my-files/docs".into());
        s.dirs.insert("/my-files/docs/sub".into());
        s.files
            .insert("/my-files/docs/top.txt".into(), b"top".to_vec());
        s.files
            .insert("/my-files/docs/sub/inner.txt".into(), b"inner".to_vec());
    }
    run(&c, fake, false);

    assert!(
        t.path().join("local/top.txt").exists(),
        "non-excluded file should download"
    );
    assert!(
        !t.path().join("local/sub/inner.txt").exists(),
        "excluded file must NOT download"
    );
    assert!(
        !t.path().join("local/sub").exists(),
        "excluded folder must not even be created locally"
    );
}

#[test]
fn scoped_sync_only_touches_its_subtree() {
    // A scoped sync reconciles ONLY files under the scope. Anything outside it
    // is never scanned, so even a file that has gone missing locally must not be
    // seen as a deletion and trashed remotely — proving scope containment.
    let t = Tmp::new("scoped");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/keep.txt"), "a1");
    write(&t.path().join("local/B/other.txt"), "b1");
    run(&c, fake, false); // full sync: A/keep.txt and B/other.txt on both sides

    // Add a file in A, and delete B/other.txt locally.
    write(&t.path().join("local/A/new.txt"), "a2");
    std::fs::remove_file(t.path().join("local/B/other.txt")).unwrap();

    // Sync ONLY sub-folder A.
    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, fake2, &log, false);
        eng.sync_pair_scoped(&c.pairs[0], false, Some("A")).unwrap();
    }

    let remote = remote_files(&store);
    assert_eq!(
        remote.get("/my-files/docs/A/new.txt").map(String::as_str),
        Some("a2"),
        "new file inside the scope must upload"
    );
    assert!(
        remote.contains_key("/my-files/docs/B/other.txt"),
        "file OUTSIDE the scope must be untouched even though it's gone locally"
    );
    assert_eq!(
        remote.get("/my-files/docs/A/keep.txt").map(String::as_str),
        Some("a1"),
        "unchanged in-scope file stays"
    );
}

#[test]
fn empty_scope_is_whole_pair_not_a_slash_folder() {
    // Regression: a scoped sync with an empty scope ("") must behave as a normal
    // whole-pair sync, NOT prefix paths with a stray "/" (which used to make
    // every entry look new and mass-recreate folders on the remote).
    let t = Tmp::new("emptyscope");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/keep.txt"), "a1");
    write(&t.path().join("local/top.txt"), "t1");
    run(&c, fake, false); // full sync: both on remote + baseline

    // A no-op scoped sync with empty scope must NOT re-create/re-upload anything.
    let before = remote_files(&store);
    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, fake2, &log, false);
        let r = eng.sync_pair_scoped(&c.pairs[0], false, Some("")).unwrap();
        assert_eq!(
            r.applied, 0,
            "empty-scope sync of an in-sync pair must be a no-op"
        );
    }
    let after = remote_files(&store);
    assert_eq!(before, after, "empty-scope sync must not change the remote");
    // No stray leading-slash keys were created.
    assert!(
        !store
            .borrow()
            .dirs
            .iter()
            .any(|d| d.contains("//") || d.ends_with('/')),
        "no malformed remote dir paths"
    );
}

#[test]
fn shallow_sync_reconciles_only_direct_children() {
    // A shallow sync of folder A uploads a new file placed directly in A, but
    // does NOT descend into A/sub — a deeper local edit is left for that folder's
    // own (recursive-inotify) event, so touching A never re-walks its subtree.
    let t = Tmp::new("shallow");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/A/file1.txt"), "one");
    write(&t.path().join("local/A/sub/file2.txt"), "two");
    run(&c, fake, false); // full sync: both files + A + A/sub on remote & baseline

    // Add a file directly in A, and (separately) change the deep file.
    write(&t.path().join("local/A/new.txt"), "new");
    write(&t.path().join("local/A/sub/file2.txt"), "two-EDITED");

    {
        let (fake2, _s) = reuse(&store);
        let log = Logger::silent();
        let mut eng = Engine::new(&c, fake2, &log, false);
        eng.sync_pair_shallow(&c.pairs[0], "A").unwrap();
    }

    let remote = remote_files(&store);
    assert_eq!(
        remote.get("/my-files/docs/A/new.txt").map(String::as_str),
        Some("new"),
        "new direct file must upload"
    );
    assert_eq!(
        remote
            .get("/my-files/docs/A/sub/file2.txt")
            .map(String::as_str),
        Some("two"),
        "deep file must be UNTOUCHED by a shallow sync of A (no recursion)"
    );
    assert_eq!(
        remote.get("/my-files/docs/A/file1.txt").map(String::as_str),
        Some("one"),
        "unchanged direct file stays"
    );
}
