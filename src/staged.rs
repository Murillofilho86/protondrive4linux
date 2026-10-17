//! Staged downloads: a file never appears under its final name until it has
//! arrived whole.
//!
//! The `proton-drive` CLI writes a download straight to its destination (and,
//! with the `remove` conflict strategy this crate uses, deletes the file already
//! there first). So a download that dies half way - the disk fills up, the CLI
//! is killed at its timeout, the process or the machine goes down - left a
//! truncated file under the real name, and sometimes took the good copy with it.
//! The next run then read that truncated file as the user's own edit and
//! uploaded it over the good copy on the remote.
//!
//! Instead the CLI is pointed at a private staging folder *inside* the folder the
//! file belongs in (same filesystem, so the last step is a rename). Only when it
//! reports success is the file checked and moved into place, atomically:
//!
//! - its size must be the one the listing promised (when the listing gave one);
//! - the user must not have touched the local file since the plan was made - a
//!   download that replaces a file is only safe if that file is still the one the
//!   plan looked at;
//! - the file is `fsync`ed before the move and the folder after, so a crash
//!   cannot leave the baseline saying "synced" over a file whose bytes never
//!   reached the disk (the next run would read that as a local edit).
//!
//! Anything that goes wrong leaves the previous local file exactly as it was.
//!
//! The staging folder's name ends in `.part`, which the built-in ignore list
//! already treats as a half-finished download: the engine never scans, uploads
//! or deletes it, on either side. A staging folder left behind by a killed
//! process is removed once it is too old to belong to a live download.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::LocalSeen;

/// Every staging folder's name starts with this...
const PREFIX: &str = ".protondrive4linux-";
/// ...and ends with this, which `ignore::DEFAULT_IGNORE` already skips.
const SUFFIX: &str = ".part";

/// How long past the CLI's own timeout a staging folder must have been idle
/// before it is taken for a dead download's leftovers. A live download cannot
/// outlive the timeout (the CLI is killed there), so this is only slack for the
/// moment between the kill and the cleanup.
pub const STALE_MARGIN: Duration = Duration::from_secs(120);

