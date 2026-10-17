//! `protondrive4linux doctor`: a read-only health check of the installation
//! (M7-003).
//!
//! Each check answers one question somebody debugging "why isn't it syncing?"
//! ends up asking: is the CLI there and recent enough, am I signed in, can I
//! reach Proton, are my files private, is anything running in the background.
//! A check never changes anything - no file is created or fixed, the sign-in
//! state is only asked about (never read: the session lives in the keyring and
//! is the CLI's business, see `SECURITY_AUDIT.md`) - so it is always safe to
//! run, including while a sync is in progress. What it does run is
//! `proton-drive version`, one listing of the remote root to learn whether the
//! session works, `systemctl --user show`, and a bare TCP connect to Proton's
//! API host (`--offline` skips the ones that touch the network).
//!
//! The checks return data rather than printing, and everything they look at
//! outside the process goes through [`Env`] (paths, the uid) or [`Host`]
//! (systemd, the network), so the tests can supply both and exercise every
//! outcome without a Proton account, a systemd, or a network.

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::config::{self, Config};
use crate::protoncli::{self, ProtonCli, Remote};

/// Proton's Drive API host: what the CLI talks to, so being able to open an
/// HTTPS connection to it is what "the network works" has to mean here.
const API_HOST: &str = "drive-api.proton.me";
const API_PORT: u16 = 443;
/// How long the whole reachability test may take, DNS included.
const NET_TIMEOUT: Duration = Duration::from_secs(5);
/// A diagnostic mustn't hang the way a transfer's 30-minute timeout would, so
/// the CLI calls it makes get this instead of the configured `cli_timeout_secs`.
const PROBE_TIMEOUT_SECS: u64 = 30;
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// The packaged systemd user units (see `systemd/`).
const WATCH_UNIT: &str = "protondrive4linux-watch.service";
const TIMER_UNIT: &str = "protondrive4linux.timer";
/// The packaged launcher and the GUI binary it starts.
const DESKTOP_FILE: &str = "protondrive4linux-gui.desktop";
const GUI_BINARY: &str = "protondrive4linux-gui";

/// How a check came out. Ordered so that the worst of several is their `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Not run (needs something an earlier check found missing, or `--offline`).
    Skip,
    Ok,
    /// Worth knowing, nothing to fix.
    Info,
    /// Works, but something deserves attention.
    Warn,
    /// Will stop syncing, or is a real exposure.
    Fail,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Skip => "skip",
            Level::Ok => "ok",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Level::Skip => "–",
            Level::Ok => "✔",
            Level::Info => "•",
            Level::Warn => "⚠",
            Level::Fail => "✘",
        }
    }
}

/// One line of the report.
#[derive(Clone, Debug)]
pub struct Check {
    pub id: &'static str,
    pub title: &'static str,
    pub level: Level,
    /// What was found. May span lines.
    pub detail: String,
    /// What to do about it (one suggestion per line), when there's something to do.
    pub hint: Option<String>,
}

impl Check {
    fn new(
        id: &'static str,
        title: &'static str,
        level: Level,
        detail: impl Into<String>,
    ) -> Check {
        Check {
            id,
            title,
            level,
            detail: detail.into(),
            hint: None,
        }
    }

    fn skipped(id: &'static str, title: &'static str, why: impl Into<String>) -> Check {
        Check::new(id, title, Level::Skip, why)
    }

    fn hint(mut self, hint: impl Into<String>) -> Check {
        self.hint = Some(hint.into());
        self
    }
}

/// How many checks landed where. `ok` includes the merely informational ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    pub ok: usize,
    pub warnings: usize,
    pub failed: usize,
    pub skipped: usize,
}

pub struct Report {
    /// This binary's `<version> (<commit>)`.
    pub version: String,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn counts(&self) -> Counts {
        let n = |f: fn(Level) -> bool| self.checks.iter().filter(|c| f(c.level)).count();
        Counts {
            ok: n(|l| matches!(l, Level::Ok | Level::Info)),
            warnings: n(|l| l == Level::Warn),
            failed: n(|l| l == Level::Fail),
            skipped: n(|l| l == Level::Skip),
        }
    }

