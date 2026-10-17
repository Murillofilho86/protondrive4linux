//! M1-001: disaster scenarios that need a REAL subprocess and REAL signals -
//! things `tests/engine.rs`'s in-process `FakeRemote` structurally can't
//! exercise (it never spawns a process, so there's nothing to kill).
//!
//! Runs the actual `protondrive4linux` binary against `fake-proton-drive`
//! (`tests/support/fake_proton_drive.rs`), a minimal proton-drive CLI
//! stand-in backed by a plain directory tree - no network, no real account.
//!
//! Scope, deliberately: SIGTERM, SIGKILL, and a CLI that dies mid-operation
//! are covered here. Two scenarios from the roadmap are NOT, and it would be
//! dishonest to fake them:
//!   - "disco cheio": this sandboxed dev/CI environment has no reasonable way
//!     to create a real disk-full condition without a privileged/quota-backed
//!     mount. Not simulated.
//!   - "corrida real entre watch e rescan periódico": needs the real
//!     `proton-drive` CLI's actual timing/caching behavior, which
//!     `fake-proton-drive` doesn't attempt to reproduce faithfully - the
//!     point of that scenario is real-CLI quirks, not the sync algorithm.
//!
//! Both stay open, tracked in docs/roadmap/M1-data-integrity.md.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

struct Env {
    _tmp: PathBuf, // removed on Drop, see below
    local: PathBuf,
    remote_root: PathBuf,
    config_path: PathBuf,
}

fn unique_tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "protondrive4linux-disaster-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sets up a pair "p": local <-> /my-files/docs, backed by fake-proton-drive.
fn setup(tag: &str) -> Env {
    let tmp = unique_tmp(tag);
    let local = tmp.join("local");
    let remote_root = tmp.join("remote");
    let state_dir = tmp.join("state");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&remote_root).unwrap();
    std::fs::create_dir_all(remote_root.join("my-files")).unwrap();

    let config_path = tmp.join("protondrive4linux.toml");
    let fake_cli = env!("CARGO_BIN_EXE_fake-proton-drive");
    let toml = format!(
        r#"[cli]
binary = {fake_cli:?}
fresh_cache = false

[options]
remote_root = "/my-files"
state_dir = {state_dir:?}
propagate_deletes = true

[[pair]]
name = "p"
local = {local:?}
remote = "docs"
"#,
    );
    std::fs::write(&config_path, toml).unwrap();

    Env {
        _tmp: tmp,
        local,
        remote_root,
        config_path,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self._tmp).ok();
    }
}

fn write_local_files(env: &Env, count: usize) {
    for i in 0..count {
        std::fs::write(
            env.local.join(format!("file-{i:04}.txt")),
            format!("payload for file {i}"),
        )
        .unwrap();
    }
}

fn remote_docs_dir(env: &Env) -> PathBuf {
    env.remote_root.join("my-files").join("docs")
}

fn count_remote_files(env: &Env) -> usize {
    match std::fs::read_dir(remote_docs_dir(env)) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .count(),
        Err(_) => 0,
    }
}

fn count_local_files(env: &Env) -> usize {
    std::fs::read_dir(&env.local)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .count()
}

