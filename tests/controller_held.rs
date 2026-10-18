//! What a frontend sees and can do when the mass-deletion guard holds deletions
//! back: the real `Controller` and the real engine against the fake
//! `proton-drive` from `tests/support/`.
//!
//! The guard itself is tested in `tests/data_loss.rs` and through the CLI in
//! `tests/disaster/`. This is the part the window and the tray stand on: the
//! pair says it is held (with how much, and on which side), nothing is deleted
//! while it is, `sync_confirming_deletes` carries the deletions out for one run,
//! `resync` brings the files back instead, and neither can be aimed at "every
//! folder" by leaving the list empty.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::Sandbox;
use neutronsync::config::{self, Config};
use neutronsync::engine::{
    run_sync_scoped, run_sync_shallow_many, run_sync_streaming, run_sync_with,
};
use neutronsync::events::{EventSink, SyncEvent};
use neutronsync::logger::Logger;
use neutronsync::service::{Controller, OverallStatus, Phase};

const FILES: usize = 150;
/// Four fifths of the folder: more than a run may delete (100 files, 20 %).
const GONE: usize = 120;

fn count_files(dir: &PathBuf) -> usize {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .count()
        })
        .unwrap_or(0)
}

struct World {
    sb: Sandbox,
    ctrl: Controller,
}

impl World {
    fn local(&self) -> PathBuf {
        self.sb.path("docs")
    }

    fn remote(&self) -> PathBuf {
        self.sb.path("remote/my-files/docs")
    }

