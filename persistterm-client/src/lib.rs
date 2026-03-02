pub mod detach;
pub mod input;
pub mod net;
pub mod render;
pub mod ssh;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::{error, info};

use persistterm_proto::codec::async_io::{read_frame_async, write_frame_async};
use persistterm_proto::{C2S, ClientCapabilities, S2C};

const MAX_RECONNECT_RETRIES: u32 = 30;

/// Reason the client session ended.
enum ExitReason {
    Detached,
    Kicked(String),
    Killed,
    ServerDisconnected,
    SessionEnded,
}

/// Result of processing a single server message.
enum MsgAction {
    Continue,
    Exit(ExitReason),
}

/// Process a single S2C message, writing any terminal output to `stdout`.
/// Returns `MsgAction::Exit` if the session should end.
fn handle_server_msg(
    msg: S2C,
    stdout: &mut std::io::Stdout,
    local_kkp_active: &mut bool,
    local_dec_modes: &mut std::collections::HashSet<u16>,
    last_pong: &mut Instant,
) -> std::io::Result<MsgAction> {
    match msg {
        S2C::Snapshot(snapshot) => {
            render::render_snapshot(stdout, &snapshot)?;
        }
        S2C::ScreenData { data } => {
            stdout.write_all(b"\x1b[?2026h")?;
            stdout.write_all(&data)?;
            stdout.write_all(b"\x1b[?2026l")?;
        }
        S2C::ScreenDiff { data } => {
            stdout.write_all(&data)?;
        }
        S2C::SetKkpMode { flags } => {
            if flags > 0 {
                if *local_kkp_active {
                    write!(stdout, "\x1b[<u")?;
                }
                write!(stdout, "\x1b[>{flags}u")?;
                *local_kkp_active = true;
            } else if *local_kkp_active {
                write!(stdout, "\x1b[<u")?;
                *local_kkp_active = false;
            }
        }
        S2C::SetDecMode { mode, enabled } => {
            if enabled {
                write!(stdout, "\x1b[?{mode}h")?;
                local_dec_modes.insert(mode);
            } else {
                write!(stdout, "\x1b[?{mode}l")?;
                local_dec_modes.remove(&mode);
            }
        }
        S2C::Clipboard { params, data } => {
            write!(stdout, "\x1b]52;{params};{data}\x07")?;
        }
        S2C::Kicked { reason } => {
            info!("kicked: {reason}");
            return Ok(MsgAction::Exit(ExitReason::Kicked(reason)));
        }
        S2C::SessionEnded => {
            info!("session ended");
            return Ok(MsgAction::Exit(ExitReason::SessionEnded));
        }
        S2C::Pong { .. } => {
            *last_pong = Instant::now();
        }
        _ => {}
    }
    Ok(MsgAction::Continue)
}

/// Clean up KKP and DEC modes on the local terminal.
fn cleanup_terminal_modes(
    stdout: &mut std::io::Stdout,
    local_kkp_active: &mut bool,
    local_dec_modes: &mut std::collections::HashSet<u16>,
) {
    if *local_kkp_active {
        let _ = write!(stdout, "\x1b[<u");
        let _ = stdout.flush();
        *local_kkp_active = false;
    }
    for mode in local_dec_modes.iter() {
        let _ = write!(stdout, "\x1b[?{mode}l");
    }
    if !local_dec_modes.is_empty() {
        let _ = stdout.flush();
        local_dec_modes.clear();
    }
}

