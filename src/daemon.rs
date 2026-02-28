use std::fs::{File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::config::Config;
use crate::paths;

/// Acquire an advisory lock for a session to prevent TOCTOU races.
fn lock_session(name: &str) -> Result<File> {
    paths::ensure_dirs()?;
    let lock_path = paths::socket_dir().join(format!("{name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .context("failed to open lock file")?;
    // Block until we acquire an exclusive lock
    use std::os::unix::io::AsRawFd;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        bail!(
            "flock failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(file)
}

/// Ensure a server is running for the given session name.
/// If one is already alive, returns Ok immediately.
/// If the socket is stale, removes it and starts a new server.
pub fn ensure_server(name: &str, program: &[String]) -> Result<()> {
    // Hold an advisory lock to prevent two clients from racing
    let _lock = lock_session(name)?;

    let sock = paths::socket_path(name);

    // Check if an existing server is alive
    if sock.exists() {
        if try_connect_sync(&sock) {
            return Ok(());
        }
        // Stale socket — remove it
        let _ = std::fs::remove_file(&sock);
    }

    // Open log file, truncating any previous content
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(paths::log_path(name))
        .context("failed to open log file")?;

    let log_file_err = log_file.try_clone().context("failed to clone log file")?;

    // Query the current terminal size so the PTY starts at the right dimensions
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // Re-exec ourselves as: mux server --session <name>
    let exe = std::env::current_exe().context("failed to get current exe")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args([
        "server",
        "--session", name,
        "--rows", &rows.to_string(),
        "--cols", &cols.to_string(),
    ]);
    if !program.is_empty() {
        cmd.arg("--");
        cmd.args(program);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log_file));
    cmd.stderr(Stdio::from(log_file_err));

    // Detach from terminal via setsid
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    cmd.spawn().context("failed to spawn server process")?;

    // Poll for socket readiness
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if sock.exists() && try_connect_sync(&sock) {
            info!(name, "server is ready");
            return Ok(());
        }
    }

    bail!("server failed to start within 5 seconds (check {})", paths::log_path(name).display());
}

/// Try a synchronous connect to verify a server is alive.
fn try_connect_sync(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Run the server in the current process (called by `mux server --session <name>`).
pub fn run_server(name: &str, rows: u16, cols: u16, program: &[String]) -> Result<()> {
    let sock = paths::socket_path(name);

    let config = Config::load();
    let resolved_program = config.resolve_program(program);
    let extra_env = config.resolve_env(name);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(async {
        info!(?sock, rows, cols, "starting server for session {name}");
        let mut session = persistterm_server::session::Session::new(
            name, rows, cols, &sock, &resolved_program, &extra_env,
        )?;
        session.run().await
    })
}
