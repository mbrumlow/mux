use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// A clonable writer wrapper so both session and wezterm-term can write to the PTY.
struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl SharedWriter {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }

    fn clone_boxed(&self) -> Box<dyn Write + Send> {
        Box::new(SharedWriter(Arc::clone(&self.0)))
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

pub struct Session {
    name: String,
    pty: PtyHandle,
    pty_writer: SharedWriter,
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
        let shared = SharedWriter::new(pty_writer);
        let term_writer = shared.clone_boxed();
        let terminal = Terminal::new(rows, cols, term_writer);
        let listener = Listener::bind(sock_path)?;

        // Spawn blocking reader for PTY output
        let (pty_tx, pty_rx) = mpsc::channel::<Vec<u8>>(256);
        std::thread::spawn(move || {
            read_pty_loop(pty_reader, pty_tx);
        });

        Ok(Self {
            name: name.to_string(),
            pty,
            pty_writer: shared,
            terminal,
            listener,
            pty_rx,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut client: Option<ClientConn> = None;
        let mut waiting: VecDeque<WaitingClient> = VecDeque::new();
        let mut dirty = false;
        // Debounce timestamps: wait for PTY output to go quiet before sending
        // a diff so we capture complete application redraws (e.g. emacs
        // updating btop in a split pane) rather than intermediate states.
        let mut dirty_since: Option<tokio::time::Instant> = None;
        let mut last_pty_at: Option<tokio::time::Instant> = None;
        // How long to wait after the last PTY data before sending a diff.
        const QUIESCE_MS: u64 = 3;
        // Maximum latency cap — send a diff at least this often during
        // continuous output (keeps responsiveness for fast-scrolling).
        const MAX_LATENCY_MS: u64 = 16;
        // Maximum time to honour an app's synchronized update (DEC 2026)
        // before force-rendering. Prevents hangs if the app never sends
        // the closing sequence.
        const MAX_SYNC_MS: u64 = 500;

        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )?;

        loop {
            // Capture sync state before select — can't borrow self inside
            // the async block since other branches need &mut self.
            let app_sync = self.terminal.is_app_sync_active();

            // biased; ensures deterministic priority:
            // client input > signals > PTY output > screen updates > new connections
            tokio::select! {
                biased;

                // ── Client messages (highest priority) ──────────────────
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

                // ── SIGTERM → clean shutdown ──────────────────────────
                _ = sigterm.recv() => {
                    info!("received SIGTERM, shutting down");
                    waiting.clear();
                    return Ok(());
                }

                // ── PTY output ────────────────────────────────────────
                data = self.pty_rx.recv() => {
                    match data {
                        Some(first) => {
                            // Process the first chunk
                            let mut all_events = self.terminal.process(&first);

                            // Drain queued PTY data so we see the final state of
                            // the app's redraw (e.g. cursor hidden mid-redraw).
                            // Cap iterations so we yield back to handle input.
                            let mut drained = 0;
                            while drained < 64 {
                                match self.pty_rx.try_recv() {
                                    Ok(more) => {
                                        let ev = self.terminal.process(&more);
                                        if ev.kkp_changed.is_some() {
                                            all_events.kkp_changed = ev.kkp_changed;
                                        }
                                        all_events.dec_mode_changes.extend(ev.dec_mode_changes);
                                        all_events.osc_forwards.extend(ev.osc_forwards);
                                        drained += 1;
                                    }
                                    Err(_) => break,
                                }
                            }

                            // Forward KKP mode changes to client
                            if let Some(flags) = all_events.kkp_changed {
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
                            for &(mode, enabled) in &all_events.dec_mode_changes {
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
                            if !all_events.osc_forwards.is_empty() {
                                if let Some(conn) = client.as_mut() {
                                    for osc_body in &all_events.osc_forwards {
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

                            // Update debounce timestamps
                            let now = tokio::time::Instant::now();
                            if dirty_since.is_none() {
                                dirty_since = Some(now);
                            }
                            last_pty_at = Some(now);
                            dirty = true;
                        }
                        None => {
                            info!("shell exited, ending session");
                            waiting.clear();
                            return Ok(());
                        }
                    }
                }

                // ── Debounced render: wait for PTY quiescence or max latency ──
                // During an app synchronized update (DEC 2026), we delay
                // rendering until the app signals completion (or a safety
                // timeout expires) so we never send a half-drawn frame.
                _ = async {
                    if app_sync {
                        // App has an active sync update — wait up to MAX_SYNC_MS
                        let max_at = dirty_since.unwrap() + Duration::from_millis(MAX_SYNC_MS);
                        tokio::time::sleep_until(max_at).await;
                    } else {
                        let quiesce_at = last_pty_at.unwrap() + Duration::from_millis(QUIESCE_MS);
                        let max_at = dirty_since.unwrap() + Duration::from_millis(MAX_LATENCY_MS);
                        tokio::time::sleep_until(quiesce_at.min(max_at)).await;
                    }
                }, if client.is_some() && dirty => {
                    if let Some(data) = self.terminal.screen_diff() {
                        let conn = client.as_mut().unwrap();
                        if let Err(e) = write_frame_async(&mut conn.writer, &S2C::ScreenDiff { data }).await {
                            warn!("failed to send screen diff, dropping client: {e}");
                            drop(client.take());
                            notify_next_waiting(&mut waiting).await;
                        }
                    }
                    dirty = false;
                    dirty_since = None;
                    last_pty_at = None;
                }

                // ── Accept new client (kick existing only after handshake) ──
                result = self.listener.accept() => {
                    match result {
                        Ok(stream) => {
                            match self.accept_client(stream).await {
                                Ok(conn) => {
                                    if let Some(mut old) = client.take() {
                                        let _ = write_frame_async(
                                            &mut old.writer,
                                            &S2C::Kicked { reason: "another client connected".to_string() },
                                        ).await;
                                        waiting.push_back(kick_to_waiting(old));
                                    }
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
    let mut buf = [0u8; 16384];
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
