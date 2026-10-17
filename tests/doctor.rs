//! End-to-end tests for `protondrive4linux doctor` (M7-003) and the private
//! config `init` writes: the real binary, run against the fake `proton-drive`
//! from `tests/support/`, in a sandbox with a scrubbed environment.
//!
//! Nothing here touches the network or the real systemd. The binary runs get
//! `--offline` (or a probe the fake answers), and an empty `PATH`, so no
//! `systemctl` or GUI binary is found and the developer's own setup can't leak
//! into the result. The sign-in tests call the library with a host that says
//! "online", because the real binary would otherwise make a TCP connection to
//! Proton to decide whether to probe the session.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use neutronsync::doctor::{self, ConfigState, Env, Host, Level, Options};

mod common;
use common::{code, settle, text, Sandbox, FAKE_CLI};

// --- the real binary ------------------------------------------------------------

#[test]
fn a_healthy_setup_passes_with_every_check_listed() {
    let sb = Sandbox::new("healthy");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let out = sb.run(&cfg, &["doctor", "--offline"]);
    let report = text(&out);
    assert_eq!(code(&out), 0, "{report}");
    for line in [
        "✔ Config",
        "1 folder pair",
        "✔ Proton CLI",
        "cli-drive 0.8.0",
        "– Network",
        "not checked (--offline)",
        "✔ Folders",
        "✔ Permissions",
        "closed to others",
        "– systemd",
        "systemctl not found",
        "• Watcher",
        "• Desktop entry",
    ] {
        assert!(report.contains(line), "missing {line:?} in:\n{report}");
    }
    assert!(!report.contains('✘'), "{report}");
    assert!(report.trim_end().ends_with("all good."), "{report}");
}

#[test]
fn json_output_is_a_parsable_report() {
    let sb = Sandbox::new("json");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let out = sb.run(&cfg, &["doctor", "--offline", "--json"]);
    assert_eq!(code(&out), 0, "{}", text(&out));
    let v: serde_json::Value = serde_json::from_str(&text(&out)).expect("valid JSON");
    assert_eq!(v["ok"], true);
    let ids: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "version",
            "config",
            "cli",
            "network",
            "credentials",
            "folders",
            "permissions",
            "systemd",
            "daemon",
            "desktop"
        ]
    );
    assert_eq!(v["summary"]["failed"], 0);
    assert!(v["version"]
        .as_str()
        .unwrap()
        .starts_with(env!("CARGO_PKG_VERSION")));
}

#[test]
fn no_config_fails_with_the_init_hint_but_the_other_checks_still_run() {
    let sb = Sandbox::new("noconfig");
    // `-c` on a path that doesn't exist, or no -c at all with nothing in the sandbox's home.
    let out = sb.command().args(["doctor", "--offline"]).output().unwrap();
    let report = text(&out);
    assert_eq!(code(&out), 1, "{report}");
    assert!(report.contains("✘ Config"), "{report}");
    assert!(report.contains("protondrive4linux init"), "{report}");
    assert!(
        report.contains("Proton CLI"),
        "the CLI check runs without a config:\n{report}"
    );
    assert!(report.contains("– Credentials"), "{report}");
}

#[test]
fn a_config_named_with_dash_c_that_does_not_exist_is_an_error() {
    let sb = Sandbox::new("typo");
    let out = sb.run(&sb.path("typo.toml"), &["doctor", "--offline"]);
    let report = text(&out);
    assert_eq!(code(&out), 1, "{report}");
    assert!(report.contains("typo.toml"), "{report}");
    assert!(report.contains("does not exist"), "{report}");
}

#[test]
fn a_config_that_will_not_parse_names_the_problem() {
    let sb = Sandbox::new("badtoml");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::write(&cfg, "[options]\nconflict = \"sideways\"\n").unwrap();
    let out = sb.run(&cfg, &["doctor", "--offline"]);
    let report = text(&out);
    assert_eq!(code(&out), 1, "{report}");
    assert!(report.contains("keep-both|newer|skip"), "{report}");
}

#[test]
fn a_cli_that_is_not_installed_is_a_failure() {
    let sb = Sandbox::new("nocli");
    let cfg = sb.write_config(&sb.path("no-such-proton-drive"));
    let out = sb.run(&cfg, &["doctor", "--offline"]);
    let report = text(&out);
    assert_eq!(code(&out), 1, "{report}");
    assert!(report.contains("✘ Proton CLI"), "{report}");
    assert!(report.contains("not found on PATH"), "{report}");
}

