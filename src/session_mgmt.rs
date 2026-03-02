use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};

use crate::daemon;
use crate::paths;

/// List all active sessions.
pub fn list_sessions() -> Result<()> {
    let dir = paths::socket_dir();
    if !dir.exists() {
        return Ok(());
    }

    let mut found = false;
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sock") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("???");

        // Skip agent symlinks (e.g. "mysession.agent.sock")
        if name.ends_with(".agent") {
            continue;
        }

        if UnixStream::connect(&path).is_ok() {
            println!("{name}");
            found = true;
        } else {
            // Stale socket — clean up
            let _ = std::fs::remove_file(&path);
        }
    }

    if !found {
        println!("no active sessions");
    }

    Ok(())
}

/// Kill a named session.
pub fn kill_session(name: &str) -> Result<()> {
    paths::validate_session_name(name)?;
    let sock = paths::socket_path(name);

    if !sock.exists() {
        anyhow::bail!("session '{name}' not found");
    }

    // Connect and get the server PID via SO_PEERCRED
    let stream = UnixStream::connect(&sock)
        .with_context(|| format!("session '{name}' is not running"))?;

    let pid = get_peer_pid(&stream)?;
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // Give the server time to shut down
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Clean up socket, lock, and agent symlink
    let _ = std::fs::remove_file(&sock);
    let lock_path = paths::socket_dir().join(format!("{name}.lock"));
    let _ = std::fs::remove_file(lock_path);
    paths::remove_agent_link(name);

    eprintln!("killed session '{name}'");
    Ok(())
}

/// Attach to a named session (starting the server if needed).
pub fn attach(name: &str, program: &[String]) -> Result<()> {
    paths::validate_session_name(name)?;

    // Update SSH agent symlink so the PTY (or an existing session) can
    // reach the caller's current SSH agent.
    let _ = paths::update_agent_link(name);

    daemon::ensure_server(name, program, None)?;

    let sock = paths::socket_path(name);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(persistterm_client::run(&sock, name))
}

/// Attach to a remote session over SSH.
pub fn attach_remote(host: &str, session: &str) -> Result<()> {
    let config = crate::config::Config::load();
    let ssh_options = persistterm_client::ssh::SshOptions {
        compression: config.ssh.compression,
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(persistterm_client::run_remote(host, session, &ssh_options))
}

/// Get the PID of the peer process via socket credentials.
#[cfg(target_os = "linux")]
fn get_peer_pid(stream: &UnixStream) -> Result<i32> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };

    if ret != 0 {
        anyhow::bail!(
            "getsockopt SO_PEERCRED failed: {}",
            std::io::Error::last_os_error()
        );
    }

    Ok(cred.pid)
}

/// Get the PID of the peer process via LOCAL_PEERPID.
#[cfg(target_os = "macos")]
fn get_peer_pid(stream: &UnixStream) -> Result<i32> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };

    if ret != 0 {
        anyhow::bail!(
            "getsockopt LOCAL_PEERPID failed: {}",
            std::io::Error::last_os_error()
        );
    }

    Ok(pid)
}
