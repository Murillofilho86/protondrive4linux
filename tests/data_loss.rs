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
//!   pair pointed at another folder).
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