    /// Whether anything is wrong enough to stop syncing (the exit status).
    /// Warnings don't count: they're things to look at, not things that break.
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.level == Level::Fail)
    }

    fn summary(&self) -> String {
        let c = self.counts();
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut s = format!(
            "{} ok, {}, {} failed",
            c.ok,
            plural(c.warnings, "warning", "warnings"),
            c.failed
        );
        if c.skipped > 0 {
            s.push_str(&format!(", {} skipped", c.skipped));
        }
        s.push_str(if c.failed > 0 {
            " - fix the ✘ items first."
        } else if c.warnings > 0 {
            " - it works, but look at the ⚠ items."
        } else {
            " - all good."
        });
        s
    }

    /// The report as text: one line per check, details and hints indented
    /// underneath, a one-line verdict at the end.
    pub fn render(&self) -> String {
        let width = self
            .checks
            .iter()
            .map(|c| c.title.chars().count())
            .max()
            .unwrap_or(0);
        let indent = " ".repeat(4 + width + 2);
        let mut out = String::from("protondrive4linux doctor\n\n");
        for c in &self.checks {
            let pad = " ".repeat(width - c.title.chars().count());
            let mut lines = c.detail.lines();
            out.push_str(&format!(
                "  {} {}{}  {}\n",
                c.level.symbol(),
                c.title,
                pad,
                lines.next().unwrap_or("")
            ));
            for line in lines {
                out.push_str(&format!("{indent}{line}\n"));
            }
            if let Some(hint) = &c.hint {
                for line in hint.lines() {
                    out.push_str(&format!("{indent}→ {line}\n"));
                }
            }
        }
        out.push('\n');
        out.push_str(&self.summary());
        out.push('\n');
        out
    }

    /// The same report for scripts and bug reports (`doctor --json`).
    pub fn to_json(&self) -> serde_json::Value {
        let c = self.counts();
        json!({
            "version": self.version,
            "ok": !self.has_failures(),
            "summary": {
                "ok": c.ok,
                "warnings": c.warnings,
                "failed": c.failed,
                "skipped": c.skipped,
            },
            "checks": self.checks.iter().map(|c| json!({
                "id": c.id,
                "title": c.title,
                "level": c.level.as_str(),
                "detail": c.detail,
                "hint": c.hint,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Where the config stands. Kept apart from a plain `Result<Config>` because
/// "there is none" and "there is one and it's broken" call for different
/// advice, and because a broken or missing config must not stop the checks
/// that don't need it (is the CLI installed? is the network up?).
pub enum ConfigState {
    Loaded(Box<Config>),
    /// No config file anywhere it looks; `default_path` is where `init` writes one.
    Missing {
        default_path: PathBuf,
    },
    /// A file that wouldn't load (unreadable, bad TOML, a value out of range).
    Invalid {
        path: PathBuf,
        error: String,
    },
}

/// Find and load the config the way every command does - with one difference:
/// a file named with `-c` that doesn't exist is an error here. The other
/// commands quietly fall back to the next place they search, which for a typo
/// means running against a config the user didn't ask for; the one command
/// that exists to explain what's going on shouldn't do that.
pub fn load_config(explicit: Option<&str>) -> ConfigState {
    if let Some(e) = explicit {
        let path = config::expand(e);
        if !path.is_file() {
            return ConfigState::Invalid {
                path,
                error: "the file given with -c does not exist".to_string(),
            };
        }
    }
    match config::find_config(explicit) {
        None => ConfigState::Missing {
            default_path: config::default_config_path(),
        },
        Some(path) => match config::load_path(&path) {
            Ok(c) => ConfigState::Loaded(Box::new(c)),
            Err(e) => ConfigState::Invalid {
                path,
                error: format!("{e:#}"),
            },
        },
    }
}

/// What the checks read from the process environment, gathered once so tests
/// can substitute their own.
#[derive(Clone, Debug)]
pub struct Env {
    /// The effective uid, `None` if `/proc` can't say (ownership checks are then skipped).
    pub uid: Option<u32>,
    /// This executable, to say which install is answering.
    pub exe: Option<PathBuf>,
    /// `$PATH`, absolute entries only - the same rule the CLI lookup follows
    /// (see `protoncli::which`), so a stray `.` can't make something else run.
    pub path_dirs: Vec<PathBuf>,
    /// `$XDG_DATA_HOME` then `$XDG_DATA_DIRS`: where launchers are looked up.
    pub data_dirs: Vec<PathBuf>,
    /// Where the GUI's "launch at login" switch puts its entry.
    pub autostart_entry: PathBuf,
    /// Names (never values: a proxy URL can carry a password) of the proxy
    /// variables that are set.
    pub proxy_vars: Vec<String>,
}

impl Env {
    pub fn from_process() -> Env {
        let path_dirs = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .filter(|d| d.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        let mut data_dirs = vec![config::data_home()];
        match std::env::var("XDG_DATA_DIRS") {
            Ok(v) if !v.is_empty() => {
                data_dirs.extend(v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from))
            }
            _ => data_dirs.extend(["/usr/local/share", "/usr/share"].map(PathBuf::from)),
        }
        let proxy_vars = [
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
        .into_iter()
        .filter(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()))
        .map(String::from)
        .collect();
        Env {
            // The owner of /proc/self is the process's effective uid - a way to
            // ask without a libc dependency.
            uid: fs::metadata("/proc/self").ok().map(|m| m.uid()),
            exe: std::env::current_exe().ok(),
            path_dirs,
            data_dirs,
            autostart_entry: config::config_home()
                .join("autostart")
                .join("protondrive4linux.desktop"),
            proxy_vars,
        }
    }
}

/// The two things a check can't learn from files and the CLI alone.
pub trait Host {
    /// Run `systemctl --user <args>`: its output, or why that couldn't be done.
    fn systemctl_user(&self, args: &[&str]) -> Result<String, String>;
    /// Open a TCP connection to `host:port`, resolving the name first; how long
    /// the connect took, or what went wrong. Nothing is sent over it.
    fn tcp_connect(&self, host: &str, port: u16, timeout: Duration) -> Result<Duration, String>;
}

/// The real thing.
pub struct SystemHost {
    systemctl: Option<PathBuf>,
    systemctl_timeout: Duration,
}

impl SystemHost {
    pub fn new(env: &Env) -> SystemHost {
        SystemHost {
            systemctl: find_executable("systemctl", &env.path_dirs),
            systemctl_timeout: SYSTEMCTL_TIMEOUT,
        }
    }
}

impl Host for SystemHost {
    fn systemctl_user(&self, args: &[&str]) -> Result<String, String> {
        let bin = self
            .systemctl
            .as_ref()
            .ok_or("systemctl not found (this system doesn't use systemd)")?;
        let mut child = Command::new(bin)
            .arg("--user")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run systemctl: {e}"))?;
        // Its output is a few hundred bytes, far below a pipe's capacity, so
        // waiting for it to exit before reading can't deadlock.
        let deadline = Instant::now() + self.systemctl_timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("systemctl did not answer in time".to_string());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(format!("could not wait for systemctl: {e}")),
            }
        };
        let (mut stdout, mut stderr) = (String::new(), String::new());
        if let Some(mut o) = child.stdout.take() {
            let _ = o.read_to_string(&mut stdout);
        }
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        if status.success() {
            Ok(stdout)
        } else {
            let why = first_line(if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            });
            Err(if why.is_empty() {
                format!("systemctl failed ({status})")
            } else {
                why
            })
        }
    }

    fn tcp_connect(&self, host: &str, port: u16, timeout: Duration) -> Result<Duration, String> {
        use std::net::{TcpStream, ToSocketAddrs};
        let deadline = Instant::now() + timeout;
        // Name lookup has no timeout of its own and can sit for a long time when
        // DNS is unreachable, so ask on a helper thread and stop waiting at the
        // deadline (the thread ends whenever the resolver gives up).
        let (tx, rx) = std::sync::mpsc::channel();
        let target = format!("{host}:{port}");
        std::thread::spawn(move || {
            let _ = tx.send(target.to_socket_addrs().map(|a| a.collect::<Vec<_>>()));
        });
        let addrs = match rx.recv_timeout(timeout) {
            Ok(Ok(a)) if !a.is_empty() => a,
            Ok(Ok(_)) => return Err(format!("{host} has no address")),
            Ok(Err(e)) => return Err(format!("could not resolve {host}: {e}")),
            Err(_) => {
                return Err(format!(
                    "looking up {host} took longer than {}s",
                    timeout.as_secs()
                ))
            }
        };
        let started = Instant::now();
        let mut last = String::new();
        for addr in addrs {
            // A cap per address, so one that black-holes (an IPv6 route that
            // goes nowhere) can't use up the time the next one needs.
            let left = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(3));
            if left.is_zero() {
                break;
            }
            match TcpStream::connect_timeout(&addr, left) {
                Ok(_) => return Ok(started.elapsed()),
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        Err(if last.is_empty() {
            format!("connecting to {host}:{port} timed out")
        } else {
            format!("could not connect to {host}:{port} ({last})")
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Don't touch the network: skips the reachability test and the sign-in probe.
    pub offline: bool,
}

/// Run every check.
pub fn run(
    version: &str,
    state: &ConfigState,
    opts: &Options,
    env: &Env,
    host: &dyn Host,
) -> Report {
    // With no usable config the checks that don't depend on one still run, on
    // the defaults - `proton-drive` on PATH is a fine thing to look for.
    let defaults = config::parse_str("", None).expect("the empty config is valid");
    let loaded: Option<&Config> = match state {
        ConfigState::Loaded(c) => Some(c.as_ref()),
        _ => None,
    };
    let cfg = loaded.unwrap_or(&defaults);

    let (cli_check, cli) = check_cli(cfg, loaded.is_some());
    let network = check_network(opts, env, host);
    let credentials = check_credentials(loaded, cli.as_ref(), opts, &network);
    Report {
        version: version.to_string(),
        checks: vec![
            check_version(version, env),
            check_config(state),
            cli_check,
            network,
            credentials,
            check_folders(loaded),
            check_permissions(state, env),
            check_systemd(host),
            check_daemon(loaded),
            check_desktop(env),
        ],
    }
}

fn check_version(version: &str, env: &Env) -> Check {
    let mut detail = format!("protondrive4linux {version}");
    if let Some(exe) = &env.exe {
        detail.push_str(&format!(" · {}", exe.display()));
    }
    Check::new("version", "Version", Level::Ok, detail)
}

fn check_config(state: &ConfigState) -> Check {
    match state {
        ConfigState::Loaded(cfg) => {
            let path = cfg
                .source_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            match cfg.pairs.len() {
                0 => Check::new(
                    "config",
                    "Config",
                    Level::Info,
                    format!("{path} · no folder pairs yet"),
                )
                .hint("Add a folder in the GUI, or a [[pair]] entry in the config."),
                1 => Check::new(
                    "config",
                    "Config",
                    Level::Ok,
                    format!("{path} · 1 folder pair"),
                ),
                n => Check::new(
                    "config",
                    "Config",
                    Level::Ok,
                    format!("{path} · {n} folder pairs"),
                ),
            }
        }
        ConfigState::Missing { default_path } => {
            Check::new("config", "Config", Level::Fail, "no config file found").hint(format!(
                "Run `protondrive4linux init` to create one at {}, or pass one with -c PATH.",
                default_path.display()
            ))
        }
        ConfigState::Invalid { path, error } => Check::new(
            "config",
            "Config",
            Level::Fail,
            format!("{}: {error}", path.display()),
        )
        .hint(
            "Fix the problem above (protondrive4linux.example.toml shows every option), \
             then run doctor again.",
        ),
    }
}

/// Resolve the CLI, run its `version`, and judge what comes back. Also hands
/// back the `ProtonCli` (with the shorter timeout) when it's usable, for the
/// sign-in probe.
fn check_cli(cfg: &Config, config_loaded: bool) -> (Check, Option<ProtonCli>) {
    let mut probe_cfg = cfg.clone();
    probe_cfg.cli_timeout_secs = PROBE_TIMEOUT_SECS;
    let cli = ProtonCli::new(&probe_cfg);

    let Some(path) = cli.resolve_binary() else {
        let mut detail = format!("`{}` not found on PATH", cfg.binary);
        if !config_loaded {
            detail.push_str(" (looked for the default name: there is no usable config)");
        }
        let check = Check::new("cli", "Proton CLI", Level::Fail, detail).hint(
            "Install the official Proton Drive CLI (https://proton.me/blog/proton-drive-cli) \
             and make sure it is on your PATH, or set cli.binary in the config.",
        );
        return (check, None);
    };

    let version = cli.version();
    let mut level = Level::Ok;
    let mut hints: Vec<String> = Vec::new();
    let mut detail = path.display().to_string();

    if version.starts_with("error:") || version.is_empty() {
        let why = if version.is_empty() {
            "it ran but printed no version".to_string()
        } else {
            first_line(version.trim_start_matches("error:"))
        };
        let check = Check::new(
            "cli",
            "Proton CLI",
            Level::Fail,
            format!("{detail} · `version` failed: {why}"),
        )
        .hint(format!(
            "Run `{} version` yourself to see why.",
            shell_quote(&path)
        ));
        return (check, None);
    }

    let banner = first_line(&version);
    detail.push_str(&format!(" · {banner}"));
    if let Some(found) = protoncli::parse_cli_version(&version) {
        if found < protoncli::MIN_CLI_VERSION {
            let (a, b, c) = protoncli::MIN_CLI_VERSION;
            level = Level::Warn;
            detail.push_str(&format!(
                "\nolder than {a}.{b}.{c}, which this build is verified against: uploads and \
                 downloads use options older versions don't have"
            ));
            hints.push("Update the proton-drive CLI.".to_string());
        }
    }
    if let Some(warning) = cli.binary_location_warning() {
        level = level.max(Level::Warn);
        detail.push_str(&format!("\n{warning}"));
        hints.push("Make the directory writable by you alone: chmod go-w on it.".to_string());
    }
    let mut check = Check::new("cli", "Proton CLI", level, detail);
    if !hints.is_empty() {
        check = check.hint(hints.join("\n"));
    }
    (check, Some(cli))
}

fn check_network(opts: &Options, env: &Env, host: &dyn Host) -> Check {
    if opts.offline {
        return Check::skipped("network", "Network", "not checked (--offline)");
    }
    match host.tcp_connect(API_HOST, API_PORT, NET_TIMEOUT) {
        Ok(took) => Check::new(
            "network",
            "Network",
            Level::Ok,
            format!("{API_HOST}:{API_PORT} reachable ({} ms)", took.as_millis()),
        ),
        Err(why) => {
            // The test connects directly. Behind a proxy that can fail while the
            // CLI, which may go through the proxy, works - so it's a warning there.
            let proxied = !env.proxy_vars.is_empty();
            let mut detail = why;
            if proxied {
                detail.push_str(&format!(
                    "\na proxy is configured ({}); this test connects directly, so it may \
                     not reflect what the CLI can do",
                    env.proxy_vars.join(", ")
                ));
            }
            Check::new(
                "network",
                "Network",
                if proxied { Level::Warn } else { Level::Fail },
                detail,
            )
            .hint(
                "Check your internet connection, DNS and firewall: Proton Drive needs HTTPS \
                 (port 443) to reach it.",
            )
        }
    }
}

/// Whether the CLI has a working session. Asks the same way the watcher and the
/// GUI do - list the remote root, and read "please log in" from the failure
/// (`protoncli::is_not_logged_in`) - so the three can't disagree. The session
/// itself is never read.
fn check_credentials(
    loaded: Option<&Config>,
    cli: Option<&ProtonCli>,
    opts: &Options,
    network: &Check,
) -> Check {
    let (id, title) = ("credentials", "Credentials");
    let Some(cfg) = loaded else {
        return Check::skipped(id, title, "needs a usable config");
    };
    let Some(cli) = cli else {
        return Check::skipped(id, title, "needs the proton-drive CLI");
    };
    if opts.offline {
        return Check::skipped(id, title, "not checked (--offline)");
    }
    if network.level == Level::Fail {
        return Check::skipped(id, title, "not checked: Proton can't be reached");
    }
    let store = cfg.credentials_store.as_deref();
    let store_note = match store {
        Some(s) => format!("store: {s}"),
        None => "store: the CLI's default".to_string(),
    };
    match cli.list_dir(&cfg.remote_root) {
        Ok(_) if store == Some("unsafe_file") => Check::new(
            id,
            title,
            Level::Warn,
            format!(
                "signed in · {store_note}\nthe CLI keeps the session in a plain file, \
                 readable by anything running as you"
            ),
        )
        .hint("Use cli.credentials_store = \"keychain\" or \"pass\" if you can."),
        Ok(_) => Check::new(id, title, Level::Ok, format!("signed in · {store_note}")),
        Err(e) => {
            let msg = format!("{e:#}");
            if protoncli::is_secret_store_unreachable(&msg) {
                Check::new(
                    id,
                    title,
                    Level::Fail,
                    format!(
                        "not signed in: the CLI could not reach its secret store (keyring \
                         locked, no D-Bus session, or a headless login) · {store_note}"
                    ),
                )
                .hint(
                    "Unlock or start your keyring in this session (GNOME Keyring, KWallet), or \
                     set cli.credentials_store = \"pass\" or \"unsafe_file\" in the config.\n\
                     Then run `protondrive4linux login`.",
                )
            } else if protoncli::is_not_logged_in(&msg) {
                Check::new(
                    id,
                    title,
                    Level::Fail,
                    format!("not signed in · {store_note}"),
                )
                .hint("Run `protondrive4linux login` (or use Sign in in the GUI).")
            } else {
                Check::new(
                    id,
                    title,
                    Level::Warn,
                    format!("could not verify the session: {}", first_line(&msg)),
                )
                .hint(
                    "Usually a network hiccup: run doctor again. \
                     `protondrive4linux doctor --probe` shows the CLI's raw output.",
                )
            }
        }
    }
}

fn check_folders(loaded: Option<&Config>) -> Check {
    let (id, title) = ("folders", "Folders");
    let Some(cfg) = loaded else {
        return Check::skipped(id, title, "needs a usable config");
    };
    if cfg.pairs.is_empty() {
        return Check::new(id, title, Level::Info, "no folder pairs configured");
    }
    let mut level = Level::Ok;
    let mut problems = Vec::new();
    for p in &cfg.pairs {
        match fs::metadata(&p.local) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                level = Level::Fail;
                problems.push(format!("{}: {} is not a folder", p.name, p.local.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                level = level.max(Level::Warn);
                problems.push(format!("{}: {} does not exist", p.name, p.local.display()));
            }
            Err(e) => {
                level = Level::Fail;
                problems.push(format!("{}: {}: {e}", p.name, p.local.display()));
            }
        }
    }
    let n = cfg.pairs.len();
    if problems.is_empty() {
        let noun = if n == 1 { "folder" } else { "folders" };
        return Check::new(
            id,
            title,
            Level::Ok,
            format!("{n} local {noun}, all present"),
        );
    }
    Check::new(id, title, level, problems.join("\n")).hint(
        "Create it, mount the drive it lives on, or fix `local` in the config. A missing \
         folder is never synced as \"everything deleted\": that pair is refused until it's back.",
    )
}

/// Which mode a path is expected to have.
#[derive(Clone, Copy)]
enum Want {
    /// Nobody else may read or change it: 0700 for a folder, 0600 for a file.
    Private,
    /// Others may look but not change it.
    NotWritableByOthers,
}

struct Finding {
    level: Level,
    text: String,
    fix: String,
}

fn octal(mode: u32) -> String {
    format!("{:04o}", mode & 0o7777)
}

/// Who owns something, judged against who is running. `None` means fine.
///
/// Someone else owning it is a failure - the classic way to get there is running
/// the app once with `sudo`, which leaves root-owned files the normal user can
/// then neither write nor remove. The exception is root owning something we only
/// read: an administrator-provided config is a legitimate setup, and root can
/// already do anything, so that is no exposure - just a note that the GUI
/// can't save settings into it. `must_write` is for what the app has to write
/// (the state folder), where root's ownership is always a problem.
fn owner_finding(
    label: &str,
    path: &Path,
    owner: u32,
    me: u32,
    must_write: bool,
) -> Option<Finding> {
    if owner == me {
        return None;
    }
    let fix = format!("sudo chown -R \"$USER\" {}", shell_quote(path));
    if owner == 0 && !must_write {
        return Some(Finding {
            level: Level::Warn,
            text: format!(
                "{label} {} belongs to root, so the GUI can't save settings into it (fine if \
                 you set it up that way)",
                path.display()
            ),
            fix,
        });
    }
    let who = if owner == 0 {
        "root".to_string()
    } else {
        format!("uid {owner}")
    };
    Some(Finding {
        level: Level::Fail,
        text: format!(
            "{label} {} belongs to {who}, not to you (created with sudo?)",
            path.display()
        ),
        fix,
    })
}

/// Judge one path's owner and mode. `None` means it's fine (or isn't there).
fn assess(
    label: &str,
    why: &str,
    path: &Path,
    want: Want,
    uid: Option<u32>,
    must_write: bool,
) -> Option<Finding> {
    let meta = fs::metadata(path).ok()?;
    let mode = meta.mode() & 0o7777;
    let private_mode = if meta.is_dir() { "700" } else { "600" };
    let q = shell_quote(path);
    if let Some(me) = uid {
        if let Some(f) = owner_finding(label, path, meta.uid(), me, must_write) {
            return Some(f);
        }
    }
    // In a sticky folder (like /tmp) others can't rename or delete what's yours,
    // so being writable doesn't expose it.
    let sticky = meta.is_dir() && mode & 0o1000 != 0;
    if mode & 0o022 != 0 && !sticky {
        return Some(Finding {
            level: Level::Fail,
            text: format!(
                "{label} {} can be changed by other users (mode {}) - {why}",
                path.display(),
                octal(mode)
            ),
            fix: match want {
                Want::Private => format!("chmod {private_mode} {q}"),
                Want::NotWritableByOthers => format!("chmod go-w {q}"),
            },
        });
    }
    if matches!(want, Want::Private) && mode & 0o077 != 0 {
        return Some(Finding {
            level: Level::Warn,
            text: format!(
                "{label} {} is open to other users (mode {}) - {why}",
                path.display(),
                octal(mode)
            ),
            fix: format!("chmod {private_mode} {q}"),
        });
    }
    None
}

/// The config and the state folder must belong to you and be closed to others.
/// The config decides which program runs as `cli.binary`, so anyone who can
/// write it (or the folder it sits in) can run code as you; the state folder
/// holds the sync log and database, which list your file names. This is what
/// the app itself creates (0700 folders, 0600 files - see `config::write_private`,
/// `logger.rs`, `stats.rs`), so a deviation is something that happened since.
fn check_permissions(state: &ConfigState, env: &Env) -> Check {
    let (id, title) = ("permissions", "Permissions");
    let config_path = match state {
        ConfigState::Loaded(c) => c.source_path.clone(),
        ConfigState::Invalid { path, .. } => Some(path.clone()),
        ConfigState::Missing { .. } => None,
    };
    let state_dir = match state {
        ConfigState::Loaded(c) => Some(c.state_dir.clone()),
        _ => None,
    };

    let mut findings: Vec<Finding> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut look = |label: &str, why: &str, path: &Path, want: Want, must_write: bool| {
        let Ok(meta) = fs::metadata(path) else {
            return;
        };
        seen.push(format!("{label} {}", octal(meta.mode())));
        findings.extend(assess(label, why, path, want, env.uid, must_write));
    };
    if let Some(file) = &config_path {
        look(
            "config file",
            "it names your folders and the CLI to run",
            file,
            Want::Private,
            false,
        );
        // A bare file name has an empty parent: that's the current directory.
        let dir = match file.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        look(
            "config folder",
            "someone could swap the config for one that runs their program",
            dir,
            Want::NotWritableByOthers,
            false,
        );
    }
    if let Some(dir) = &state_dir {
        look(
            "state folder",
            "it holds the sync log and database, which list your file names",
            dir,
            Want::Private,
            true,
        );
    }

    if findings.is_empty() {
        // Nothing was looked at: say so rather than hand out a green tick.
        if seen.is_empty() {
            return Check::skipped(
                id,
                title,
                "nothing to check yet: no config file or state folder exists",
            );
        }
        let mut detail = format!("{}: yours, closed to others", seen.join(" · "));
        if let Some(dir) = &state_dir {
            if !dir.exists() {
                detail.push_str("\nthe state folder doesn't exist yet; it will be created, private, on first use");
            }
        }
        return Check::new(id, title, Level::Ok, detail);
    }
    let level = findings
        .iter()
        .map(|f| f.level)
        .max()
        .unwrap_or(Level::Warn);
    let text = findings
        .iter()
        .map(|f| f.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let fixes = findings
        .iter()
        .map(|f| f.fix.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Check::new(id, title, level, text).hint(fixes)
}

/// One unit's state, as `systemctl show` reports it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Unit {
    id: String,
    load: String,
    active: String,
    sub: String,
    /// `UnitFileState`: enabled, disabled, static, masked, ...
    enabled: String,
}

/// Parse `systemctl show` output for several units: `Key=Value` lines, one
/// block per unit, blocks separated by an empty line.
fn parse_units(out: &str) -> Vec<Unit> {
    out.split("\n\n")
        .map(|block| {
            let mut u = Unit::default();
            for line in block.lines() {
                let Some((k, v)) = line.split_once('=') else {
                    continue;
                };
                let v = v.trim().to_string();
                match k {
                    "Id" => u.id = v,
                    "LoadState" => u.load = v,
                    "ActiveState" => u.active = v,
                    "SubState" => u.sub = v,
                    "UnitFileState" => u.enabled = v,
                    _ => {}
                }
            }
            u
        })
        .filter(|u| !u.id.is_empty())
        .collect()
}

fn check_systemd(host: &dyn Host) -> Check {
    let (id, title) = ("systemd", "systemd");
    let out = match host.systemctl_user(&[
        "show",
        "--no-pager",
        "--property=Id,LoadState,ActiveState,SubState,UnitFileState",
        WATCH_UNIT,
        TIMER_UNIT,
    ]) {
        Ok(o) => o,
        Err(why) => return Check::skipped(id, title, format!("not checked: {why}")),
    };
    let units = parse_units(&out);
    let installed = |name: &str| units.iter().find(|u| u.id == name && u.load != "not-found");
    let (watch, timer) = (installed(WATCH_UNIT), installed(TIMER_UNIT));
    if watch.is_none() && timer.is_none() {
        return Check::new(
            id,
            title,
            Level::Info,
            "no protondrive4linux units installed (optional: the tray app or \
             `protondrive4linux watch` keep sync going without systemd)",
        );
    }

    let mut level = Level::Ok;
    let mut hints: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for (name, unit) in [(WATCH_UNIT, watch), (TIMER_UNIT, timer)] {
        let Some(u) = unit else {
            lines.push(format!("{name}: not installed"));
            continue;
        };
        let mut state = u.active.clone();
        if !u.sub.is_empty() && u.sub != u.active {
            state.push_str(&format!(" ({})", u.sub));
        }
        if !u.enabled.is_empty() {
            state.push_str(&format!(" · {}", u.enabled));
        }
        lines.push(format!("{name}: {state}"));
        let running = u.active == "active";
        if u.active == "failed" {
            level = level.max(Level::Warn);
            hints.push(format!("journalctl --user -u {name} -e"));
        } else if u.enabled == "enabled" && !running {
            level = level.max(Level::Warn);
            hints.push(format!("systemctl --user start {name}"));
        }
    }
    if watch.is_some_and(|u| u.active == "active") && timer.is_some_and(|u| u.active == "active") {
        level = level.max(Level::Warn);
        lines.push(
            "both live sync and the periodic timer are active; use one, not both".to_string(),
        );
        hints.push(format!("systemctl --user disable --now {TIMER_UNIT}"));
    }
    let mut check = Check::new(id, title, level, lines.join("\n"));
    if !hints.is_empty() {
        check = check.hint(hints.join("\n"));
    }
    check
}

/// Is a watcher (the tray app's, `protondrive4linux watch`, or the systemd
/// service - they all take the same lock) running right now?
fn check_daemon(loaded: Option<&Config>) -> Check {
    let (id, title) = ("daemon", "Watcher");
    let Some(cfg) = loaded else {
        return Check::skipped(
            id,
            title,
            "needs a usable config (its lock is in the state folder)",
        );
    };
    let lock = cfg.state_dir.join(crate::watcher::LOCK_FILE);
    let Ok(text) = fs::read_to_string(&lock) else {
        return Check::new(
            id,
            title,
            Level::Info,
            "none running (start one with the tray app, `protondrive4linux watch`, or the \
             systemd service)",
        );
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return Check::new(
            id,
            title,
            Level::Info,
            format!(
                "{} is unreadable; the next watcher replaces it",
                lock.display()
            ),
        );
    };
    let alive = Path::new(&format!("/proc/{pid}")).exists();
    let program = if alive {
        fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    } else {
        None
    };
    lock_verdict(pid, alive, program.as_deref())
}

/// What a lock file naming `pid` means. A lock outlives a watcher that was
/// killed rather than stopped (a crash, a power cut, SIGKILL), and the pid it
/// names may since have gone to an unrelated program - routine after a reboot,
/// when pids start over - so a live pid only counts if it is one of ours.
/// `program` is its executable's name, `None` when `/proc` won't say (another
/// user's process), which is taken as ours rather than called stale on a guess.
fn lock_verdict(pid: u32, alive: bool, program: Option<&str>) -> Check {
    let (id, title) = ("daemon", "Watcher");
    let stale = |why: String| {
        Check::new(
            id,
            title,
            Level::Info,
            format!("none running (a stale lock names pid {pid}, {why}; harmless)"),
        )
    };
    if !alive {
        return stale("which is gone".to_string());
    }
    match program {
        Some(name) if !name.contains("protondrive4linux") => {
            stale(format!("which is now another program, `{name}`"))
        }
        Some(name) => Check::new(id, title, Level::Ok, format!("running: pid {pid} ({name})")),
        None => Check::new(id, title, Level::Ok, format!("running: pid {pid}")),
    }
}

fn check_desktop(env: &Env) -> Check {
    let (id, title) = ("desktop", "Desktop entry");
    let entry = env
        .data_dirs
        .iter()
        .map(|d| d.join("applications").join(DESKTOP_FILE))
        .find(|p| p.is_file());
    let gui = find_executable(GUI_BINARY, &env.path_dirs);
    let at_login = if env.autostart_entry.is_file() {
        "on"
    } else {
        "off"
    };
    match (entry, gui) {
        (Some(entry), Some(_)) => Check::new(
            id,
            title,
            Level::Ok,
            format!("{} · launch at login: {at_login}", entry.display()),
        ),
        (Some(entry), None) => Check::new(
            id,
            title,
            Level::Warn,
            format!(
                "{} exists but `{GUI_BINARY}` is not on your PATH",
                entry.display()
            ),
        )
        .hint("Install the GUI build (the .deb, .rpm and AUR packages include it), or remove the stale launcher."),
        (None, Some(gui)) => Check::new(
            id,
            title,
            Level::Warn,
            format!(
                "{} is installed but has no launcher, so it won't show in the app menu",
                gui.display()
            ),
        )
        .hint(format!(
            "Copy packaging/{DESKTOP_FILE} from the source tree to ~/.local/share/applications/ \
             (the packages install it for you)."
        )),
        (None, None) => Check::new(
            id,
            title,
            Level::Info,
            "the GUI isn't installed (command-line setup)",
        ),
    }
}

/// `name` in the first of `dirs` that has it as an executable file.
fn find_executable(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| {
        fs::metadata(p)
            .map(|m| m.is_file() && m.mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// The first non-empty line of `s`, trimmed and capped, for one-line display.
fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// `path` as one shell word, quoted only when it needs it, so a hint can be pasted.
fn shell_quote(path: &Path) -> String {
    let s = path.display().to_string();
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        s
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A scratch directory removed on drop.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let dir = std::env::temp_dir().join(format!(
                "protondrive4linux-doctor-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }

        fn join(&self, p: &str) -> PathBuf {
            self.0.join(p)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mkdir(path: &Path, mode: u32) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        chmod(path, mode);
        path.to_path_buf()
    }

    fn write_file(path: &Path, contents: &str, mode: u32) -> PathBuf {
        fs::write(path, contents).unwrap();
        chmod(path, mode);
        path.to_path_buf()
    }

    /// Start `path` once and stop it again, retrying while the kernel says
    /// "text file busy" (ETXTBSY). That happens when another test thread forks
    /// while this file is still open for writing: the child briefly holds a copy
    /// of the descriptor, and exec-ing the file in that window fails. Once one
    /// start has succeeded nobody holds it for writing any more, so the later
    /// runs inside the code under test can't hit it.
    fn settle(path: &Path) {
        for _ in 0..400 {
            match Command::new(path)
                .arg("--settle")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Err(e) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Ok(mut child) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Err(_) => return,
            }
        }
    }

    /// An executable shell script standing in for `proton-drive` or `systemctl`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = write_file(&dir.join(name), &format!("#!/bin/sh\n{body}\n"), 0o755);
        settle(&path);
        path
    }

    /// Our own uid, read directly: `Env::from_process` reads `$PATH`, which the
    /// `protoncli` tests rewrite and ask every other test not to touch.
    fn my_uid() -> Option<u32> {
        fs::metadata("/proc/self").ok().map(|m| m.uid())
    }

    fn env_for(tmp: &Tmp) -> Env {
        Env {
            uid: my_uid(),
            exe: None,
            path_dirs: vec![],
            data_dirs: vec![tmp.join("share")],
            autostart_entry: tmp.join("config/autostart/protondrive4linux.desktop"),
            proxy_vars: vec![],
        }
    }

    struct FakeHost {
        systemctl: Result<String, String>,
        tcp: Result<Duration, String>,
    }

    impl FakeHost {
        fn healthy() -> FakeHost {
            FakeHost {
                systemctl: Ok(String::new()),
                tcp: Ok(Duration::from_millis(23)),
            }
        }
    }

    impl Host for FakeHost {
        fn systemctl_user(&self, _args: &[&str]) -> Result<String, String> {
            self.systemctl.clone()
        }
        fn tcp_connect(&self, _: &str, _: u16, _: Duration) -> Result<Duration, String> {
            self.tcp.clone()
        }
    }

    /// A config that points at `binary`, with its state under `tmp`.
    fn config_with(tmp: &Tmp, binary: &Path, extra: &str) -> Config {
        let toml = format!(
            "[cli]\nbinary = {:?}\n[options]\nstate_dir = {:?}\n{extra}",
            binary.display().to_string(),
            tmp.join("state").display().to_string(),
        );
        config::parse_str(&toml, Some(tmp.join("config/c.toml"))).unwrap()
    }

    const SIGNED_IN: &str =
        r#"case "$1" in version) echo "cli-drive 0.8.0";; filesystem) echo "[]";; esac"#;

    fn check<'a>(report: &'a Report, id: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|c| c.id == id)
            .expect("check exists")
    }

    // --- report --------------------------------------------------------------

    fn sample() -> Report {
        Report {
            version: "1.2.3 (abc)".into(),
            checks: vec![
                Check::new("a", "Short", Level::Ok, "fine"),
                Check::new("b", "A longer title", Level::Warn, "first\nsecond")
                    .hint("do this\nor that"),
                Check::skipped("c", "Third", "no"),
            ],
        }
    }

    #[test]
    fn the_worst_level_is_the_largest() {
        assert!(Level::Fail > Level::Warn && Level::Warn > Level::Info);
        assert!(Level::Info > Level::Ok && Level::Ok > Level::Skip);
    }

    #[test]
    fn the_report_aligns_titles_and_indents_details_and_hints_under_them() {
        let text = sample().render();
        assert!(text.starts_with("protondrive4linux doctor\n\n"));
        // Titles are padded to the longest one ("A longer title" = 14 chars).
        assert!(text.contains("  ✔ Short           fine\n"), "{text}");
        assert!(text.contains("  ⚠ A longer title  first\n"), "{text}");
        // Continuation lines sit under the detail column.
        let indent = " ".repeat(4 + 14 + 2);
        assert!(text.contains(&format!("{indent}second\n")), "{text}");
        assert!(text.contains(&format!("{indent}→ do this\n")), "{text}");
        assert!(text.contains(&format!("{indent}→ or that\n")), "{text}");
        assert!(text.contains("  – Third           no\n"), "{text}");
    }

    #[test]
    fn counts_and_the_verdict_follow_the_levels() {
        let report = sample();
        assert_eq!(
            report.counts(),
            Counts {
                ok: 1,
                warnings: 1,
                failed: 0,
                skipped: 1
            }
        );
        assert!(!report.has_failures(), "a warning doesn't fail the run");
        assert!(report.summary().contains("1 warning"));
        assert!(report.summary().ends_with("look at the ⚠ items."));

        let mut bad = sample();
        bad.checks
            .push(Check::new("d", "Bad", Level::Fail, "broken"));
        assert!(bad.has_failures());
        assert!(bad.summary().contains("1 failed"));
        assert!(bad.summary().ends_with("fix the ✘ items first."));

        let good = Report {
            version: String::new(),
            checks: vec![Check::new("a", "A", Level::Info, "x")],
        };
        assert!(good.summary().ends_with("all good."));
        assert!(good.summary().starts_with("1 ok, 0 warnings, 0 failed"));
    }

    #[test]
    fn json_carries_the_summary_and_every_check() {
        let v = sample().to_json();
        assert_eq!(v["version"], "1.2.3 (abc)");
        assert_eq!(v["ok"], true);
        assert_eq!(v["summary"]["warnings"], 1);
        assert_eq!(v["checks"].as_array().unwrap().len(), 3);
        assert_eq!(v["checks"][1]["level"], "warn");
        assert_eq!(v["checks"][1]["hint"], "do this\nor that");
        assert!(v["checks"][0]["hint"].is_null());
    }

    // --- config --------------------------------------------------------------

    #[test]
    fn a_named_config_that_does_not_exist_is_an_error_not_a_fallback() {
        let tmp = Tmp::new("c-missing");
        let wanted = tmp.join("nope.toml");
        match load_config(wanted.to_str()) {
            ConfigState::Invalid { path, error } => {
                assert_eq!(path, wanted);
                assert!(error.contains("-c"), "{error}");
            }
            _ => panic!("expected Invalid"),
        }
    }

    #[test]
    fn a_config_that_will_not_parse_is_reported_with_its_path_and_the_reason() {
        let tmp = Tmp::new("c-bad");
        let path = write_file(
            &tmp.join("c.toml"),
            "[options]\nconflict = \"nonsense\"\n",
            0o600,
        );
        match load_config(path.to_str()) {
            ConfigState::Invalid { path: p, error } => {
                assert_eq!(p, path);
                assert!(error.contains("keep-both|newer|skip"), "{error}");
            }
            _ => panic!("expected Invalid"),
        }
        let c = check_config(&load_config(path.to_str()));
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.starts_with(&path.display().to_string()));
        assert!(c.hint.unwrap().contains("doctor again"));
    }

    #[test]
    fn config_levels_cover_missing_empty_and_populated() {
        let missing = check_config(&ConfigState::Missing {
            default_path: "/x/y.toml".into(),
        });
        assert_eq!(missing.level, Level::Fail);
        assert!(missing.hint.unwrap().contains("init"));

        let tmp = Tmp::new("c-levels");
        let bin = tmp.join("bin");
        let none = ConfigState::Loaded(Box::new(config_with(&tmp, &bin, "")));
        let c = check_config(&none);
        assert_eq!(c.level, Level::Info);
        assert!(c.detail.ends_with("no folder pairs yet"), "{}", c.detail);

        let pair =
            |n: &str| format!("[[pair]]\nname = \"{n}\"\nlocal = \"/a/{n}\"\nremote = \"{n}\"\n");
        let two = ConfigState::Loaded(Box::new(config_with(
            &tmp,
            &bin,
            &format!("{}{}", pair("a"), pair("b")),
        )));
        let c = check_config(&two);
        assert_eq!(c.level, Level::Ok);
        assert!(c.detail.ends_with("2 folder pairs"), "{}", c.detail);
    }

    // --- proton cli ----------------------------------------------------------

    #[test]
    fn a_cli_that_is_not_installed_fails_with_the_install_hint() {
        let tmp = Tmp::new("cli-none");
        let cfg = config_with(&tmp, &tmp.join("does-not-exist"), "");
        let (c, cli) = check_cli(&cfg, true);
        assert_eq!(c.level, Level::Fail);
        assert!(cli.is_none());
        assert!(c.detail.contains("not found on PATH"));
        assert!(c.hint.unwrap().contains("proton-drive-cli"));
    }

    #[test]
    fn without_a_config_the_cli_is_looked_for_by_its_default_name_and_says_so() {
        let defaults = config::parse_str("", None).unwrap();
        let mut cfg = defaults.clone();
        cfg.binary = "protondrive4linux-no-such-binary".into();
        let (c, _) = check_cli(&cfg, false);
        assert!(
            c.detail.contains("there is no usable config"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn a_working_cli_shows_its_path_and_version() {
        let tmp = Tmp::new("cli-ok");
        let bin = script(&tmp.0, "proton-drive", SIGNED_IN);
        let (c, cli) = check_cli(&config_with(&tmp, &bin, ""), true);
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
        assert!(cli.is_some());
        assert!(c.detail.starts_with(&bin.display().to_string()));
        assert!(c.detail.ends_with("cli-drive 0.8.0"), "{}", c.detail);
    }

    #[test]
    fn a_cli_older_than_the_verified_version_is_a_warning_with_the_reason() {
        let tmp = Tmp::new("cli-old");
        let bin = script(&tmp.0, "proton-drive", r#"echo "cli-drive 0.7.2""#);
        let (c, cli) = check_cli(&config_with(&tmp, &bin, ""), true);
        assert_eq!(c.level, Level::Warn);
        assert!(cli.is_some(), "an old CLI is still worth probing");
        assert!(c.detail.contains("older than 0.8.0"), "{}", c.detail);
        assert!(c.hint.unwrap().contains("Update"));
    }

    #[test]
    fn a_version_that_cannot_be_read_is_shown_not_judged() {
        let tmp = Tmp::new("cli-odd");
        let bin = script(&tmp.0, "proton-drive", r#"echo "proton-drive (dev build)""#);
        let (c, _) = check_cli(&config_with(&tmp, &bin, ""), true);
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
        assert!(c.detail.ends_with("proton-drive (dev build)"));
    }

    #[test]
    fn a_cli_whose_version_command_fails_is_a_failure() {
        let tmp = Tmp::new("cli-broken");
        let bin = script(&tmp.0, "proton-drive", "exit 0");
        let (c, cli) = check_cli(&config_with(&tmp, &bin, ""), true);
        assert_eq!(c.level, Level::Fail);
        assert!(cli.is_none());
        assert!(c.detail.contains("printed no version"), "{}", c.detail);
    }

    #[test]
    fn a_cli_in_a_directory_others_can_write_to_is_flagged() {
        let tmp = Tmp::new("cli-loose");
        let dir = mkdir(&tmp.join("bin"), 0o777);
        let bin = script(&dir, "proton-drive", SIGNED_IN);
        let (c, _) = check_cli(&config_with(&tmp, &bin, ""), true);
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("writable directory"), "{}", c.detail);
    }

    // --- network -------------------------------------------------------------

    #[test]
    fn network_reports_the_connect_time_or_the_reason_it_failed() {
        let tmp = Tmp::new("net");
        let env = env_for(&tmp);
        let on = Options { offline: false };
        let ok = check_network(&on, &env, &FakeHost::healthy());
        assert_eq!(ok.level, Level::Ok);
        assert_eq!(ok.detail, "drive-api.proton.me:443 reachable (23 ms)");

        let down = FakeHost {
            tcp: Err("could not resolve drive-api.proton.me: no such host".into()),
            ..FakeHost::healthy()
        };
        let c = check_network(&on, &env, &down);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("could not resolve"));
        assert!(c.hint.unwrap().contains("443"));

        let off = check_network(&Options { offline: true }, &env, &down);
        assert_eq!(off.level, Level::Skip);
        assert!(off.detail.contains("--offline"));
    }

    #[test]
    fn behind_a_proxy_an_unreachable_host_is_a_warning_that_names_the_variable_only() {
        let tmp = Tmp::new("net-proxy");
        let mut env = env_for(&tmp);
        env.proxy_vars = vec!["HTTPS_PROXY".into()];
        let down = FakeHost {
            tcp: Err("timed out".into()),
            ..FakeHost::healthy()
        };
        let c = check_network(&Options::default(), &env, &down);
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("HTTPS_PROXY"), "{}", c.detail);
    }

    #[test]
    fn the_real_connect_reaches_a_listener_and_reports_a_closed_port() {
        let host = SystemHost::new(&env_for(&Tmp::new("tcp")));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(host
            .tcp_connect("127.0.0.1", port, Duration::from_secs(2))
            .is_ok());
        // Resolves a name too, not just a literal address.
        assert!(host
            .tcp_connect("localhost", port, Duration::from_secs(2))
            .is_ok());
        drop(listener);
        // These tests start a lot of scripts. A thread that forks at the wrong
        // moment leaves its child holding a copy of the listening socket until
        // it execs, so the port can stay open for a few milliseconds after the
        // listener is gone. Wait for it to close rather than assume it does.
        let mut refused = None;
        for _ in 0..100 {
            match host.tcp_connect("127.0.0.1", port, Duration::from_secs(2)) {
                Err(e) => {
                    refused = Some(e);
                    break;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let err = refused.expect("the port closes once nothing listens on it");
        assert!(err.contains("could not connect"), "{err}");
    }

    #[test]
    fn a_name_that_does_not_resolve_is_reported_as_such() {
        let host = SystemHost::new(&env_for(&Tmp::new("dns")));
        // .invalid is reserved (RFC 2606): it never resolves.
        let err = host
            .tcp_connect("protondrive4linux.invalid", 443, Duration::from_secs(5))
            .unwrap_err();
        assert!(err.contains("protondrive4linux.invalid"), "{err}");
    }

    // --- credentials ---------------------------------------------------------

    fn credentials_with(
        tmp: &Tmp,
        cli_body: &str,
        extra: &str,
        opts: Options,
        net: Level,
    ) -> Check {
        let bin = script(&tmp.0, "proton-drive", cli_body);
        let cfg = config_with(tmp, &bin, extra);
        let (_, cli) = check_cli(&cfg, true);
        let network = Check::new("network", "Network", net, "");
        check_credentials(Some(&cfg), cli.as_ref(), &opts, &network)
    }

    #[test]
    fn a_session_the_cli_accepts_is_signed_in() {
        let tmp = Tmp::new("cred-in");
        let c = credentials_with(&tmp, SIGNED_IN, "", Options::default(), Level::Ok);
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
        assert_eq!(c.detail, "signed in · store: the CLI's default");
    }

    #[test]
    fn the_plain_file_store_is_flagged_even_when_signed_in() {
        let tmp = Tmp::new("cred-unsafe");
        let bin = script(&tmp.0, "proton-drive", SIGNED_IN);
        let toml = format!(
            "[cli]\nbinary = {:?}\ncredentials_store = \"unsafe_file\"\n",
            bin.display().to_string()
        );
        let cfg = config::parse_str(&toml, None).unwrap();
        let (_, cli) = check_cli(&cfg, true);
        let net = Check::new("network", "Network", Level::Ok, "");
        let c = check_credentials(Some(&cfg), cli.as_ref(), &Options::default(), &net);
        assert_eq!(c.level, Level::Warn);
        assert!(
            c.detail.starts_with("signed in · store: unsafe_file"),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("plain file"), "{}", c.detail);
        assert!(c.hint.unwrap().contains("keychain"));
    }

    #[test]
    fn no_session_is_a_failure_that_says_how_to_sign_in() {
        let tmp = Tmp::new("cred-out");
        let body = r#"case "$1" in version) echo "cli-drive 0.8.0";; *) echo "You need to login first" >&2; exit 1;; esac"#;
        let c = credentials_with(&tmp, body, "", Options::default(), Level::Ok);
        assert_eq!(c.level, Level::Fail, "{}", c.detail);
        assert!(c.detail.starts_with("not signed in"));
        assert!(c.hint.unwrap().contains("protondrive4linux login"));
    }

    #[test]
    fn an_unreachable_keyring_is_told_apart_from_never_having_signed_in() {
        let tmp = Tmp::new("cred-keyring");
        let body = r#"case "$1" in version) echo "cli-drive 0.8.0";; *) echo "Failed to load session from secrets: Could not connect: No such file or directory (code: 1)" >&2; exit 1;; esac"#;
        let c = credentials_with(&tmp, body, "", Options::default(), Level::Ok);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("secret store"), "{}", c.detail);
        assert!(c.hint.unwrap().contains("credentials_store"));
    }

    #[test]
    fn an_error_that_is_not_about_signing_in_is_unverified_not_signed_out() {
        let tmp = Tmp::new("cred-net");
        let body = r#"case "$1" in version) echo "cli-drive 0.8.0";; *) echo "connection reset by peer" >&2; exit 1;; esac"#;
        let c = credentials_with(&tmp, body, "", Options::default(), Level::Ok);
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("connection reset"), "{}", c.detail);
    }

    #[test]
    fn credentials_are_not_probed_offline_without_a_network_or_without_what_they_need() {
        let tmp = Tmp::new("cred-skip");
        let offline = credentials_with(&tmp, SIGNED_IN, "", Options { offline: true }, Level::Skip);
        assert_eq!(offline.level, Level::Skip);
        assert!(offline.detail.contains("--offline"));
        let no_net = credentials_with(&tmp, SIGNED_IN, "", Options::default(), Level::Fail);
        assert_eq!(no_net.level, Level::Skip);
        assert!(no_net.detail.contains("can't be reached"));

        let net = Check::new("network", "Network", Level::Ok, "");
        let no_config = check_credentials(None, None, &Options::default(), &net);
        assert!(no_config.detail.contains("usable config"));
        let cfg = config::parse_str("", None).unwrap();
        let no_cli = check_credentials(Some(&cfg), None, &Options::default(), &net);
        assert!(no_cli.detail.contains("proton-drive CLI"));
    }

    // --- folders -------------------------------------------------------------

    #[test]
    fn folders_distinguish_present_missing_and_not_a_folder() {
        let tmp = Tmp::new("folders");
        let here = mkdir(&tmp.join("here"), 0o755);
        let file = write_file(&tmp.join("file"), "x", 0o644);
        let pair = |n: &str, p: &Path| {
            format!(
                "[[pair]]\nname = \"{n}\"\nlocal = {:?}\nremote = \"{n}\"\n",
                p.display().to_string()
            )
        };
        let bin = tmp.join("bin");

        let good = config_with(&tmp, &bin, &pair("a", &here));
        let c = check_folders(Some(&good));
        assert_eq!(
            (c.level, c.detail.as_str()),
            (Level::Ok, "1 local folder, all present")
        );

        let missing = config_with(&tmp, &bin, &pair("gone", &tmp.join("gone")));
        let c = check_folders(Some(&missing));
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("gone:") && c.detail.contains("does not exist"));
        assert!(c
            .hint
            .unwrap()
            .contains("never synced as \"everything deleted\""));

        let not_dir = config_with(&tmp, &bin, &pair("f", &file));
        let c = check_folders(Some(&not_dir));
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("is not a folder"));

        let empty = config_with(&tmp, &bin, "");
        assert_eq!(check_folders(Some(&empty)).level, Level::Info);
        assert_eq!(check_folders(None).level, Level::Skip);
    }

    // --- permissions ---------------------------------------------------------

    /// A config file and state folder under `tmp` with the given modes.
    fn state_with_modes(tmp: &Tmp, file: u32, conf_dir: u32, state: u32) -> ConfigState {
        let conf_dir_path = mkdir(&tmp.join("config"), conf_dir);
        let state_path = mkdir(&tmp.join("state"), state);
        let cfg_path = write_file(&conf_dir_path.join("c.toml"), "", file);
        let mut cfg = config_with(tmp, &tmp.join("bin"), "");
        cfg.source_path = Some(cfg_path);
        cfg.state_dir = state_path;
        ConfigState::Loaded(Box::new(cfg))
    }

    #[test]
    fn private_files_and_folders_pass_and_show_their_modes() {
        let tmp = Tmp::new("perm-ok");
        let state = state_with_modes(&tmp, 0o600, 0o700, 0o700);
        let c = check_permissions(&state, &env_for(&tmp));
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
        assert!(c.detail.contains("config file 0600"), "{}", c.detail);
        assert!(c.detail.contains("state folder 0700"), "{}", c.detail);
        assert!(c.hint.is_none());
    }

    #[test]
    fn a_state_folder_open_to_others_is_a_warning_with_the_chmod_to_fix_it() {
        let tmp = Tmp::new("perm-state");
        let state = state_with_modes(&tmp, 0o600, 0o700, 0o755);
        let c = check_permissions(&state, &env_for(&tmp));
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("state folder") && c.detail.contains("0755"));
        assert!(c.detail.contains("list your file names"));
        assert!(c.hint.unwrap().starts_with("chmod 700 "));
    }

    #[test]
    fn a_world_readable_config_is_a_warning_and_a_writable_one_a_failure() {
        let tmp = Tmp::new("perm-cfg");
        let readable = state_with_modes(&tmp, 0o644, 0o700, 0o700);
        let c = check_permissions(&readable, &env_for(&tmp));
        assert_eq!(c.level, Level::Warn);
        assert!(c.hint.unwrap().starts_with("chmod 600 "));

        let tmp = Tmp::new("perm-cfg-w");
        let writable = state_with_modes(&tmp, 0o666, 0o700, 0o700);
        let c = check_permissions(&writable, &env_for(&tmp));
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("can be changed by other users"));
        assert!(c.detail.contains("runs their program") || c.detail.contains("CLI to run"));
    }

    #[test]
    fn a_config_folder_others_can_write_to_is_a_failure_but_a_sticky_one_is_not() {
        let tmp = Tmp::new("perm-dir");
        let loose = state_with_modes(&tmp, 0o600, 0o775, 0o700);
        let c = check_permissions(&loose, &env_for(&tmp));
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("config folder"));
        assert!(c.hint.unwrap().starts_with("chmod go-w "));

        let tmp = Tmp::new("perm-sticky");
        let sticky = state_with_modes(&tmp, 0o600, 0o1777, 0o700);
        let c = check_permissions(&sticky, &env_for(&tmp));
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
    }

    #[test]
    fn something_owned_by_another_user_is_a_failure_that_suggests_chown() {
        let tmp = Tmp::new("perm-owner");
        let state = state_with_modes(&tmp, 0o600, 0o700, 0o700);
        let mut env = env_for(&tmp);
        // Pretend we're somebody else, so everything here looks foreign.
        env.uid = env.uid.map(|u| u + 1);
        let c = check_permissions(&state, &env);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("not to you"), "{}", c.detail);
        assert!(c.hint.unwrap().contains("sudo chown -R \"$USER\""));
    }

    #[test]
    fn root_owning_something_we_only_read_is_a_note_but_root_owning_the_state_folder_is_not() {
        let p = Path::new("/etc/protondrive4linux.toml");
        assert!(owner_finding("config file", p, 1000, 1000, false).is_none());

        // An administrator-provided config: no exposure, only the GUI can't save into it.
        let managed = owner_finding("config file", p, 0, 1000, false).unwrap();
        assert_eq!(managed.level, Level::Warn);
        assert!(managed.text.contains("belongs to root"), "{}", managed.text);
        assert!(managed.text.contains("fine if you set it up that way"));

        // What the app must write: root's ownership is the sudo accident.
        let state = owner_finding("state folder", p, 0, 1000, true).unwrap();
        assert_eq!(state.level, Level::Fail);
        assert!(
            state.text.contains("belongs to root, not to you"),
            "{}",
            state.text
        );
        assert!(state.fix.starts_with("sudo chown -R \"$USER\" "));

        // Another ordinary user can change it: always a failure.
        let other = owner_finding("config file", p, 1001, 1000, false).unwrap();
        assert_eq!(other.level, Level::Fail);
        assert!(other.text.contains("uid 1001"), "{}", other.text);
    }

    #[test]
    fn a_state_folder_that_does_not_exist_yet_is_not_a_problem() {
        let tmp = Tmp::new("perm-none");
        let state = state_with_modes(&tmp, 0o600, 0o700, 0o700);
        if let ConfigState::Loaded(c) = &state {
            fs::remove_dir_all(&c.state_dir).unwrap();
        }
        let c = check_permissions(&state, &env_for(&tmp));
        assert_eq!(c.level, Level::Ok);
        assert!(c.detail.contains("doesn't exist yet"), "{}", c.detail);
    }

    #[test]
    fn with_no_config_there_is_nothing_to_check_and_it_says_so() {
        let tmp = Tmp::new("perm-nocfg");
        let state = ConfigState::Missing {
            default_path: "/x".into(),
        };
        let c = check_permissions(&state, &env_for(&tmp));
        assert_eq!(c.level, Level::Skip, "no green tick for looking at nothing");
        assert!(c.detail.starts_with("nothing to check yet"), "{}", c.detail);
    }

    #[test]
    fn paths_in_hints_are_quoted_only_when_they_need_it() {
        assert_eq!(
            shell_quote(Path::new("/home/me/.config/x")),
            "/home/me/.config/x"
        );
        assert_eq!(
            shell_quote(Path::new("/home/me/My Files")),
            "'/home/me/My Files'"
        );
        assert_eq!(shell_quote(Path::new("/it's")), "'/it'\\''s'");
    }

    // --- systemd -------------------------------------------------------------

    fn unit(id: &str, load: &str, active: &str, sub: &str, enabled: &str) -> String {
        format!(
            "Id={id}\nLoadState={load}\nActiveState={active}\nSubState={sub}\nUnitFileState={enabled}\n"
        )
    }

    fn systemd(watch: &str, timer: &str) -> Check {
        let host = FakeHost {
            systemctl: Ok(format!("{watch}\n{timer}")),
            ..FakeHost::healthy()
        };
        check_systemd(&host)
    }

    #[test]
    fn systemctl_show_output_is_parsed_per_unit() {
        let out = format!(
            "{}\n{}",
            unit(WATCH_UNIT, "loaded", "active", "running", "enabled"),
            unit(TIMER_UNIT, "not-found", "inactive", "dead", "")
        );
        let units = parse_units(&out);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].id, WATCH_UNIT);
        assert_eq!(
            (units[0].active.as_str(), units[0].sub.as_str()),
            ("active", "running")
        );
        assert_eq!(units[0].enabled, "enabled");
        assert_eq!(units[1].load, "not-found");
    }

    #[test]
    fn without_the_units_installed_systemd_is_just_information() {
        let c = systemd(
            &unit(WATCH_UNIT, "not-found", "inactive", "dead", ""),
            &unit(TIMER_UNIT, "not-found", "inactive", "dead", ""),
        );
        assert_eq!(c.level, Level::Info);
        assert!(c.detail.contains("optional"));
    }

    #[test]
    fn a_running_enabled_service_is_fine() {
        let c = systemd(
            &unit(WATCH_UNIT, "loaded", "active", "running", "enabled"),
            &unit(TIMER_UNIT, "loaded", "inactive", "dead", "disabled"),
        );
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
        assert!(c
            .detail
            .contains(&format!("{WATCH_UNIT}: active (running) · enabled")));
        assert!(c
            .detail
            .contains(&format!("{TIMER_UNIT}: inactive (dead) · disabled")));
    }

    #[test]
    fn an_enabled_service_that_is_not_running_or_has_failed_is_a_warning_with_the_command() {
        let stopped = systemd(
            &unit(WATCH_UNIT, "loaded", "inactive", "dead", "enabled"),
            &unit(TIMER_UNIT, "not-found", "inactive", "dead", ""),
        );
        assert_eq!(stopped.level, Level::Warn);
        assert_eq!(
            stopped.hint.unwrap(),
            format!("systemctl --user start {WATCH_UNIT}")
        );

        let failed = systemd(
            &unit(WATCH_UNIT, "loaded", "failed", "failed", "enabled"),
            &unit(TIMER_UNIT, "not-found", "inactive", "dead", ""),
        );
        assert_eq!(failed.level, Level::Warn);
        assert!(failed
            .hint
            .unwrap()
            .contains(&format!("journalctl --user -u {WATCH_UNIT}")));
    }

    #[test]
    fn installed_but_not_enabled_is_not_a_problem() {
        let c = systemd(
            &unit(WATCH_UNIT, "loaded", "inactive", "dead", "disabled"),
            &unit(TIMER_UNIT, "loaded", "inactive", "dead", "disabled"),
        );
        assert_eq!(c.level, Level::Ok, "{}", c.detail);
    }

    #[test]
    fn the_service_and_the_timer_both_active_is_a_warning_naming_the_one_to_drop() {
        let c = systemd(
            &unit(WATCH_UNIT, "loaded", "active", "running", "enabled"),
            &unit(TIMER_UNIT, "loaded", "active", "waiting", "enabled"),
        );
        assert_eq!(c.level, Level::Warn);
        assert!(c.detail.contains("use one, not both"));
        assert!(c
            .hint
            .unwrap()
            .contains(&format!("disable --now {TIMER_UNIT}")));
    }

    #[test]
    fn when_systemd_cannot_be_asked_the_check_is_skipped_with_the_reason() {
        let host = FakeHost {
            systemctl: Err("Failed to connect to bus: No medium found".into()),
            ..FakeHost::healthy()
        };
        let c = check_systemd(&host);
        assert_eq!(c.level, Level::Skip);
        assert!(c.detail.contains("Failed to connect to bus"));
    }

    #[test]
    fn systemctl_is_run_with_user_and_its_failure_text_is_passed_on() {
        let tmp = Tmp::new("sysctl");
        let mut env = env_for(&tmp);
        env.path_dirs = vec![tmp.0.clone()];

        script(&tmp.0, "systemctl", r#"echo "args: $*""#);
        let out = SystemHost::new(&env)
            .systemctl_user(&["show", "x"])
            .unwrap();
        assert_eq!(out.trim(), "args: --user show x");

        script(
            &tmp.0,
            "systemctl",
            r#"echo "Failed to connect to bus" >&2; exit 1"#,
        );
        let err = SystemHost::new(&env).systemctl_user(&["show"]).unwrap_err();
        assert_eq!(err, "Failed to connect to bus");
    }

    #[test]
    fn a_systemctl_that_never_answers_is_killed_after_the_timeout() {
        let tmp = Tmp::new("sysctl-hang");
        let mut env = env_for(&tmp);
        env.path_dirs = vec![tmp.0.clone()];
        script(&tmp.0, "systemctl", "exec sleep 30");
        let mut host = SystemHost::new(&env);
        host.systemctl_timeout = Duration::from_millis(200);
        let started = Instant::now();
        let err = host.systemctl_user(&["show"]).unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn no_systemctl_on_the_path_means_no_systemd() {
        let tmp = Tmp::new("sysctl-none");
        let host = SystemHost::new(&env_for(&tmp));
        assert!(host
            .systemctl_user(&["show"])
            .unwrap_err()
            .contains("systemctl not found"));
    }

    // --- watcher / desktop entry ---------------------------------------------

    fn loaded_state(tmp: &Tmp) -> Config {
        let cfg = config_with(tmp, &tmp.join("bin"), "");
        mkdir(&cfg.state_dir, 0o700);
        cfg
    }

    #[test]
    fn a_lock_naming_a_pid_that_is_not_ours_or_is_gone_is_stale_and_no_lock_is_idle() {
        let tmp = Tmp::new("daemon");
        let cfg = loaded_state(&tmp);
        let lock = cfg.state_dir.join(crate::watcher::LOCK_FILE);

        let c = check_daemon(Some(&cfg));
        assert_eq!(c.level, Level::Info);
        assert!(c.detail.starts_with("none running"));

        // A live pid that isn't one of ours - this test binary - is a reused one.
        fs::write(&lock, std::process::id().to_string()).unwrap();
        let c = check_daemon(Some(&cfg));
        assert_eq!(c.level, Level::Info);
        assert!(
            c.detail.contains("which is now another program"),
            "{}",
            c.detail
        );

        // Beyond pid_max (4194304), so no such process.
        fs::write(&lock, "4999999").unwrap();
        let c = check_daemon(Some(&cfg));
        assert_eq!(c.level, Level::Info);
        assert!(c.detail.contains("stale lock"), "{}", c.detail);

        fs::write(&lock, "not a pid").unwrap();
        assert!(check_daemon(Some(&cfg)).detail.contains("unreadable"));
        assert_eq!(check_daemon(None).level, Level::Skip);
    }

    #[test]
    fn a_live_pid_counts_only_if_it_is_one_of_ours() {
        let running = lock_verdict(42, true, Some("protondrive4linux-gui"));
        assert_eq!(running.level, Level::Ok);
        assert_eq!(running.detail, "running: pid 42 (protondrive4linux-gui)");

        // Pids start over after a reboot, so a leftover lock can name anything.
        let reused = lock_verdict(42, true, Some("firefox"));
        assert_eq!(reused.level, Level::Info);
        assert!(
            reused.detail.contains("now another program, `firefox`"),
            "{}",
            reused.detail
        );

        // Another user's process: /proc won't name it, so don't call it stale on a guess.
        let unknown = lock_verdict(42, true, None);
        assert_eq!(
            (unknown.level, unknown.detail.as_str()),
            (Level::Ok, "running: pid 42")
        );

        let gone = lock_verdict(42, false, None);
        assert_eq!(gone.level, Level::Info);
        assert!(gone.detail.contains("which is gone"), "{}", gone.detail);
    }

    #[test]
    fn the_launcher_and_the_gui_binary_are_expected_together() {
        let tmp = Tmp::new("desktop");
        let apps = mkdir(&tmp.join("share/applications"), 0o755);
        let bins = mkdir(&tmp.join("bin"), 0o755);
        let mut env = env_for(&tmp);
        env.path_dirs = vec![bins.clone()];

        let none = check_desktop(&env);
        assert_eq!(none.level, Level::Info);
        assert!(none.detail.contains("isn't installed"));

        script(&bins, GUI_BINARY, "exit 0");
        let no_launcher = check_desktop(&env);
        assert_eq!(no_launcher.level, Level::Warn);
        assert!(
            no_launcher.detail.contains("no launcher"),
            "{}",
            no_launcher.detail
        );
        assert!(no_launcher.hint.unwrap().contains(DESKTOP_FILE));

        write_file(&apps.join(DESKTOP_FILE), "[Desktop Entry]\n", 0o644);
        let both = check_desktop(&env);
        assert_eq!(both.level, Level::Ok);
        assert!(
            both.detail.ends_with("launch at login: off"),
            "{}",
            both.detail
        );

        fs::create_dir_all(env.autostart_entry.parent().unwrap()).unwrap();
        write_file(&env.autostart_entry, "[Desktop Entry]\n", 0o644);
        assert!(check_desktop(&env).detail.ends_with("launch at login: on"));

        fs::remove_file(bins.join(GUI_BINARY)).unwrap();
        let stale = check_desktop(&env);
        assert_eq!(stale.level, Level::Warn);
        assert!(
            stale.detail.contains("is not on your PATH"),
            "{}",
            stale.detail
        );
    }

    #[test]
    fn launchers_are_found_in_any_data_directory() {
        let tmp = Tmp::new("desktop-dirs");
        let second = mkdir(&tmp.join("usr-share/applications"), 0o755);
        write_file(&second.join(DESKTOP_FILE), "[Desktop Entry]\n", 0o644);
        let bins = mkdir(&tmp.join("bin"), 0o755);
        script(&bins, GUI_BINARY, "exit 0");
        let mut env = env_for(&tmp);
        env.data_dirs = vec![tmp.join("nothing-here"), tmp.join("usr-share")];
        env.path_dirs = vec![bins];
        assert_eq!(check_desktop(&env).level, Level::Ok);
    }

    // --- the whole run ---------------------------------------------------------

    fn full_run(tmp: &Tmp, cli_body: &str, opts: Options) -> Report {
        let bin = script(&tmp.0, "proton-drive", cli_body);
        let home = mkdir(&tmp.join("home-docs"), 0o755);
        let pair = format!(
            "[[pair]]\nname = \"docs\"\nlocal = {:?}\nremote = \"docs\"\n",
            home.display().to_string()
        );
        let mut cfg = config_with(tmp, &bin, &pair);
        let conf_dir = mkdir(&tmp.join("config"), 0o700);
        cfg.source_path = Some(write_file(&conf_dir.join("c.toml"), "", 0o600));
        mkdir(&cfg.state_dir, 0o700);
        run(
            "9.9.9 (test)",
            &ConfigState::Loaded(Box::new(cfg)),
            &opts,
            &env_for(tmp),
            &FakeHost::healthy(),
        )
    }

    #[test]
    fn a_healthy_install_has_every_check_in_order_and_no_failures() {
        let tmp = Tmp::new("run-ok");
        let report = full_run(&tmp, SIGNED_IN, Options::default());
        let ids: Vec<_> = report.checks.iter().map(|c| c.id).collect();
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
        assert!(!report.has_failures(), "{}", report.render());
        assert_eq!(check(&report, "credentials").level, Level::Ok);
        assert!(check(&report, "version")
            .detail
            .starts_with("protondrive4linux 9.9.9 (test)"));
    }

    #[test]
    fn offline_skips_exactly_the_checks_that_need_the_network() {
        let tmp = Tmp::new("run-offline");
        let report = full_run(&tmp, SIGNED_IN, Options { offline: true });
        let skipped: Vec<_> = report
            .checks
            .iter()
            .filter(|c| c.level == Level::Skip && c.detail.contains("--offline"))
            .map(|c| c.id)
            .collect();
        assert_eq!(skipped, ["network", "credentials"]);
        assert!(!report.has_failures(), "{}", report.render());
    }

    #[test]
    fn a_signed_out_session_fails_the_run() {
        let tmp = Tmp::new("run-out");
        let body = r#"case "$1" in version) echo "cli-drive 0.8.0";; *) echo "Please log in" >&2; exit 1;; esac"#;
        let report = full_run(&tmp, body, Options::default());
        assert_eq!(check(&report, "credentials").level, Level::Fail);
        assert!(report.has_failures());
        assert_eq!(report.to_json()["ok"], false);
    }

    #[test]
    fn without_a_config_the_independent_checks_still_run() {
        let tmp = Tmp::new("run-noconfig");
        let report = run(
            "9.9.9 (test)",
            &ConfigState::Missing {
                default_path: tmp.join("c.toml"),
            },
            &Options::default(),
            &env_for(&tmp),
            &FakeHost::healthy(),
        );
        assert_eq!(check(&report, "config").level, Level::Fail);
        assert_eq!(
            check(&report, "network").level,
            Level::Ok,
            "no config needed"
        );
        for dependent in ["credentials", "folders", "daemon"] {
            assert_eq!(check(&report, dependent).level, Level::Skip, "{dependent}");
        }
        assert!(report.has_failures());
    }
}
