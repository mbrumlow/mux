use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use persistterm_proto::codec::async_io::{read_frame_async, write_frame_async};
use persistterm_proto::{C2S, S2C};

use crate::net::Listener;
use crate::pty::PtyHandle;
use crate::terminal::Terminal;

/// Active client connection state.
struct ClientConn {
    writer: tokio::io::WriteHalf<tokio::net::UnixStream>,
    msg_rx: mpsc::Receiver<C2S>,
    _read_task: tokio::task::JoinHandle<()>,
}

/// A kicked client that is still connected and waiting to reclaim.
struct WaitingClient {
    writer: tokio::io::WriteHalf<tokio::net::UnixStream>,
    dead: Arc<AtomicBool>,
}

/// Convert a kicked ClientConn into a WaitingClient, keeping the socket alive.
fn kick_to_waiting(conn: ClientConn) -> WaitingClient {
    let ClientConn { writer, mut msg_rx, _read_task } = conn;
    let dead = Arc::new(AtomicBool::new(false));
    let dead_clone = dead.clone();
    tokio::spawn(async move {
        // Drain incoming messages to keep the reader task alive
        while msg_rx.recv().await.is_some() {}
        // Channel closed — socket is gone
        dead_clone.store(true, Ordering::Relaxed);
    });
    WaitingClient { writer, dead }
}

/// Notify the most recently kicked waiting client that the session is available.
async fn notify_next_waiting(waiting: &mut VecDeque<WaitingClient>) {
    while let Some(mut w) = waiting.pop_back() {
        if w.dead.load(Ordering::Relaxed) {
            continue;
        }
        if let Err(e) = write_frame_async(&mut w.writer, &S2C::SessionAvailable).await {
            warn!("failed to notify waiting client: {e}");
            continue;
        }
        info!("notified waiting client to reclaim session");
        return;
    }
}

pub struct Session {
    name: String,
    pty: PtyHandle,
    pty_writer: Box<dyn Write + Send>,
    terminal: Terminal,
    listener: Listener,
    pty_rx: mpsc::Receiver<Vec<u8>>,
}

