//! Minimal logger: console (level-gated), an optional file log, and an optional
//! channel sink (used by the GUI to stream lines into its log pane).

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Logger {
    verbose: bool,
    quiet: bool,
    file: Option<Mutex<File>>,
    tx: Option<Sender<String>>,
    console: bool,
}

impl Logger {
    pub fn new(log_dir: &Path, verbose: bool, quiet: bool) -> Self {
        // Logs record decrypted file paths, so keep them private to the user:
        // the directory is created 0700 and the log file 0600 (modes apply when
        // this process creates them).
        let file = DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(log_dir)
            .ok()
            .and_then(|_| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(log_dir.join("sync.log"))
                    .ok()
            })
            .map(Mutex::new);
        Logger {
            verbose,
            quiet,
            file,
            tx: None,
            console: true,
        }
    }

    /// A logger that writes nowhere (used in tests).
    pub fn silent() -> Self {
        Logger {
            verbose: false,
            quiet: true,
            file: None,
            tx: None,
            console: false,
        }
    }

    /// A logger that streams lines to a channel (used by the GUI). No console.
    pub fn channel(tx: Sender<String>, verbose: bool) -> Self {
        Logger {
            verbose,
            quiet: false,
            file: None,
            tx: Some(tx),
            console: false,
        }
    }

    fn to_file(&self, level: &str, msg: &str) {
        if let Some(f) = &self.file {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Ok(mut fh) = f.lock() {
                let _ = writeln!(fh, "{ts} {level:<7} {msg}");
            }
        }
    }

    fn sink(&self, msg: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg.to_string());
        }
    }

    pub fn info(&self, msg: &str) {
        if self.console && !self.quiet {
            println!("{msg}");
        }
        self.to_file("INFO", msg);
        self.sink(msg);
    }

    pub fn debug(&self, msg: &str) {
        if self.console && self.verbose {
            println!("{msg}");
        }
        self.to_file("DEBUG", msg);
        if self.verbose {
            self.sink(msg);
        }
    }

    pub fn warn(&self, msg: &str) {
        if self.console {
            eprintln!("{msg}");
        }
        self.to_file("WARNING", msg);
        self.sink(msg);
    }

    pub fn error(&self, msg: &str) {
        if self.console {
            eprintln!("{msg}");
        }
        self.to_file("ERROR", msg);
        self.sink(msg);
    }
}
