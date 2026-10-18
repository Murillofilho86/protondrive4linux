//! The data-loss review (M9-003) as tests: what happens to the user's files when
//! things go wrong. Each test is a way a sync client has lost somebody's data,
//! run against the real engine and an in-memory remote, and asserts the safe
//! outcome. They are grouped by what goes wrong:
//!
//! - downloads that die, deliver too little, or land on a file the user has
//!   since edited;
//! - conflicts (`conflict copy` names, a failing transfer half way);
//! - a staging folder from a download that was killed;
//! - deletions that must not happen because the engine's view of one side is
//!   blind or suspicious (an unreadable folder, an empty root, a symlink, a
//!   pair pointed at another folder);
//! - a deletion too big to be believed (the mass-deletion guard).
//!
//! The sync semantics themselves (the decision table, the baseline) are in
//! `tests/engine.rs`; the processes-and-signals scenarios are in
//! `tests/disaster/`. See `docs/DATA_LOSS_REVIEW.md` for the whole matrix.

#[path = "common/fake_remote.rs"]
mod fake_remote;

use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::time::UNIX_EPOCH;

use fake_remote::*;
use neutronsync::engine::Engine;
use neutronsync::logger::Logger;

/// A whole-pair sync by the streaming walk (what the watcher's full walk does).
fn run_streaming(c: &neutronsync::config::Config, f: FakeRemote) {
    let log = Logger::silent();
    let mut eng = Engine::new(c, f, &log, false);
    eng.sync_pair_streaming(&c.pairs[0]).unwrap();
}

/// The pair root's direct children, hidden ones too, as names.
fn names_including_hidden(t: &Tmp) -> Vec<String> {
    local_names(t)
}

// --- downloads: they arrive whole or not at all -------------------------------------

#[test]
fn a_download_that_dies_half_way_leaves_nothing_under_the_final_name() {
    let t = Tmp::new("dl-dies-new");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false);
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/movie.mkv".into(), b"0123456789".to_vec());
    store.borrow_mut().partial_then_fail = Some(("/my-files/docs/movie.mkv".into(), 4));

    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert!(
        !t.path().join("local/movie.mkv").exists(),
        "a half-written file sits under the final name"
    );
    assert_eq!(
        names_including_hidden(&t),
        Vec::<String>::new(),
        "and nothing was left behind, not even the staging folder"
    );

    // The disk has room again: the next run fetches it whole, with no junk copy
    // of the partial one, locally or on the remote.
    store.borrow_mut().partial_then_fail = None;
    clear_retry_backoff(&c, "docs");
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/movie.mkv")).unwrap(),
        "0123456789"
    );
    assert_eq!(names_including_hidden(&t), ["movie.mkv"]);
    assert_eq!(
        store.borrow().files.len(),
        1,
        "junk was uploaded to the remote: {:?}",
        store.borrow().files.keys().collect::<Vec<_>>()
    );
}

#[test]
fn a_download_that_dies_over_an_existing_file_keeps_the_old_copy() {
    let t = Tmp::new("dl-dies-over");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "v1");
    run(&c, fake, false);
    store.borrow_mut().files.insert(
        "/my-files/docs/a.txt".into(),
        b"v2 is a lot longer".to_vec(),
    );
    store.borrow_mut().partial_then_fail = Some(("/my-files/docs/a.txt".into(), 5));

    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "v1",
        "the old local copy was clobbered by a partial download"
    );

    // And once the disk has room, the new version lands.
    store.borrow_mut().partial_then_fail = None;
    clear_retry_backoff(&c, "docs");
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "v2 is a lot longer"
    );
    assert_eq!(names_including_hidden(&t), ["a.txt"], "no conflict copy");
}

#[test]
fn a_download_that_claims_success_but_is_short_is_never_uploaded_over_the_good_copy() {
    // The CLI exits 0 having written 4 of 10 bytes. Taken at its word, the file
    // is recorded as synced, the next run reads its size as a local edit, and
    // uploads the 4 bytes over the whole file on the remote.
    let t = Tmp::new("dl-short");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false);
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/big.bin".into(), b"0123456789".to_vec());
    store.borrow_mut().truncate_download = Some(("/my-files/docs/big.bin".into(), 4));

    for _ in 0..3 {
        let (f, _s) = reuse(&store);
        run(&c, f, false);
        clear_retry_backoff(&c, "docs");
    }
    assert_eq!(
        store
            .borrow()
            .files
            .get("/my-files/docs/big.bin")
            .map(|v| v.len()),
        Some(10),
        "the good remote copy was replaced by the truncated download"
    );
    assert!(
        !t.path().join("local/big.bin").exists(),
        "a truncated file was put in place"
    );
}

#[test]
fn an_edit_made_while_the_run_is_in_flight_is_not_overwritten_by_a_download() {
    let t = Tmp::new("dl-edit");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "v1");
    run(&c, fake, false);
    // The remote moves on to a longer v2 and the local file is unchanged, so a
    // plain download is planned...
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/a.txt".into(), b"remote v2 content".to_vec());
    // ...and while it is running the user saves their own edit.
    let local = t.path().join("local/a.txt");
    store.borrow_mut().on_download = Some(Rc::new(move |_| {
        std::fs::write(&local, "my precious unsynced edit").unwrap();
    }));
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let all = everything(&t, &store);
    assert!(
        all.iter().any(|b| b == "my precious unsynced edit"),
        "the edit made during the run was overwritten: {all:?}"
    );

    // Nothing is lost on the next run either: the two edits meet as a conflict,
    // and both versions are kept.
    store.borrow_mut().on_download = None;
    clear_retry_backoff(&c, "docs");
    for _ in 0..2 {
        let (f, _s) = reuse(&store);
        run(&c, f, false);
    }
    let all = everything(&t, &store);
    assert!(
        all.iter().any(|b| b == "my precious unsynced edit"),
        "{all:?}"
    );
    assert!(all.iter().any(|b| b == "remote v2 content"), "{all:?}");
}

#[test]
fn a_file_created_while_the_run_is_in_flight_is_not_overwritten_by_a_download() {
    let t = Tmp::new("dl-create");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false);
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/new.txt".into(), b"from the cloud".to_vec());
    // The plan sees no local new.txt; then the user creates one.
    let local = t.path().join("local/new.txt");
    store.borrow_mut().on_download = Some(Rc::new(move |_| {
        std::fs::write(&local, "I just made this").unwrap();
    }));
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let all = everything(&t, &store);
    assert!(
        all.iter().any(|b| b == "I just made this"),
        "a file the user created meanwhile was overwritten: {all:?}"
    );
    // ...and the next run keeps both.
    store.borrow_mut().on_download = None;
    clear_retry_backoff(&c, "docs");
    for _ in 0..2 {
        let (f, _s) = reuse(&store);
        run(&c, f, false);
    }
    let all = everything(&t, &store);
    assert!(all.iter().any(|b| b == "I just made this"), "{all:?}");
    assert!(all.iter().any(|b| b == "from the cloud"), "{all:?}");
}

