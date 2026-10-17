//! Infrastructure shared by the end-to-end tests: a sandbox with a scrubbed
//! environment to run the real `protondrive4linux` binary in, and the fake
//! `proton-drive` it talks to (from `tests/support/`). Nothing here touches the
//! network or the developer's own setup.

// Each test binary uses a different part of this.
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

pub const BIN: &str = env!("CARGO_BIN_EXE_protondrive4linux");
pub const FAKE_CLI: &str = env!("CARGO_BIN_EXE_fake-proton-drive");

pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    pub fn new(tag: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!(
            "protondrive4linux-it-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for d in ["remote/my-files", "docs", "empty-bin", "home", "xdg-share"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        Sandbox { root }
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.root.join(p)
    }

    /// A config for the sandbox: the CLI at `cli`, state under the sandbox, one
    /// pair. The file is 0600 in a 0700 folder, as `init` would leave it.
    pub fn write_config(&self, cli: &Path) -> PathBuf {
        let dir = self.path("conf");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let file = dir.join("c.toml");
        fs::write(
            &file,
            format!(
                "[cli]\nbinary = {:?}\n[options]\nstate_dir = {:?}\n\
                 [[pair]]\nname = \"docs\"\nlocal = {:?}\nremote = \"docs\"\n",
                cli.display().to_string(),
                self.path("state").display().to_string(),
                self.path("docs").display().to_string(),
            ),
        )
        .unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        file
    }

    /// Run `protondrive4linux -c CONFIG <args>` with only the environment we
    /// give it (the fake CLI is told where its remote lives and which version to
    /// report).
    pub fn run(&self, config: &Path, args: &[&str]) -> Output {
        let mut cmd = self.command();
        cmd.arg("-c").arg(config).args(args);
        cmd.output().expect("the binary runs")
    }

    pub fn command(&self) -> Command {
        self.command_for(BIN)
    }

    /// The same scrubbed environment for another of this crate's binaries.
    pub fn command_for(&self, bin: &str) -> Command {
        let mut cmd = Command::new(bin);
        cmd.env_clear()
            .current_dir(&self.root)
            .env("PATH", self.path("empty-bin"))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("home/.config"))
            .env("XDG_DATA_HOME", self.path("home/.local/share"))
            .env("XDG_DATA_DIRS", self.path("xdg-share"))
            .env("FAKE_REMOTE_ROOT", self.path("remote"))
            .env("FAKE_CLI_VERSION", "cli-drive 0.8.0");
        cmd
    }

    /// A stand-in `proton-drive` that runs the fake CLI with `env` set, so a
    /// library-level test can pick the version or sign-out state per test
    /// without touching this process's environment.
    pub fn wrapper(&self, env: &str) -> PathBuf {
        let path = self.path("proton-drive");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nFAKE_REMOTE_ROOT={:?} FAKE_CLI_VERSION='cli-drive 0.8.0' {env} \
                 exec {FAKE_CLI:?} \"$@\"\n",
                self.path("remote").display().to_string()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        settle(&path);
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Put back anything a test made unreadable before deleting.
        for d in ["conf", "state"] {
            let _ = fs::set_permissions(self.root.join(d), fs::Permissions::from_mode(0o700));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Start `path` once and stop it again, retrying while the kernel says "text
/// file busy" (ETXTBSY): another test thread forking while this file is still
/// open for writing leaves its child holding a copy of the descriptor for a
/// moment, and exec-ing the file then fails. After one successful start nobody
/// holds it for writing, so later runs - including the ones the binary makes -
/// are safe.
pub fn settle(path: &Path) {
    for _ in 0..400 {
        match Command::new(path)
            .arg("--settle")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(5)),
            Ok(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(_) => return,
        }
    }
}

pub fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn code(out: &Output) -> i32 {
    out.status.code().expect("exited normally")
}
