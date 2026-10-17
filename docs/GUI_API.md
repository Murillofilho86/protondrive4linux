# GUI backend API (`neutronsync::service`)

The backend for a rich GUI. The frontend stays thin: hold one `Controller`, read a cheap `snapshot()` each frame, and issue non-blocking commands. All slow work runs on background threads; engine events become observable state.

## Lifecycle

```rust
use neutronsync::service::{Controller, Phase, ActivityKind};

// once, in App::new:
let cfg = neutronsync::config::load(None).unwrap_or_else(|_| /* starter */);
let controller = Controller::new(cfg);   // kicks off an account check

// each frame (egui):
let s = controller.snapshot();           // cheap clone of AppState
// ...render s...
if s.busy || s.watching {
    ctx.request_repaint_after(std::time::Duration::from_millis(150));
}
```

## Reading state — `AppState`

```rust
struct AppState {
    pairs: Vec<PairState>,
    account: AccountState,
    activity: VecDeque<ActivityItem>,   // newest at the back, capped at 500
    busy: bool,                          // a sync is running
    watching: bool,                      // watch daemon active
}

struct PairState {
    name, local, remote: String,
    phase: Phase,                        // Idle | Scanning | Syncing | Synced | Error
    progress: Progress,                  // .done / .total, .fraction() -> f32 for a bar
    scanned_folders: usize,              // live count during a remote scan
    current_op: Option<String>,          // e.g. "upload sub/a.txt" — good for a status line
    last_error: Option<String>,
    last_synced: Option<i64>,            // epoch seconds
    tracked: usize,                      // files synced, counted from the baseline (files only, not folders)
    retrying: usize,                     // failed files waiting out a retry backoff (M4-004)
}

struct AccountState { checked, binary_found, signed_in: bool, version: String }
struct ActivityItem { ts: i64, kind: ActivityKind /*Info|Sync|Error*/, text: String }
```

Rendering hints:
- Per-pair row: name, `phase`, a progress bar from `progress.fraction()`, and `current_op` as a subtitle. Colour by `phase` (Synced=green, Error=red, Scanning/Syncing=accent).
- Activity tab: iterate `activity` (it's already capped), colour by `kind`.
- Account tab: dot from `signed_in`, show `version`.

## One-glance status

For places with room for a single state (the tray icon), `AppState::overall()` collapses everything into one `OverallStatus` — the most urgent thing across every pair — and `AppState::status_line(now)` words it as a short sentence:

| `OverallStatus` | When | Example line |
| --- | --- | --- |
| `SignedOut` | the Proton session expired | `Signed out of Proton — sign in to resume` |
| `Error` | a pair's last run had errors, or files are waiting to retry | `docs: upload a.txt: <reason>` / `docs: 2 files failed, retrying automatically` |
| `Syncing` | a scan or transfer is in flight (or the first pass isn't done) | `Syncing docs (3/10)` |
| `Synced` | live sync is on and everything is up to date | `Up to date · synced 2 minutes ago` |
| `Paused` | live sync is off | `Auto sync is off` / `No folders to sync` |

`controller.status_summary()` returns both under the state lock without cloning the activity feed, so it is cheap enough to poll several times a second. `PairState::retrying` counts files that failed and are still inside their backoff window: a run that merely *skips* them finishes with zero errors, so without it a pair would read as healthy while files are failing.

## Conflict inbox

Every conflict settlement is reported as `SyncEvent::Conflict { pair, path, resolution, copy }` (`ConflictResolution::{KeptBoth, KeptLocal, KeptRemote, Skipped}`) once it has actually completed, and the controller persists it to the shared stats DB, so a window sees conflicts the tray daemon (or a CLI run) recorded:

```rust
controller.conflict_count();                 // unresolved, for a badge
controller.conflicts(false, 200);            // open ones, newest first (true = include resolved history)
controller.dismiss_conflict(id);             // acknowledge one (moves to the history)
controller.dismiss_all_conflicts();
```

A `Skipped` conflict is re-reported on every run until the user settles it; the store keeps one open entry and clears it automatically once the file syncs.

## Commands (all non-blocking)

```rust
controller.sync(vec![], false);              // sync all pairs
controller.sync(vec!["docs".into()], true);  // dry-run just "docs"
controller.cancel();                         // stop the current sync (before next op)
controller.start_watch(vec![]);              // live-sync all pairs
controller.stop_watch();
controller.refresh_account();                // re-check login/version
controller.is_busy();  controller.is_watching();
```

## Editing config

```rust
let mut cfg = controller.config();           // a working copy
cfg.pairs.push(pair);                         // edit it (add/remove pairs, options)
controller.commit_config(cfg);               // store + refresh the pair list
controller.save(&path)?;                      // persist to disk
```

`commit_config` preserves the phase/progress of pairs whose names are unchanged, so the UI doesn't flicker when you tweak options.

## Notes

- `Controller` methods take `&self` (interior mutability), so you can keep it in your `App` by value; no `Arc`/`Mutex` needed on your side.
- Progress `total` is the number of actionable operations for the pair; `done` ticks up per finished op. During a scan, `phase == Scanning` and `scanned_folders` climbs (there's no total for the scan — the CLI can't tell us the tree size up front).
- The watch daemon feeds the same state, so a running watch shows pairs cycling Scanning → Synced with activity entries, and `watching == true`.
- In tray mode the daemon and the window are **separate processes**. The process running the work calls `publish_status()` to write a live snapshot to `status.json`; a window reads it with `Controller::read_status(state_dir)` and renders that instead of its own idle state. This is why an open window reflects what the background daemon is doing. A window in tray mode also checks, every couple of seconds, that a live process holds the tray lock and that `status.json` is less than ten seconds old (`tray_policy::window_liveness`): a dead supervisor is started again, a tray that stopped updating gets a banner on the Activity page. `--tray` itself is only a supervisor: the tray runs as its child (`--tray-child`) and is restarted after a crash. A CLI `protondrive4linux sync` also records its ops into the shared activity store, so those runs appear in the feed too.