#[test]
fn a_downloaded_file_carries_the_remote_modification_time_and_is_then_left_alone() {
    // Otherwise the next scan reads it as a local edit (its mtime is "now") and
    // uploads it straight back.
    let t = Tmp::new("dl-mtime");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false);
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/a.txt".into(), b"hello".to_vec());
    store
        .borrow_mut()
        .mtimes
        .insert("/my-files/docs/a.txt".into(), 1_000_000);
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let mtime = std::fs::metadata(t.path().join("local/a.txt"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(mtime, 1_000_000);
    // the next run has nothing to do with it
    let before = store.borrow().files.clone();
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(store.borrow().files, before);
    assert_eq!(names_including_hidden(&t), ["a.txt"]);
}

// --- staging folders ----------------------------------------------------------

#[test]
fn a_download_in_progress_is_never_scanned_uploaded_or_deleted() {
    // What a killed run leaves in a folder: a staging folder holding half a file.
    let t = Tmp::new("stage-ignored");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "x");
    run(&c, fake, false);
    let staging = t.path().join("local/.protondrive4linux-1-2-3.part");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("movie.mkv"), "half a movie").unwrap();

    // Batch and streaming walks both: neither uploads it, nor makes a remote folder for it.
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    eng.sync_pair_streaming(&c.pairs[0]).unwrap();
    assert_eq!(
        remote_files(&store).keys().collect::<Vec<_>>(),
        ["/my-files/docs/a.txt"],
        "a staging folder reached the remote"
    );
    assert!(
        store.borrow().dirs.iter().all(|d| !d.contains(".part")),
        "{:?}",
        store.borrow().dirs
    );
    assert!(
        staging.join("movie.mkv").exists(),
        "a fresh one is left alone"
    );
}

#[test]
fn a_staging_folder_left_by_a_dead_process_is_cleaned_up_once_it_is_stale() {
    let t = Tmp::new("stage-sweep");
    let mut c = cfg(t.path());
    // The CLI is allowed no time at all, so anything is older than that + the
    // margin only if we also age it: the margin is checked by the unit tests of
    // `staged`; here the point is that a scan reaches the sweep.
    c.cli_timeout_secs = 0;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "x");
    run(&c, fake, false);
    let staging = t.path().join("local/.protondrive4linux-9-9-9.part");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("movie.mkv"), "half a movie").unwrap();
    // age it past the margin (a directory can be opened read-only to set its time)
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    for p in [staging.join("movie.mkv"), staging.clone()] {
        std::fs::File::open(&p)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }

    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert!(!staging.exists(), "the stale staging folder was not swept");
    assert!(t.path().join("local/a.txt").exists());
}

// --- conflicts ----------------------------------------------------------------

#[test]
fn a_conflict_copy_never_overwrites_a_file_that_already_has_its_name() {
    let t = Tmp::new("conf-name");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "base");
    run(&c, fake, false);
    // Pre-create every name the conflict copy could get in the next few seconds.
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut precious = Vec::new();
    for dt in -1..6 {
        let stamp = neutronsync::datefmt::epoch_to_stamp(now + dt);
        let p = t.path().join(format!("local/a (conflict {stamp}).txt"));
        std::fs::write(&p, format!("PRECIOUS {dt}")).unwrap();
        precious.push((p, format!("PRECIOUS {dt}")));
    }
    // Now a real conflict (different sizes, so it is one).
    write(
        &t.path().join("local/a.txt"),
        "local edit, longer than the remote one",
    );
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/a.txt".into(), b"remote edit".to_vec());
    let (f, _s) = reuse(&store);
    run(&c, f, false);

    for (p, body) in precious {
        assert_eq!(
            std::fs::read_to_string(&p).unwrap_or_default(),
            body,
            "{} was overwritten",
            p.display()
        );
    }
    let all = everything(&t, &store);
    assert!(
        all.iter()
            .any(|b| b == "local edit, longer than the remote one"),
        "the local edit was lost: {all:?}"
    );
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "remote edit"
    );
}

#[test]
fn a_conflict_whose_download_fails_loses_nothing_and_converges() {
    let t = Tmp::new("conf-fail");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "base");
    run(&c, fake, false);
    write(
        &t.path().join("local/a.txt"),
        "local edit, longer than the remote one",
    );
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/a.txt".into(), b"remote edit".to_vec());
    store.borrow_mut().fail_download = Some("/my-files/docs/a.txt".into());
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    {
        let all = everything(&t, &store);
        assert!(
            all.iter().any(|b| b.contains("local edit")),
            "local edit lost after a failed conflict: {all:?}"
        );
        assert!(all.iter().any(|b| b == "remote edit"), "{all:?}");
    }
    store.borrow_mut().fail_download = None;
    clear_retry_backoff(&c, "docs");
    for _ in 0..2 {
        let (f, _s) = reuse(&store);
        run(&c, f, false);
    }
    let all = everything(&t, &store);
    assert!(all.iter().any(|b| b.contains("local edit")), "{all:?}");
    assert!(all.iter().any(|b| b == "remote edit"), "{all:?}");
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "remote edit"
    );
}

// --- deletions on a blind or suspicious view ----------------------------------------

#[test]
fn an_unreadable_local_folder_never_deletes_what_it_holds() {
    // The batch scan (`protondrive4linux sync`, the GUI's Sync now) walked the
    // tree and dropped the walk's errors, so a folder it could not open looked
    // empty, and every file in it looked deleted.
    if is_root() {
        return; // root reads anything
    }
    let t = Tmp::new("blind-unreadable");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/sub/b.txt"), "keep me");
    write(&t.path().join("local/top.txt"), "top");
    run(&c, fake, false);

    let sub = t.path().join("local/sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (f, _s) = reuse(&store);
    let _ = try_run(&c, f, false);
    // streaming walk as well
    let (f, _s) = reuse(&store);
    run_streaming(&c, f);
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        store
            .borrow()
            .files
            .contains_key("/my-files/docs/sub/b.txt"),
        "the remote copy of a file in an unreadable folder was trashed"
    );
    // and once the folder can be read again nothing has changed
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(store.borrow().files.len(), 2);
    assert!(sub.join("b.txt").exists());
}

