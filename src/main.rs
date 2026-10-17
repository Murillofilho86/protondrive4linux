//! protondrive4linux CLI: a thin front-end over the neutronsync core library.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use neutronsync::config::{self, Config};
use neutronsync::doctor;
use neutronsync::engine::Engine;
use neutronsync::events::{EventSink, SyncEvent};
use neutronsync::logger::Logger;
use neutronsync::protoncli::ProtonCli;
use neutronsync::stats::Stats;
use neutronsync::EXAMPLE_CONFIG;

/// Records each finished op into `stats.db` so a CLI sync shows up in the same
/// activity feed the GUI reads (the GUI restores the feed from this table).
/// Best-effort: a DB error is ignored and never interrupts the sync. `Stats`
/// wraps a `Mutex<Connection>`, so this is `Sync` and safe to share across the
/// concurrent download workers.
struct DbSink {
    stats: Stats,
}

impl EventSink for DbSink {
    fn emit(&self, ev: &SyncEvent) {
        let now = neutronsync::datefmt::now_epoch();
        match ev {
            SyncEvent::OpFinished {
                pair,
                action,
                path,
                ok,
                error,
            } => {
                let _ = self
                    .stats
                    .record_op(pair, action, path, *ok, error.as_deref(), now);
            }
            // So a conflict settled by a CLI run still reaches the GUI's inbox.
            SyncEvent::Conflict {
                pair,
                path,
                resolution,
                copy,
            } => {
                let _ = self
                    .stats
                    .record_conflict(pair, path, *resolution, copy.as_deref(), now);
            }
            _ => {}
        }
    }
}

