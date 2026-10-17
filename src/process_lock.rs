//! Single-instance locks for the processes this app starts - the watcher, the
//! tray daemon, the GUI window - and a way to ask who holds one.
//!
//! A lock is a file in the state folder, and two things about it do different
//! jobs:
//!
//! * **Holding it** is an exclusive file lock (`flock`) on the open file, kept
//!   for as long as the process lives. The kernel drops it the instant the
//!   process ends, however it ended - a clean exit, SIGTERM, SIGKILL, a crash,
//!   a power cut - so a lock can't go stale, and two processes starting at the
//!   same instant can't both win it.
//! * **Describing it** is the record written inside: the holder's pid, the time
//!   it started and the id of this boot. Anything that only wants to *look* (the
//!   window asking "is the tray alive?", the tray's Quit, `doctor`) reads the
//!   record and checks it against `/proc`. Looking never takes the lock, so it
//!   can't make a daemon that is starting think the lock is taken.
//!
//! This replaces a bare PID file checked with `/proc/<pid>` existing, which
//! failed in two ways. A pid outlives its process in the file (SIGTERM, a crash
//! and every reboot leave it behind), and once the number is handed to some
//! other program, "that pid exists" was read as "the watcher is running": a new
//! watcher then refused to start ("already running (pid N)") and the tray
//! daemon was never launched. Worse, the tray's Quit sent SIGTERM to whatever
//! the pid in `window.lock` had become. The start time and the boot id make the
//! record name one process instance, not a number that gets reused.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The executables that count as ours when all a lock records is a pid - what
/// versions of this app from before this module wrote.
const OUR_PROGRAMS: [&str; 2] = ["protondrive4linux", "protondrive4linux-gui"];

/// What a lock file says right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    /// No record: never taken, or its holder let go of it.
    Free,
    /// A live process holds it.
    Held { pid: u32 },
    /// A record whose process is gone (ended without letting go: SIGKILL, a
    /// crash, a reboot), or whose pid now belongs to something else. Harmless:
    /// the next holder simply replaces it.
    Stale { pid: u32 },
}

/// Why a lock could not be taken.
#[derive(Debug)]
pub enum AcquireError {
    /// Somebody else holds it (`pid` when the record says who).
    Held {
        pid: Option<u32>,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::Held { pid: Some(pid) } => write!(f, "already held (pid {pid})"),
            AcquireError::Held { pid: None } => write!(f, "already held"),
            AcquireError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AcquireError {}

/// A lock held by this process. Dropping it lets go.
#[derive(Debug)]
pub struct ProcessLock {
    file: File,
    path: PathBuf,
}

impl ProcessLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        // Say nothing is held before the lock itself goes (closing `file` does
        // that), so a look in between sees an empty record rather than a
        // live-looking one nobody holds. The file stays: removing it would let
        // one process lock the old file while another creates a new one at the
        // same path, and both would think they held it.
        let _ = self.file.set_len(0);
    }
}

/// Take the lock at `path`, creating its folder (private to the user) and the
/// file as needed.
pub fn acquire(path: &Path) -> Result<ProcessLock, AcquireError> {
    if let Some(dir) = path.parent() {
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(AcquireError::Io)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(AcquireError::Held { pid: holder(path) }),
        // A filesystem that can't take file locks (some network and FUSE mounts):
        // fall back on the record alone, which is all a PID file ever was.
        Err(TryLockError::Error(_)) => {}
    }
    // We hold the file lock, so nobody else that locks holds it. One that
    // doesn't may still be running and named in the record - an older version of
    // this app, or a holder on a filesystem without locks - so don't take over
    // from it.
    if let Some(pid) = holder(path) {
        return Err(AcquireError::Held { pid: Some(pid) });
    }
    let record = record_for(std::process::id()).ok_or_else(|| {
        AcquireError::Io(std::io::Error::other(
            "cannot read this process's identity from /proc",
        ))
    })?;
    file.set_len(0).map_err(AcquireError::Io)?;
    (&file)
        .write_all(record.as_bytes())
        .map_err(AcquireError::Io)?;
    Ok(ProcessLock {
        file,
        path: path.to_path_buf(),
    })
}

/// The pid of the live process that holds the lock at `path`, if there is one.
pub fn holder(path: &Path) -> Option<u32> {
    match inspect(path) {
        LockState::Held { pid } => Some(pid),
        _ => None,
    }
}

/// Read the lock at `path` and judge its record against `/proc`. Never takes
/// the lock.
pub fn inspect(path: &Path) -> LockState {
    let Ok(text) = fs::read_to_string(path) else {
        return LockState::Free;
    };
    let fields: Vec<&str> = text.split_whitespace().collect();
    match fields.as_slice() {
        [] => LockState::Free,
        [pid, _start, _boot] => {
            let Ok(pid) = pid.parse::<u32>() else {
                return LockState::Free;
            };
            if record_for(pid).as_deref() == Some(fields.join(" ").as_str()) {
                LockState::Held { pid }
            } else {
                LockState::Stale { pid }
            }
        }
        // What versions from before this module wrote: a bare pid. With no start
        // time to check, the best that can be asked is whether that pid is one of
        // our programs - never whether some process merely has that number.
        [pid] => {
            let Ok(pid) = pid.parse::<u32>() else {
                return LockState::Free;
            };
            if legacy_holder_alive(pid) {
                LockState::Held { pid }
            } else {
                LockState::Stale { pid }
            }
        }
        _ => LockState::Free,
    }
}

