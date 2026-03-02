use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use tracing::debug;

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
/// Verifies ownership and permissions to prevent pre-creation attacks.
pub fn ensure_dirs() -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);

    let sock_dir = socket_dir();
    builder.create(&sock_dir)?;
    verify_dir_security(&sock_dir)?;

    let log = log_dir();
    builder.create(&log)?;
    verify_dir_security(&log)?;

    Ok(())
}

/// Stable symlink path for SSH agent forwarding for a named session.
pub fn agent_link_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{name}.agent.sock"))
}

/// Atomically update the stable agent symlink to point to the current
/// `SSH_AUTH_SOCK`. Returns `Ok(true)` if updated, `Ok(false)` if no agent
/// is available (env var unset or target missing). Skips self-referencing
/// symlinks to avoid loops when running mux inside mux.
pub fn update_agent_link(name: &str) -> Result<bool> {
    let real_sock = match std::env::var("SSH_AUTH_SOCK") {
        Ok(val) if !val.is_empty() => PathBuf::from(val),
        _ => return Ok(false),
    };

    // Skip if the current SSH_AUTH_SOCK already points to our own stable
    // symlink (nested mux).
    let link = agent_link_path(name);
    if real_sock == link {
        debug!("skipping self-referencing agent symlink for session {name}");
        return Ok(false);
    }

    // Verify the real socket exists
    if !real_sock.exists() {
        debug!(?real_sock, "SSH_AUTH_SOCK target does not exist, skipping");
        return Ok(false);
    }

    // Atomic update: create temp symlink then rename over the stable path
    let pid = std::process::id();
    let tmp = socket_dir().join(format!("{name}.agent.sock.tmp.{pid}"));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&real_sock, &tmp)
        .with_context(|| format!("failed to create temp agent symlink at {}", tmp.display()))?;
    fs::rename(&tmp, &link)
        .with_context(|| format!("failed to rename agent symlink to {}", link.display()))?;

    debug!(?link, ?real_sock, "updated agent symlink for session {name}");
    Ok(true)
}

/// Best-effort removal of the stable agent symlink for a session.
pub fn remove_agent_link(name: &str) {
    let link = agent_link_path(name);
    let _ = fs::remove_file(&link);
}

/// Verify that a directory is owned by the current user and has mode 0o700.
/// Prevents attacks where an adversary pre-creates the directory with
/// permissive permissions (e.g. in /tmp).
fn verify_dir_security(path: &PathBuf) -> Result<()> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;

    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        bail!(
            "directory {} is owned by uid {} but expected uid {} — \
             possible symlink or pre-creation attack",
            path.display(),
            meta.uid(),
            uid
        );
    }

    let mode = meta.mode() & 0o777;
    if mode != 0o700 {
        // Attempt to fix permissions before failing
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!(
                "directory {} has mode {:04o} (expected 0700) and could not be fixed",
                path.display(),
                mode
            ))?;
    }

    Ok(())
}
