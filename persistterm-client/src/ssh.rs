use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::client;
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use tokio::io::{AsyncRead, AsyncWrite};

/// Options for SSH connections.
pub struct SshOptions {
    pub compression: bool,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self { compression: false }
    }
}

struct SshHandler {
    host: String,
    port: u16,
}

impl client::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        match russh::keys::known_hosts::check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                bail!(
                    "Unknown host key for {}:{}.\n\
                     Connect with `ssh {}` first to verify and accept the host key.",
                    self.host,
                    self.port,
                    self.host
                );
            }
            Err(e) => {
                bail!(
                    "Host key verification failed for {}:{}: {}\n\
                     WARNING: The host key may have changed. This could indicate a MITM attack.\n\
                     Remove the offending key from ~/.ssh/known_hosts and reconnect.",
                    self.host,
                    self.port,
                    e
                );
            }
        }
    }
}

/// Parse a host string that may contain user@host:port.
fn parse_ssh_host(host_str: &str) -> (Option<String>, String, u16) {
    let (user, rest) = match host_str.split_once('@') {
        Some((u, r)) => (Some(u.to_string()), r),
        None => (None, host_str),
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => (h.to_string(), port),
            Err(_) => (rest.to_string(), 22),
        },
        None => (rest.to_string(), 22),
    };
    (user, host, port)
}

/// Connect to a remote host via SSH and exec `mux bridge --session <name>`.
/// Returns a (reader, writer) pair for the SSH channel.
pub async fn connect(
    host_str: &str,
    session_name: &str,
    program: &[String],
    options: &SshOptions,
) -> Result<(
    Box<dyn AsyncRead + Unpin + Send>,
    Box<dyn AsyncWrite + Unpin + Send>,
)> {
    let (user, host, port) = parse_ssh_host(host_str);
    let username = user.unwrap_or_else(|| {
        std::env::var("USER").unwrap_or_else(|_| "root".to_string())
    });

    let preferred = if options.compression {
        russh::Preferred {
            compression: Cow::Borrowed(&[
                russh::compression::ZLIB_LEGACY,
                russh::compression::ZLIB,
                russh::compression::NONE,
            ]),
            ..Default::default()
        }
    } else {
        Default::default()
    };

    let config = Arc::new(client::Config {
        preferred,
        keepalive_interval: Some(Duration::from_secs(10)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });

    let handler = SshHandler {
        host: host.clone(),
        port,
    };

    let mut session = client::connect(config, (&*host, port), handler)
        .await
        .with_context(|| format!("failed to connect to {host}:{port}"))?;

    // Authenticate
    let authenticated = try_agent_auth(&mut session, &username).await
        || try_key_file_auth(&mut session, &username).await?;

    if !authenticated {
        bail!("authentication failed for {username}@{host}:{port} — no valid key found");
    }

    // Open channel and exec bridge command
    let channel = session
        .channel_open_session()
        .await
        .context("failed to open SSH session channel")?;
    let mux_bin = std::env::var("MUX_REMOTE_BIN").unwrap_or_else(|_| "mux".to_string());
    // Validate MUX_REMOTE_BIN to prevent command injection on the remote host.
    // Allow only path-like characters: alphanumeric, '/', '-', '_', '.'
    if !mux_bin.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.')) {
        bail!("MUX_REMOTE_BIN contains invalid characters: {mux_bin:?}");
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut command = format!("{mux_bin} bridge --session {session_name} --cols {cols} --rows {rows}");
    if !program.is_empty() {
        command.push_str(" --");
        for arg in program {
            // Shell-escape each argument using single quotes
            command.push(' ');
            command.push('\'');
            command.push_str(&arg.replace('\'', "'\\''"));
            command.push('\'');
        }
    }
    channel
        .exec(true, command)
        .await
        .context("failed to exec bridge command")?;

    // Convert to AsyncRead + AsyncWrite stream and split
    let stream = channel.into_stream();
    let (reader, writer) = tokio::io::split(stream);
    Ok((Box::new(reader), Box::new(writer)))
}

/// Try authenticating via SSH agent. Returns true on success.
async fn try_agent_auth(session: &mut client::Handle<SshHandler>, username: &str) -> bool {
    let mut agent = match russh::keys::agent::client::AgentClient::connect_env().await {
        Ok(a) => a,
        Err(_) => return false,
    };

    let identities = match agent.request_identities().await {
        Ok(ids) => ids,
        Err(_) => return false,
    };

    for identity in identities {
        match session
            .authenticate_publickey_with(username, identity, None, &mut agent)
            .await
        {
            Ok(result) if result.success() => return true,
            _ => continue,
        }
    }

    false
}

/// Try authenticating with key files from ~/.ssh/. Returns true on success.
async fn try_key_file_auth(
    session: &mut client::Handle<SshHandler>,
    username: &str,
) -> Result<bool> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()));
    let key_files = [
        home.join(".ssh/id_ed25519"),
        home.join(".ssh/id_rsa"),
        home.join(".ssh/id_ecdsa"),
    ];

    let rsa_hash = match session.best_supported_rsa_hash().await {
        Ok(opt) => opt.flatten(),
        Err(_) => None,
    };

    for path in &key_files {
        if !path.exists() {
            continue;
        }
        let key = match load_secret_key(path, None) {
            Ok(k) => k,
            Err(_) => continue, // encrypted or unreadable, skip
        };
        let auth_result = session
            .authenticate_publickey(
                username,
                PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash.clone()),
            )
            .await?;
        if auth_result.success() {
            return Ok(true);
        }
    }

    Ok(false)
}