/// A staging folder whose owning process is gone is swept after only this long
/// without a write, not the whole timeout: a download killed with the process
/// (a crash, a `kill -9`, a power cut and reboot) should not leave its half file
/// lying around for half an hour. The wait covers a process in another PID
/// namespace, which looks gone from here but is still writing.
const OWNER_GONE_GRACE: Duration = Duration::from_secs(60);

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Whether `name` is one of our staging folders.
pub fn is_staging_name(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

/// A name no other staging folder (this process's, another's, an earlier run's)
/// has, so concurrent downloads into one folder never share one.
fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{PREFIX}{}-{nanos}-{}{SUFFIX}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The path a new staging folder for `dest_dir` will have. Only a name: nothing
/// is created (see [`Staging::create`]).
pub fn plan_dir(dest_dir: &Path) -> PathBuf {
    dest_dir.join(unique_name())
}

/// The pid a staging folder's name records, if it is one of ours.
fn owner_pid(name: &str) -> Option<u32> {
    name.strip_prefix(PREFIX)?
        .split('-')
        .next()?
        .parse::<u32>()
        .ok()
}

/// Whether the process that made the staging folder `name` no longer exists
/// (so nothing is writing to it). A reused pid reads as alive, which is the safe
/// way to be wrong: the age limit still applies.
fn owner_gone(name: &str) -> bool {
    owner_pid(name).is_some_and(|pid| !Path::new("/proc").join(pid.to_string()).exists())
}

/// What a download is expected to deliver, and what it may replace.
#[derive(Clone, Copy, Debug)]
pub struct Expect {
    /// The size the remote listing gave, or 0 when it gave none (0 cannot be told
    /// from an empty file, so a 0 is never used to reject anything).
    pub size: u64,
    /// The modification time to stamp on the file (whole seconds).
    pub mtime: Option<i64>,
    /// What was at the destination when the plan was made.
    pub local: LocalSeen,
}

/// Why a staged file was not put in place. The previous local file is untouched
/// in every case.
#[derive(Debug)]
pub enum InstallError {
    /// The CLI said it succeeded but no file arrived (or more than one did).
    Missing(String),
    /// The file that arrived is not the size the listing promised.
    WrongSize {
        got: u64,
        want: u64,
    },
    /// The local file changed - or appeared - after the plan was made.
    LocalChanged,
    Io(io::Error),
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::Missing(what) => write!(f, "the download delivered no file ({what})"),
            InstallError::WrongSize { got, want } => write!(
                f,
                "the download is incomplete: {got} bytes arrived, {want} were expected; \
                 your local copy was not touched"
            ),
            InstallError::LocalChanged => write!(
                f,
                "the local file changed while it was being downloaded; your version was \
                 left alone and the two will be reconciled on the next run"
            ),
            InstallError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<io::Error> for InstallError {
    fn from(e: io::Error) -> Self {
        InstallError::Io(e)
    }
}

/// A private folder a download is written into. Removed (with whatever is in it)
/// when dropped, whether the download worked or not.
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    /// Create a fresh, empty staging folder inside `dest_dir`.
    pub fn create(dest_dir: &Path) -> io::Result<Staging> {
        let dir = plan_dir(dest_dir);
        fs::create_dir(&dir)?;
        Ok(Staging { dir })
    }

    /// Take over a staging folder that [`plan_dir`] named and a worker created.
    pub fn adopt(dir: &Path) -> Staging {
        Staging {
            dir: dir.to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file the CLI delivered, preferring the expected `name` and otherwise
    /// the one regular file in the folder (a CLI that normalises a name must not
    /// make a good download unusable).
    fn delivered(&self, name: &str) -> Result<PathBuf, InstallError> {
        let exact = self.dir.join(name);
        if fs::symlink_metadata(&exact).is_ok_and(|m| m.is_file()) {
            return Ok(exact);
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .collect();
        match files.len() {
            1 => Ok(files.remove(0)),
            0 => Err(InstallError::Missing(format!("nothing named {name:?}"))),
            n => Err(InstallError::Missing(format!(
                "{n} files and none named {name:?}"
            ))),
        }
    }

    /// Check the delivered file and move it to `<parent of the staging folder>/name`.
    /// See the module docs for what is checked.
    pub fn install(&self, name: &str, expect: &Expect) -> Result<(), InstallError> {
        let dest_dir = self.dir.parent().ok_or_else(|| {
            InstallError::Io(io::Error::other("a staging folder has a parent folder"))
        })?;
        let staged = self.delivered(name)?;
        let meta = fs::metadata(&staged)?;
        if expect.size > 0 && meta.len() != expect.size {
            return Err(InstallError::WrongSize {
                got: meta.len(),
                want: expect.size,
            });
        }

        // The modification time first, so the file never shows up in place with
        // the wrong one (a scan in between would read it as an edit).
        stamp_mtime(&staged, expect.mtime);
        // The bytes on disk before the baseline can say they are.
        fs::File::open(&staged)?.sync_all()?;

        let final_path = dest_dir.join(name);
        if !local_matches(&final_path, expect.local) {
            return Err(InstallError::LocalChanged);
        }
        fs::rename(&staged, &final_path)?;
        // And the rename itself. Not every filesystem can sync a folder.
        if let Ok(d) = fs::File::open(dest_dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Whole seconds since the epoch of a file's modification time.
fn mtime_secs(meta: &fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Whether `path` is still what `seen` says it was.
fn local_matches(path: &Path, seen: LocalSeen) -> bool {
    match (fs::symlink_metadata(path), seen) {
        (Err(e), LocalSeen::Absent) => e.kind() == io::ErrorKind::NotFound,
        (Ok(m), LocalSeen::File { size, mtime }) => {
            m.is_file() && m.len() == size && mtime_secs(&m) == mtime
        }
        _ => false,
    }
}

/// Set `path`'s modification time, best effort (a filesystem that refuses just
/// leaves the time the download gave it, which costs a redundant upload at worst).
fn stamp_mtime(path: &Path, mtime: Option<i64>) {
    let Some(secs) = mtime.filter(|s| *s >= 0) else {
        return;
    };
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(secs as u64));
    }
}

/// Remove `path` if it is one of our staging folders and nothing in it has been
/// written to for `older_than` - or, once its owning process is gone, for a
/// minute: the leftovers of a download whose process died. Returns whether it
/// was removed. A live download keeps writing its file (or is killed at the CLI
/// timeout, which `older_than` outlasts), so it is never taken.
pub fn sweep_if_stale(path: &Path, older_than: Duration) -> bool {
    let name = path.file_name().and_then(|n| n.to_str());
    let Some(name) = name.filter(|n| is_staging_name(n)) else {
        return false;
    };
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let older_than = if owner_gone(name) {
        older_than.min(OWNER_GONE_GRACE)
    } else {
        older_than
    };
    let newest = std::iter::once(path.to_path_buf())
        .chain(
            fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path()),
        )
        .filter_map(|p| fs::symlink_metadata(p).ok()?.modified().ok())
        .max();
    let idle = newest
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .unwrap_or(Duration::ZERO);
    if idle < older_than {
        return false;
    }
    fs::remove_dir_all(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Dir {
            let p = std::env::temp_dir().join(format!(
                "protondrive4linux-staged-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn expect(size: u64, local: LocalSeen) -> Expect {
        Expect {
            size,
            mtime: Some(1_000_000),
            local,
        }
    }

    /// A staging folder in `d` with `name` delivered into it.
    fn delivered(d: &Dir, name: &str, body: &str) -> Staging {
        let s = Staging::create(&d.0).unwrap();
        fs::write(s.dir().join(name), body).unwrap();
        s
    }

    #[test]
    fn a_complete_download_is_moved_into_place_with_its_time_and_the_staging_folder_goes() {
        let d = Dir::new("ok");
        let s = delivered(&d, "a.txt", "hello");
        let dir = s.dir().to_path_buf();
        s.install("a.txt", &expect(5, LocalSeen::Absent)).unwrap();
        assert_eq!(fs::read_to_string(d.0.join("a.txt")).unwrap(), "hello");
        let m = fs::metadata(d.0.join("a.txt")).unwrap();
        assert_eq!(
            mtime_secs(&m),
            Some(1_000_000),
            "stamped before it appeared"
        );
        drop(s);
        assert!(!dir.exists(), "the staging folder is removed");
    }

    #[test]
    fn it_replaces_the_file_the_plan_looked_at_in_one_step() {
        let d = Dir::new("replace");
        fs::write(d.0.join("a.txt"), "old").unwrap();
        let m = fs::metadata(d.0.join("a.txt")).unwrap();
        let seen = LocalSeen::File {
            size: m.len(),
            mtime: mtime_secs(&m),
        };
        let s = delivered(&d, "a.txt", "the new content");
        s.install("a.txt", &expect(15, seen)).unwrap();
        assert_eq!(
            fs::read_to_string(d.0.join("a.txt")).unwrap(),
            "the new content"
        );
    }

    #[test]
    fn a_short_download_is_refused_and_the_old_copy_is_untouched() {
        let d = Dir::new("short");
        fs::write(d.0.join("a.txt"), "v1").unwrap();
        let m = fs::metadata(d.0.join("a.txt")).unwrap();
        let seen = LocalSeen::File {
            size: m.len(),
            mtime: mtime_secs(&m),
        };
        let s = delivered(&d, "a.txt", "0123"); // 4 bytes arrived, 10 were promised
        let err = s.install("a.txt", &expect(10, seen)).unwrap_err();
        assert!(
            matches!(err, InstallError::WrongSize { got: 4, want: 10 }),
            "{err}"
        );
        assert_eq!(fs::read_to_string(d.0.join("a.txt")).unwrap(), "v1");
    }

    #[test]
    fn a_listing_that_gave_no_size_never_rejects_a_download() {
        // 0 is also what an empty file lists as, so it can't be used to judge.
        let d = Dir::new("nosize");
        let s = delivered(&d, "a.txt", "whatever arrived");
        s.install("a.txt", &expect(0, LocalSeen::Absent)).unwrap();
        assert!(d.0.join("a.txt").exists());
    }

    #[test]
    fn an_edit_made_since_the_plan_is_never_overwritten() {
        let d = Dir::new("changed");
        fs::write(d.0.join("a.txt"), "v1").unwrap();
        let m = fs::metadata(d.0.join("a.txt")).unwrap();
        let seen = LocalSeen::File {
            size: m.len(),
            mtime: mtime_secs(&m),
        };
        // the user saves something longer while the download is running
        fs::write(d.0.join("a.txt"), "my edit, made meanwhile").unwrap();
        let s = delivered(&d, "a.txt", "remote v2");
        let err = s.install("a.txt", &expect(9, seen)).unwrap_err();
        assert!(matches!(err, InstallError::LocalChanged), "{err}");
        assert_eq!(
            fs::read_to_string(d.0.join("a.txt")).unwrap(),
            "my edit, made meanwhile"
        );
    }

    #[test]
    fn a_file_that_appeared_since_the_plan_is_never_overwritten() {
        let d = Dir::new("appeared");
        fs::write(d.0.join("a.txt"), "created meanwhile").unwrap();
        let s = delivered(&d, "a.txt", "remote");
        let err = s
            .install("a.txt", &expect(6, LocalSeen::Absent))
            .unwrap_err();
        assert!(matches!(err, InstallError::LocalChanged), "{err}");
        assert_eq!(
            fs::read_to_string(d.0.join("a.txt")).unwrap(),
            "created meanwhile"
        );
    }

    #[test]
    fn a_file_that_vanished_since_the_plan_is_not_brought_back_blindly() {
        let d = Dir::new("vanished");
        let s = delivered(&d, "a.txt", "remote");
        let seen = LocalSeen::File {
            size: 3,
            mtime: Some(5),
        };
        let err = s.install("a.txt", &expect(6, seen)).unwrap_err();
        assert!(matches!(err, InstallError::LocalChanged), "{err}");
        assert!(!d.0.join("a.txt").exists());
    }

    #[test]
    fn nothing_delivered_is_an_error_and_a_lone_file_under_another_name_is_used() {
        let d = Dir::new("names");
        let empty = Staging::create(&d.0).unwrap();
        let err = empty
            .install("a.txt", &expect(0, LocalSeen::Absent))
            .unwrap_err();
        assert!(matches!(err, InstallError::Missing(_)), "{err}");

        // The CLI normalised the name: one file, so there is no doubt which.
        let odd = delivered(&d, "a.txt.normalised", "body");
        odd.install("a.txt", &expect(4, LocalSeen::Absent)).unwrap();
        assert_eq!(fs::read_to_string(d.0.join("a.txt")).unwrap(), "body");

        // Two files and neither is the one asked for: no guessing.
        let two = Staging::create(&d.0).unwrap();
        fs::write(two.dir().join("x"), "1").unwrap();
        fs::write(two.dir().join("y"), "2").unwrap();
        let err = two
            .install("b.txt", &expect(0, LocalSeen::Absent))
            .unwrap_err();
        assert!(matches!(err, InstallError::Missing(_)), "{err}");
        assert!(!d.0.join("b.txt").exists());
    }

    #[test]
    fn staging_folders_are_unique_and_ignored_by_the_engine() {
        let d = Dir::new("names2");
        let a = Staging::create(&d.0).unwrap();
        let b = Staging::create(&d.0).unwrap();
        assert_ne!(a.dir(), b.dir());
        for s in [&a, &b] {
            let name = s.dir().file_name().unwrap().to_str().unwrap();
            assert!(is_staging_name(name), "{name}");
            // The whole point of the `.part` suffix: never scanned or synced.
            assert!(
                crate::ignore::is_ignored_junk(name),
                "{name} would be synced"
            );
            assert!(crate::ignore::is_ignored_junk(&format!("sub/{name}/file")));
        }
        assert!(!is_staging_name("report.part"), "only our own folders");
        assert!(!is_staging_name(".protondrive4linux-notes.txt"));
    }

    #[test]
    fn only_a_stale_staging_folder_is_swept() {
        let d = Dir::new("sweep");
        let s = delivered(&d, "partial.bin", "half a file");
        let dir = s.dir().to_path_buf();
        // Fresh: it may well be a live download's.
        assert!(!sweep_if_stale(&dir, Duration::from_secs(3600)));
        assert!(dir.exists());
        // Idle for longer than the limit (here: any age is too old).
        assert!(sweep_if_stale(&dir, Duration::ZERO));
        assert!(!dir.exists());
        std::mem::forget(s); // already gone; nothing for Drop to do
    }

    #[test]
    fn a_folder_whose_owner_is_gone_is_swept_after_a_minute_not_the_whole_timeout() {
        let d = Dir::new("owner");
        // A pid no process has (above the kernel's maximum).
        let dead = d.0.join(".protondrive4linux-4294967290-1-1.part");
        fs::create_dir(&dead).unwrap();
        fs::write(dead.join("movie.mkv"), "half a movie").unwrap();
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        for p in [dead.join("movie.mkv"), dead.clone()] {
            fs::File::open(&p).unwrap().set_modified(hour_ago).unwrap();
        }
        // The timeout is far longer than an hour, but its process is dead.
        assert!(owner_gone(dead.file_name().unwrap().to_str().unwrap()));
        assert!(sweep_if_stale(&dead, Duration::from_secs(48 * 3600)));
        assert!(!dead.exists());

        // The same, just written to: it might be in another PID namespace.
        let fresh = d.0.join(".protondrive4linux-4294967291-1-1.part");
        fs::create_dir(&fresh).unwrap();
        assert!(!sweep_if_stale(&fresh, Duration::from_secs(48 * 3600)));
        assert!(fresh.exists());

        // And one whose owner is alive (this process) waits for the full limit,
        // however old: the limit, not the pid, decides.
        let mine = d.0.join(format!(
            ".protondrive4linux-{}-1-1.part",
            std::process::id()
        ));
        fs::create_dir(&mine).unwrap();
        fs::File::open(&mine)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
        assert!(!owner_gone(mine.file_name().unwrap().to_str().unwrap()));
        assert!(!sweep_if_stale(&mine, Duration::from_secs(48 * 3600)));
        assert!(sweep_if_stale(&mine, Duration::from_secs(60)));
    }

    #[test]
    fn the_sweep_never_touches_anything_that_is_not_ours() {
        let d = Dir::new("sweep2");
        let theirs = d.0.join("backup.part");
        fs::create_dir(&theirs).unwrap();
        fs::write(theirs.join("x"), "x").unwrap();
        let file = d.0.join(".protondrive4linux-1-2-3.part");
        fs::write(&file, "a file, not a folder").unwrap();
        assert!(!sweep_if_stale(&theirs, Duration::ZERO));
        assert!(!sweep_if_stale(&file, Duration::ZERO));
        assert!(theirs.exists() && file.exists());
    }
}