#[test]
fn a_state_folder_open_to_others_warns_without_failing() {
    let sb = Sandbox::new("openstate");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::create_dir_all(sb.path("state")).unwrap();
    fs::set_permissions(sb.path("state"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = sb.run(&cfg, &["doctor", "--offline"]);
    let report = text(&out);
    assert_eq!(code(&out), 0, "a warning is not a failure:\n{report}");
    assert!(report.contains("⚠ Permissions"), "{report}");
    assert!(report.contains("chmod 700"), "{report}");
    assert!(
        report.contains("it works, but look at the ⚠ items."),
        "{report}"
    );
}

#[test]
fn a_config_others_can_write_is_a_failure() {
    let sb = Sandbox::new("writablecfg");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o666)).unwrap();
    let out = sb.run(&cfg, &["doctor", "--offline"]);
    let report = text(&out);
    assert_eq!(code(&out), 1, "{report}");
    assert!(report.contains("✘ Permissions"), "{report}");
    assert!(report.contains("can be changed by other users"), "{report}");
}

#[test]
fn a_missing_local_folder_is_a_warning() {
    let sb = Sandbox::new("nofolder");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::remove_dir_all(sb.path("docs")).unwrap();
    let out = sb.run(&cfg, &["doctor", "--offline"]);
    let report = text(&out);
    assert_eq!(code(&out), 0, "{report}");
    assert!(report.contains("⚠ Folders"), "{report}");
    assert!(report.contains("does not exist"), "{report}");
}

/// Start a process whose executable is named `name` (a copy of the fake CLI,
/// which sleeps `FAKE_CLI_SLEEP_MS` before doing anything), and make the state
/// folder ready for a lock file.
fn sleeper(sb: &Sandbox, name: &str) -> std::process::Child {
    let exe = sb.path(name);
    fs::copy(FAKE_CLI, &exe).unwrap();
    settle(&exe);
    let child = Command::new(&exe)
        .arg("version")
        .env("FAKE_REMOTE_ROOT", sb.path("remote"))
        .env("FAKE_CLI_SLEEP_MS", "20000")
        .spawn()
        .unwrap();
    fs::create_dir_all(sb.path("state")).unwrap();
    fs::set_permissions(sb.path("state"), fs::Permissions::from_mode(0o700)).unwrap();
    child
}

fn write_lock(sb: &Sandbox, record: &str) {
    fs::write(sb.path("state/watch.lock"), record).unwrap();
}

#[test]
fn a_running_watcher_is_reported_from_its_lock() {
    let sb = Sandbox::new("watcher");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let mut watcher = sleeper(&sb, "protondrive4linux-watch-test");
    let pid = watcher.id();
    // The record a real watcher writes: its pid, start time and the boot id.
    write_lock(&sb, &neutronsync::process_lock::record_for(pid).unwrap());
    let report = text(&sb.run(&cfg, &["doctor", "--offline"]));
    let _ = watcher.kill();
    let _ = watcher.wait();
    assert!(
        report.contains(&format!(
            "✔ Watcher        running: pid {pid} (protondrive4linux-watch-test)"
        )),
        "{report}"
    );
}

#[test]
fn a_lock_whose_pid_now_belongs_to_another_process_is_stale() {
    let sb = Sandbox::new("reusedpid");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let mut other = sleeper(&sb, "not-ours");
    let pid = other.id();
    // Same pid, live process, but it did not write this record: a different
    // start time. What a lock left over from before a reboot looks like.
    let mine = neutronsync::process_lock::record_for(pid).unwrap();
    let boot = mine.split_whitespace().nth(2).unwrap();
    write_lock(&sb, &format!("{pid} 1 {boot}"));
    let identity = text(&sb.run(&cfg, &["doctor", "--offline"]));
    // And the bare pid older versions wrote: only "is it one of our programs" can
    // be asked, and a process called `not-ours` isn't.
    write_lock(&sb, &pid.to_string());
    let legacy = text(&sb.run(&cfg, &["doctor", "--offline"]));
    let _ = other.kill();
    let _ = other.wait();
    for report in [identity, legacy] {
        assert!(report.contains("• Watcher        none running"), "{report}");
        assert!(
            report.contains(&format!(
                "a leftover lock names pid {pid}, which is now another program"
            )),
            "{report}"
        );
    }
}

#[test]
fn probe_lists_the_remote_root_and_shows_how_it_was_parsed() {
    let sb = Sandbox::new("probe");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::write(sb.path("remote/my-files/a.txt"), "hello").unwrap();
    let out = sb.run(&cfg, &["doctor", "--offline", "--probe"]);
    let report = text(&out);
    assert_eq!(code(&out), 0, "{report}");
    assert!(
        report.contains("Probing `filesystem list -j /my-files`"),
        "{report}"
    );
    assert!(
        report.contains("--- parsed by protondrive4linux ---"),
        "{report}"
    );
    assert!(
        report.contains("file size=5") && report.contains("a.txt"),
        "{report}"
    );
}

#[test]
fn a_path_argument_probes_that_path() {
    let sb = Sandbox::new("probepath");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    fs::create_dir_all(sb.path("remote/my-files/sub")).unwrap();
    fs::write(sb.path("remote/my-files/sub/b.txt"), "hello").unwrap();
    let out = sb.run(&cfg, &["doctor", "--offline", "/my-files/sub"]);
    let report = text(&out);
    assert_eq!(code(&out), 0, "{report}");
    assert!(
        report.contains("Probing `filesystem list -j /my-files/sub`"),
        "{report}"
    );
    assert!(
        report.contains("file size=5") && report.contains("b.txt"),
        "{report}"
    );
}

