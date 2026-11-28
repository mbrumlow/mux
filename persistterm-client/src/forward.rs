//! OAuth-style loopback callback forwarding.
//!
//! CLI login flows (`aws login`, `gcloud auth login`, `az login`, …) start a
//! one-shot HTTP server on a loopback port of the machine they run on and put
//! it in the authorize URL as `redirect_uri=http://127.0.0.1:PORT/...`. When
//! that URL is opened in the browser of a *different* machine (ours), the
//! final redirect hits our loopback, where nothing is listening.
//!
//! When we see such a URL we listen on the same loopback port locally and
//! relay each connection over SSH (direct-tcpip) to the same port on the
//! remote host.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Semaphore;
use tracing::{info, warn};
use url::{Host, Url};

use crate::ssh::PortForwarder;

/// How long a callback listener stays open waiting for the browser.
const LISTEN_WINDOW: Duration = Duration::from_secs(300);
const RELAY_LIFETIME: Duration = Duration::from_secs(120);
const RELAY_IDLE: Duration = Duration::from_secs(30);
// Process-wide budgets apply across ports, address families, and reconnects.
static LISTENERS: Semaphore = Semaphore::const_new(8);
static CONNECTIONS: Semaphore = Semaphore::const_new(16);

/// Query parameters that carry the callback address in common auth flows.
const CALLBACK_PARAMS: &[&str] = &[
    "redirect_uri",
    "redirect_url",
    "redirect",
    "callback",
    "callback_uri",
    "callback_url",
    "return_to",
    "reply_url",
];

/// Which loopback name the callback URL used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopbackHost {
    V4,
    V6,
    /// `localhost` — may resolve to either family in the browser.
    Name,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopbackCallback {
    pub host: LoopbackHost,
    pub port: u16,
}

impl LoopbackCallback {
    fn local_addrs(&self) -> Vec<SocketAddr> {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, self.port));
        match self.host {
            LoopbackHost::V4 => vec![v4],
            LoopbackHost::V6 => vec![v6],
            LoopbackHost::Name => vec![v4, v6],
        }
    }

    fn remote_host(&self) -> &'static str {
        match self.host {
            LoopbackHost::V4 => "127.0.0.1",
            LoopbackHost::V6 => "::1",
            LoopbackHost::Name => "localhost",
        }
    }
}

/// Find a loopback callback address in an authorize-style URL.
///
/// Only an explicit, unprivileged port on `127.0.0.1`, `[::1]` or `localhost`
/// qualifies; anything else is ignored.
pub fn detect_callback(url: &str) -> Option<LoopbackCallback> {
    let url = Url::parse(url).ok()?;
    url.query_pairs()
        .filter(|(k, _)| CALLBACK_PARAMS.iter().any(|p| k.eq_ignore_ascii_case(p)))
        .find_map(|(_, v)| parse_loopback(&v))
}

fn parse_loopback(value: &str) -> Option<LoopbackCallback> {
    let cb = Url::parse(value).ok()?;
    if !matches!(cb.scheme(), "http" | "https") {
        return None;
    }
    let host = match cb.host()? {
        Host::Ipv4(ip) if ip == Ipv4Addr::LOCALHOST => LoopbackHost::V4,
        Host::Ipv6(ip) if ip == Ipv6Addr::LOCALHOST => LoopbackHost::V6,
        Host::Domain(d) if d.eq_ignore_ascii_case("localhost") => LoopbackHost::Name,
        _ => return None,
    };
    let port = cb.port()?;
    if port < 1024 {
        return None;
    }
    Some(LoopbackCallback { host, port })
}

/// Start listening for the callback. Binds synchronously so the listener is
/// up before the caller opens the browser; the relay runs in the background
/// and stops after [`LISTEN_WINDOW`]. Must be called inside a tokio runtime.
pub fn start(forwarder: PortForwarder, cb: LoopbackCallback) -> std::io::Result<()> {
    let listener_permit = LISTENERS
        .try_acquire()
        .map_err(|_| std::io::Error::other("callback listener limit reached"))?;
    let deadline = tokio::time::Instant::now() + LISTEN_WINDOW;
    let mut listeners = Vec::new();
    let mut last_err = None;
    for addr in cb.local_addrs() {
        let bound = std::net::TcpListener::bind(addr).and_then(|l| {
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l)
        });
        match bound {
            Ok(l) => listeners.push(l),
            Err(e) => {
                warn!(%addr, "callback forward: cannot listen: {e}");
                last_err = Some(e);
            }
        }
    }
    if listeners.is_empty() {
        return Err(last_err.unwrap_or_else(|| std::io::Error::other("no address to bind")));
    }

    let port = cb.port;
    let remote_host = cb.remote_host();
    info!(port, remote_host, "callback forward: listening");

    tokio::spawn(async move {
        let _permit = listener_permit;
        let mut tasks = tokio::task::JoinSet::new();
        for listener in listeners {
            let forwarder = forwarder.clone();
            tasks.spawn(async move {
                loop {
                    let (tcp, peer) = match listener.accept().await {
                        Ok(conn) => conn,
                        Err(_) => break,
                    };
                    let Ok(permit) = CONNECTIONS.try_acquire() else {
                        warn!(port, "callback connection limit reached");
                        continue;
                    };
                    let forwarder = forwarder.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        // Includes SSH channel creation and all relay I/O. Existing
                        // connections cannot outlive the listener's auth window.
                        let relay_deadline =
                            deadline.min(tokio::time::Instant::now() + RELAY_LIFETIME);
                        let result = tokio::time::timeout_at(relay_deadline, async {
                            let remote = forwarder.open(remote_host, port, peer).await?;
                            relay_streams(tcp, remote, RELAY_IDLE).await?;
                            Ok::<_, anyhow::Error>(())
                        })
                        .await;
                        if !matches!(result, Ok(Ok(()))) {
                            warn!(port, "callback relay ended or timed out");
                        }
                    });
                }
            });
        }
        let _ = tokio::time::timeout_at(deadline, async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        // Dropping JoinSet cancels both address-family listeners.
        info!(port, "callback forward: listeners closed");
    });
    Ok(())
}

