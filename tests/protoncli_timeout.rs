//! Regression test for the "proton-drive CLI hangs, sync freezes forever"
//! report: `ProtonCli` used to call `Command::output()` with no timeout, so a
//! wedged (or slow beyond reason) CLI process blocked the calling thread
//! indefinitely - which, if that happened to be the GUI's own thread, showed
//! up to the user as the whole window becoming unresponsive with no way to
//! recover short of killing the process by hand.
//!
//! Drives the real `ProtonCli` (not the in-memory `FakeRemote` used by
//! `tests/engine.rs`) against `fake-proton-drive` (see
//! `tests/support/fake_proton_drive.rs`), using its `FAKE_CLI_SLEEP_MS` knob
//! to simulate a CLI call that never comes back within a reasonable time.
//!
//! Sets process-wide env vars, so this file intentionally has a single test
//! function - cargo runs tests within one binary concurrently by default, and
//! two tests racing on the same env vars would be flaky.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use neutronsync::config::{Config, ConflictPolicy, LocalDelete, UpdateChannel};
use neutronsync::models::Compare;
use neutronsync::protoncli::{ProtonCli, Remote};

fn unique_tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "protondrive4linux-protoncli-timeout-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cfg(binary: &str, cli_timeout_secs: u64, root: &Path) -> Config {
    Config {
        binary: binary.into(),
        upload_flags: vec![],
        download_flags: vec![],
        credentials_store: None,
        fresh_cache: false,
        scan_threads: 0,
        download_threads: 0,
        cli_timeout_secs,
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
        pairs: vec![],
        source_path: None,
    }
}

#[test]
fn hung_cli_call_fails_after_its_timeout_instead_of_blocking_forever() {
    let fake_cli = env!("CARGO_BIN_EXE_fake-proton-drive");
    let tmp = unique_tmp("hang");
    let remote_root = tmp.join("remote");
    std::fs::create_dir_all(remote_root.join("my-files")).unwrap();

    // SAFETY: this is the only test in this binary (see module doc) - no
    // other test can race on this process's environment.
    unsafe {
        std::env::set_var("FAKE_REMOTE_ROOT", &remote_root);
    }

    // Sanity check the happy path still works with the new code path in
    // place: a fast call, under a generous timeout, succeeds normally.
    unsafe {
        std::env::remove_var("FAKE_CLI_SLEEP_MS");
    }
    let fast = ProtonCli::new(&cfg(fake_cli, 30, &tmp));
    fast.list_dir("/my-files")
        .expect("a fast call under a generous timeout must still succeed");

    // Now the actual regression: the CLI call sleeps far longer than the
    // configured timeout. Before the fix this hung forever; now it must come
    // back as an error, and come back quickly - bounded well under the sleep
    // duration, proving the timeout (not the sleep ending) is what returned.
    unsafe {
        std::env::set_var("FAKE_CLI_SLEEP_MS", "5000");
    }
    let slow = ProtonCli::new(&cfg(fake_cli, 1, &tmp));
    let start = Instant::now();
    let result = slow.list_dir("/my-files");
    let elapsed = start.elapsed();

    std::fs::remove_dir_all(&tmp).ok();

    assert!(
        result.is_err(),
        "a CLI call stuck well past its timeout must fail, not succeed"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "timeout took {elapsed:?} to fire - should be ~1s, not close to the 5s sleep"
    );
}
