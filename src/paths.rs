use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use anyhow::{bail, Result};

/// Validate a session name: [a-zA-Z0-9_-], max 64 chars.
pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if name.len() > 64 {
        bail!("session name must be at most 64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("session name may only contain [a-zA-Z0-9_-]");
    }
    Ok(())
}

/// Directory for session sockets: $XDG_RUNTIME_DIR/mux/ or /tmp/mux-<uid>/
pub fn socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("mux")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/mux-{uid}"))
    }
}

/// Socket path for a named session.
pub fn socket_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{name}.sock"))
}

/// Directory for server log files: $XDG_STATE_HOME/mux/ or ~/.local/state/mux/
pub fn log_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(dir).join("mux")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/state/mux")
    } else {
        PathBuf::from("/tmp/mux-logs")
    }
}

/// Log file path for a named session.
pub fn log_path(name: &str) -> PathBuf {
    log_dir().join(format!("{name}.log"))
}

/// Log file path for a remote client session (host_session.log).
pub fn client_log_path(host: &str, session: &str) -> PathBuf {
    log_dir().join(format!("{host}_{session}.log"))
}

/// Create socket and log directories with 0o700 permissions.
pub fn ensure_dirs() -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(socket_dir())?;
    builder.create(log_dir())?;
    Ok(())
}