impl Session {
    pub fn new(
        name: &str,
        rows: u16,
        cols: u16,
        sock_path: &Path,
        program: &[String],
        extra_env: &[(String, String)],
    ) -> Result<Self> {
        let (pty, pty_reader, pty_writer) = PtyHandle::spawn(rows, cols, program, name, extra_env)?;
        let terminal = Terminal::new(rows, cols);
        let listener = Listener::bind(sock_path)?;

        // Spawn blocking reader for PTY output
        let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>(256);
        std::thread::spawn(move || {
            read_pty_loop(pty_reader, pty_tx);
        });

        Ok(Self {
            name: name.to_string(),
            pty,
            pty_writer,
            terminal,
            listener,
            pty_rx,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut client: Option<ClientConn> = None;
        let mut waiting: VecDeque<WaitingClient> = VecDeque::new();
        let mut dirty = false;
        let mut interval = tokio::time::interval(Duration::from_millis(16));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )?;

        loop {
            tokio::select! {
                // ── SIGTERM → clean shutdown ──────────────────────────
                _ = sigterm.recv() => {
                    info!("received SIGTERM, shutting down");
                    waiting.clear();
                    return Ok(());
                }
                // ── PTY output (always active) ──────────────────────────
                data = self.pty_rx.recv() => {
                    match data {
                        Some(data) => {
                            let events = self.terminal.process(&data);

                            // Inject terminal query responses back into PTY
                            if !events.pty_responses.is_empty() {
                                for response in &events.pty_responses {
                                    if let Err(e) = self.pty_writer.write_all(response) {
                                        error!("failed to inject terminal query response: {e}");
                                    }
                                }
                                let _ = self.pty_writer.flush();
                            }

                            // Forward KKP mode changes to client
                            if let Some(flags) = events.kkp_changed {
                                info!(flags, "KKP mode changed");
                                if let Some(conn) = client.as_mut() {
                                    if let Err(e) = write_frame_async(
                                        &mut conn.writer,
                                        &S2C::SetKkpMode { flags },
                                    ).await {
                                        warn!("failed to send KKP mode, dropping client: {e}");
                                        drop(client.take());
                                        notify_next_waiting(&mut waiting).await;
                                    }
                                }
                            }

                            // Forward DEC private mode changes to client
                            for &(mode, enabled) in &events.dec_mode_changes {
                                info!(mode, enabled, "DEC private mode changed");
                                if let Some(conn) = client.as_mut() {
                                    if let Err(e) = write_frame_async(
                                        &mut conn.writer,
                                        &S2C::SetDecMode { mode, enabled },
                                    ).await {
                                        warn!("failed to send DEC mode, dropping client: {e}");
                                        drop(client.take());
                                        notify_next_waiting(&mut waiting).await;
                                        break;
                                    }
                                }
                            }

                            // Forward OSC sequences (clipboard etc.) to client
                            if !events.osc_forwards.is_empty() {
                                if let Some(conn) = client.as_mut() {
                                    for osc_body in &events.osc_forwards {
                                        // Parse "52;PARAMS;DATA" from the body
                                        if let Ok(body_str) = std::str::from_utf8(osc_body) {
                                            if let Some(rest) = body_str.strip_prefix("52;") {
                                                if let Some((params, data)) = rest.split_once(';') {
                                                    if let Err(e) = write_frame_async(
                                                        &mut conn.writer,
                                                        &S2C::Clipboard {
                                                            params: params.to_string(),
                                                            data: data.to_string(),
                                                        },
                                                    ).await {
                                                        warn!("failed to send clipboard, dropping client: {e}");
                                                        drop(client.take());
                                                        notify_next_waiting(&mut waiting).await;
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            dirty = true;
                        }
                        None => {
                            info!("shell exited, ending session");
                            waiting.clear();
                            return Ok(());
                        }
                    }
                }

                // ── Accept new client (kick existing if any) ─────────
                result = self.listener.accept() => {
                    match result {
                        Ok(stream) => {
                            // Kick existing client if any
                            if let Some(mut old) = client.take() {
                                let _ = write_frame_async(
                                    &mut old.writer,
                                    &S2C::Kicked { reason: "another client connected".to_string() },
                                ).await;
                                waiting.push_back(kick_to_waiting(old));
                            }
                            match self.accept_client(stream).await {
                                Ok(conn) => {
                                    client = Some(conn);
                                    dirty = false;
                                }
                                Err(e) => {
                                    warn!("client handshake failed: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            error!("accept error: {e}");
                        }
                    }
                }

                // ── Timer tick → send diff if dirty ──────────────────────
                _ = interval.tick(), if client.is_some() && dirty => {
                    if let Some(data) = self.terminal.screen_diff() {
                        let conn = client.as_mut().unwrap();
                        if let Err(e) = write_frame_async(&mut conn.writer, &S2C::ScreenDiff { data }).await {
                            warn!("failed to send screen diff, dropping client: {e}");
                            drop(client.take());
                            notify_next_waiting(&mut waiting).await;
                        }
                    }
                    dirty = false;
                }

                // ── Client messages ─────────────────────────────────────
                msg = async {
                    match client.as_mut() {
                        Some(c) => c.msg_rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match msg {
                        Some(C2S::RawInput { data }) => {
                            if let Err(e) = self.pty_writer.write_all(&data) {
                                error!("failed to write to PTY: {e}");
                            }
                            let _ = self.pty_writer.flush();
                        }
                        Some(C2S::Resize { width, height }) => {
                            info!(width, height, "client resize");
                            if let Err(e) = self.pty.resize(height, width) {
                                error!("failed to resize PTY: {e}");
                            }
                            self.terminal.resize(height, width);
                            // Send full screen after resize
                            let conn = client.as_mut().unwrap();
                            let data = self.terminal.screen_formatted();
                            if let Err(e) = write_frame_async(&mut conn.writer, &S2C::ScreenData { data }).await {
                                warn!("failed to send resize screen data, dropping client: {e}");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
                            } else {
                                self.terminal.reset_prev_screen();
                                dirty = false;
                            }
                        }
                        Some(C2S::Ping { t }) => {
                            let conn = client.as_mut().unwrap();
                            if let Err(e) = write_frame_async(&mut conn.writer, &S2C::Pong { t }).await {
                                warn!("failed to send pong, dropping client: {e}");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
                            }
                        }
                        Some(C2S::KillSession) => {
                            info!("client requested session kill");
                            waiting.clear();
                            return Ok(());
                        }
                        Some(C2S::RequestSnapshot) => {
                            let conn = client.as_mut().unwrap();
                            let data = self.terminal.screen_formatted();
                            if let Err(e) = write_frame_async(&mut conn.writer, &S2C::ScreenData { data }).await {
                                warn!("failed to send snapshot, dropping client: {e}");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
                            } else {
                                self.terminal.reset_prev_screen();
                                dirty = false;
                            }
                        }
                        Some(_) => {}
                        None => {
                            info!("client disconnected");
                            drop(client.take());
                            notify_next_waiting(&mut waiting).await;
                        }
                    }
                }
            }
        }
    }

    /// Perform client handshake and return a ClientConn on success.
    async fn accept_client(
        &mut self,
        stream: tokio::net::UnixStream,
    ) -> Result<ClientConn> {
        let (mut reader, mut writer) = tokio::io::split(stream);

        // Handshake: read Hello
        let hello: C2S = read_frame_async(&mut reader).await?;
        match &hello {
            C2S::Hello { caps } => {
                info!(term = %caps.term, kkp = caps.supports_kkp, "client hello");
            }
            _ => {
                anyhow::bail!("expected Hello, got something else");
            }
        }

        // Send Welcome
        write_frame_async(
            &mut writer,
            &S2C::Welcome {
                session_id: self.name.clone(),
            },
        )
        .await?;

        // Send initial full screen data
        let data = self.terminal.screen_formatted();
        write_frame_async(&mut writer, &S2C::ScreenData { data }).await?;
        self.terminal.reset_prev_screen();

        // Send current KKP state if active
        let kkp = self.terminal.kkp_flags();
        if kkp > 0 {
            write_frame_async(&mut writer, &S2C::SetKkpMode { flags: kkp }).await?;
        }

        // Replay all active DEC private modes
        for &mode in self.terminal.dec_modes() {
            write_frame_async(&mut writer, &S2C::SetDecMode { mode, enabled: true }).await?;
        }

        // Spawn reader task
        let (msg_tx, msg_rx) = mpsc::channel::<C2S>(64);
        let read_task = tokio::spawn(async move {
            loop {
                match read_frame_async::<_, C2S>(&mut reader).await {
                    Ok(msg) => {
                        if msg_tx.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if e.kind() != std::io::ErrorKind::UnexpectedEof {
                            warn!("error reading client message: {e}");
                        }
                        break;
                    }
                }
            }
        });

        info!("client connected and ready");
        Ok(ClientConn {
            writer,
            msg_rx,
            _read_task: read_task,
        })
    }
}

fn read_pty_loop(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                info!("PTY closed");
                break;
            }
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
            Err(e) => {
                error!("PTY read error: {e}");
                break;
            }
        }
    }
}