/// Spawn `protondrive4linux --config <cfg> sync p`, with the given extra env
/// vars forwarded down to fake-proton-drive (which inherits the child's
/// environment, same as the real proton-drive CLI would).
fn spawn_sync(env: &Env, extra_env: &BTreeMap<&str, String>) -> std::process::Child {
    let bin = env!("CARGO_BIN_EXE_protondrive4linux");
    let mut cmd = Command::new(bin);
    cmd.args(["--config", env.config_path.to_str().unwrap(), "sync", "p"])
        .env("FAKE_REMOTE_ROOT", &env.remote_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn().expect("failed to spawn protondrive4linux")
}

/// Like [`spawn_sync`], but in a process group of its own, so a signal sent to
/// the group reaches the CLI children as well - as a power cut, an OOM kill of
/// the whole session, or the CLI hitting its timeout would. Killing only the
/// parent leaves its children to finish what they were writing, which is not
/// the disaster being tested.
fn spawn_sync_in_own_group(env: &Env, extra_env: &BTreeMap<&str, String>) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let bin = env!("CARGO_BIN_EXE_protondrive4linux");
    let mut cmd = Command::new(bin);
    cmd.args(["--config", env.config_path.to_str().unwrap(), "sync", "p"])
        .env("FAKE_REMOTE_ROOT", &env.remote_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn().expect("failed to spawn protondrive4linux")
}

/// Send `signal` to every process in the group `pid` leads.
fn signal_group(signal: &str, pid: u32) {
    let status = Command::new("kill")
        .args([format!("-{signal}"), format!("-{pid}")])
        .status()
        .expect("failed to invoke kill(1)");
    assert!(status.success(), "kill -{signal} -{pid} failed");
}

/// Run one full, uninterrupted sync (no artificial delay/crash) and wait for
/// it to finish - used both to establish a baseline and to prove recovery.
fn run_clean_sync(env: &Env) {
    let mut child = spawn_sync(env, &BTreeMap::new());
    let status = child
        .wait_timeout(Duration::from_secs(30))
        .expect("clean sync must finish within 30s");
    assert!(
        status.success(),
        "a clean sync run must succeed: {status:?}"
    );
}

// `std::process::Child` has no built-in wait-with-timeout in stable std; a
// small poll loop is simplest without adding a dependency for one test file.
trait WaitTimeout {
    fn wait_timeout(&mut self, timeout: Duration) -> std::io::Result<std::process::ExitStatus>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout(&mut self, timeout: Duration) -> std::io::Result<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.kill();
                let _ = self.wait();
                panic!("process did not exit within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

const FILE_COUNT: usize = 100;
const PER_CALL_SLEEP_MS: u64 = 50;
const KILL_AFTER: Duration = Duration::from_millis(500);

fn interrupted_sync_then_recovers(signal: &str) {
    let env = setup(signal);
    write_local_files(&env, FILE_COUNT);

    let mut extra = BTreeMap::new();
    extra.insert("FAKE_CLI_SLEEP_MS", PER_CALL_SLEEP_MS.to_string());
    let mut child = spawn_sync(&env, &extra);
    let pid = child.id();

    std::thread::sleep(KILL_AFTER);

    // Confirm the run was genuinely interrupted mid-batch, not already done -
    // otherwise this test would not be exercising an interruption at all.
    let done_before_kill = count_remote_files(&env);
    assert!(
        done_before_kill < FILE_COUNT,
        "test timing assumption broken: all {FILE_COUNT} files already synced \
         before the signal was sent (only {done_before_kill} expected partial) - \
         widen KILL_AFTER or PER_CALL_SLEEP_MS"
    );

    if signal == "KILL" {
        child.kill().expect("SIGKILL failed");
    } else {
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("failed to invoke kill(1)");
        assert!(status.success(), "kill -TERM {pid} failed to send");
    }

    let exit = child
        .wait_timeout(Duration::from_secs(10))
        .expect("interrupted process must actually terminate");
    assert!(
        !exit.success(),
        "a signal-terminated process must not report success: {exit:?}"
    );

    // The interruption itself must be safe: nothing corrupted, no local file
    // touched or lost.
    assert_eq!(
        count_local_files(&env),
        FILE_COUNT,
        "no local file may be lost by the interruption itself"
    );

    // Recovery: a clean second run must complete and converge fully - every
    // file present on both sides with matching content, nothing left pending
    // forever, no duplicate/conflict artifacts from the interruption.
    run_clean_sync(&env);

    assert_eq!(
        count_remote_files(&env),
        FILE_COUNT,
        "every file must be present on remote after the recovery run"
    );
    for i in 0..FILE_COUNT {
        let name = format!("file-{i:04}.txt");
        let expected = format!("payload for file {i}");
        let remote_content = std::fs::read_to_string(remote_docs_dir(&env).join(&name))
            .unwrap_or_else(|e| panic!("remote {name} missing after recovery: {e}"));
        assert_eq!(
            remote_content, expected,
            "remote content mismatch for {name}"
        );
        let local_content = std::fs::read_to_string(env.local.join(&name)).unwrap();
        assert_eq!(local_content, expected, "local content mismatch for {name}");
    }
    // No conflict copies from the interruption (nothing was independently
    // changed on the other side - this must be a clean converge, not a
    // pile of "(conflict ...)" files).
    let stray: Vec<_> = std::fs::read_dir(&env.local)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("conflict"))
        .collect();
    assert!(
        stray.is_empty(),
        "an interrupted-then-recovered sync must never fabricate a conflict: {stray:?}"
    );
}

#[test]
fn sigterm_mid_sync_recovers_cleanly() {
    interrupted_sync_then_recovers("TERM");
}

#[test]
fn sigkill_mid_sync_recovers_cleanly() {
    interrupted_sync_then_recovers("KILL");
}

#[test]
fn crashing_cli_does_not_corrupt_state() {
    // "Proton CLI encerrado inesperadamente": every proton-drive invocation
    // fails outright (simulating the binary itself crashing/erroring, not
    // just a slow network). The sync must report errors and touch nothing
    // destructively; a later run with the CLI healthy again must converge.
    let env = setup("crash");
    write_local_files(&env, 10);

    let mut extra = BTreeMap::new();
    extra.insert("FAKE_CLI_CRASH", "1".to_string());
    let mut child = spawn_sync(&env, &extra);
    let status = child
        .wait_timeout(Duration::from_secs(10))
        .expect("a run against a crashing CLI must still terminate on its own");
    // The run completes (protondrive4linux itself doesn't crash), but with
    // nothing achieved - a crashing CLI is not a signal-terminated process.
    let _ = status;

    assert_eq!(
        count_local_files(&env),
        10,
        "nothing local may be lost while the CLI is unavailable"
    );
    assert_eq!(
        count_remote_files(&env),
        0,
        "nothing may reach a definitely-broken remote"
    );

    run_clean_sync(&env);
    assert_eq!(
        count_remote_files(&env),
        10,
        "once the CLI is healthy again, a normal run must converge fully"
    );
}

// --- killed mid-download -------------------------------------------------------

/// The names in the pair's local folder that are not hidden staging folders.
fn visible_local(env: &Env) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(&env.local)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    v.sort();
    v
}

/// Is a download half written right now - a file called `file-NNNN.txt` that
/// has some of its bytes but not all, wherever it is (under its real name, or in
/// a staging folder)? Looks at nothing about *how* the run stages its files, so
/// the test means the same against any implementation.
fn a_download_is_half_written(env: &Env, full_len: &dyn Fn(usize) -> usize) -> bool {
    walkdir::WalkDir::new(&env.local)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(i) = name
                .strip_prefix("file-")
                .and_then(|r| r.get(..4))
                .and_then(|n| n.parse::<usize>().ok())
            else {
                return false;
            };
            let len = e.metadata().map(|m| m.len() as usize).unwrap_or(0);
            len > 0 && len < full_len(i)
        })
}

