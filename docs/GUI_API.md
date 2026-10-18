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
    signed_out: bool,                    // no Proton session: signing in resumes sync
    keyring_unreachable: bool,           // the CLI can't reach the keyring holding the session:
                                         // a wait, not a sign-out (never both flags at once)
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
    held: Option<HeldDeletes>,           // deletions the mass-deletion guard is holding back (below)
}

struct HeldDeletes { remote, local, tracked: usize, at: i64 }   // .files(), .percent(), .deletions(), .where_(), .removed_from()

struct AccountState { checked, binary_found, signed_in, keyring_unreachable: bool, version: String }
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
| `WaitingForKeyring` | the CLI can't reach the keyring its session is kept in (locked, no D-Bus session, headless) | `Waiting for your keyring — unlock it to resume` |
| `Error` | a pair's last run had errors, files are waiting to retry, or deletions are held back | `docs: upload a.txt: <reason>` / `docs: 2 files failed, retrying automatically` / `docs: 300 deletions held back — open the app to review` |
| `Syncing` | a scan or transfer is in flight (or the first pass isn't done) | `Syncing docs (3/10)` |
| `Synced` | live sync is on and everything is up to date | `Up to date · synced 2 minutes ago` |
| `Paused` | live sync is off | `Auto sync is off` / `No folders to sync` |

`WaitingForKeyring` is not a sign-out and must not be rendered as one: signing in again does nothing for a keyring that is out of reach, the session is usually still in it, and it ends by itself once the keyring answers. Show a waiting page (no sign-in button), and look again now and then: `controller.refresh_account(true)` re-checks the account (`AccountState::keyring_unreachable`) and, when the session is back, wakes the watcher; the window does this every 30 seconds while it waits. In the event stream it is `SyncEvent::KeyringUnreachable`, and a `SyncEvent::Auth` (either value) is what ends it.

`controller.status_summary()` returns both under the state lock without cloning the activity feed, so it is cheap enough to poll several times a second. `PairState::retrying` counts files that failed and are still inside their backoff window: a run that merely *skips* them finishes with zero errors, so without it a pair would read as healthy while files are failing.

## Deletions held back — the mass-deletion guard

With `propagate_deletes` on, one run may delete at most the larger of `max_delete_files` (100) and `max_delete_percent` (20 %) of the files a pair tracks (see `docs/SYNC_MODEL.md`). A run that would delete more deletes **nothing**, still transfers, and reports `SyncEvent::DeletesHeld { pair, remote, local, tracked }` - how many files it would have removed from Proton Drive and from this computer, of how many the pair tracks. The pair goes to `Phase::Error` and `PairState::held` says what is held. It is a decision for the user, not a failure: render it as a wait (an amber banner or card line, not red), say it was probably a mistake and that **nothing was deleted**, and offer the two ways out:

```rust
controller.resync(vec!["docs".into()]);                  // "Restore them": merge both sides afresh, nothing deleted - the files come back
controller.sync_confirming_deletes(vec!["docs".into()]); // "Delete them": it was on purpose; for this one run only
```

Both are the same non-blocking background run as `sync`, for the folders **named** - an empty list does nothing, so neither can be aimed at every folder by accident. Confirm before the second (the deleted files go to Proton's trash and to the desktop trash, unless `local_delete = "remove"`); the guard is back for the next run. `AppState::held_pairs()` lists what is held, and `controller.held_summary()` gives that plus whether those are the *only* problems (`AppState::only_held_problems()`) for a notification that should not also say "sync problem".

`held` stays until the engine says `SyncEvent::DeletesReleased { pair }`: a run over the **whole** folder - a sync or a full walk - went through to its end and held nothing back (a run that holds them again re-dates `HeldDeletes::at` instead). A pass over just the folders that changed (what the watcher does between full walks), a scoped sync, a run that could not see all of the remote, and a cancelled one never say it - they say nothing about the rest - so a banner built on `held` does not come and go while the user works. Every whole run emits it just before `PairFinished`, held before or not; `StateSink` also drops the error that was the hold with it. Every run that meets the same deletions reports them again, so a frontend that has already shown the news should key on the counts, as the tray's notice does (`Notifier::held`: once per newly held folder, again only if the number changes and not sooner than ten minutes later).

In tray mode the window has no access to the tray's state except through `status.json`, which can say a hold is standing for a while after the window (its own process, its own `Controller`) has settled it. The shipped window therefore remembers when it answered each folder and shows a hold only if `HeldDeletes::at` is later than that: a hold reported before the answer is the state as it was; one reported after is the problem still being there.

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
controller.resync(vec!["docs".into()]);      // merge both sides afresh: nothing deleted, deleted files come back
controller.sync_confirming_deletes(vec!["docs".into()]); // carry out the deletions the guard held back, this run only
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
