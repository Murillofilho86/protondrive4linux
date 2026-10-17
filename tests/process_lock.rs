//! End-to-end tests for the single-instance lock `protondrive4linux watch`
//! takes: real `watch` processes against the fake `proton-drive`, in a sandbox,
//! killed with real signals.
//!
//! What these pin down is what a PID file got wrong: a lock left behind by a
//! watcher that was killed (or by a previous boot) must never keep a new one
//! from starting, a pid that now belongs to some other program must never be
//! taken for a running watcher, and of several watchers starting at the same
//! moment exactly one may win.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use neutronsync::process_lock::{self, LockState};

mod common;
use common::{Sandbox, FAKE_CLI};

/// A spawned `watch`, killed when dropped so a failing assertion can't leave
/// one running.
struct Watcher {
    child: Child,
    stderr: PathBuf,
}

impl Watcher {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Its stderr so far (where `error: ...` lands).
    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr).unwrap_or_default()
    }

    /// The exit code if it has exited, `None` while it runs.
    fn exited(&mut self) -> Option<i32> {
        self.child
            .try_wait()
            .unwrap()
            .map(|s| s.code().unwrap_or(-1))
    }

    /// Wait up to `secs` for it to exit.
    fn wait_exit(&mut self, secs: u64) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(code) = self.exited() {
                return Some(code);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL: no destructors, no cleanup - the lock is left exactly as it was.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_watcher(sb: &Sandbox, config: &Path, tag: &str) -> Watcher {
    let stderr = sb.path(&format!("watcher-{tag}.stderr"));
    let mut cmd = sb.command();
    cmd.arg("-c")
        .arg(config)
        .arg("watch")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
    Watcher {
        child: cmd.spawn().expect("the binary starts"),
        stderr,
    }
}

fn lock_path(sb: &Sandbox) -> PathBuf {
    sb.path("state/watch.lock")
}

/// Wait until `pid` is the live holder of the watcher lock.
fn wait_held(sb: &Sandbox, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while process_lock::holder(&lock_path(sb)) != Some(pid) {
        assert!(
            Instant::now() < deadline,
            "pid {pid} never became the holder; the lock says {:?}",
            process_lock::inspect(&lock_path(sb))
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_second_watcher_is_refused_while_the_first_runs() {
    let sb = Sandbox::new("lock-second");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let first = spawn_watcher(&sb, &cfg, "a");
    wait_held(&sb, first.pid());

    let mut second = spawn_watcher(&sb, &cfg, "b");
    assert_eq!(second.wait_exit(15), Some(2), "{}", second.stderr());
    let err = second.stderr();
    assert!(
        err.contains(&format!(
            "a protondrive4linux watch is already running (pid {})",
            first.pid()
        )),
        "{err}"
    );
    // The first one is untouched by the attempt.
    assert_eq!(process_lock::holder(&lock_path(&sb)), Some(first.pid()));
}

#[test]
fn a_watcher_that_was_killed_does_not_keep_the_next_one_from_starting() {
    let sb = Sandbox::new("lock-killed");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let mut first = spawn_watcher(&sb, &cfg, "a");
    let dead = first.pid();
    wait_held(&sb, dead);

    first.kill();
    // SIGKILL leaves the record behind, naming a process that no longer exists.
    assert_eq!(
        process_lock::inspect(&lock_path(&sb)),
        LockState::Stale { pid: dead }
    );

    let mut second = spawn_watcher(&sb, &cfg, "b");
    wait_held(&sb, second.pid());
    assert_eq!(second.exited(), None, "{}", second.stderr());
}

#[test]
fn a_pid_that_now_belongs_to_some_other_program_is_not_a_running_watcher() {
    let sb = Sandbox::new("lock-reused");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::create_dir_all(sb.path("state")).unwrap();

    // The lock names this test process: alive, but not a watcher, and not the
    // process that wrote the record. This is what a lock left over from before a
    // reboot looks like once its pid has been handed out again, and it used to
    // make `watch` refuse to start with "already running".
    let me = std::process::id();
    let mine = process_lock::record_for(me).unwrap();
    let boot = mine.split_whitespace().nth(2).unwrap().to_string();
    let stale_identity = format!("{me} 1 {boot}");
    for (what, record) in [
        ("an identity record with another start time", stale_identity),
        ("a bare pid, as older versions wrote", me.to_string()),
    ] {
        fs::write(lock_path(&sb), record).unwrap();
        let mut watcher = spawn_watcher(&sb, &cfg, "reused");
        wait_held(&sb, watcher.pid());
        assert_eq!(
            watcher.exited(),
            None,
            "started despite {what}: {}",
            watcher.stderr()
        );
        watcher.kill();
    }
}

#[test]
fn a_watcher_from_an_older_version_that_is_still_running_is_respected() {
    let sb = Sandbox::new("lock-older");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::create_dir_all(sb.path("state")).unwrap();
    // Stand in for the old watcher: a process whose executable is named
    // `protondrive4linux`, and the bare pid that version wrote into its lock. It
    // never took a file lock, so only the record says it is there.
    let exe = sb.path("protondrive4linux");
    fs::copy(FAKE_CLI, &exe).unwrap();
    common::settle(&exe);
    let mut old = std::process::Command::new(&exe)
        .arg("version")
        .env("FAKE_REMOTE_ROOT", sb.path("remote"))
        .env("FAKE_CLI_SLEEP_MS", "30000")
        .spawn()
        .unwrap();
    fs::write(lock_path(&sb), old.id().to_string()).unwrap();

    let mut new = spawn_watcher(&sb, &cfg, "new");
    let code = new.wait_exit(15);
    let err = new.stderr();
    let _ = old.kill();
    let _ = old.wait();
    assert_eq!(code, Some(2), "{err}");
    assert!(
        err.contains(&format!("already running (pid {})", old.id())),
        "{err}"
    );
}

// Processes start milliseconds apart, so this does not by itself catch a lock that
// loses the race (the old PID file passed it nearly every time): the test that
// does is `of_many_that_start_at_once_exactly_one_gets_the_lock` in
// `src/process_lock.rs`, which releases 16 threads from a barrier. This one
// checks the same promise across real processes, end to end: one runs, the rest
// say so and exit.
#[test]
fn of_several_watchers_started_at_once_over_a_stale_lock_exactly_one_runs() {
    let sb = Sandbox::new("lock-race");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::create_dir_all(sb.path("state")).unwrap();
    // Everybody finds the lock stale, the case where each used to overwrite it.
    fs::write(lock_path(&sb), "4999999 1 gone").unwrap();

    let mut watchers: Vec<Watcher> = (0..6)
        .map(|i| spawn_watcher(&sb, &cfg, &format!("race{i}")))
        .collect();
    // Everyone has had time to start, take or lose the lock, and report.
    let deadline = Instant::now() + Duration::from_secs(30);
    let codes: Vec<Option<i32>> = loop {
        let codes: Vec<Option<i32>> = watchers.iter_mut().map(|w| w.exited()).collect();
        if codes.iter().filter(|c| c.is_some()).count() == 5 {
            break codes;
        }
        assert!(
            Instant::now() < deadline,
            "exit codes so far {codes:?}; stderr: {:?}",
            watchers.iter().map(|w| w.stderr()).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let running: Vec<u32> = watchers
        .iter()
        .zip(&codes)
        .filter(|(_, code)| code.is_none())
        .map(|(w, _)| w.pid())
        .collect();
    assert_eq!(running.len(), 1, "survivors: {running:?}");
    for (w, code) in watchers.iter().zip(&codes) {
        if let Some(code) = code {
            assert_eq!(*code, 2, "{}", w.stderr());
            assert!(w.stderr().contains("already running"), "{}", w.stderr());
        }
    }
    assert_eq!(process_lock::holder(&lock_path(&sb)), Some(running[0]));
}