/// The record that identifies process `pid`: its pid, when it started (in clock
/// ticks since boot) and this boot's id. `None` if there is no such live
/// process, or `/proc` can't say.
pub fn record_for(pid: u32) -> Option<String> {
    let (state, start) = proc_stat(pid)?;
    if !is_alive_state(state) {
        return None;
    }
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot = boot.trim();
    (!boot.is_empty()).then(|| format!("{pid} {start} {boot}"))
}

/// A process's state letter and start time, from `/proc/<pid>/stat`.
fn proc_stat(pid: u32) -> Option<(char, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may itself hold spaces and
    // parentheses, so the fields that follow start after the LAST ')'. They run
    // from field 3 (the state) on; the start time is field 22.
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let start = fields.nth(18)?;
    Some((state, start.to_string()))
}

/// A zombie has exited and is only waiting to be reaped: it holds nothing.
fn is_alive_state(state: char) -> bool {
    !matches!(state, 'Z' | 'X' | 'x')
}

fn legacy_holder_alive(pid: u32) -> bool {
    let Some((state, _)) = proc_stat(pid) else {
        return false;
    };
    if !is_alive_state(state) {
        return false;
    }
    // Another user's process can't be read, and isn't ours either.
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|name| is_our_program(&name))
}

/// Whether an executable's file name is one of this app's. A program replaced
/// while running (an upgrade) reads as `name (deleted)`.
fn is_our_program(name: &str) -> bool {
    OUR_PROGRAMS.contains(&name.trim_end_matches(" (deleted)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    struct Tmp(PathBuf);

    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let dir = std::env::temp_dir().join(format!(
                "protondrive4linux-lock-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }

        fn lock(&self) -> PathBuf {
            self.0.join("x.lock")
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Take the lock, waiting a moment if a thread forking elsewhere in this
    /// test process briefly holds a copy of a lock file we just let go of (it
    /// keeps the file lock until it execs, a few milliseconds at most).
    fn acquire_eventually(path: &Path) -> ProcessLock {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match acquire(path) {
                Ok(l) => return l,
                Err(e) if Instant::now() >= deadline => panic!("never became free: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn me() -> u32 {
        std::process::id()
    }

    fn my_record() -> String {
        record_for(me()).expect("this process is readable")
    }

    #[test]
    fn a_lock_describes_its_holder_by_pid_start_time_and_boot() {
        let tmp = Tmp::new("describe");
        let lock = acquire(&tmp.lock()).unwrap();
        let text = fs::read_to_string(tmp.lock()).unwrap();
        assert_eq!(text, my_record());
        let fields: Vec<_> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 3, "{text}");
        assert_eq!(fields[0], me().to_string());
        assert_eq!(inspect(&tmp.lock()), LockState::Held { pid: me() });
        assert_eq!(holder(&tmp.lock()), Some(me()));
        drop(lock);
    }

    #[test]
    fn the_lock_file_is_private_and_so_is_its_folder() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = Tmp::new("modes");
        let path = tmp.0.join("fresh").join("x.lock");
        let _lock = acquire(&path).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn a_second_taker_is_refused_and_told_who_holds_it() {
        let tmp = Tmp::new("second");
        let _first = acquire(&tmp.lock()).unwrap();
        match acquire(&tmp.lock()) {
            Err(AcquireError::Held { pid }) => assert_eq!(pid, Some(me())),
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[test]
    fn letting_go_empties_the_record_and_frees_the_lock() {
        let tmp = Tmp::new("release");
        drop(acquire(&tmp.lock()).unwrap());
        assert_eq!(fs::read_to_string(tmp.lock()).unwrap(), "");
        assert_eq!(inspect(&tmp.lock()), LockState::Free);
        assert_eq!(holder(&tmp.lock()), None);
        let _again = acquire_eventually(&tmp.lock());
    }

    #[test]
    fn a_missing_empty_or_garbled_record_is_free() {
        let tmp = Tmp::new("free");
        assert_eq!(inspect(&tmp.lock()), LockState::Free, "no file");
        fs::write(tmp.lock(), "").unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Free, "empty");
        fs::write(tmp.lock(), "not a pid").unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Free, "garbled");
        fs::write(tmp.lock(), "1 2 3 4 5").unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Free, "too many fields");
    }

    #[test]
    fn a_pid_that_was_reused_is_not_the_holder() {
        let tmp = Tmp::new("reuse");
        // The live process with this very pid, but not the one that wrote the
        // record: a different start time.
        let record = my_record();
        let fields: Vec<_> = record.split_whitespace().collect();
        fs::write(tmp.lock(), format!("{} 1 {}", fields[0], fields[2])).unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Stale { pid: me() });
        assert_eq!(holder(&tmp.lock()), None);
    }

    #[test]
    fn a_record_from_an_earlier_boot_is_stale_even_if_the_pid_and_time_match() {
        let tmp = Tmp::new("boot");
        let record = my_record();
        let fields: Vec<_> = record.split_whitespace().collect();
        fs::write(
            tmp.lock(),
            format!(
                "{} {} 00000000-0000-0000-0000-000000000000",
                fields[0], fields[1]
            ),
        )
        .unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Stale { pid: me() });
    }

    #[test]
    fn a_process_that_is_gone_is_stale() {
        let tmp = Tmp::new("gone");
        // Beyond pid_max (4194304), so no such process.
        fs::write(tmp.lock(), "4999999 12345 some-boot").unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Stale { pid: 4999999 });
        fs::write(tmp.lock(), "4999999").unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Stale { pid: 4999999 });
    }

    #[test]
    fn a_bare_pid_from_an_older_version_counts_only_if_that_pid_is_one_of_ours() {
        let tmp = Tmp::new("legacy");
        // This test binary is alive but isn't protondrive4linux: the same shape as
        // a stale lock whose pid was reused by another program.
        fs::write(tmp.lock(), me().to_string()).unwrap();
        assert_eq!(inspect(&tmp.lock()), LockState::Stale { pid: me() });

        assert!(is_our_program("protondrive4linux"));
        assert!(is_our_program("protondrive4linux-gui"));
        assert!(
            is_our_program("protondrive4linux-gui (deleted)"),
            "upgraded while running"
        );
        assert!(!is_our_program("firefox"));
        assert!(!is_our_program("protondrive4linux-watch-test"));
        assert!(!is_our_program("not-protondrive4linux"));
    }

    #[test]
    fn a_zombie_holds_nothing() {
        let tmp = Tmp::new("zombie");
        // Exits at once and is left unreaped, so it stays as a zombie.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while proc_stat(pid).map(|(s, _)| s) != Some('Z') {
            assert!(Instant::now() < deadline, "the child never became a zombie");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (_, start) = proc_stat(pid).unwrap();
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        fs::write(tmp.lock(), format!("{pid} {start} {}", boot.trim())).unwrap();
        assert_eq!(record_for(pid), None);
        assert_eq!(inspect(&tmp.lock()), LockState::Stale { pid });
        child.wait().unwrap();
    }

    #[test]
    fn a_stale_record_does_not_stop_a_new_holder() {
        let tmp = Tmp::new("takeover");
        fs::write(tmp.lock(), format!("{} 1 not-this-boot", me())).unwrap();
        let _lock = acquire(&tmp.lock()).expect("a stale lock is not a held one");
        assert_eq!(fs::read_to_string(tmp.lock()).unwrap(), my_record());
        // A bare pid that was reused by an unrelated program, the old failure.
        let tmp = Tmp::new("takeover-legacy");
        fs::write(tmp.lock(), me().to_string()).unwrap();
        assert!(acquire(&tmp.lock()).is_ok());
    }

    #[test]
    fn a_live_holder_that_does_not_use_file_locks_is_respected() {
        let tmp = Tmp::new("nolock");
        // The shape an older version, or a holder without file locks, leaves: a
        // current identity record in a file nobody has locked.
        fs::write(tmp.lock(), my_record()).unwrap();
        match acquire(&tmp.lock()) {
            Err(AcquireError::Held { pid }) => assert_eq!(pid, Some(me())),
            other => panic!("expected Held, got {other:?}"),
        }
        // And the record is left alone.
        assert_eq!(fs::read_to_string(tmp.lock()).unwrap(), my_record());
    }

    #[test]
    fn of_many_that_start_at_once_exactly_one_gets_the_lock() {
        let tmp = Tmp::new("race");
        // A stale record is the case that used to race: everyone saw it stale and
        // overwrote it.
        fs::write(tmp.lock(), format!("{} 1 not-this-boot", me())).unwrap();
        for round in 0..50 {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let (barrier, path) = (barrier.clone(), tmp.lock());
                    std::thread::spawn(move || {
                        barrier.wait();
                        acquire(&path)
                    })
                })
                .collect();
            let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let winners = results.iter().filter(|r| r.is_ok()).count();
            assert_eq!(winners, 1, "round {round}: {winners} threads got the lock");
            drop(results);
            // The next round starts from a stale record again.
            fs::write(tmp.lock(), format!("{} 1 not-this-boot", me())).unwrap();
            // Let any copy a forking thread elsewhere holds go before the next round.
            drop(acquire_eventually(&tmp.lock()));
            fs::write(tmp.lock(), format!("{} 1 not-this-boot", me())).unwrap();
        }
    }
}
