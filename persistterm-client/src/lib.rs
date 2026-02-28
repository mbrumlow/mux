pub mod detach;
pub mod input;
pub mod net;
pub mod render;

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info};

use persistterm_proto::codec::async_io::{read_frame_async, write_frame_async};
use persistterm_proto::{C2S, ClientCapabilities, S2C};

/// Reason the client session ended.
enum ExitReason {
    Detached,
    Kicked(String),
    Killed,
    ServerDisconnected,
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
    sock_path: &Path,
    session_name: &str,
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<(ExitReason, Option<mpsc::Receiver<S2C>>)> {
    // Get terminal size
    let (cols, rows) = crossterm::terminal::size()?;

    // Connect
    let stream = net::connect(sock_path).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    // Send Hello
    let hello = C2S::Hello {
        caps: ClientCapabilities {
            supports_kkp: true,
            supports_truecolor: true,
            term: std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string()),
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

    // Set terminal title to HOSTNAME/session_name
    {
        let hostname = std::env::var("HOSTNAME")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| std::env::var("HOST").ok().filter(|s| !s.is_empty()))
                .or_else(|| {
                    hostname::get()
                        .ok()
                        .and_then(|s| s.into_string().ok())
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_else(|| "unknown".to_string());
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b]0;{hostname}/{session_name}\x07");
        let _ = stdout.flush();
    }

    // Send initial resize
    write_frame_async(
        &mut writer,
        &C2S::Resize {
            width: cols,
            height: rows,
        },
    )
    .await?;

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
                }

                // Terminal resize
                _ = sigwinch.recv() => {
                    let (cols, rows) = crossterm::terminal::size()?;
                    write_frame_async(&mut writer, &C2S::Resize { width: cols, height: rows }).await?;
                }

                // Server messages (lowest priority — render after input is handled)
                msg = async {
                    match server_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match msg {
                        Some(S2C::Snapshot(snapshot)) => {
                            render::render_snapshot(&mut stdout, &snapshot)?;
                        }
                        Some(S2C::ScreenData { data }) => {
                            stdout.write_all(b"\x1b[2J\x1b[H")?;
                            stdout.write_all(&data)?;
                        }
                        Some(S2C::ScreenDiff { data }) => {
                            stdout.write_all(&data)?;
                        }
                        Some(S2C::SetKkpMode { flags }) => {
                            if flags > 0 {
                                if local_kkp_active {
                                    write!(stdout, "\x1b[<u")?;
                                }
                                write!(stdout, "\x1b[>{flags}u")?;
                                local_kkp_active = true;
                            } else if local_kkp_active {
                                write!(stdout, "\x1b[<u")?;
                                local_kkp_active = false;
                            }
                        }
                        Some(S2C::SetDecMode { mode, enabled }) => {
                            if enabled {
                                write!(stdout, "\x1b[?{mode}h")?;
                                local_dec_modes.insert(mode);
                            } else {
                                write!(stdout, "\x1b[?{mode}l")?;
                                local_dec_modes.remove(&mode);
                            }
                        }
                        Some(S2C::Clipboard { params, data }) => {
                            write!(stdout, "\x1b]52;{params};{data}\x07")?;
                        }
                        Some(S2C::Kicked { reason }) => {
                            info!("kicked: {reason}");
                            exit_reason = ExitReason::Kicked(reason);
                            break;
                        }
                        Some(S2C::SessionEnded) => {
                            info!("session ended");
                            exit_reason = ExitReason::ServerDisconnected;
                            break;
                        }
                        Some(S2C::Pong { .. }) => {}
                        Some(_) => {}
                        None => {
                            info!("server disconnected");
                            exit_reason = ExitReason::ServerDisconnected;
                            break;
                        }
                    }
                    // Drain queued server messages before flushing so
                    // multiple updates (e.g. DEC mode + diff) are written
                    // to the terminal in a single flush.
                    while let Some(rx) = server_rx.as_mut() {
                        match rx.try_recv() {
                            Ok(S2C::Snapshot(snapshot)) => {
                                render::render_snapshot(&mut stdout, &snapshot)?;
                            }
                            Ok(S2C::ScreenData { data }) => {
                                stdout.write_all(b"\x1b[2J\x1b[H")?;
                                stdout.write_all(&data)?;
                            }
                            Ok(S2C::ScreenDiff { data }) => {
                                stdout.write_all(&data)?;
                            }
                            Ok(S2C::SetKkpMode { flags }) => {
                                if flags > 0 {
                                    if local_kkp_active {
                                        write!(stdout, "\x1b[<u")?;
                                    }
                                    write!(stdout, "\x1b[>{flags}u")?;
                                    local_kkp_active = true;
                                } else if local_kkp_active {
                                    write!(stdout, "\x1b[<u")?;
                                    local_kkp_active = false;
                                }
                            }
                            Ok(S2C::SetDecMode { mode, enabled }) => {
                                if enabled {
                                    write!(stdout, "\x1b[?{mode}h")?;
                                    local_dec_modes.insert(mode);
                                } else {
                                    write!(stdout, "\x1b[?{mode}l")?;
                                    local_dec_modes.remove(&mode);
                                }
                            }
                            Ok(S2C::Clipboard { params, data }) => {
                                write!(stdout, "\x1b]52;{params};{data}\x07")?;
                            }
                            Ok(S2C::Kicked { reason }) => {
                                info!("kicked: {reason}");
                                exit_reason = ExitReason::Kicked(reason);
                                stdout.flush()?;
                                break;
                            }
                            Ok(S2C::SessionEnded) => {
                                info!("session ended");
                                exit_reason = ExitReason::ServerDisconnected;
                                stdout.flush()?;
                                break;
                            }
                            Ok(S2C::Pong { .. }) => {}
                            Ok(_) => {}
                            Err(_) => break,
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

pub async fn run(sock_path: &Path, session_name: &str) -> Result<()> {
    // Enable raw mode
    let _raw = input::RawInput::enable()?;

    // Spawn stdin reader (shared across reconnections)
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    input::spawn_stdin_reader(stdin_tx);

    let result = loop {
        match run_session(sock_path, session_name, &mut stdin_rx).await {
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
            Ok((ExitReason::ServerDisconnected, _)) => break Ok(Some(ExitReason::ServerDisconnected)),
            Err(e) => break Err(e),
        }
    };

    // Ensure terminal is cleaned up on exit
    drop(_raw);
    let _ = crossterm::terminal::disable_raw_mode();
    let mut stdout = std::io::stdout();
    // Pop any KKP modes we may have pushed, reset attributes, clear screen, show cursor
    let _ = write!(stdout, "\x1b[<u\x1b[0m\x1b[2J\x1b[H\x1b[?25h");
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
        Ok(Some(ExitReason::ServerDisconnected)) => {
            eprintln!("mux: server disconnected");
        }
        Ok(None) => {}
        Err(_) => {}
    }

    result.map(|_| ())
}