#[test]
fn an_empty_local_root_never_deletes_the_remote() {
    // An unmounted drive leaves its mount point behind, empty.
    let t = Tmp::new("blind-empty-root");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    for i in 0..5 {
        write(
            &t.path().join(format!("local/f{i}.txt")),
            &format!("data {i}"),
        );
    }
    write(&t.path().join("local/sub/deep.txt"), "deep");
    run(&c, fake, false);
    std::fs::remove_dir_all(t.path().join("local")).unwrap();
    std::fs::create_dir_all(t.path().join("local")).unwrap();

    for walk in ["batch", "streaming"] {
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            let _ = try_run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
        assert_eq!(
            store.borrow().files.len(),
            6,
            "{walk}: remote files were trashed because the local root came back empty: {:?}",
            store.borrow().files.keys().collect::<Vec<_>>()
        );
    }

    // The drive comes back: everything is where it was, nothing to do.
    for i in 0..5 {
        write(
            &t.path().join(format!("local/f{i}.txt")),
            &format!("data {i}"),
        );
    }
    write(&t.path().join("local/sub/deep.txt"), "deep");
    let before = store.borrow().files.clone();
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(store.borrow().files, before);
}

#[test]
fn emptying_a_small_pair_is_a_real_deletion() {
    // The empty-root guard is for a folder full of files going missing at once,
    // not for deleting the last file or two of a small one.
    let t = Tmp::new("blind-small-pair");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "a");
    write(&t.path().join("local/b.txt"), "b");
    run(&c, fake, false);
    std::fs::remove_file(t.path().join("local/a.txt")).unwrap();
    std::fs::remove_file(t.path().join("local/b.txt")).unwrap();
    for walk in ["batch", "streaming"] {
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
    }
    assert!(
        store.borrow().files.is_empty(),
        "{:?}",
        store.borrow().files.keys().collect::<Vec<_>>()
    );
}

#[test]
fn an_empty_folder_below_the_root_is_still_a_real_deletion() {
    // The guard is for the root: emptying a sub-folder on purpose propagates.
    let t = Tmp::new("blind-subfolder-emptied");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/keep.txt"), "keep");
    write(&t.path().join("local/old/a.txt"), "a");
    write(&t.path().join("local/old/b.txt"), "b");
    run(&c, fake, false);
    std::fs::remove_file(t.path().join("local/old/a.txt")).unwrap();
    std::fs::remove_file(t.path().join("local/old/b.txt")).unwrap();
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let keys: Vec<String> = store.borrow().files.keys().cloned().collect();
    assert_eq!(keys, ["/my-files/docs/keep.txt"], "{keys:?}");
}

#[test]
fn a_tracked_file_replaced_by_a_symlink_is_not_a_deletion() {
    let t = Tmp::new("blind-symlink-file");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/conf.toml"), "important settings");
    write(&t.path().join("local/other.txt"), "other");
    run(&c, fake, false);
    // the user points the file at their dotfiles repo
    std::fs::remove_file(t.path().join("local/conf.toml")).unwrap();
    std::os::unix::fs::symlink(
        "/nonexistent/dotfiles/conf.toml",
        t.path().join("local/conf.toml"),
    )
    .unwrap();
    for walk in ["batch", "streaming"] {
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
        assert_eq!(
            store
                .borrow()
                .files
                .get("/my-files/docs/conf.toml")
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .as_deref(),
            Some("important settings"),
            "{walk}: the remote copy was trashed or replaced because the path became a symlink"
        );
    }
}

#[test]
fn a_tracked_folder_replaced_by_a_symlink_is_not_a_deletion_and_is_never_followed() {
    let t = Tmp::new("blind-symlink-dir");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/photos/a.jpg"), "photo a");
    run(&c, fake, false);
    // the user moves the folder elsewhere and leaves a link to it
    let elsewhere = t.path().join("elsewhere");
    std::fs::rename(t.path().join("local/photos"), &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, t.path().join("local/photos")).unwrap();
    // something new lands in the link's target - it must not be uploaded through the link
    write(&elsewhere.join("new.jpg"), "photo new");
    for walk in ["batch", "streaming"] {
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
        let keys: Vec<String> = store.borrow().files.keys().cloned().collect();
        assert_eq!(
            keys,
            ["/my-files/docs/photos/a.jpg"],
            "{walk}: the folder was trashed, or followed through its link: {keys:?}"
        );
    }
}

#[test]
fn a_download_is_never_written_through_a_symlinked_folder() {
    let t = Tmp::new("blind-symlink-write");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    std::fs::create_dir_all(t.path().join("local")).unwrap();
    run(&c, fake, false);
    // a link inside the root, to a folder outside it
    let outside = t.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, t.path().join("local/docs")).unwrap();
    // the remote has a real folder of the same name with a file in it
    store
        .borrow_mut()
        .dirs
        .insert("/my-files/docs/docs".to_string());
    store.borrow_mut().files.insert(
        "/my-files/docs/docs/secret.txt".into(),
        b"from the cloud".to_vec(),
    );
    for walk in ["batch", "streaming"] {
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            let _ = try_run(&c, f, false);
        } else {
            let log = Logger::silent();
            let mut eng = Engine::new(&c, f, &log, false);
            let _ = eng.sync_pair_streaming(&c.pairs[0]);
        }
        let leaked: Vec<String> = std::fs::read_dir(&outside)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leaked.is_empty(),
            "{walk}: a download was written through a symlink to outside the sync root: {leaked:?}"
        );
    }
}

#[test]
fn pointing_a_pair_at_another_local_folder_starts_a_fresh_baseline_not_a_mass_delete() {
    let t = Tmp::new("blind-pair-moved");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    for i in 0..4 {
        write(
            &t.path().join(format!("local/f{i}.txt")),
            &format!("data {i}"),
        );
    }
    run(&c, fake, false);
    // The config now says the same pair lives in another (empty) folder.
    std::fs::create_dir_all(t.path().join("elsewhere")).unwrap();
    c.pairs[0].local = t.path().join("elsewhere");
    let (f, _s) = reuse(&store);
    let _ = try_run(&c, f, false);
    assert_eq!(
        store.borrow().files.len(),
        4,
        "the remote was trashed after the pair's local folder was changed"
    );
    // ...and the new folder is simply filled from the remote (a fresh union).
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(
        std::fs::read_to_string(t.path().join("elsewhere/f2.txt")).unwrap(),
        "data 2"
    );
    assert_eq!(store.borrow().files.len(), 4);
}

