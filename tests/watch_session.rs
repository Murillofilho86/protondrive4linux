//! What the app does when the CLI can't reach the keyring its session is kept in
//! (locked, no D-Bus session, a headless box): the real watcher and the real
//! account check, against the fake `proton-drive` from `tests/support/`.
//!
//! The point is the difference from a sign-out. Both stop every CLI call, but
//! signing in again does nothing for a keyring that is out of reach, so the app
//! must not say "signed out", must not ask for a sign-in, and must carry on by
//! itself the moment the keyring answers.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::Sandbox;
use neutronsync::config::{self, Config};
use neutronsync::events::{EventSink, SyncEvent};
use neutronsync::logger::Logger;
use neutronsync::protoncli::{session_problem, ProtonCli, Remote, SessionProblem};
use neutronsync::service::{Controller, OverallStatus};

/// Remembers every event, as its `Debug` text (the events don't implement `Eq`).
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl EventSink for Recorder {
    fn emit(&self, ev: &SyncEvent) {
        self.0.lock().unwrap().push(format!("{ev:?}"));
    }
}

impl Recorder {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn has(&self, starts_with: &str) -> bool {
        self.all().iter().any(|e| e.starts_with(starts_with))
    }
}

/// Stops the watcher when dropped, including while a failed assertion unwinds:
/// `thread::scope` waits for the watcher thread, which would otherwise run
/// forever and turn a failing test into a hung one.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Poll `cond` until it holds, or fail the test after `secs`.
fn wait_for(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// A sandbox whose CLI is the fake one, with its keyring "locked" for as long as
/// the returned flag file exists.
fn locked(tag: &str) -> (Sandbox, Config, PathBuf) {
    let sb = Sandbox::new(tag);
    let flag = sb.path("keyring-locked");
    fs::write(&flag, "").unwrap();
    let cli = sb.wrapper(&format!(
        "FAKE_CLI_KEYRING_FLAG={:?}",
        flag.display().to_string()
    ));
    let file = sb.write_config(&cli);
    // The state folder is the tray's to create; the lock file goes in it.
    fs::create_dir_all(sb.path("state")).unwrap();
    fs::set_permissions(sb.path("state"), fs::Permissions::from_mode(0o700)).unwrap();
    let cfg = config::load(Some(file.to_str().unwrap())).expect("the sandbox config loads");
    (sb, cfg, flag)
}

#[test]
fn a_cli_that_cannot_reach_its_keyring_is_not_reported_as_signed_out() {
    let (_sb, cfg, flag) = locked("keyring-msg");
    let cli = ProtonCli::new(&cfg);

    let err = cli.list_dir(&cfg.remote_root).unwrap_err().to_string();
    assert!(
        err.starts_with("the CLI can't reach its keyring:"),
        "it says what is wrong, not 'not signed in': {err}"
    );
    assert_eq!(
        session_problem(&err),
        Some(SessionProblem::SecretStoreUnreachable),
        "and the message still classifies the same way: {err}"
    );

    // Once it answers, the listing works.
    fs::remove_file(&flag).unwrap();
    assert!(cli.list_dir(&cfg.remote_root).is_ok());
}

#[test]
fn the_account_check_waits_for_the_keyring_and_does_not_call_it_a_sign_out() {
    let (_sb, cfg, flag) = locked("keyring-account");
    let ctrl = Controller::new(cfg);

    // `Controller::new` starts the first check; wait for its answer.
    wait_for("the first account check", 20, || {
        let s = ctrl.snapshot();
        s.account.checked && !s.account.checking
    });
    let s = ctrl.snapshot();
    assert!(s.account.binary_found);
    assert!(!s.account.signed_in);
    assert!(s.account.keyring_unreachable);
    assert!(s.keyring_unreachable, "the page state says so too");
    assert!(!s.signed_out, "a keyring out of reach is not a sign-out");
    assert_eq!(s.overall(), OverallStatus::WaitingForKeyring);

    // Unlock, check again: signed in, nothing left over.
    fs::remove_file(&flag).unwrap();
    ctrl.refresh_account(false);
    wait_for("the account to read as signed in", 20, || {
        let s = ctrl.snapshot();
        !s.account.checking && s.account.signed_in
    });
    let s = ctrl.snapshot();
    assert!(!s.account.keyring_unreachable);
    assert!(!s.keyring_unreachable && !s.signed_out);
}

#[test]
fn the_watcher_waits_for_the_keyring_then_resumes_by_itself() {
    let (_sb, cfg, flag) = locked("keyring-watch");
    let events = Recorder::default();
    let stop = AtomicBool::new(false);
    let log = Logger::silent();
    let pairs = cfg.pairs.clone();

    std::thread::scope(|s| {
        let _stop_when_done = StopOnDrop(&stop);
        let watcher =
            s.spawn(|| neutronsync::watcher::watch_with(&cfg, pairs, &log, &stop, Some(&events)));

        // Out of reach at the start: said once, as a wait.
        wait_for("the watcher to notice the keyring", 20, || {
            events.has("KeyringUnreachable")
        });
        // Give it time to do something it should not, then look.
        std::thread::sleep(Duration::from_millis(1800));
        assert_eq!(
            events.all(),
            ["KeyringUnreachable"],
            "no sign-out, no sync attempts and no per-folder errors while it waits"
        );

        // Unlock the keyring and nudge the watcher the way the window does,
        // rather than waiting out its minute-long re-probe.
        fs::remove_file(&flag).unwrap();
        neutronsync::auth_signal::signal(&cfg.state_dir);

        wait_for("the watcher to resume", 30, || {
            events.has("Auth { signed_in: true }")
        });
        wait_for("the pair to sync once it is back", 30, || {
            events.has("PairFinished")
        });
        assert!(
            !events.has("Auth { signed_in: false }"),
            "it was never signed out: {:?}",
            events.all()
        );
        let errors: Vec<String> = events
            .all()
            .into_iter()
            .filter(|e| e.starts_with("Error"))
            .collect();
        assert!(errors.is_empty(), "{errors:?}");

        stop.store(true, Ordering::Relaxed);
        watcher.join().unwrap().expect("the watcher stops cleanly");
    });
}

#[test]
fn a_sign_out_is_still_a_sign_out() {
    // The other half of the distinction: with no session at all the watcher
    // still says "signed out" (and not "keyring"), and the CLI's message keeps
    // its old wording.
    let sb = Sandbox::new("keyring-signed-out");
    let cli = sb.wrapper("FAKE_CLI_SIGNED_OUT=1");
    let file = sb.write_config(&cli);
    fs::create_dir_all(sb.path("state")).unwrap();
    fs::set_permissions(sb.path("state"), fs::Permissions::from_mode(0o700)).unwrap();
    let cfg = config::load(Some(file.to_str().unwrap())).unwrap();

    let err = ProtonCli::new(&cfg)
        .list_dir(&cfg.remote_root)
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("not signed in:"), "{err}");
    assert_eq!(session_problem(&err), Some(SessionProblem::SignedOut));

    let events = Recorder::default();
    let stop = AtomicBool::new(false);
    let log = Logger::silent();
    let pairs = cfg.pairs.clone();
    std::thread::scope(|s| {
        let _stop_when_done = StopOnDrop(&stop);
        let watcher =
            s.spawn(|| neutronsync::watcher::watch_with(&cfg, pairs, &log, &stop, Some(&events)));
        wait_for("the watcher to notice the sign-out", 20, || {
            events.has("Auth { signed_in: false }")
        });
        assert!(!events.has("KeyringUnreachable"), "{:?}", events.all());
        stop.store(true, Ordering::Relaxed);
        watcher.join().unwrap().expect("the watcher stops cleanly");
    });
}
