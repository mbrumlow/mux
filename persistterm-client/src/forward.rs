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

use tracing::{info, warn};
use url::{Host, Url};

use crate::ssh::PortForwarder;

/// How long a callback listener stays open waiting for the browser.
const LISTEN_WINDOW: Duration = Duration::from_secs(300);

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

    for listener in listeners {
        let forwarder = forwarder.clone();
        tokio::spawn(async move {
            let accept_loop = async {
                loop {
                    let (tcp, peer) = match listener.accept().await {
                        Ok(conn) => conn,
                        Err(e) => {
                            warn!(port, "callback forward: accept failed: {e}");
                            break;
                        }
                    };
                    let forwarder = forwarder.clone();
                    tokio::spawn(async move {
                        relay(forwarder, tcp, peer, remote_host, port).await;
                    });
                }
            };
            let _ = tokio::time::timeout(LISTEN_WINDOW, accept_loop).await;
            info!(port, "callback forward: listener closed");
        });
    }
    Ok(())
}

async fn relay(
    forwarder: PortForwarder,
    mut tcp: tokio::net::TcpStream,
    peer: SocketAddr,
    remote_host: &'static str,
    port: u16,
) {
    let mut remote = match forwarder.open(remote_host, port, peer).await {
        Ok(stream) => stream,
        Err(e) => {
            warn!(port, "callback forward: remote connect failed: {e:#}");
            return;
        }
    };
    match tokio::io::copy_bidirectional(&mut tcp, &mut remote).await {
        Ok((up, down)) => info!(port, up, down, "callback forward: connection relayed"),
        Err(e) => info!(port, "callback forward: connection ended: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_aws_signin() {
        let url = "https://us-west-2.signin.aws.amazon.com/v1/authorize?response_type=code\
                   &client_id=arn%3Aaws%3Asignin%3A%3A%3Adevtools%2Fsame-device&scope=openid\
                   &redirect_uri=http%3A%2F%2F127.0.0.1%3A45293%2Foauth%2Fcallback&code_challenge=x";
        assert_eq!(
            detect_callback(url),
            Some(LoopbackCallback { host: LoopbackHost::V4, port: 45293 })
        );
    }

    #[test]
    fn detects_localhost_and_v6() {
        assert_eq!(
            detect_callback("https://accounts.example.com/o/auth?redirect_uri=http://localhost:8085/"),
            Some(LoopbackCallback { host: LoopbackHost::Name, port: 8085 })
        );
        assert_eq!(
            detect_callback("https://idp.example.com/auth?redirect_uri=http%3A%2F%2F%5B%3A%3A1%5D%3A9000%2Fcb"),
            Some(LoopbackCallback { host: LoopbackHost::V6, port: 9000 })
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