/// `<version> (<commit-sha>)`, e.g. `0.4.2 (a1b2c3d4e5f6)`; `build.rs` sets
/// `PROTONDRIVE_BUILD_SHA` to "unknown" when `.git` isn't available (e.g.
/// building from the `git archive` source tarball instead of a checkout).
const VERSION_WITH_SHA: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("PROTONDRIVE_BUILD_SHA"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "protondrive4linux",
    version = VERSION_WITH_SHA,
    about = "Bidirectional Proton Drive folder sync built on the official proton-drive CLI"
)]
struct Cli {
    /// Path to the config file.
    #[arg(short, long, global = true)]
    config: Option<String>,
    /// Debug output.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Warnings and errors only.
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config file.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Browser login (proton-drive auth login).
    Login,
    /// proton-drive auth logout.
    Logout,
    /// Show config, resolved pairs and CLI availability.
    Status,
    /// Check the installation: CLI, sign-in, network, file permissions,
    /// systemd, desktop entry. Changes nothing.
    Doctor {
        /// Also list this remote path and show how its JSON is parsed.
        path: Option<String>,
        /// Also list the remote root and show how its JSON is parsed
        /// (debugging the CLI adapter).
        #[arg(long, conflicts_with = "json")]
        probe: bool,
        /// Skip the checks that use the network (reachability and sign-in). A
        /// listing you ask for with --probe or PATH still runs.
        #[arg(long)]
        offline: bool,
        /// Print the report as JSON.
        #[arg(long, conflicts_with_all = ["probe", "path"])]
        json: bool,
    },
    /// Run the bidirectional sync.
    Sync {
        /// Pair name(s) to sync (default: all).
        pair: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        /// Rebuild the record of what was in sync from scratch: both sides are
        /// merged and nothing is deleted - the way to get back files a held-back
        /// mass deletion would have removed.
        #[arg(long)]
        resync: bool,
        /// Carry out a mass deletion the guard held back (more than
        /// `max_delete_files` and `max_delete_percent` of what a pair tracks).
        /// For one run: say it is intended.
        #[arg(long)]
        allow_mass_delete: bool,
        #[arg(long)]
        json: bool,
    },
    /// Watch local folders and sync on change (plus a periodic rescan).
    Watch {
        /// Pair name(s) to watch (default: all).
        pair: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    match &cli.command {
        Cmd::Init { force } => cmd_init(cli, *force),
        Cmd::Login => cmd_login(cli),
        Cmd::Logout => cmd_logout(cli),
        Cmd::Status => cmd_status(cli),
        Cmd::Doctor {
            path,
            probe,
            offline,
            json,
        } => cmd_doctor(cli, path.as_deref(), *probe, *offline, *json),
        Cmd::Sync {
            pair,
            dry_run,
            resync,
            allow_mass_delete,
            json,
        } => cmd_sync(cli, pair, *dry_run, *resync, *allow_mass_delete, *json),
        Cmd::Watch { pair } => cmd_watch(cli, pair),
    }
}

fn load_cfg(cli: &Cli) -> anyhow::Result<Config> {
    config::load(cli.config.as_deref())
}

fn cmd_init(cli: &Cli, force: bool) -> anyhow::Result<ExitCode> {
    let dest: PathBuf = match &cli.config {
        Some(c) => config::expand(c),
        None => config::default_config_path(),
    };
    if dest.exists() && !force {
        println!(
            "Config already exists: {} (use --force to overwrite)",
            dest.display()
        );
        return Ok(ExitCode::from(1));
    }
    // Private to the user (folder 0700, file 0600) like every other way the
    // config gets written; `doctor` checks for exactly that.
    config::write_private(&dest, EXAMPLE_CONFIG)?;
    {
        // The mode above only applies to a file this call creates; `--force` over
        // an older, looser one should tighten it too.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("Wrote starter config: {}", dest.display());
    println!("Edit it, then run:  protondrive4linux login  &&  protondrive4linux sync --dry-run");
    Ok(ExitCode::SUCCESS)
}

fn cmd_login(cli: &Cli) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    let proton = ProtonCli::new(&cfg);
    proton.login()?;
    println!("Logged in.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_logout(cli: &Cli) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    ProtonCli::new(&cfg).logout()?;
    println!("Logged out.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_status(cli: &Cli) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    let proton = ProtonCli::new(&cfg);
    let resolved = proton
        .resolve_binary()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "NOT FOUND on PATH".to_string());
    println!("protondrive4linux {VERSION_WITH_SHA}");
    if let Some(p) = &cfg.source_path {
        println!("config          {}", p.display());
    }
    println!("proton-drive    {resolved} (configured: {})", cfg.binary);
    if let Some(w) = proton.binary_location_warning() {
        println!("  ⚠ {w}");
    }
    println!("remote root     {}", cfg.remote_root);
    println!(
        "propagate del.  {} (local -> {:?})",
        cfg.propagate_deletes, cfg.local_delete
    );
    println!("conflict        {:?}", cfg.conflict);
    println!("compare         {:?}", cfg.compare);
    println!("state dir       {}", cfg.state_dir.display());
    println!("pairs:");
    for p in &cfg.pairs {
        let exists = if p.local.exists() { "ok" } else { "missing" };
        println!(
            "  - {:<16} {}  [{exists}]  <->  {}",
            p.name,
            p.local.display(),
            p.remote
        );
    }
    // Only if a stats DB already exists: `status` must not create one.
    if cfg.state_dir.join("stats.db").exists() {
        if let Ok(n) = Stats::open(&cfg.state_dir).and_then(|s| s.conflict_count()) {
            if n > 0 {
                println!(
                    "conflicts       {n} unresolved (listed on the GUI's Conflicts page; \
                     see the sync log for how each was settled)"
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_doctor(
    cli: &Cli,
    path: Option<&str>,
    probe: bool,
    offline: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let state = doctor::load_config(cli.config.as_deref());
    let env = doctor::Env::from_process();
    let host = doctor::SystemHost::new(&env);
    let report = doctor::run(
        VERSION_WITH_SHA,
        &state,
        &doctor::Options { offline },
        &env,
        &host,
    );
    let mut code = u8::from(report.has_failures());
    if json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
        return Ok(ExitCode::from(code));
    }
    print!("{}", report.render());
    if probe || path.is_some() {
        match &state {
            doctor::ConfigState::Loaded(cfg) => {
                let target = path.unwrap_or(&cfg.remote_root);
                if adapter_probe(cfg, target)? != ExitCode::SUCCESS {
                    code = 1;
                }
            }
            _ => println!("\nThe CLI probe needs a usable config; see the Config line above."),
        }
    }
    Ok(ExitCode::from(code))
}

/// List `target` through the CLI and show both the raw JSON and what the
/// adapter makes of it - for when a CLI update changes the output shape.
fn adapter_probe(cfg: &Config, target: &str) -> anyhow::Result<ExitCode> {
    let proton = ProtonCli::new(cfg);
    println!("\nProbing `filesystem list -j {target}` ...");
    let raw = proton.raw_list(target)?;
    println!("--- raw JSON (first 1500 chars) ---");
    let shown: String = raw.chars().take(1500).collect();
    println!("{}", if shown.is_empty() { "(empty)" } else { &shown });
    println!("--- parsed by protondrive4linux ---");
    match ProtonCli::parse_list(&raw, target) {
        Ok(entries) => {
            if entries.is_empty() {
                println!(
                    "  (no entries parsed - if the raw JSON is non-empty, adjust protoncli.rs)"
                );
            }
            for e in entries.iter().take(50) {
                let kind = if e.is_dir { "dir " } else { "file" };
                println!(
                    "  {kind} size={:<12} mtime={:?} sha1={:?}  {}",
                    e.size, e.mtime, e.sha1, e.path
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!("  PARSE FAILED: {e}");
            Ok(ExitCode::from(1))
        }
    }
}

fn cmd_sync(
    cli: &Cli,
    pairs: &[String],
    dry_run: bool,
    resync: bool,
    allow_mass_delete: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    let mut cfg = load_cfg(cli)?;
    cfg.allow_mass_delete = allow_mass_delete;
    let log = Logger::new(&cfg.log_dir(), cli.verbose, cli.quiet);

    // Select pairs.
    let selected: Vec<&neutronsync::config::Pair> = if pairs.is_empty() {
        cfg.pairs.iter().collect()
    } else {
        let mut out = Vec::new();
        for name in pairs {
            match cfg.pairs.iter().find(|p| &p.name == name) {
                Some(p) => out.push(p),
                None => {
                    log.error(&format!("unknown pair: {name}"));
                    return Ok(ExitCode::from(2));
                }
            }
        }
        out
    };

    let proton = ProtonCli::new(&cfg);
    if proton.resolve_binary().is_none() {
        log.error(&format!(
            "proton-drive not found (configured: {:?}). Install it or set cli.binary.",
            cfg.binary
        ));
        return Ok(ExitCode::from(1));
    }
    if dry_run {
        log.info("DRY RUN - no changes will be made.\n");
    }

    // Record ops into stats.db so this CLI sync appears in the GUI activity
    // feed too (not just sync.log). Skipped on a dry run (nothing happened) and
    // declared before the engine so its borrow outlives the observer.
    let db_sink = if dry_run {
        None
    } else {
        Stats::open(&cfg.state_dir)
            .ok()
            .map(|stats| DbSink { stats })
    };
    let mut engine = Engine::new(&cfg, proton, &log, dry_run);
    if let Some(sink) = &db_sink {
        engine.set_observer(Some(sink as &dyn EventSink), None);
    }
    let mut total_errors = 0usize;
    let mut summaries: Vec<(String, usize, usize, String)> = Vec::new();
    for pair in &selected {
        match engine.sync_pair(pair, resync) {
            Ok(res) => {
                total_errors += res.errors.len();
                log.info(&format!(
                    "pair {:?}: {} applied, {}{}\n",
                    res.pair,
                    neutronsync::datefmt::counted(res.applied, "change", "changes"),
                    neutronsync::datefmt::counted(res.errors.len(), "error", "errors"),
                    if dry_run { " (dry run)" } else { "" }
                ));
                summaries.push((res.pair, res.applied, res.errors.len(), res.plan_summary));
            }
            Err(e) => {
                total_errors += 1;
                log.error(&format!("pair {:?} failed: {e}", pair.name));
            }
        }
    }

    // Keep the activity-feed table bounded, same cap the GUI uses.
    if let Some(sink) = &db_sink {
        let _ = sink.stats.prune_ops(1000);
    }

    if json {
        let arr: Vec<serde_json::Value> = summaries
            .iter()
            .map(|(name, applied, errors, plan)| {
                serde_json::json!({"name": name, "applied": applied, "errors": errors, "plan": plan})
            })
            .collect();
        let out = serde_json::json!({"dry_run": dry_run, "pairs": arr});
        println!("{}", serde_json::to_string_pretty(&out)?);
    }

    Ok(if total_errors > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_watch(cli: &Cli, pairs: &[String]) -> anyhow::Result<ExitCode> {
    let cfg = load_cfg(cli)?;
    let log = Logger::new(&cfg.log_dir(), cli.verbose, cli.quiet);

    let selected: Vec<neutronsync::config::Pair> = if pairs.is_empty() {
        cfg.pairs.clone()
    } else {
        let mut out = Vec::new();
        for name in pairs {
            match cfg.pairs.iter().find(|p| &p.name == name) {
                Some(p) => out.push(p.clone()),
                None => {
                    log.error(&format!("unknown pair: {name}"));
                    return Ok(ExitCode::from(2));
                }
            }
        }
        out
    };

    if ProtonCli::new(&cfg).resolve_binary().is_none() {
        log.error(&format!(
            "proton-drive not found (configured: {:?}). Install it or set cli.binary.",
            cfg.binary
        ));
        return Ok(ExitCode::from(1));
    }
    // CLI runs until Ctrl-C; the stop flag is only used by the GUI toggle.
    let stop = std::sync::atomic::AtomicBool::new(false);
    // Record into the shared stats DB like `sync` does, so a headless watcher
    // (the systemd unit) still feeds the GUI's Activity feed and Conflicts inbox.
    let db_sink = Stats::open(&cfg.state_dir)
        .ok()
        .map(|stats| DbSink { stats });
    neutronsync::watcher::watch_with(
        &cfg,
        selected,
        &log,
        &stop,
        db_sink.as_ref().map(|s| s as &dyn EventSink),
    )?;
    Ok(ExitCode::SUCCESS)
}