/// Kill a sync (`signal`) while a download is half written, and check that no
/// file under its real name is ever partial, whatever was there before.
fn killed_mid_download(signal: &str, had_old_copies: bool) {
    let env = setup(&format!("dl-{signal}-{had_old_copies}"));
    const N: usize = 12;
    // Different versions have different sizes (size+mtime cannot tell two edits
    // of one length within one second apart, which is not what this tests).
    let body = |i: usize, v: u32| {
        format!(
            "version {v} of file {i}: {}",
            "x".repeat(2000 + i + 97 * v as usize)
        )
    };
    let remote = remote_docs_dir(&env);
    std::fs::create_dir_all(&remote).unwrap();
    if had_old_copies {
        // v1 on both sides, synced; then the remote moves on to v2.
        for i in 0..N {
            std::fs::write(remote.join(format!("file-{i:04}.txt")), body(i, 1)).unwrap();
        }
        run_clean_sync(&env);
        assert_eq!(count_local_files(&env), N);
    }
    let latest = if had_old_copies { 2 } else { 1 };
    for i in 0..N {
        std::fs::write(remote.join(format!("file-{i:04}.txt")), body(i, latest)).unwrap();
    }

    let mut extra = BTreeMap::new();
    extra.insert("FAKE_CLI_HALF_WRITE_MS", "1500".to_string());
    let mut child = spawn_sync_in_own_group(&env, &extra);
    let pid = child.id();

    // Wait until a download is genuinely half written, then pull the plug. (When
    // the old copies are being replaced, a v1 file is shorter than v2, so only a
    // file shorter than ITS OWN latest version counts - and a v1 file is, so look
    // for one that is neither complete v1 nor complete v2.)
    let complete = |i: usize| body(i, latest).len();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !a_download_is_half_written(&env, &|i| {
        if had_old_copies {
            // Anything between "v1 whole" and "v2 whole" would be a complete older
            // file or a partial new one; a partial is shorter than v1 itself.
            body(i, 1).len()
        } else {
            complete(i)
        }
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "never saw a download in progress"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The whole group, CLI children included: they die mid-write, as in a crash.
    signal_group(signal, pid);
    child
        .wait_timeout(Duration::from_secs(10))
        .expect("the killed process must terminate");
    // Nothing is left running to finish what was being written.
    std::thread::sleep(Duration::from_millis(300));

    // Every file under its real name is whole: the old version or the new one,
    // never half of either.
    for name in visible_local(&env) {
        let got = std::fs::read_to_string(env.local.join(&name)).unwrap();
        let i: usize = name["file-".len().."file-".len() + 4].parse().unwrap();
        let ok = got == body(i, latest) || (had_old_copies && got == body(i, 1));
        assert!(ok, "{name} is partial or corrupt after the kill: {got:?}");
    }

    // Recovery converges on the remote's content, with no conflict copies and
    // no partial file ever uploaded back over the good ones.
    run_clean_sync(&env);
    for i in 0..N {
        let name = format!("file-{i:04}.txt");
        assert_eq!(
            std::fs::read_to_string(env.local.join(&name)).unwrap(),
            body(i, latest),
            "{name} local after recovery"
        );
        assert_eq!(
            std::fs::read_to_string(remote.join(&name)).unwrap(),
            body(i, latest),
            "{name} on the remote after recovery"
        );
    }
    let visible = visible_local(&env);
    assert_eq!(visible.len(), N, "stray files after recovery: {visible:?}");
    assert!(
        visible.iter().all(|n| !n.contains("conflict")),
        "{visible:?}"
    );
}

#[test]
fn sigkill_mid_download_never_leaves_a_partial_file_under_its_real_name() {
    killed_mid_download("KILL", false);
}

#[test]
fn sigkill_mid_download_over_existing_files_keeps_each_old_or_new_never_half() {
    killed_mid_download("KILL", true);
}

#[test]
fn sigterm_mid_download_never_leaves_a_partial_file_under_its_real_name() {
    killed_mid_download("TERM", false);
}
