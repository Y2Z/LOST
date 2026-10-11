//! `-b` background mode (ARCHITECTURE.md §10.1), Unix only.
//!
//! Starts `lost -s` again as a detached process in its own session, with its
//! output going to `server.log` in the data folder, checks that it came up,
//! and returns. systemd and Docker should run `lost -s` in the foreground
//! instead.

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::Result;

/// Log file of a background server, inside the data folder.
pub const LOG_FILE: &str = "server.log";

/// How long to watch the new server for an early exit (port taken, …).
const STARTUP_CHECK: Duration = Duration::from_millis(700);

/// Starts the server in the background and returns its PID and log path.
pub fn start(data_dir: &Path, address: &str, port: u16) -> Result<(u32, PathBuf)> {
    let log_path = data_dir.join(LOG_FILE);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("can't open {}: {e}", log_path.display()))?;

    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .args(["-s", "-a", address, "-p", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // A new session: no controlling terminal, so closing the terminal or
    // pressing Ctrl+C there doesn't stop the server.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;

    std::thread::sleep(STARTUP_CHECK);
    if let Some(status) = child.try_wait()? {
        return Err(format!(
            "the background server stopped right away ({status}); see {}",
            log_path.display()
        )
        .into());
    }
    Ok((child.id(), log_path))
}
