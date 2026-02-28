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

/// Run a single connection session. Returns the exit reason.
async fn run_session(
    sock_path: &Path,
    session_name: &str,
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<ExitReason> {
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
        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());
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
    let (server_tx, mut server_rx) = mpsc::channel::<S2C>(64);
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

    let result: Result<()> = async {
        loop {
            tokio::select! {
                // Local stdin → send to server
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

                // Server messages
                msg = server_rx.recv() => {
                    match msg {
                        Some(S2C::Snapshot(snapshot)) => {
                            render::render_snapshot(&mut stdout, &snapshot)?;
                        }
                        Some(S2C::ScreenData { data }) => {
                            stdout.write_all(b"\x1b[2J\x1b[H")?;
                            stdout.write_all(&data)?;
                            stdout.flush()?;
                        }
                        Some(S2C::ScreenDiff { data }) => {
                            stdout.write_all(&data)?;
                            stdout.flush()?;
                        }
                        Some(S2C::SetKkpMode { flags }) => {
                            if flags > 0 {
                                if local_kkp_active {
                                    write!(stdout, "\x1b[<u")?;
                                }
                                write!(stdout, "\x1b[>{flags}u")?;
                                stdout.flush()?;
                                local_kkp_active = true;
                            } else if local_kkp_active {
                                write!(stdout, "\x1b[<u")?;
                                stdout.flush()?;
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
                            stdout.flush()?;
                        }
                        Some(S2C::Clipboard { params, data }) => {
                            write!(stdout, "\x1b]52;{params};{data}\x07")?;
                            stdout.flush()?;
                        }
                        Some(S2C::Kicked { reason }) => {
                            info!("kicked: {reason}");
                            exit_reason = ExitReason::Kicked(reason);
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
                }

                // Terminal resize
                _ = sigwinch.recv() => {
                    let (cols, rows) = crossterm::terminal::size()?;
                    write_frame_async(&mut writer, &C2S::Resize { width: cols, height: rows }).await?;
                }
            }
        }
        Ok(())
    }.await;

    // Clean up terminal modes from this session
    cleanup_terminal_modes(&mut stdout, &mut local_kkp_active, &mut local_dec_modes);

    result?;
    Ok(exit_reason)
}

/// Wait for user input on the kicked overlay. Returns true to reconnect, false to exit.
async fn wait_for_overlay_input(stdin_rx: &mut mpsc::Receiver<Vec<u8>>) -> bool {
    loop {
        if let Some(data) = stdin_rx.recv().await {
            for &b in &data {
                match b {
                    // Space or Enter → reconnect
                    0x20 | 0x0d | 0x0a => return true,
                    // 'q' or Esc → exit
                    b'q' | 0x1b => return false,
                    _ => {}
                }
            }
        } else {
            // stdin closed
            return false;
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
            Ok(ExitReason::Kicked(reason)) => {
                // Show overlay and wait for user decision
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let mut stdout = std::io::stdout();
                let _ = render::render_kicked_overlay(&mut stdout, cols, rows);

                if wait_for_overlay_input(&mut stdin_rx).await {
                    // User chose to reconnect — loop back
                    info!("reclaiming session after kick");
                    continue;
                } else {
                    // User chose to exit
                    break Ok(Some(ExitReason::Kicked(reason)));
                }
            }
            Ok(ExitReason::Detached) => break Ok(Some(ExitReason::Detached)),
            Ok(ExitReason::Killed) => break Ok(Some(ExitReason::Killed)),
            Ok(ExitReason::ServerDisconnected) => break Ok(Some(ExitReason::ServerDisconnected)),
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