#[test]
fn pointing_a_pair_at_another_remote_folder_starts_a_fresh_baseline_too() {
    let t = Tmp::new("blind-pair-remote");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    c.local_delete = neutronsync::config::LocalDelete::Remove;
    let (fake, store) = FakeRemote::new();
    for i in 0..4 {
        write(
            &t.path().join(format!("local/f{i}.txt")),
            &format!("data {i}"),
        );
    }
    run(&c, fake, false);
    // the remote folder is now another (empty) one
    store
        .borrow_mut()
        .dirs
        .insert("/my-files/other".to_string());
    c.pairs[0].remote = "/my-files/other".into();
    let (f, _s) = reuse(&store);
    let _ = try_run(&c, f, false);
    assert_eq!(
        std::fs::read_dir(t.path().join("local")).unwrap().count(),
        4,
        "the local files were deleted because the pair's remote folder was changed"
    );
}

#[test]
fn an_unchanged_pair_keeps_its_baseline_across_runs() {
    // The identity check must not itself discard the baseline: a deletion still
    // propagates on the run after it was made.
    let t = Tmp::new("blind-pair-same");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "a");
    write(&t.path().join("local/b.txt"), "b");
    run(&c, fake, false);
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    std::fs::remove_file(t.path().join("local/a.txt")).unwrap();
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let keys: Vec<String> = store.borrow().files.keys().cloned().collect();
    assert_eq!(keys, ["/my-files/docs/b.txt"], "{keys:?}");
}

// --- same size is not same content ---------------------------------------------------

/// Both sides edit `a.txt` to different content of the SAME size, at different
/// times. `remote_sha1`: whether the remote lists a content digest.
fn dual_edit_same_size(remote_sha1: bool) -> (Tmp, std::rc::Rc<std::cell::RefCell<Store>>) {
    let t = Tmp::new(if remote_sha1 {
        "same-size-sha"
    } else {
        "same-size"
    });
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    store.borrow_mut().report_sha1 = remote_sha1;
    write(&t.path().join("local/a.txt"), "AAAA");
    set_local_mtime(&t.path().join("local/a.txt"), 1_000_000);
    run(&c, fake, false);
    // the remote lists the baseline's modification time
    store
        .borrow_mut()
        .mtimes
        .insert("/my-files/docs/a.txt".into(), 1_000_000);
    write(&t.path().join("local/a.txt"), "LLLL");
    set_local_mtime(&t.path().join("local/a.txt"), 1_000_100);
    store
        .borrow_mut()
        .files
        .insert("/my-files/docs/a.txt".into(), b"RRRR".to_vec());
    store
        .borrow_mut()
        .mtimes
        .insert("/my-files/docs/a.txt".into(), 1_000_200);
    (t, store)
}

#[test]
fn two_different_edits_of_the_same_size_are_a_conflict_not_metadata_drift() {
    // "Same size, different mtime" used to be taken for drift: the baseline was
    // refreshed to the local file and nothing was done - and on the next run the
    // remote's mtime no longer matched the baseline, so it was downloaded over
    // the local file. The local edit was gone from both sides.
    for remote_sha1 in [false, true] {
        let (t, store) = dual_edit_same_size(remote_sha1);
        let c = cfg(t.path());
        for i in 0..3 {
            let (f, _s) = reuse(&store);
            run(&c, f, false);
            let all = everything(&t, &store);
            assert!(
                all.iter().any(|b| b == "LLLL"),
                "sha1={remote_sha1} run {i}: the local edit is gone everywhere: {all:?}"
            );
            assert!(
                all.iter().any(|b| b == "RRRR"),
                "sha1={remote_sha1} run {i}: the remote edit is gone everywhere: {all:?}"
            );
        }
        // and it settles: one conflict copy, nothing multiplying
        let conflicts = local_names(&t)
            .into_iter()
            .filter(|n| n.contains("conflict"))
            .count();
        assert_eq!(conflicts, 1, "sha1={remote_sha1}: {:?}", local_names(&t));
    }
}

#[test]
fn the_same_content_with_drifted_times_is_not_a_conflict_when_the_digest_proves_it() {
    // Both sides touched, nothing changed: with the remote's SHA-1 to hand the
    // two are provably the same file, so no conflict copy is made.
    let t = Tmp::new("same-size-drift");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    store.borrow_mut().report_sha1 = true;
    write(&t.path().join("local/a.txt"), "SAME");
    set_local_mtime(&t.path().join("local/a.txt"), 1_000_000);
    run(&c, fake, false);
    store
        .borrow_mut()
        .mtimes
        .insert("/my-files/docs/a.txt".into(), 1_000_000);
    // both sides are "touched": new times, identical bytes
    set_local_mtime(&t.path().join("local/a.txt"), 1_000_100);
    store
        .borrow_mut()
        .mtimes
        .insert("/my-files/docs/a.txt".into(), 1_000_200);
    for _ in 0..3 {
        let (f, _s) = reuse(&store);
        run(&c, f, false);
    }
    assert_eq!(local_names(&t), ["a.txt"], "a needless conflict copy");
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/a.txt")).unwrap(),
        "SAME"
    );
}

#[test]
fn files_made_independently_on_both_sides_with_equal_sizes_are_both_kept() {
    // The first sync of a folder that already exists on both sides: two
    // different `notes.txt` of the same length. One was silently replaced by the
    // other.
    for remote_sha1 in [false, true] {
        let t = Tmp::new("first-sync-same-size");
        let c = cfg(t.path());
        let (fake, store) = FakeRemote::new();
        store.borrow_mut().report_sha1 = remote_sha1;
        write(&t.path().join("local/notes.txt"), "LLLL");
        set_local_mtime(&t.path().join("local/notes.txt"), 1_000_100);
        store
            .borrow_mut()
            .files
            .insert("/my-files/docs/notes.txt".into(), b"RRRR".to_vec());
        store
            .borrow_mut()
            .mtimes
            .insert("/my-files/docs/notes.txt".into(), 1_000_200);
        run(&c, fake, false);
        for i in 0..3 {
            let (f, _s) = reuse(&store);
            run(&c, f, false);
            let all = everything(&t, &store);
            assert!(
                all.iter().any(|b| b == "LLLL"),
                "sha1={remote_sha1} run {i}: LLLL lost: {all:?}"
            );
            assert!(
                all.iter().any(|b| b == "RRRR"),
                "sha1={remote_sha1} run {i}: RRRR lost: {all:?}"
            );
        }
    }
}