/// Relay with a shared idle deadline, preserving TCP half-close semantics.
async fn relay_streams<L, R>(mut left: L, mut right: R, idle: Duration) -> std::io::Result<()>
where
    L: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    let mut left_buf = [0u8; 8192];
    let mut right_buf = [0u8; 8192];
    let (mut left_open, mut right_open) = (true, true);
    while left_open || right_open {
        tokio::time::timeout(idle, async {
            tokio::select! {
                n = left.read(&mut left_buf), if left_open => {
                    let n = n?;
                    if n == 0 {
                        left_open = false;
                        right.shutdown().await?;
                    } else {
                        right.write_all(&left_buf[..n]).await?;
                        right.flush().await?;
                    }
                }
                n = right.read(&mut right_buf), if right_open => {
                    let n = n?;
                    if n == 0 {
                        right_open = false;
                        left.shutdown().await?;
                    } else {
                        left.write_all(&right_buf[..n]).await?;
                        left.flush().await?;
                    }
                }
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "callback relay idle"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_relay_closes_both_streams() {
        let (left, mut peer_left) = tokio::io::duplex(64);
        let (right, _peer_right) = tokio::io::duplex(64);
        let err = relay_streams(left, right, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(peer_left.read(&mut [0; 1]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn relay_preserves_response_after_request_half_close() {
        let (left, mut browser) = tokio::io::duplex(64);
        let (right, mut server) = tokio::io::duplex(64);
        let task = tokio::spawn(relay_streams(left, right, Duration::from_secs(1)));
        browser.write_all(b"request").await.unwrap();
        browser.shutdown().await.unwrap();
        let mut request = Vec::new();
        server.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");
        server.write_all(b"response").await.unwrap();
        server.shutdown().await.unwrap();
        let mut response = Vec::new();
        browser.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response");
        task.await.unwrap().unwrap();
    }

    #[test]
    fn callback_budgets_reject_excess_and_release_capacity() {
        let connections: Vec<_> = (0..16)
            .map(|_| CONNECTIONS.try_acquire().unwrap())
            .collect();
        assert!(CONNECTIONS.try_acquire().is_err());
        drop(connections);
        assert!(CONNECTIONS.try_acquire().is_ok());
        let listeners: Vec<_> = (0..8).map(|_| LISTENERS.try_acquire().unwrap()).collect();
        assert!(LISTENERS.try_acquire().is_err());
        drop(listeners);
        assert!(LISTENERS.try_acquire().is_ok());
    }

    #[test]
    fn detects_aws_signin() {
        let url = "https://us-west-2.signin.aws.amazon.com/v1/authorize?response_type=code\
                   &client_id=arn%3Aaws%3Asignin%3A%3A%3Adevtools%2Fsame-device&scope=openid\
                   &redirect_uri=http%3A%2F%2F127.0.0.1%3A45293%2Foauth%2Fcallback&code_challenge=x";
        assert_eq!(
            detect_callback(url),
            Some(LoopbackCallback {
                host: LoopbackHost::V4,
                port: 45293
            })
        );
    }

    #[test]
    fn detects_localhost_and_v6() {
        assert_eq!(
            detect_callback(
                "https://accounts.example.com/o/auth?redirect_uri=http://localhost:8085/"
            ),
            Some(LoopbackCallback {
                host: LoopbackHost::Name,
                port: 8085
            })
        );
        assert_eq!(
            detect_callback(
                "https://idp.example.com/auth?redirect_uri=http%3A%2F%2F%5B%3A%3A1%5D%3A9000%2Fcb"
            ),
            Some(LoopbackCallback {
                host: LoopbackHost::V6,
                port: 9000
            })
        );
    }

    #[test]
    fn ignores_non_loopback_and_unsafe_ports() {
        for url in [
            "https://example.com/",
            "https://idp.example.com/auth?redirect_uri=https%3A%2F%2Fapp.example.com%2Fcb",
            "https://idp.example.com/auth?redirect_uri=http%3A%2F%2F127.0.0.1%2Fcb",
            "https://idp.example.com/auth?redirect_uri=http%3A%2F%2F127.0.0.1%3A80%2Fcb",
            "https://idp.example.com/auth?redirect_uri=http%3A%2F%2F127.0.0.2%3A8080%2Fcb",
            "https://idp.example.com/auth?redirect_uri=http%3A%2F%2F192.168.1.5%3A8080%2Fcb",
            "https://idp.example.com/auth?redirect_uri=http%3A%2F%2Flocalhost.evil.com%3A8080%2F",
            "https://idp.example.com/auth?next=http%3A%2F%2F127.0.0.1%3A8080%2Fcb",
            "https://idp.example.com/auth?redirect_uri=ftp%3A%2F%2F127.0.0.1%3A8080%2F",
        ] {
            assert_eq!(detect_callback(url), None, "{url}");
        }
    }
}
