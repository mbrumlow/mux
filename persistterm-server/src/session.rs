use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use persistterm_proto::codec::async_io::{
    read_frame_async_limited, write_frame_async as write_frame,
};
use persistterm_proto::{C2S, MAX_CLIENT_FRAME_SIZE, S2C};

use crate::net::Listener;
use crate::pty::PtyHandle;
use crate::terminal::Terminal;

const MUX_VERSION: &str = match option_env!("MUX_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

// Round up so the maximum rate cannot exceed 60 frames per second.
const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

fn validate_size(rows: u16, cols: u16) -> Result<()> {
    anyhow::ensure!(
        rows <= 512 && cols <= 1024 && u32::from(rows.max(1)) * u32::from(cols.max(1)) <= 65_536,
        "terminal exceeds 1024 columns, 512 rows, or 65536 cells"
    );
    Ok(())
}

/// A timed-out frame may be partially written; callers must discard the connection.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

async fn write_frame_async<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    message: &S2C,
) -> std::io::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, write_frame(writer, message))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "client write stalled"))?
}

/// Dropping a connection must also close its reader half, even if it is idle.
struct ReadTask(tokio::task::JoinHandle<()>);

impl Drop for ReadTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Active client connection state.
struct ClientConn {
    writer: tokio::io::WriteHalf<tokio::net::UnixStream>,
    msg_rx: mpsc::Receiver<C2S>,
    _read_task: ReadTask,
    connected_at: Instant,
}

/// Maximum number of kicked clients kept in the waiting queue.
const MAX_WAITING_CLIENTS: usize = 8;

/// Maximum time for a connecting client to complete the handshake (Hello →
/// Welcome → initial screen). The handshake shares the session loop, so a
/// stuck connection must not be allowed to block PTY draining indefinitely.
const HANDSHAKE_TIMEOUT_SECS: u64 = 5;

/// A kicked client that is still connected and waiting to reclaim.
struct WaitingClient {
    writer: tokio::io::WriteHalf<tokio::net::UnixStream>,
    dead: Arc<AtomicBool>,
    _read_task: ReadTask,
}

/// Convert a kicked ClientConn into a WaitingClient, keeping the socket alive.
fn kick_to_waiting(conn: ClientConn) -> WaitingClient {
    let ClientConn {
        writer,
        mut msg_rx,
        _read_task,
        connected_at: _,
    } = conn;
    let dead = Arc::new(AtomicBool::new(false));
    let dead_clone = dead.clone();
    tokio::spawn(async move {
        // Drain incoming messages to keep the reader task alive
        while msg_rx.recv().await.is_some() {}
        // Channel closed — socket is gone
        dead_clone.store(true, Ordering::Relaxed);
    });
    WaitingClient {
        writer,
        dead,
        _read_task,
    }
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

/// Notify all connected clients (active + waiting) that the session has ended.
async fn notify_session_ended(
    client: &mut Option<ClientConn>,
    waiting: &mut VecDeque<WaitingClient>,
) {
    // Bound the entire shutdown notification phase, including all waiting clients.
    let _ = tokio::time::timeout(WRITE_TIMEOUT, async {
        if let Some(conn) = client.as_mut() {
            let _ = write_frame_async(&mut conn.writer, &S2C::SessionEnded).await;
        }
        for w in waiting.iter_mut() {
            if !w.dead.load(Ordering::Relaxed) {
                let _ = write_frame_async(&mut w.writer, &S2C::SessionEnded).await;
            }
        }
    })
    .await;
}

/// Validate OSC 52 clipboard pieces before forwarding to the client terminal.
/// The emulator sandboxes all other session output, but the clipboard is
/// replayed as a raw escape sequence on the client's terminal — an OSC body
/// may legally contain ESC bytes, which some terminals treat as terminating
/// the OSC, turning the remainder into arbitrary escape injection. Only pass
/// well-formed selection params and base64 payloads (or a `?` query).
fn valid_osc52(params: &str, data: &str) -> bool {
    params
        .chars()
        .all(|c| matches!(c, 'c' | 'p' | 'q' | 's' | '0'..='7'))
        && (data == "?"
            || data
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')))
}

/// Channel-backed writer that offloads blocking PTY writes to a dedicated thread.
/// Both the session loop and wezterm-term can clone and write without blocking
/// the tokio runtime.
struct ChannelWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl ChannelWriter {
    fn spawn(mut inner: Box<dyn Write + Send>) -> Self {
        // Input frames are capped at 64 KiB: at most 16 MiB queued here.
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        std::thread::spawn(move || {
            while let Some(data) = rx.blocking_recv() {
                if inner.write_all(&data).and_then(|()| inner.flush()).is_err() {
                    break;
                }
            }
        });
        Self { tx }
    }

    fn clone_boxed(&self) -> Box<dyn Write + Send> {
        Box::new(ChannelWriter {
            tx: self.tx.clone(),
        })
    }
}

// The emulator's synchronous reply writer cannot await capacity. Return an
// explicit error on saturation; user input uses reserve() in the session loop
// instead, retaining the pending bytes while servicing signals and connections.
impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > MAX_CLIENT_FRAME_SIZE as usize {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "PTY reply too large",
            ));
        }
        self.tx.try_send(buf.to_vec()).map_err(|e| {
            let kind = match e {
                mpsc::error::TrySendError::Full(_) => std::io::ErrorKind::WouldBlock,
                mpsc::error::TrySendError::Closed(_) => std::io::ErrorKind::BrokenPipe,
            };
            warn!("PTY reply could not be queued: {kind:?}");
            std::io::Error::new(kind, "PTY reply could not be queued")
        })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // The worker flushes each queued write.
        Ok(())
    }
}