// --- deleting a folder must not take what never synced with it -------------------

#[test]
fn a_folder_removed_remotely_keeps_local_files_that_never_synced() {
    // The folder goes (propagated from the remote), but a file made here after the
    // last sync - whose upload then fails - lives in it. `local_delete = remove`
    // unlinks without a trash to recover from.
    for walk in ["batch", "streaming"] {
        let t = Tmp::new("dir-delete-local");
        let mut c = cfg(t.path());
        c.propagate_deletes = true;
        c.local_delete = neutronsync::config::LocalDelete::Remove;
        let (fake, store) = FakeRemote::new();
        write(&t.path().join("local/sub/old.txt"), "old");
        run(&c, fake, false);
        {
            let mut s = store.borrow_mut();
            s.files.remove("/my-files/docs/sub/old.txt");
            s.dirs.remove("/my-files/docs/sub");
            s.fail_upload = Some("/my-files/docs/sub/new.txt".into());
        }
        write(
            &t.path().join("local/sub/new.txt"),
            "brand new, never synced",
        );
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            let _ = try_run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
        assert!(
            t.path().join("local/sub/new.txt").exists(),
            "{walk}: a never-synced local file was deleted along with the remotely removed folder"
        );
        // Once the upload can work again the file reaches the remote - by the
        // same kind of walk, which has to go into the folder that was kept.
        store.borrow_mut().fail_upload = None;
        clear_retry_backoff(&c, "docs");
        for _ in 0..2 {
            let (f, _s) = reuse(&store);
            if walk == "batch" {
                run(&c, f, false);
            } else {
                run_streaming(&c, f);
            }
        }
        assert_eq!(
            store
                .borrow()
                .files
                .get("/my-files/docs/sub/new.txt")
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .as_deref(),
            Some("brand new, never synced"),
            "{walk}"
        );
        assert!(t.path().join("local/sub/new.txt").exists(), "{walk}");
    }
}

#[test]
fn a_folder_removed_remotely_keeps_a_local_file_edited_since_the_last_sync() {
    let t = Tmp::new("dir-delete-edited");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    c.local_delete = neutronsync::config::LocalDelete::Remove;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/sub/doc.txt"), "v1");
    run(&c, fake, false);
    {
        let mut s = store.borrow_mut();
        s.files.remove("/my-files/docs/sub/doc.txt");
        s.dirs.remove("/my-files/docs/sub");
    }
    write(&t.path().join("local/sub/doc.txt"), "v2, edited here since");
    let (f, _s) = reuse(&store);
    run_streaming(&c, f);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/sub/doc.txt")).unwrap(),
        "v2, edited here since"
    );
}

#[test]
fn a_folder_removed_remotely_is_still_removed_when_nothing_in_it_is_new() {
    // The guard must not make deleting folders impossible.
    for walk in ["batch", "streaming"] {
        let t = Tmp::new("dir-delete-clean");
        let mut c = cfg(t.path());
        c.propagate_deletes = true;
        c.local_delete = neutronsync::config::LocalDelete::Remove;
        let (fake, store) = FakeRemote::new();
        write(&t.path().join("local/keep.txt"), "keep");
        write(&t.path().join("local/sub/old.txt"), "old");
        write(&t.path().join("local/sub/deeper/x.txt"), "x");
        run(&c, fake, false);
        {
            let mut s = store.borrow_mut();
            s.files.retain(|k, _| !k.contains("/sub/"));
            s.dirs.retain(|d| !d.contains("/sub"));
        }
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
        assert!(!t.path().join("local/sub").exists(), "{walk}: not removed");
        assert!(t.path().join("local/keep.txt").exists());
    }
}

#[test]
fn a_folder_removed_locally_keeps_remote_files_this_machine_never_saw() {
    for walk in ["batch", "streaming"] {
        let t = Tmp::new("dir-delete-remote");
        let mut c = cfg(t.path());
        c.propagate_deletes = true;
        let (fake, store) = FakeRemote::new();
        write(&t.path().join("local/sub/old.txt"), "old");
        run(&c, fake, false);
        // another device adds a file in the folder; this machine removes the folder
        store.borrow_mut().files.insert(
            "/my-files/docs/sub/from-phone.txt".into(),
            b"photo".to_vec(),
        );
        std::fs::remove_dir_all(t.path().join("local/sub")).unwrap();
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
        assert!(
            store
                .borrow()
                .files
                .contains_key("/my-files/docs/sub/from-phone.txt"),
            "{walk}: a remote file this machine never saw was trashed with the folder"
        );
        // The folder stays for now; the next runs settle what is in it: the file
        // this machine knew (and the user deleted) goes, the new one comes down.
        for _ in 0..2 {
            let (f, _s) = reuse(&store);
            run(&c, f, false);
        }
        let keys: Vec<String> = store.borrow().files.keys().cloned().collect();
        assert_eq!(
            keys,
            ["/my-files/docs/sub/from-phone.txt"],
            "{walk}: {keys:?}"
        );
        assert_eq!(
            std::fs::read_to_string(t.path().join("local/sub/from-phone.txt")).unwrap(),
            "photo",
            "{walk}"
        );
    }
}

#[test]
fn a_folder_removed_locally_is_still_removed_remotely_when_nothing_in_it_is_new() {
    for walk in ["batch", "streaming"] {
        let t = Tmp::new("dir-delete-remote-clean");
        let mut c = cfg(t.path());
        c.propagate_deletes = true;
        let (fake, store) = FakeRemote::new();
        write(&t.path().join("local/keep.txt"), "keep");
        write(&t.path().join("local/sub/old.txt"), "old");
        write(&t.path().join("local/sub/deeper/x.txt"), "x");
        run(&c, fake, false);
        std::fs::remove_dir_all(t.path().join("local/sub")).unwrap();
        let (f, _s) = reuse(&store);
        if walk == "batch" {
            run(&c, f, false);
        } else {
            run_streaming(&c, f);
        }
        let keys: Vec<String> = store.borrow().files.keys().cloned().collect();
        assert_eq!(keys, ["/my-files/docs/keep.txt"], "{walk}: {keys:?}");
    }
}

// --- renames ---------------------------------------------------------------------

