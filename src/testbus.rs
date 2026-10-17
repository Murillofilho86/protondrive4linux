//! A private, hermetic session bus for tests of the D-Bus code.
//!
//! "Hermetic" matters: a stock `dbus-daemon --session` reads the machine's
//! service files, so a call to a name nobody on the bus owns (a notification
//! daemon, the settings portal) would *activate the real one* - spawning real
//! desktop services from a unit test, and making "nobody is listening" tests
//! pass or fail depending on what's installed. This bus is configured with no
//! service directories at all, so only what a test serves itself exists.

use std::io::BufRead;
use std::process::{Child, Command, Stdio};

pub struct PrivateBus {
    child: Child,
    pub address: String,
    config: std::path::PathBuf,
}

impl PrivateBus {
    /// `None` when there is no `dbus-daemon` to run (the test then skips).
    pub fn start() -> Option<Self> {
        let config = std::env::temp_dir().join(format!(
            "protondrive4linux-testbus-{}-{}.conf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos()
        ));
        // No <servicedir>, so nothing is activatable; anyone may own any name.
        std::fs::write(
            &config,
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        )
        .ok()?;
        let mut child = Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        let address = line.trim().to_string();
        if address.is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&config);
            return None;
        }
        Some(PrivateBus {
            child,
            address,
            config,
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.config);
    }
}