#[test]
fn a_probe_that_the_cli_rejects_shows_the_clis_own_message() {
    let sb = Sandbox::new("probefail");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    // The fake answers a missing folder the way the real CLI does.
    let out = sb.run(&cfg, &["doctor", "--offline", "/my-files/nope"]);
    assert_eq!(code(&out), 2, "{}", text(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("list failed for \"/my-files/nope\""), "{err}");
    assert!(err.contains("No such file or directory"), "{err}");
}

#[test]
fn json_cannot_be_combined_with_a_probe() {
    let sb = Sandbox::new("conflict");
    let cfg = sb.write_config(Path::new(FAKE_CLI));
    let out = sb.run(&cfg, &["doctor", "--json", "--probe"]);
    assert_eq!(code(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be used with"));
}

// --- the sign-in check, through the library with an "online" host ----------------

struct Online;

impl Host for Online {
    fn systemctl_user(&self, _: &[&str]) -> Result<String, String> {
        Err("no systemd in tests".into())
    }
    fn tcp_connect(&self, _: &str, _: u16, _: Duration) -> Result<Duration, String> {
        Ok(Duration::from_millis(1))
    }
}

fn report_for(sb: &Sandbox, cli: &Path) -> doctor::Report {
    let cfg = sb.write_config(cli);
    let state = doctor::load_config(cfg.to_str());
    assert!(matches!(state, ConfigState::Loaded(_)));
    let env = Env {
        uid: Env::from_process().uid,
        exe: None,
        path_dirs: vec![],
        data_dirs: vec![],
        autostart_entry: sb.path("none"),
        proxy_vars: vec![],
    };
    doctor::run("test", &state, &Options::default(), &env, &Online)
}

fn level_of(r: &doctor::Report, id: &str) -> Level {
    r.checks.iter().find(|c| c.id == id).unwrap().level
}

#[test]
fn a_signed_in_session_is_reported_as_such() {
    let sb = Sandbox::new("signedin");
    let report = report_for(&sb, &sb.wrapper(""));
    assert_eq!(
        level_of(&report, "credentials"),
        Level::Ok,
        "{}",
        report.render()
    );
    assert_eq!(level_of(&report, "network"), Level::Ok);
    assert!(!report.has_failures(), "{}", report.render());
}

#[test]
fn a_signed_out_session_fails_and_says_how_to_sign_in() {
    let sb = Sandbox::new("signedout");
    let report = report_for(&sb, &sb.wrapper("FAKE_CLI_SIGNED_OUT=1"));
    let text = report.render();
    assert_eq!(level_of(&report, "credentials"), Level::Fail, "{text}");
    assert!(report.has_failures());
    assert!(text.contains("not signed in"), "{text}");
    assert!(text.contains("protondrive4linux login"), "{text}");
}

#[test]
fn a_cli_older_than_the_verified_version_is_a_warning() {
    let sb = Sandbox::new("oldcli");
    let wrapper = sb.wrapper("FAKE_CLI_VERSION='cli-drive 0.7.9'");
    let report = report_for(&sb, &wrapper);
    assert_eq!(level_of(&report, "cli"), Level::Warn, "{}", report.render());
    assert!(!report.has_failures());
}

// --- `init` writes a private config ----------------------------------------------

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn init_creates_the_config_folder_0700_and_the_file_0600() {
    let sb = Sandbox::new("init");
    let cfg = sb.path("fresh/conf/protondrive4linux.toml");
    let out = sb.run(&cfg, &["init"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(mode(&cfg), 0o600);
    assert_eq!(mode(cfg.parent().unwrap()), 0o700);
    assert!(fs::read_to_string(&cfg).unwrap().contains("[[pair]]"));

    // And doctor finds nothing to say about a config straight from `init`.
    let out = sb.run(&cfg, &["doctor", "--offline"]);
    let report = text(&out);
    assert!(report.contains("✔ Permissions"), "{report}");
}

#[test]
fn init_force_tightens_an_older_looser_config() {
    let sb = Sandbox::new("initforce");
    let cfg = sb.path("conf/protondrive4linux.toml");
    fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    fs::write(&cfg, "# old\n").unwrap();
    fs::set_permissions(&cfg, fs::Permissions::from_mode(0o644)).unwrap();

    let refused = sb.run(&cfg, &["init"]);
    assert_eq!(code(&refused), 1, "no --force, no overwrite");
    assert_eq!(fs::read_to_string(&cfg).unwrap(), "# old\n");

    let out = sb.run(&cfg, &["init", "--force"]);
    assert_eq!(code(&out), 0);
    assert_eq!(mode(&cfg), 0o600, "a forced rewrite also fixes the mode");
    assert!(fs::read_to_string(&cfg).unwrap().contains("[[pair]]"));
}
