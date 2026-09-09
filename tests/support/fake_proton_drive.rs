//! Minimal proton-drive CLI stand-in for `tests/disaster/`.
//!
//! Implements just enough of the real CLI's interface (`version`,
//! `filesystem list -j`, `upload`, `download`, `create-folder`, `trash`) for
//! the real `protondrive4linux` binary to run a full sync against it, backed
//! by a plain directory tree instead of the network. Never touches Proton.
//!
//! State lives under `$FAKE_REMOTE_ROOT` (required): a remote path like
//! `/my-files/docs/a.txt` maps 1:1 to `$FAKE_REMOTE_ROOT/my-files/docs/a.txt`.
//!
//! Two env knobs support disaster testing (`tests/disaster/main.rs`):
//! - `FAKE_CLI_SLEEP_MS`: sleep this long before doing anything, so a test can
//!   reliably send a signal to the parent while this call is in flight.
//! - `FAKE_CLI_CRASH`: if set, exit 1 immediately instead of doing the
//!   requested operation (simulates the CLI itself dying/erroring).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, UNIX_EPOCH};

fn remote_root() -> PathBuf {
    match env::var("FAKE_REMOTE_ROOT") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            eprintln!("fake-proton-drive: FAKE_REMOTE_ROOT must be set");
            std::process::exit(2);
        }
    }
}

/// Map a proton-style absolute remote path ("/my-files/docs") onto a real
/// filesystem path under the fake remote root.
fn mapped(root: &Path, remote_path: &str) -> PathBuf {
    root.join(remote_path.trim_start_matches('/'))
}

fn maybe_sleep() {
    if let Ok(ms) = env::var("FAKE_CLI_SLEEP_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}

fn maybe_crash() -> bool {
    if env::var("FAKE_CLI_CRASH").is_ok() {
        eprintln!("fake-proton-drive: simulated crash");
        return true;
    }
    false
}

/// Args after the subcommand, with any leading flags (before "--") and the
/// "--" itself stripped - this stub doesn't need to honor
/// --file-conflict-strategy etc., just accept and ignore them like the real
/// CLI would for a case it doesn't need to disambiguate.
fn positional_args(args: &[String]) -> Vec<&str> {
    match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].iter().map(String::as_str).collect(),
        None => args.iter().map(String::as_str).collect(),
    }
}

fn epoch_secs(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn cmd_list(root: &Path, args: &[String]) -> ExitCode {
    let pos = positional_args(args);
    let Some(&path) = pos.first() else {
        eprintln!("fake-proton-drive: list needs a path");
        return ExitCode::from(2);
    };
    let dir = mapped(root, path);
    if !dir.is_dir() {
        eprintln!("Error: No such file or directory");
        return ExitCode::FAILURE;
    }
    let mut out = String::from("[");
    let mut first = true;
    let mut entries: Vec<_> = match fs::read_dir(&dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !first {
            out.push(',');
        }
        first = false;
        if meta.is_dir() {
            out.push_str(&format!(
                r#"{{"name":"{}","type":"folder"}}"#,
                json_escape(&name)
            ));
        } else {
            let size = meta.len();
            let mtime = epoch_secs(entry.metadata().map(|m| m.modified().unwrap_or(UNIX_EPOCH)));
            out.push_str(&format!(
                r#"{{"name":"{}","type":"file","activeRevision":{{"claimedSize":{size},"claimedModificationTime":{mtime}}}}}"#,
                json_escape(&name)
            ));
        }
    }
    out.push(']');
    println!("{out}");
    ExitCode::SUCCESS
}

fn cmd_create_folder(root: &Path, args: &[String]) -> ExitCode {
    let pos = positional_args(args);
    let (Some(&parent), Some(&name)) = (pos.first(), pos.get(1)) else {
        eprintln!("fake-proton-drive: create-folder needs <parent> <name>");
        return ExitCode::from(2);
    };
    let dir = mapped(root, parent).join(name);
    match fs::create_dir_all(&dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_upload(root: &Path, args: &[String]) -> ExitCode {
    let pos = positional_args(args);
    let (Some(&local_path), Some(&remote_parent)) = (pos.first(), pos.get(1)) else {
        eprintln!("fake-proton-drive: upload needs <local> <remote_parent>");
        return ExitCode::from(2);
    };
    let dest_dir = mapped(root, remote_parent);
    if let Err(e) = fs::create_dir_all(&dest_dir) {
        eprintln!("Error: {e}");
        return ExitCode::FAILURE;
    }
    let name = match Path::new(local_path).file_name() {
        Some(n) => n,
        None => {
            eprintln!("fake-proton-drive: bad local path {local_path}");
            return ExitCode::from(2);
        }
    };
    match fs::copy(local_path, dest_dir.join(name)) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_download(root: &Path, args: &[String]) -> ExitCode {
    let pos = positional_args(args);
    let (Some(&remote_path), Some(&local_dest_dir)) = (pos.first(), pos.get(1)) else {
        eprintln!("fake-proton-drive: download needs <remote> <local_dest_dir>");
        return ExitCode::from(2);
    };
    let src = mapped(root, remote_path);
    if let Err(e) = fs::create_dir_all(local_dest_dir) {
        eprintln!("Error: {e}");
        return ExitCode::FAILURE;
    }
    let name = match Path::new(remote_path).file_name() {
        Some(n) => n,
        None => {
            eprintln!("fake-proton-drive: bad remote path {remote_path}");
            return ExitCode::from(2);
        }
    };
    match fs::copy(&src, Path::new(local_dest_dir).join(name)) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_trash(root: &Path, args: &[String]) -> ExitCode {
    let pos = positional_args(args);
    let Some(&path) = pos.first() else {
        eprintln!("fake-proton-drive: trash needs a path");
        return ExitCode::from(2);
    };
    let target = mapped(root, path);
    let result = if target.is_dir() {
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    maybe_sleep();
    if maybe_crash() {
        return ExitCode::FAILURE;
    }

    let args: Vec<String> = env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("fake-proton-drive: no command given");
        return ExitCode::from(2);
    };

    if cmd == "version" {
        println!("fake-cli-drive 0.0.0-test");
        return ExitCode::SUCCESS;
    }

    if cmd == "filesystem" {
        let root = remote_root();
        let rest = &args[1..];
        let Some(sub) = rest.first() else {
            eprintln!("fake-proton-drive: filesystem needs a subcommand");
            return ExitCode::from(2);
        };
        let sub_args = &rest[1..].to_vec();
        return match sub.as_str() {
            "list" => cmd_list(&root, sub_args),
            "create-folder" => cmd_create_folder(&root, sub_args),
            "upload" => cmd_upload(&root, sub_args),
            "download" => cmd_download(&root, sub_args),
            "trash" => cmd_trash(&root, sub_args),
            other => {
                eprintln!("fake-proton-drive: unsupported filesystem subcommand {other:?}");
                ExitCode::from(2)
            }
        };
    }

    eprintln!("fake-proton-drive: unsupported command {cmd:?}");
    ExitCode::from(2)
}