pub struct Session {
    name: String,
    pty: PtyHandle,
    pty_writer: ChannelWriter,
    terminal: Terminal,
    listener: Listener,
    browser_listener: Option<Listener>,
    browser_token: String,
    pty_rx: mpsc::Receiver<Box<[u8]>>,
    started_at: Instant,
    program: Vec<String>,
}

impl Session {
    #[allow(clippy::too_many_arguments)] // Session construction includes PTY and browser setup.
    pub fn new(
        name: &str,
        rows: u16,
        cols: u16,
        sock_path: &Path,
        program: &[String],
        extra_env: &[(String, String)],
        browser_sock_path: Option<&Path>,
        browser_token: &str,
    ) -> Result<Self> {
        validate_size(rows, cols)?;
        let (pty, pty_reader, pty_writer) = PtyHandle::spawn(rows, cols, program, name, extra_env)?;
        let channel_writer = ChannelWriter::spawn(pty_writer);
        let term_writer = channel_writer.clone_boxed();
        let terminal = Terminal::new(rows, cols, term_writer);
        let listener = Listener::bind(sock_path)?;

        let browser_listener = match browser_sock_path {
            Some(p) => match Listener::bind(p) {
                Ok(l) => Some(l),
                Err(e) => {
                    warn!("failed to bind browser socket: {e}");
                    None
                }
            },
            None => None,
        };

        // Spawn blocking reader for PTY output
        let (pty_tx, pty_rx) = mpsc::channel::<Box<[u8]>>(256);
        std::thread::spawn(move || {
            read_pty_loop(pty_reader, pty_tx);
        });

        Ok(Self {
            name: name.to_string(),
            pty,
            pty_writer: channel_writer,
            terminal,
            listener,
            browser_listener,
            browser_token: browser_token.to_string(),
            pty_rx,
            started_at: Instant::now(),
            program: program.to_vec(),
        })
    }

    /// Handle a single browser sideband connection: read the request line,
    /// validate token and URL, respond OK/ERR, and return the URL on success.
    async fn handle_browser_conn(
        stream: tokio::net::UnixStream,
        expected_token: &str,
    ) -> Option<String> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let (reader, mut writer) = tokio::io::split(stream);
            let mut buf_reader = tokio::io::BufReader::new(reader);
            let mut line = String::new();

            // Read at most 8KB + token size to prevent memory exhaustion
            let max_len = 8192 + expected_token.len() + 2;
            let n = (&mut buf_reader)
                .take(max_len as u64)
                .read_line(&mut line)
                .await;
            match n {
                Ok(0) | Err(_) => {
                    let _ = writer.write_all(b"ERR bad request\n").await;
                    return None;
                }
                Ok(_) => {}
            }

            let line = line.trim_end();
            let (token, url) = match line.split_once(' ') {
                Some(pair) => pair,
                None => {
                    let _ = writer.write_all(b"ERR bad format\n").await;
                    return None;
                }
            };

            // Constant-time token comparison
            if !constant_time_eq(token.as_bytes(), expected_token.as_bytes()) {
                let _ = writer.write_all(b"ERR bad token\n").await;
                return None;
            }

            // Validate URL: must be http or https, max 8KB
            if url.len() > 8192 {
                let _ = writer.write_all(b"ERR url too long\n").await;
                return None;
            }
            if !url.starts_with("http://") && !url.starts_with("https://") {
                let _ = writer.write_all(b"ERR invalid scheme\n").await;
                return None;
            }

