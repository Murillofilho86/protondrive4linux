//! End-to-end tests for the `--tray` supervisor in the GUI binary - the parts
//! that need no display: what it does with a missing config, a lock somebody
//! else holds, a missing desktop session, and a tray started without it.
//!
//! The restart loop itself is tested against a scripted stand-in in
//! `src/tray_policy.rs`; restarting a real tray after a SIGKILL, forwarding
//! SIGTERM and the kernel stopping the tray when the supervisor dies need a
//! desktop session to run GTK in, so they are checked by hand (see the PR that
//! added this), not here.
//!
//! The environment is scrubbed, so there is no `DISPLAY` and no
//! `WAYLAND_DISPLAY`: no desktop session, on any machine.

#![cfg(feature = "gui")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use neutronsync::process_lock::{self, LockState};

mod common;
use common::{Sandbox, FAKE_CLI};

const GUI: &str = env!("CARGO_BIN_EXE_protondrive4linux-gui");

/// Run `cmd` to completion, failing the test (instead of hanging it) if it takes
/// longer than `secs`.
fn run_to_exit(mut cmd: Command, secs: u64) -> Output {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "still running after {secs}s; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn gui(sb: &Sandbox, config: &Path, args: &[&str]) -> Command {
    let mut cmd = sb.command_for(GUI);
    cmd.env("PROTONDRIVE4LINUX_CONFIG", config).args(args);
    cmd
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn lock(sb: &Sandbox) -> std::path::PathBuf {
    sb.path("state/tray-daemon.lock")
}

#[test]
fn without_a_desktop_session_the_supervisor_stops_at_once_and_says_why() {
    let sb = Sandbox::new("tray-nosession");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let out = run_to_exit(gui(&sb, &cfg, &["--tray"]), 20);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    // It said so where the user can find it: the sync log.
    let log = fs::read_to_string(sb.path("state/logs/sync.log")).unwrap_or_default();
    assert!(
        log.contains("background sync stopped: the display session is gone"),
        "sync.log: {log:?}"
    );
    // And let go of the lock on the way out: nothing is left claiming to run.
    assert_eq!(process_lock::inspect(&lock(&sb)), LockState::Free);
}

#[test]
fn a_second_supervisor_is_refused_while_one_holds_the_lock() {
    let sb = Sandbox::new("tray-second");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    // This test stands in for the running supervisor.
    let held = process_lock::acquire(&lock(&sb)).expect("the lock is free");

    let out = run_to_exit(gui(&sb, &cfg, &["--tray"]), 20);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("a tray daemon is already running"),
        "{}",
        stderr(&out)
    );
    // The refused one left the holder alone and logged nothing.
    assert_eq!(
        process_lock::holder(&lock(&sb)),
        Some(std::process::id()),
        "still ours"
    );
    assert!(!sb.path("state/logs/sync.log").exists());
    drop(held);
}

#[test]
fn a_tray_started_by_hand_refuses_to_run_without_its_supervisor() {
    let sb = Sandbox::new("tray-child-alone");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let out = run_to_exit(gui(&sb, &cfg, &["--tray-child"]), 20);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--tray-child is started by --tray"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_tray_started_by_someone_who_does_not_hold_the_lock_is_refused_too() {
    let sb = Sandbox::new("tray-child-wrong-parent");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    // A lock record naming a process that is not this tray's parent - the shape
    // of a leftover, or of a tray started under somebody else's lock.
    fs::create_dir_all(sb.path("state")).unwrap();
    let mine = process_lock::record_for(std::process::id()).unwrap();
    let boot = mine.split_whitespace().nth(2).unwrap();
    fs::write(lock(&sb), format!("{} 1 {boot}", std::process::id())).unwrap();
    let out = run_to_exit(gui(&sb, &cfg, &["--tray-child"]), 20);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn with_no_config_there_is_nothing_to_run_and_it_exits_cleanly() {
    let sb = Sandbox::new("tray-noconfig");
    let out = run_to_exit(
        {
            let mut cmd = sb.command_for(GUI);
            cmd.arg("--tray");
            cmd
        },
        20,
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no config found; nothing to run in tray mode"),
        "{}",
        stderr(&out)
    );
    let child = run_to_exit(
        {
            let mut cmd = sb.command_for(GUI);
            cmd.arg("--tray-child");
            cmd
        },
        20,
    );
    assert_eq!(
        child.status.code(),
        Some(0),
        "a tray with nothing to run is a clean exit, so the supervisor would stop: {}",
        stderr(&child)
    );
}