#[test]
fn a_new_file_with_the_size_and_time_of_a_deleted_one_is_not_a_rename() {
    // delete a.bin, create b.bin: the same size and the same (whole-second)
    // modification time - a build output, a batch of files extracted together -
    // looked like a move. The remote then renamed the OLD content to b.bin and the
    // new content was never uploaded.
    for remote_sha1 in [true, false] {
        let t = Tmp::new("rename-false");
        let mut c = cfg(t.path());
        c.propagate_deletes = true;
        let (fake, store) = FakeRemote::new();
        store.borrow_mut().report_sha1 = remote_sha1;
        write(&t.path().join("local/a.bin"), "AAAAAAAA");
        set_local_mtime(&t.path().join("local/a.bin"), 1_000_000);
        run(&c, fake, false);
        std::fs::remove_file(t.path().join("local/a.bin")).unwrap();
        write(&t.path().join("local/b.bin"), "BBBBBBBB");
        set_local_mtime(&t.path().join("local/b.bin"), 1_000_000);
        let (f, _s) = reuse(&store);
        run(&c, f, false);
        let rf = remote_files(&store);
        // With the remote's digest the claim is checked and refused. Without
        // one, nothing can tell the two apart and the old behaviour stands.
        if remote_sha1 {
            assert_eq!(
                rf.get("/my-files/docs/b.bin").map(String::as_str),
                Some("BBBBBBBB"),
                "b.bin's real content never reached the remote: {rf:?}"
            );
            assert_eq!(store.borrow().moves, 0, "it was taken for a rename");
        }
    }
}

#[test]
fn a_real_rename_is_still_a_move_even_with_digests() {
    let t = Tmp::new("rename-true");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    store.borrow_mut().report_sha1 = true;
    write(&t.path().join("local/a.txt"), "hello world");
    run(&c, fake, false);
    std::fs::rename(t.path().join("local/a.txt"), t.path().join("local/b.txt")).unwrap();
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let rf = remote_files(&store);
    assert_eq!(
        rf.get("/my-files/docs/b.txt").map(String::as_str),
        Some("hello world")
    );
    assert!(!rf.contains_key("/my-files/docs/a.txt"));
    assert_eq!(store.borrow().moves, 1, "should have been a move");
}

// --- permission denied -----------------------------------------------------------

#[test]
fn a_download_into_a_folder_that_cannot_be_written_fails_cleanly() {
    // A read-only destination: nothing is half written, nothing is lost, the error
    // says so, and the file arrives once the folder is writable again.
    if is_root() {
        return; // root writes anywhere
    }
    let t = Tmp::new("perm-write");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/ro/keep.txt"), "mine");
    run(&c, fake, false);
    store.borrow_mut().files.insert(
        "/my-files/docs/ro/new.txt".into(),
        b"from the cloud".to_vec(),
    );
    let ro = t.path().join("local/ro");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    let r = eng.sync_pair(&c.pairs[0], false).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!r.errors.is_empty(), "the failure must be reported");
    assert_eq!(
        std::fs::read_to_string(ro.join("keep.txt")).unwrap(),
        "mine",
        "an existing file was touched"
    );
    let names: Vec<String> = std::fs::read_dir(&ro)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["keep.txt"], "something was left behind: {names:?}");

    clear_retry_backoff(&c, "docs");
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(
        std::fs::read_to_string(ro.join("new.txt")).unwrap(),
        "from the cloud"
    );
}

#[test]
fn a_file_that_cannot_be_read_is_an_error_and_never_touches_the_remote_copy() {
    if is_root() {
        return;
    }
    let t = Tmp::new("perm-read");
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    let (fake, store) = FakeRemote::new();
    write(&t.path().join("local/a.txt"), "v1");
    write(&t.path().join("local/b.txt"), "other");
    run(&c, fake, false);
    // edited, then made unreadable: the upload cannot read it
    write(&t.path().join("local/a.txt"), "v2, longer");
    let a = t.path().join("local/a.txt");
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (f, _s) = reuse(&store);
    let log = Logger::silent();
    let mut eng = Engine::new(&c, f, &log, false);
    let r = eng.sync_pair(&c.pairs[0], false).unwrap();
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(!r.errors.is_empty(), "the failure must be reported");
    let rf = remote_files(&store);
    assert_eq!(
        rf.get("/my-files/docs/a.txt").map(String::as_str),
        Some("v1"),
        "the remote copy was damaged or removed: {rf:?}"
    );
    assert_eq!(
        rf.get("/my-files/docs/b.txt").map(String::as_str),
        Some("other")
    );
    // readable again: the edit goes up
    clear_retry_backoff(&c, "docs");
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    assert_eq!(
        remote_files(&store)
            .get("/my-files/docs/a.txt")
            .map(String::as_str),
        Some("v2, longer")
    );
}

// --- large trees -------------------------------------------------------------------

#[test]
fn a_large_tree_converges_in_both_directions_with_both_walks_and_then_does_nothing() {
    let t = Tmp::new("large-tree");
    let c = cfg(t.path());
    let (fake, store) = FakeRemote::new();
    let n = 3000usize;
    for i in 0..n {
        write(
            &t.path()
                .join(format!("local/d{}/s{}/f{i}.txt", i % 30, i % 7)),
            &format!("content {i}"),
        );
    }
    let started = std::time::Instant::now();
    run(&c, fake, false);
    assert_eq!(store.borrow().files.len(), n);

    // A second machine: an empty folder with the same remote. Everything comes down.
    let other = Tmp::new("large-tree-other");
    let c2 = cfg(other.path());
    std::fs::create_dir_all(other.path().join("local")).unwrap();
    let (f, _s) = reuse(&store);
    run_streaming(&c2, f);
    let down = walkdir::WalkDir::new(other.path().join("local"))
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .count();
    assert_eq!(down, n, "not everything came down");
    assert_eq!(
        std::fs::read_to_string(other.path().join("local/d7/s0/f7.txt")).unwrap(),
        "content 7"
    );

    // Converged: another pass with either walk changes nothing anywhere.
    let before = store.borrow().files.clone();
    let (f, _s) = reuse(&store);
    run(&c, f, false);
    let (f, _s) = reuse(&store);
    run_streaming(&c2, f);
    assert_eq!(store.borrow().files, before);
    assert!(
        started.elapsed().as_secs() < 60,
        "a 3000-file tree took {:?}",
        started.elapsed()
    );
}

// --- the mass-deletion guard ----------------------------------------------------------

/// Remembers every event as its `Debug` text.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<String>>);

impl neutronsync::events::EventSink for Recorder {
    fn emit(&self, ev: &neutronsync::events::SyncEvent) {
        self.0.lock().unwrap().push(format!("{ev:?}"));
    }
}