            let _ = writer.write_all(b"OK\n").await;
            Some(url.to_string())
        })
        .await;

        match result {
            Ok(url) => url,
            Err(_) => {
                warn!("browser request timed out");
                None
            }
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut client: Option<ClientConn> = None;
        let mut waiting: VecDeque<WaitingClient> = VecDeque::new();
        let mut pending_input = None;
        let mut next_frame_at = tokio::time::Instant::now();
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
        // Periodic full-screen refresh interval. Repairs screen corruption
        // caused by the outer terminal (e.g. kitty's clear_terminal action)
        // manipulating its display behind mux's back.
        const REFRESH_SECS: u64 = 2;
        let mut refresh_interval = tokio::time::interval(Duration::from_secs(REFRESH_SECS));
        // Don't fire immediately on startup (client gets a full screen on connect).
        refresh_interval.reset();

        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        loop {
            // Capture sync state before select — can't borrow self inside
            // the async block since other branches need &mut self.
            let app_sync = self.terminal.is_app_sync_active();

            // Fair selection keeps a busy input stream from starving shutdown or accepts.
            let pty_tx = self.pty_writer.tx.clone();
            tokio::select! {
                permit = pty_tx.reserve(), if pending_input.is_some() => {
                    match permit {
                        Ok(permit) => permit.send(pending_input.take().unwrap()),
                        Err(_) => anyhow::bail!("PTY writer closed"),
                    }
                }

                // ── Client messages ──────────────────
                msg = async {
                    match client.as_mut() {
                        Some(c) => c.msg_rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if pending_input.is_none() => {
                    match msg {
                        Some(C2S::RawInput { data }) => {
                            pending_input = Some(data);
                        }
                        Some(C2S::Resize { width, height }) => {
                            if validate_size(height, width).is_err() {
                                warn!(width, height, "rejecting oversized terminal");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
                                continue;
                            }
                            let (height, width) = (height.max(1), width.max(1));
                            if (height, width) == self.terminal.size() {
                                continue;
                            }
                            info!(width, height, "client resize");
                            if let Err(e) = self.pty.resize(height, width) {
                                error!("failed to resize PTY: {e}");
                            }
                            self.terminal.resize(height, width);
                            // Give the child time to redraw at the new size
                            // before snapshotting, so we don't capture (and
                            // send) a stale pre-reflow frame.
                            self.drain_pty_after_resize().await;
                            // Send full screen after resize
                            let conn = client.as_mut().unwrap();
                            let data = self.terminal.screen_formatted();
                            tokio::time::sleep_until(next_frame_at).await;
                            next_frame_at = tokio::time::Instant::now() + FRAME_INTERVAL;
                            if let Err(e) = write_frame_async(&mut conn.writer, &S2C::ScreenData { data }).await {
                                warn!("failed to send resize screen data, dropping client: {e}");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
                            } else {
                                self.terminal.reset_prev_screen();
                                dirty = false;
                                dirty_since = None;
                                last_pty_at = None;
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
                            notify_session_ended(&mut client, &mut waiting).await;
                            waiting.clear();
                            return Ok(());
                        }
                        Some(C2S::RequestSnapshot) => {
                            let conn = client.as_mut().unwrap();
                            let data = self.terminal.screen_formatted();
                            tokio::time::sleep_until(next_frame_at).await;
                            next_frame_at = tokio::time::Instant::now() + FRAME_INTERVAL;
                            if let Err(e) = write_frame_async(&mut conn.writer, &S2C::ScreenData { data }).await {
                                warn!("failed to send snapshot, dropping client: {e}");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
                            } else {
                                self.terminal.reset_prev_screen();
                                dirty = false;
                                dirty_since = None;
                                last_pty_at = None;
                            }
                        }
                        Some(C2S::RequestSessionInfo) => {
                            let conn = client.as_mut().unwrap();
                            let (rows, cols) = self.terminal.size();
                            let info = S2C::SessionInfo {
                                session_name: self.name.clone(),
                                server_version: MUX_VERSION.to_string(),
                                program: self.program.clone(),
                                uptime_secs: self.started_at.elapsed().as_secs(),
                                terminal_size: (cols, rows),
                                pid: std::process::id(),
                                child_pid: self.pty.child_pid(),
                                attached_secs: conn.connected_at.elapsed().as_secs(),
                                waiting_clients: waiting.iter().filter(|w| !w.dead.load(Ordering::Relaxed)).count(),
                            };
                            if let Err(e) = write_frame_async(&mut conn.writer, &info).await {
                                warn!("failed to send session info, dropping client: {e}");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
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
                    notify_session_ended(&mut client, &mut waiting).await;
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
                                                    if !valid_osc52(params, data) {
                                                        warn!("dropping malformed OSC 52 (invalid params or data)");
                                                        continue;
                                                    }
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
                            notify_session_ended(&mut client, &mut waiting).await;
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
                        tokio::time::sleep_until(max_at.max(next_frame_at)).await;
                    } else {
                        let quiesce_at = last_pty_at.unwrap() + Duration::from_millis(QUIESCE_MS);
                        let max_at = dirty_since.unwrap() + Duration::from_millis(MAX_LATENCY_MS);
                        tokio::time::sleep_until(quiesce_at.min(max_at).max(next_frame_at)).await;
                    }
                }, if client.is_some() && dirty => {
                    if let Some(data) = self.terminal.screen_diff() {
                        next_frame_at = tokio::time::Instant::now() + FRAME_INTERVAL;
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

                // ── Periodic full-screen refresh (repairs screen corruption) ──
                _ = refresh_interval.tick(), if client.is_some() && !dirty => {
                    if tokio::time::Instant::now() >= next_frame_at && self.terminal.refresh_if_changed() {
                        if let Some(data) = self.terminal.screen_diff() {
                            next_frame_at = tokio::time::Instant::now() + FRAME_INTERVAL;
                            let conn = client.as_mut().unwrap();
                            if let Err(e) = write_frame_async(&mut conn.writer, &S2C::ScreenDiff { data }).await {
                                warn!("failed to send refresh, dropping client: {e}");
                                drop(client.take());
                                notify_next_waiting(&mut waiting).await;
                            }
                        }
                    }
                }

                // ── Accept new client (kick existing only after handshake) ──
                result = self.listener.accept() => {
                    match result {
                        Ok(stream) => {
                            // The handshake runs inline in the session loop; without
                            // a timeout a wedged connection would stall PTY draining
                            // until backpressure freezes the child process.
                            match tokio::time::timeout(
                                Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
                                self.accept_client(stream),
                            ).await {
                                Ok(Ok(conn)) => {
                                    if let Some(mut old) = client.take() {
                                        let kicked = write_frame_async(
                                            &mut old.writer,
                                            &S2C::Kicked { reason: "another client connected".to_string() },
                                        ).await;
                                        // Evict oldest waiting client if at capacity
                                        while waiting.len() >= MAX_WAITING_CLIENTS {
                                            waiting.pop_front();
                                        }
                                        if kicked.is_ok() {
                                            waiting.push_back(kick_to_waiting(old));
                                        }
                                    }
                                    pending_input = None; // Do not forward the previous client's pending input.
                                    client = Some(conn);
                                    next_frame_at = tokio::time::Instant::now() + FRAME_INTERVAL;
                                    dirty = false;
                                    dirty_since = None;
                                    last_pty_at = None;
                                }
                                Ok(Err(e)) => {
                                    warn!("client handshake failed: {e}");
                                }
                                Err(_) => {
                                    warn!("client handshake timed out");
                                }
                            }
                        }
                        Err(e) => {
                            error!("accept error: {e}");
                        }
                    }
                }

                // ── Browser sideband: open-url requests ──
                result = async {
                    match self.browser_listener.as_ref() {
                        Some(l) => l.accept().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match result {
                        Ok(stream) => {
                            if let Some(url) = Self::handle_browser_conn(stream, &self.browser_token).await {
                                if let Some(conn) = client.as_mut() {
                                    if let Err(e) = write_frame_async(
                                        &mut conn.writer,
                                        &S2C::OpenUrl { url: url.clone() },
                                    ).await {
                                        warn!("failed to send OpenUrl, dropping client: {e}");
                                        drop(client.take());
                                        notify_next_waiting(&mut waiting).await;
                                    } else {
                                        info!("forwarded browser open-url to client");
                                    }
                                } else {
                                    warn!("browser open-url request but no client attached");
                                }
                            }
                        }
                        Err(e) => {
                            warn!("browser socket accept error: {e}");
                        }
                    }
                }
            }
        }
    }

    /// After resizing the PTY, drain output briefly so the child process
    /// can re-render at the new size before we capture the screen.
    async fn drain_pty_after_resize(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let mut got_data = false;

        loop {
            let timeout = if got_data {
                // After receiving data, wait for the app to finish its full
                // redraw — complex apps (emacs, btop) redraw in multiple
                // bursts with gaps between them.
                (tokio::time::Instant::now() + Duration::from_millis(50)).min(deadline)
            } else {
                // Wait up to 100ms for the child to start responding to SIGWINCH
                (tokio::time::Instant::now() + Duration::from_millis(100)).min(deadline)
            };

            tokio::select! {
                data = self.pty_rx.recv() => {
                    match data {
                        Some(data) => {
                            self.terminal.process(&data);
                            got_data = true;
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(timeout) => {
                    break;
                }
            }
        }
    }

    /// Perform client handshake and return a ClientConn on success.
    async fn accept_client(&mut self, stream: tokio::net::UnixStream) -> Result<ClientConn> {
        let (mut reader, mut writer) = tokio::io::split(stream);

        // Handshake: read Hello
        let hello: C2S = read_frame_async_limited(&mut reader, MAX_CLIENT_FRAME_SIZE).await?;
        match &hello {
            C2S::Hello { caps } => {
                validate_size(caps.height, caps.width)?;
                info!(term = %caps.term, kkp = caps.supports_kkp,
                      width = caps.width, height = caps.height, "client hello");
                // Resize to client dimensions before sending initial screen data
                if caps.width > 0 && caps.height > 0 {
                    let size_changed = (caps.height, caps.width) != self.terminal.size();
                    if let Err(e) = self.pty.resize(caps.height, caps.width) {
                        error!("failed to resize PTY on hello: {e}");
                    }
                    self.terminal.resize(caps.height, caps.width);
                    // If the size changed, give the child time to re-render
                    if size_changed {
                        self.drain_pty_after_resize().await;
                    }
                }
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

        // Send initial full screen data (at client's dimensions)
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
            write_frame_async(
                &mut writer,
                &S2C::SetDecMode {
                    mode,
                    enabled: true,
                },
            )
            .await?;
        }

        // Spawn reader task
        let (msg_tx, msg_rx) = mpsc::channel::<C2S>(64);
        let read_task = tokio::spawn(async move {
            loop {
                match read_frame_async_limited::<_, C2S>(&mut reader, MAX_CLIENT_FRAME_SIZE).await {
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
            _read_task: ReadTask(read_task),
            connected_at: Instant::now(),
        })
    }
}

/// Constant-time byte comparison to prevent timing side-channels on token auth.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn read_pty_loop(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<Box<[u8]>>) {
    let mut buf = [0u8; 32768];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                info!("PTY closed");
                break;
            }
            Ok(n) => {
                if tx.blocking_send(buf[..n].into()).is_err() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_limits_include_area() {
        assert!(validate_size(64, 1024).is_ok());
        assert!(validate_size(0, 0).is_ok());
        assert!(validate_size(513, 1).is_err());
        assert!(validate_size(1, 1025).is_err());
        assert!(validate_size(256, 512).is_err());
    }

    #[tokio::test]
    async fn oversized_input_is_rejected_before_reading_payload() {
        let (mut sender, mut reader) = tokio::io::duplex(4);
        use tokio::io::AsyncWriteExt;
        sender
            .write_all(&(MAX_CLIENT_FRAME_SIZE + 1).to_be_bytes())
            .await
            .unwrap();
        // Keep the sender alive: a decoder that attempts to read the body hangs.
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            read_frame_async_limited::<_, C2S>(&mut reader, MAX_CLIENT_FRAME_SIZE),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(result.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn stalled_frame_write_times_out() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let err = write_frame_async(&mut writer, &S2C::SessionEnded)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn pty_capacity_wait_preserves_input_and_sync_replies_do_not_block() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut writer = ChannelWriter { tx };
        writer.write_all(b"first").unwrap();
        assert_eq!(
            writer.write(b"reply").unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let tx = writer.tx.clone();
        let pending = tx.reserve();
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        assert_eq!(rx.recv().await.unwrap(), b"first");
        pending.await.unwrap().send(b"second".to_vec());
        assert_eq!(rx.recv().await.unwrap(), b"second");
    }

    #[test]
    fn osc52_accepts_base64_set() {
        assert!(valid_osc52("c", "aGVsbG8=")); // "hello"
        assert!(valid_osc52("", "aGVsbG8="));
        assert!(valid_osc52("cp0", "QUJD"));
    }

    #[test]
    fn osc52_accepts_query() {
        assert!(valid_osc52("c", "?"));
    }

    #[test]
    fn osc52_rejects_escape_bytes() {
        assert!(!valid_osc52("c", "aGVs\x1b[31mbG8="));
        assert!(!valid_osc52("c\x1b", "aGVsbG8="));
        assert!(!valid_osc52("c", "?\x1b[2J"));
    }

    #[test]
    fn osc52_rejects_non_base64() {
        assert!(!valid_osc52("c", "hello world"));
        assert!(!valid_osc52("x", "aGVsbG8="));
    }
}