/// Run a single connection session. Returns the exit reason and optionally
/// the server message receiver (kept alive when kicked, for auto-reclaim).
async fn run_session(
    reader: Box<dyn AsyncRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    session_name: &str,
    display_host: &str,
    is_remote: bool,
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<(ExitReason, Option<mpsc::Receiver<S2C>>)> {
    // Get terminal size
    let (cols, rows) = crossterm::terminal::size()?;

    let mut reader = reader;
    let mut writer = writer;

    // Send Hello (includes terminal dimensions so server can resize before first render)
    let hello = C2S::Hello {
        caps: ClientCapabilities {
            supports_kkp: true,
            supports_truecolor: true,
            term: std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
            width: cols,
            height: rows,
        },
    };
    write_frame_async(&mut writer, &hello).await?;

    // Read Welcome
    let welcome: S2C = read_frame_async(&mut reader).await?;
    match &welcome {
        S2C::Welcome { session_id } => {
            info!(session_id, "connected to server");
        }
        _ => {
            anyhow::bail!("expected Welcome, got something else");
        }
    };

    // Set terminal title to display_host/session_name (save original title first)
    {
        let mut stdout = std::io::stdout();
        // Save current title on the XTERM title stack, then set our title
        let _ = write!(stdout, "\x1b[22;0t\x1b]0;{display_host}/{session_name}\x07");
        let _ = stdout.flush();
    }

    // Spawn task to read server messages
    let (server_tx, server_rx) = mpsc::channel::<S2C>(64);
    tokio::spawn(async move {
        loop {
            match read_frame_async::<_, S2C>(&mut reader).await {
                Ok(msg) => {
                    if server_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::UnexpectedEof {
                        error!("server read error: {e}");
                    }
                    break;
                }
            }
        }
    });

    // SIGWINCH handler
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;

    let mut detach_filter = detach::DetachFilter::new();
    let mut stdout = std::io::stdout();
    let mut local_kkp_active = false;
    let mut local_dec_modes = std::collections::HashSet::<u16>::new();

    let mut exit_reason = ExitReason::ServerDisconnected;
    let mut server_rx = Some(server_rx);

    // Keepalive state (only active for remote sessions)
    let mut keepalive_interval = tokio::time::interval(Duration::from_secs(5));
    keepalive_interval.tick().await; // consume initial tick
    let mut last_pong = Instant::now();

    let result: Result<()> = async {
        loop {
            // biased; ensures input is always forwarded before rendering output
            tokio::select! {
                biased;

                // Local stdin → send to server (highest priority)
                Some(data) = stdin_rx.recv() => {
                    let result = detach_filter.feed(&data);
                    if !result.forward.is_empty() {
                        write_frame_async(&mut writer, &C2S::RawInput { data: result.forward }).await?;
                    }
                    if result.kill {
                        write_frame_async(&mut writer, &C2S::KillSession).await?;
                        exit_reason = ExitReason::Killed;
                        break;
                    }
                    if result.detach {
                        exit_reason = ExitReason::Detached;
                        break;
                    }
                    if result.refresh {
                        write_frame_async(&mut writer, &C2S::RequestSnapshot).await?;
                    }
                }

                // Terminal resize
                _ = sigwinch.recv() => {
                    let (cols, rows) = crossterm::terminal::size()?;
                    write_frame_async(&mut writer, &C2S::Resize { width: cols, height: rows }).await?;
                }

                // Application-layer keepalive (remote sessions only)
                _ = keepalive_interval.tick(), if is_remote => {
                    if last_pong.elapsed() > Duration::from_secs(15) {
                        info!("keepalive timeout — no pong received in 15s");
                        exit_reason = ExitReason::ServerDisconnected;
                        break;
                    }
                    let t = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    write_frame_async(&mut writer, &C2S::Ping { t }).await?;
                }

                // Server messages (lowest priority — render after input is handled)
                msg = async {
                    match server_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match msg {
                        Some(msg) => {
                            if let MsgAction::Exit(reason) = handle_server_msg(
                                msg, &mut stdout, &mut local_kkp_active,
                                &mut local_dec_modes, &mut last_pong,
                            )? {
                                exit_reason = reason;
                                stdout.flush()?;
                                break;
                            }
                        }
                        None => {
                            info!("server disconnected");
                            exit_reason = ExitReason::ServerDisconnected;
                            break;
                        }
                    }
                    // Drain queued server messages before flushing so
                    // multiple updates (e.g. DEC mode + diff) are written
                    // to the terminal in a single flush.
                    if let Some(rx) = server_rx.as_mut() {
                        while let Ok(msg) = rx.try_recv() {
                            if let MsgAction::Exit(reason) = handle_server_msg(
                                msg, &mut stdout, &mut local_kkp_active,
                                &mut local_dec_modes, &mut last_pong,
                            )? {
                                exit_reason = reason;
                                stdout.flush()?;
                                break;
                            }
                        }
                        if matches!(exit_reason, ExitReason::Kicked(_) | ExitReason::SessionEnded) {
                            break;
                        }
                    }
                    stdout.flush()?;
                }
            }
        }
        Ok(())
    }.await;

    // Clean up terminal modes from this session
    cleanup_terminal_modes(&mut stdout, &mut local_kkp_active, &mut local_dec_modes);

    // Take server_rx for Kicked case (keep socket alive for auto-reclaim)
    let kept_rx = if matches!(exit_reason, ExitReason::Kicked(_)) {
        server_rx.take()
    } else {
        None
    };

    result?;
    Ok((exit_reason, kept_rx))
}

/// Action from the kicked overlay.
enum OverlayAction {
    Reconnect,
    SessionEnded,
    Exit,
}

/// Wait for overlay input or server notification (auto-reclaim).
async fn wait_for_overlay_action(
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
    server_rx: Option<&mut mpsc::Receiver<S2C>>,
) -> OverlayAction {
    let mut server_rx = server_rx;
    loop {
        tokio::select! {
            data = stdin_rx.recv() => {
                if let Some(data) = data {
                    for &b in &data {
                        match b {
                            // Space or Enter → reconnect
                            0x20 | 0x0d | 0x0a => return OverlayAction::Reconnect,
                            // 'q' or Esc → exit
                            b'q' | 0x1b => return OverlayAction::Exit,
                            _ => {}
                        }
                    }
                } else {
                    // stdin closed
                    return OverlayAction::Exit;
                }
            }
            msg = async {
                match server_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match msg {
                    Some(S2C::SessionEnded) => return OverlayAction::SessionEnded,
                    Some(S2C::SessionAvailable) => return OverlayAction::Reconnect,
                    Some(_) => {}
                    None => {
                        // Server gone (connection dropped) — session is gone
                        return OverlayAction::SessionEnded;
                    }
                }
            }
        }
    }
}

/// Resolve the local hostname for terminal title display.
fn local_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOST").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            hostname::get()
                .ok()
                .and_then(|s| s.into_string().ok())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

pub async fn run(sock_path: &Path, session_name: &str) -> Result<()> {
    // Enable raw mode
    let _raw = input::RawInput::enable()?;

    // Spawn stdin reader (shared across reconnections)
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    input::spawn_stdin_reader(stdin_tx);

    let hostname = local_hostname();

    let result = loop {
        let stream = net::connect(sock_path).await?;
        let (reader, writer) = tokio::io::split(stream);
        match run_session(
            Box::new(reader),
            Box::new(writer),
            session_name,
            &hostname,
            false,
            &mut stdin_rx,
        )
        .await
        {
            Ok((ExitReason::Kicked(reason), mut server_rx)) => {
                // Show overlay and wait for user decision or auto-reclaim
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let mut stdout = std::io::stdout();
                let _ = render::render_kicked_overlay(&mut stdout, cols, rows);

                let action = wait_for_overlay_action(
                    &mut stdin_rx,
                    server_rx.as_mut(),
                ).await;

                match action {
                    OverlayAction::Reconnect => {
                        info!("reclaiming session after kick");
                        continue;
                    }
                    OverlayAction::SessionEnded => {
                        // Update overlay to show session ended, wait for key
                        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                        let mut stdout = std::io::stdout();
                        let _ = render::render_session_ended_overlay(&mut stdout, cols, rows);
                        // Wait for any key to dismiss
                        let _ = stdin_rx.recv().await;
                        break Ok(Some(ExitReason::ServerDisconnected));
                    }
                    OverlayAction::Exit => {
                        break Ok(Some(ExitReason::Kicked(reason)));
                    }
                }
            }
            Ok((ExitReason::Detached, _)) => break Ok(Some(ExitReason::Detached)),
            Ok((ExitReason::Killed, _)) => break Ok(Some(ExitReason::Killed)),
            Ok((ExitReason::SessionEnded, _)) => break Ok(Some(ExitReason::SessionEnded)),
            Ok((ExitReason::ServerDisconnected, _)) => break Ok(Some(ExitReason::ServerDisconnected)),
            Err(e) => break Err(e),
        }
    };

    // Ensure terminal is cleaned up on exit
    drop(_raw);
    let _ = crossterm::terminal::disable_raw_mode();
    let mut stdout = std::io::stdout();
    // Pop any KKP modes we may have pushed, reset attributes, clear screen, show cursor,
    // and restore the original terminal title from the XTERM title stack
    let _ = write!(stdout, "\x1b[<u\x1b[0m\x1b[2J\x1b[H\x1b[?25h\x1b[23;0t");
    let _ = stdout.flush();

    // Print exit reason to stderr (after terminal is restored)
    match &result {
        Ok(Some(ExitReason::Detached)) => {
            eprintln!("mux: detached from session '{session_name}'");
        }
        Ok(Some(ExitReason::Kicked(reason))) => {
            eprintln!("mux: kicked — {reason}");
        }
        Ok(Some(ExitReason::Killed)) => {
            eprintln!("mux: killed session '{session_name}'");
        }
        Ok(Some(ExitReason::SessionEnded)) | Ok(Some(ExitReason::ServerDisconnected)) => {
            eprintln!("mux: server disconnected");
        }
        Ok(None) => {}
        Err(_) => {}
    }

    result.map(|_| ())
}

/// Action from the reconnect overlay.
enum ReconnectAction {
    Retry,
    Exit,
}

/// Wait for manual user input only (no countdown, no server_rx).
/// Used when a remote client is kicked — the user must explicitly choose to reclaim.
async fn wait_for_manual_action(
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
) -> ReconnectAction {
    loop {
        match stdin_rx.recv().await {
            Some(data) => {
                for &b in &data {
                    match b {
                        // Space or Enter → retry
                        0x20 | 0x0d | 0x0a => return ReconnectAction::Retry,
                        // q or Esc → exit
                        b'q' | 0x1b => return ReconnectAction::Exit,
                        _ => {}
                    }
                }
            }
            None => return ReconnectAction::Exit,
        }
    }
}

/// Show reconnect overlay with countdown and wait for user input or timeout.
async fn show_reconnect_overlay(
    host: &str,
    session: &str,
    attempt: u32,
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
) -> ReconnectAction {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut stdout = std::io::stdout();
    let countdown_secs: u32 = 3;
    let mut remaining = countdown_secs;

    let _ = render::render_reconnect_overlay(
        &mut stdout, cols, rows, host, session, attempt, remaining,
    );

    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.tick().await; // first tick is immediate

    loop {
        tokio::select! {
            data = stdin_rx.recv() => {
                if let Some(data) = data {
                    for &b in &data {
                        match b {
                            // Enter or Space → retry now
                            0x0d | 0x0a | 0x20 => return ReconnectAction::Retry,
                            // q or Esc → exit
                            b'q' | 0x1b => return ReconnectAction::Exit,
                            _ => {}
                        }
                    }
                } else {
                    return ReconnectAction::Exit;
                }
            }
            _ = interval.tick() => {
                remaining = remaining.saturating_sub(1);
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let _ = render::render_reconnect_overlay(
                    &mut stdout, cols, rows, host, session, attempt, remaining,
                );
                if remaining == 0 {
                    return ReconnectAction::Retry;
                }
            }
        }
    }
}

/// Terminal cleanup shared by run() and run_remote().
fn cleanup_terminal(session_name: &str, result: &Result<Option<ExitReason>>) {
    let _ = crossterm::terminal::disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b[<u\x1b[0m\x1b[2J\x1b[H\x1b[?25h\x1b[23;0t");
    let _ = stdout.flush();

    match result {
        Ok(Some(ExitReason::Detached)) => {
            eprintln!("mux: detached from session '{session_name}'");
        }
        Ok(Some(ExitReason::Kicked(reason))) => {
            eprintln!("mux: kicked — {reason}");
        }
        Ok(Some(ExitReason::Killed)) => {
            eprintln!("mux: killed session '{session_name}'");
        }
        Ok(Some(ExitReason::SessionEnded)) | Ok(Some(ExitReason::ServerDisconnected)) => {
            eprintln!("mux: server disconnected");
        }
        Ok(None) => {}
        Err(_) => {}
    }
}

/// Attach to a remote session over SSH with automatic reconnection.
pub async fn run_remote(host: &str, session: &str, program: &[String], ssh_options: &ssh::SshOptions) -> Result<()> {
    let _raw = input::RawInput::enable()?;

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    input::spawn_stdin_reader(stdin_tx);

    let mut attempt = 0u32;

    let result: Result<Option<ExitReason>> = loop {
        // Connect via SSH
        let ssh_conn = match ssh::connect(host, session, program, ssh_options).await {
            Ok(conn) => {
                attempt = 0;
                conn
            }
            Err(e) if attempt < MAX_RECONNECT_RETRIES => {
                attempt += 1;
                info!("SSH connection failed (attempt {attempt}): {e}");
                match show_reconnect_overlay(host, session, attempt, &mut stdin_rx).await {
                    ReconnectAction::Retry => continue,
                    ReconnectAction::Exit => break Ok(Some(ExitReason::Detached)),
                }
            }
            Err(e) => break Err(e),
        };

        let (reader, writer) = ssh_conn;

        match run_session(reader, writer, session, host, true, &mut stdin_rx).await {
            Ok((ExitReason::Detached, _)) => break Ok(Some(ExitReason::Detached)),
            Ok((ExitReason::Killed, _)) => break Ok(Some(ExitReason::Killed)),
            Ok((ExitReason::SessionEnded, _)) => break Ok(Some(ExitReason::SessionEnded)),
            Ok((ExitReason::ServerDisconnected, _)) => {
                // SSH dropped or remote bridge exited — try to reconnect
                attempt += 1;
                if attempt > MAX_RECONNECT_RETRIES {
                    break Ok(Some(ExitReason::ServerDisconnected));
                }
                info!("server disconnected, attempting reconnect (attempt {attempt})");
                match show_reconnect_overlay(host, session, attempt, &mut stdin_rx).await {
                    ReconnectAction::Retry => continue,
                    ReconnectAction::Exit => break Ok(Some(ExitReason::Detached)),
                }
            }
            Ok((ExitReason::Kicked(_), _)) => {
                // Remote kicked — show manual-only overlay (no auto-reconnect)
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let mut stdout = std::io::stdout();
                let _ = render::render_remote_kicked_overlay(&mut stdout, cols, rows);
                match wait_for_manual_action(&mut stdin_rx).await {
                    ReconnectAction::Retry => { attempt = 0; continue; }
                    ReconnectAction::Exit => break Ok(Some(ExitReason::Detached)),
                }
            }
            Err(e) => {
                attempt += 1;
                if attempt > MAX_RECONNECT_RETRIES {
                    break Err(e);
                }
                info!("session error (attempt {attempt}): {e}");
                match show_reconnect_overlay(host, session, attempt, &mut stdin_rx).await {
                    ReconnectAction::Retry => continue,
                    ReconnectAction::Exit => break Ok(Some(ExitReason::Detached)),
                }
            }
        }
    };

    drop(_raw);
    cleanup_terminal(session, &result);

    result.map(|_| ())
}