    /// Wait for the run the controller started to end.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        // `busy` is set before the thread starts, so a run in flight reads busy.
        while self.ctrl.is_busy() {
            assert!(Instant::now() < deadline, "the run never finished");
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn sync(&self) {
        self.ctrl.sync(vec![], false);
        self.settle();
    }

    fn held(&self) -> Option<neutronsync::service::HeldDeletes> {
        self.ctrl.snapshot().pairs[0].held
    }
}

/// A synced folder of [`FILES`] files, of which [`GONE`] are then deleted
/// locally - an unmounted drive, an `rm -rf` - and one ordinary run that has
/// met the guard.
fn held_world(tag: &str) -> World {
    let sb = Sandbox::new(tag);
    let cli = sb.wrapper("");
    let file = sb.write_config(&cli);
    // The state folder is the tray's to create; the lock files go in it.
    fs::create_dir_all(sb.path("state")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(sb.path("state"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut cfg: Config = config::load(Some(file.to_str().unwrap())).expect("the config loads");
    cfg.propagate_deletes = true;
    let ctrl = Controller::new(cfg);
    let w = World { sb, ctrl };

    for i in 0..FILES {
        fs::write(
            w.local().join(format!("file-{i:04}.txt")),
            format!("file {i}"),
        )
        .unwrap();
    }
    w.sync();
    assert_eq!(count_files(&w.remote()), FILES, "the first sync uploads");
    assert!(w.held().is_none());

    for i in 0..GONE {
        fs::remove_file(w.local().join(format!("file-{i:04}.txt"))).unwrap();
    }
    w.sync();
    w
}

#[test]
fn a_held_pair_says_so_and_nothing_is_deleted_until_somebody_decides() {
    let w = held_world("held-state");

    let snap = w.ctrl.snapshot();
    let pair = &snap.pairs[0];
    let h = pair.held.expect("the pair is marked as held");
    assert_eq!((h.remote, h.local, h.tracked), (GONE, 0, FILES));
    assert_eq!((h.files(), h.percent()), (GONE, 80));
    assert_eq!(h.removed_from(), "from Proton Drive");
    assert_eq!(pair.phase, Phase::Error);
    assert_eq!(snap.overall(), OverallStatus::Error);
    assert!(snap.only_held_problems(), "nothing else is wrong");
    assert_eq!(
        snap.status_line(0),
        format!("docs: {GONE} deletions held back — open the app to review")
    );
    assert_eq!(
        count_files(&w.remote()),
        FILES,
        "the guard held, yet something was deleted"
    );

    // Running again meets the same thing, and still deletes nothing.
    w.sync();
    assert_eq!(w.held().map(|h| h.files()), Some(GONE));
    assert_eq!(count_files(&w.remote()), FILES);
}

#[test]
fn confirming_carries_the_deletions_out_for_that_one_run() {
    let w = held_world("held-confirm");

    w.ctrl.sync_confirming_deletes(vec!["docs".into()]);
    w.settle();
    assert_eq!(
        count_files(&w.remote()),
        FILES - GONE,
        "the user said to delete them"
    );
    assert_eq!(w.held(), None, "and the pair is no longer held");
    assert_eq!(w.ctrl.snapshot().pairs[0].phase, Phase::Synced);

    // The yes was for that run only: the next is guarded as ever, and quiet
    // because there is nothing left to hold.
    w.sync();
    assert_eq!(w.held(), None);
    assert_eq!(count_files(&w.remote()), FILES - GONE);
    // (Another mass deletion afterwards would be held again; the config the
    // controller keeps was never told to allow it.)
    assert!(!w.ctrl.config().allow_mass_delete);
}

#[test]
fn restoring_brings_the_files_back_and_ends_the_hold() {
    let w = held_world("held-restore");

    w.ctrl.resync(vec!["docs".into()]);
    w.settle();
    assert_eq!(
        count_files(&w.local()),
        FILES,
        "the deleted files came back"
    );
    assert_eq!(count_files(&w.remote()), FILES, "and nothing was deleted");
    assert_eq!(w.held(), None);
    assert_eq!(w.ctrl.snapshot().pairs[0].phase, Phase::Synced);

    // And it stays settled.
    w.sync();
    assert_eq!(w.held(), None);
    assert_eq!(count_files(&w.local()), FILES);
}

#[test]
fn confirming_or_restoring_needs_a_folder_named_and_never_means_all_of_them() {
    let w = held_world("held-names");

    w.ctrl.sync_confirming_deletes(vec![]);
    assert!(!w.ctrl.is_busy(), "an empty list started a run");
    w.ctrl.resync(vec![]);
    assert!(!w.ctrl.is_busy(), "an empty list started a run");
    // Nothing happened to the held pair or its files.
    assert_eq!(w.held().map(|h| h.files()), Some(GONE));
    assert_eq!(count_files(&w.remote()), FILES);
    assert_eq!(count_files(&w.local()), FILES - GONE);

    // A name that is not a folder of this config does nothing either.
    w.ctrl.sync_confirming_deletes(vec!["nope".into()]);
    w.settle();
    assert_eq!(count_files(&w.remote()), FILES);
}

/// Remembers, in order, which of the events that matter here a run emitted.
#[derive(Default)]
struct Tags(std::sync::Mutex<Vec<&'static str>>);

impl EventSink for Tags {
    fn emit(&self, ev: &SyncEvent) {
        let tag = match ev {
            SyncEvent::DeletesHeld { .. } => "held",
            SyncEvent::DeletesReleased { .. } => "released",
            SyncEvent::PairFinished { .. } => "finished",
            _ => return,
        };
        self.0.lock().unwrap().push(tag);
    }
}

impl Tags {
    fn of(run: impl FnOnce(&Tags)) -> Vec<&'static str> {
        let tags = Tags::default();
        run(&tags);
        let got = tags.0.lock().unwrap().clone();
        got
    }
}

/// What lets a hold be cleared is a run over the whole pair that held nothing, so
/// each way of running the engine must say so truthfully: a pass over one folder
/// that claimed it would clear a hold it never looked at, and one that was cut
/// short could not have seen what it says is gone.
#[test]
fn only_a_run_over_the_whole_pair_that_holds_nothing_releases_a_hold() {
    let sb = Sandbox::new("held-release");
    let cli = sb.wrapper("");
    let file = sb.write_config(&cli);
    fs::create_dir_all(sb.path("state")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(sb.path("state"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let cfg = config::load(Some(file.to_str().unwrap())).expect("the config loads");
    let pair = cfg.pairs[0].clone();
    fs::create_dir_all(sb.path("docs/sub")).unwrap();
    fs::write(sb.path("docs/a.txt"), "a").unwrap();
    fs::write(sb.path("docs/sub/b.txt"), "b").unwrap();
    let log = Logger::silent();

    // A sync of the pair, and the full walk the watcher does between passes:
    // each releases before it finishes, so the finish already sees it released.
    assert_eq!(
        Tags::of(|t| {
            run_sync_with(
                &cfg,
                std::slice::from_ref(&pair),
                false,
                false,
                &log,
                Some(t),
                None,
            );
        }),
        ["released", "finished"],
        "a sync of the pair"
    );
    assert_eq!(
        Tags::of(|t| {
            run_sync_streaming(&cfg, &pair, &log, Some(t), None);
        }),
        ["released", "finished"],
        "the full walk"
    );
    // The passes it makes over just the folders that changed say nothing.
    assert_eq!(
        Tags::of(|t| {
            let folders = ["".to_string(), "sub".to_string()];
            run_sync_shallow_many(&cfg, &pair, &folders, &log, Some(t), None);
        }),
        ["finished"],
        "a pass over some folders"
    );
    assert_eq!(
        Tags::of(|t| {
            run_sync_scoped(&cfg, &pair, "sub", &log, Some(t), None);
        }),
        ["finished"],
        "a sync of one sub-folder"
    );
    // A walk that was told to stop did not get to look.
    let stopped = std::sync::atomic::AtomicBool::new(true);
    let tags = Tags::of(|t| {
        run_sync_streaming(&cfg, &pair, &log, Some(t), Some(&stopped));
    });
    assert!(!tags.contains(&"released"), "a cancelled walk: {tags:?}");
}

/// ...and a run that does hold something reports that instead.
#[test]
fn a_run_that_holds_deletions_back_does_not_release_them() {
    let w = held_world("held-not-released");
    let cfg = w.ctrl.config();
    let pair = cfg.pairs[0].clone();
    let log = Logger::silent();
    for (name, tags) in [
        (
            "sync",
            Tags::of(|t| {
                run_sync_with(
                    &cfg,
                    std::slice::from_ref(&pair),
                    false,
                    false,
                    &log,
                    Some(t),
                    None,
                );
            }),
        ),
        (
            "full walk",
            Tags::of(|t| {
                run_sync_streaming(&cfg, &pair, &log, Some(t), None);
            }),
        ),
    ] {
        assert_eq!(tags, ["held", "finished"], "{name}");
    }
}