impl Recorder {
    fn held(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.starts_with("DeletesHeld"))
            .cloned()
            .collect()
    }
}

/// A pair of `folders` folders holding `per` files each, synced, with deletes
/// propagating and the default allowance (the larger of 100 files and 20%).
fn synced_tree(
    tag: &str,
    folders: usize,
    per: usize,
) -> (
    Tmp,
    neutronsync::config::Config,
    std::rc::Rc<std::cell::RefCell<Store>>,
) {
    let t = Tmp::new(tag);
    let mut c = cfg(t.path());
    c.propagate_deletes = true;
    c.local_delete = neutronsync::config::LocalDelete::Remove;
    let (fake, store) = FakeRemote::new();
    for d in 0..folders {
        for i in 0..per {
            write(
                &t.path().join(format!("local/d{d:03}/f{i:03}.txt")),
                &format!("content {d} {i}"),
            );
        }
    }
    run(&c, fake, false);
    assert_eq!(store.borrow().files.len(), folders * per);
    (t, c, store)
}

/// One run, batch or streaming, with a recorder; returns the result's errors.
fn run_with_events(
    c: &neutronsync::config::Config,
    store: &std::rc::Rc<std::cell::RefCell<Store>>,
    streaming: bool,
    resync: bool,
) -> (Vec<String>, Recorder) {
    let rec = Recorder::default();
    let (f, _s) = reuse(store);
    let log = Logger::silent();
    let mut eng = Engine::new(c, f, &log, false);
    eng.set_observer(Some(&rec), None);
    let r = if streaming {
        eng.sync_pair_streaming(&c.pairs[0]).unwrap()
    } else {
        eng.sync_pair(&c.pairs[0], resync).unwrap()
    };
    (r.errors, rec)
}

#[test]
fn a_deletion_far_bigger_than_the_allowance_is_held_back_and_says_how_to_go_on() {
    for streaming in [false, true] {
        let (t, c, store) = synced_tree("guard-big", 4, 100); // 400 files
                                                              // 300 of the 400 are deleted here: 75%, far over max(100, 20%)
        for d in 0..3 {
            std::fs::remove_dir_all(t.path().join(format!("local/d{d:03}"))).unwrap();
        }
        let (errors, rec) = run_with_events(&c, &store, streaming, false);
        assert_eq!(
            store.borrow().files.len(),
            400,
            "streaming={streaming}: the remote lost files it should have kept"
        );
        assert_eq!(
            rec.held().len(),
            1,
            "streaming={streaming}: {:?}",
            rec.held()
        );
        assert!(
            rec.held()[0].contains("remote: 300, local: 0, tracked: 400"),
            "{:?}",
            rec.held()
        );
        // It is a problem, not a quiet success, and it tells the user the way on.
        assert!(
            errors.iter().any(|e| e.contains("held back 300 deletions")
                && e.contains("--allow-mass-delete")
                && e.contains("--resync")),
            "streaming={streaming}: {errors:?}"
        );
    }
}

#[test]
fn a_held_deletion_is_not_forgotten_and_goes_ahead_when_the_user_says_so() {
    for streaming in [false, true] {
        let (t, mut c, store) = synced_tree("guard-confirm", 4, 100);
        for d in 0..3 {
            std::fs::remove_dir_all(t.path().join(format!("local/d{d:03}"))).unwrap();
        }
        // Held, and held again on the next run: nothing is decided by waiting.
        for _ in 0..2 {
            let (errors, _rec) = run_with_events(&c, &store, streaming, false);
            assert!(errors.iter().any(|e| e.contains("held back")), "{errors:?}");
            assert_eq!(store.borrow().files.len(), 400);
        }
        // `sync --allow-mass-delete`: it was meant.
        c.allow_mass_delete = true;
        let (errors, rec) = run_with_events(&c, &store, streaming, false);
        assert!(errors.is_empty(), "streaming={streaming}: {errors:?}");
        assert!(rec.held().is_empty());
        assert_eq!(
            store.borrow().files.len(),
            100,
            "streaming={streaming}: the confirmed deletion did not happen"
        );
        // ...and it is settled: the next ordinary run has nothing left to do.
        c.allow_mass_delete = false;
        let (errors, rec) = run_with_events(&c, &store, streaming, false);
        assert!(errors.is_empty() && rec.held().is_empty(), "{errors:?}");
        assert_eq!(store.borrow().files.len(), 100);
    }
}

#[test]
fn resync_gets_the_files_back_instead_of_deleting_them() {
    let (t, c, store) = synced_tree("guard-resync", 4, 100);
    for d in 0..3 {
        std::fs::remove_dir_all(t.path().join(format!("local/d{d:03}"))).unwrap();
    }
    let (errors, _rec) = run_with_events(&c, &store, false, false);
    assert!(errors.iter().any(|e| e.contains("held back")));
    // `sync --resync`: both sides are merged afresh - the missing files come down.
    let (errors, rec) = run_with_events(&c, &store, false, true);
    assert!(errors.is_empty() && rec.held().is_empty(), "{errors:?}");
    assert_eq!(store.borrow().files.len(), 400);
    assert_eq!(
        std::fs::read_to_string(t.path().join("local/d001/f042.txt")).unwrap(),
        "content 1 42"
    );
    // and the pair is healthy again
    let (errors, rec) = run_with_events(&c, &store, false, false);
    assert!(errors.is_empty() && rec.held().is_empty());
}

#[test]
fn a_deletion_within_the_allowance_goes_through_untouched() {
    for streaming in [false, true] {
        let (t, c, store) = synced_tree("guard-small", 4, 100); // allowed: max(100, 80) = 100
                                                                // 60 files, 15%: an ordinary clean-up
        for i in 0..60 {
            std::fs::remove_file(t.path().join(format!("local/d003/f{i:03}.txt"))).unwrap();
        }
        let (errors, rec) = run_with_events(&c, &store, streaming, false);
        assert!(errors.is_empty(), "streaming={streaming}: {errors:?}");
        assert!(rec.held().is_empty());
        assert_eq!(store.borrow().files.len(), 340, "streaming={streaming}");
    }
}

#[test]
fn the_allowance_is_the_larger_of_the_count_and_the_percentage() {
    // 1000 files: 20% is 200, more than the 100-file floor.
    let (t, c, store) = synced_tree("guard-percent", 10, 100);
    for i in 0..100 {
        std::fs::remove_file(t.path().join(format!("local/d009/f{i:03}.txt"))).unwrap();
    }
    for i in 0..90 {
        std::fs::remove_file(t.path().join(format!("local/d008/f{i:03}.txt"))).unwrap();
    }
    // 190 deletions: over the floor, under 20%
    let (errors, _rec) = run_with_events(&c, &store, false, false);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(store.borrow().files.len(), 810);
    // The pair now tracks 810 files, so a run may delete 162 (20%): 180 is too many
    for i in 0..100 {
        std::fs::remove_file(t.path().join(format!("local/d007/f{i:03}.txt"))).unwrap();
    }
    for i in 0..80 {
        std::fs::remove_file(t.path().join(format!("local/d006/f{i:03}.txt"))).unwrap();
    }
    let (errors, rec) = run_with_events(&c, &store, false, false);
    assert!(
        errors.iter().any(|e| e.contains("held back 180")),
        "{errors:?}"
    );
    assert_eq!(rec.held().len(), 1);
    assert_eq!(
        store.borrow().files.len(),
        810,
        "nothing of the 180 was deleted"
    );
}

#[test]
fn a_small_pair_can_never_trip_it() {
    let (t, c, store) = synced_tree("guard-tiny", 1, 50);
    // 40 of 50 deleted (80%) - but a run may always delete 100, and there are only 50
    for i in 0..40 {
        std::fs::remove_file(t.path().join(format!("local/d000/f{i:03}.txt"))).unwrap();
    }
    let (errors, _rec) = run_with_events(&c, &store, false, false);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(store.borrow().files.len(), 10);
}

#[test]
fn deletions_that_the_remote_makes_are_held_back_too() {
    // The other direction: the remote suddenly lists most of the tree as gone.
    for streaming in [false, true] {
        let (t, c, store) = synced_tree("guard-remote", 4, 100);
        {
            let mut s = store.borrow_mut();
            s.files.retain(|k, _| k.contains("/d003/"));
            s.dirs
                .retain(|d| !(d.contains("/d000") || d.contains("/d001") || d.contains("/d002")));
        }
        let (errors, rec) = run_with_events(&c, &store, streaming, false);
        let kept = walkdir::WalkDir::new(t.path().join("local"))
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .count();
        assert_eq!(
            kept, 400,
            "streaming={streaming}: local files were deleted on the strength of an emptied remote"
        );
        assert_eq!(
            rec.held().len(),
            1,
            "streaming={streaming}: {:?}",
            rec.held()
        );
        // the files would have gone from THIS computer, and the message says so
        assert!(
            rec.held()[0].contains("remote: 0, local: 300"),
            "{:?}",
            rec.held()
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("held back 300 deletions on this computer")),
            "streaming={streaming}: {errors:?}"
        );
    }
}

#[test]
fn a_walk_spends_one_allowance_not_one_per_folder() {
    // 60 folders of 10 files. 3 files are deleted from every folder - 180, over
    // the 120 allowed - and 3 new ones made in each, so a count of the files
    // still on disk (which the walk takes first) cannot tell anything is wrong.
    // Each folder alone deletes only 3; together they must not exceed 120.
    let (t, c, store) = synced_tree("guard-shared", 60, 10);
    for d in 0..60 {
        for i in 0..3 {
            std::fs::remove_file(t.path().join(format!("local/d{d:03}/f{i:03}.txt"))).unwrap();
            write(
                &t.path().join(format!("local/d{d:03}/new{i}.txt")),
                &format!("new {d} {i}"),
            );
        }
    }
    let (errors, rec) = run_with_events(&c, &store, true, false);
    let deleted = 600 + 180 - store.borrow().files.len();
    assert_eq!(
        deleted, 120,
        "the walk deleted {deleted} files, not exactly its allowance"
    );
    assert_eq!(rec.held().len(), 1, "{:?}", rec.held());
    assert!(
        errors.iter().any(|e| e.contains("held back 60 deletions")),
        "{errors:?}"
    );
    // the uploads were not held with them
    assert!(store
        .borrow()
        .files
        .contains_key("/my-files/docs/d059/new2.txt"));
}

#[test]
fn a_walk_that_finds_most_of_the_tree_gone_deletes_nothing_at_all() {
    // The same 60 folders, with 7 files gone from each: 420 files, spread so thin
    // that no folder trips anything alone. The count of files on disk says it
    // before the first deletion, not after the first 120.
    let (t, c, store) = synced_tree("guard-census", 60, 10);
    for d in 0..60 {
        for i in 0..7 {
            std::fs::remove_file(t.path().join(format!("local/d{d:03}/f{i:03}.txt"))).unwrap();
        }
    }
    let (errors, rec) = run_with_events(&c, &store, true, false);
    assert_eq!(
        store.borrow().files.len(),
        600,
        "the walk deleted files before noticing the tree was gone"
    );
    assert_eq!(rec.held().len(), 1, "{:?}", rec.held());
    assert!(errors.iter().any(|e| e.contains("held back")), "{errors:?}");
}

#[test]
fn nothing_is_held_when_deletes_are_not_propagated_anyway() {
    let (t, mut c, store) = synced_tree("guard-off", 4, 100);
    c.propagate_deletes = false;
    for d in 0..3 {
        std::fs::remove_dir_all(t.path().join(format!("local/d{d:03}"))).unwrap();
    }
    let (errors, rec) = run_with_events(&c, &store, false, false);
    assert!(errors.is_empty() && rec.held().is_empty(), "{errors:?}");
    assert_eq!(store.borrow().files.len(), 400);
}

#[test]
fn the_guard_can_be_turned_down_or_off() {
    // max_delete_percent = 100: a run can never delete more than everything.
    let (t, mut c, store) = synced_tree("guard-config", 4, 100);
    c.max_delete_files = 0;
    c.max_delete_percent = 100;
    for d in 0..3 {
        std::fs::remove_dir_all(t.path().join(format!("local/d{d:03}"))).unwrap();
    }
    let (errors, _rec) = run_with_events(&c, &store, false, false);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(store.borrow().files.len(), 100);

    // a stricter guard holds back what the default would have allowed
    let (t, mut c, store) = synced_tree("guard-config-strict", 4, 100);
    c.max_delete_files = 10;
    c.max_delete_percent = 5;
    for i in 0..60 {
        std::fs::remove_file(t.path().join(format!("local/d003/f{i:03}.txt"))).unwrap();
    }
    let (errors, _rec) = run_with_events(&c, &store, false, false);
    assert!(
        errors.iter().any(|e| e.contains("held back 60")),
        "{errors:?}"
    );
    assert_eq!(store.borrow().files.len(), 400);
}
