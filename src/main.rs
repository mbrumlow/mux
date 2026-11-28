mod cli;
mod config;
mod daemon;
mod paths;
mod session_mgmt;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands, SessionTarget};

/// Default log filter: only mux's own crates. The embedded terminal emulator
/// (wezterm-term) logs raw escape-sequence payloads — i.e. session content —
/// at warn/info level via the `log` bridge, so everything outside our crates
/// stays off unless RUST_LOG explicitly enables it.
fn default_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "off,mux=info,persistterm_server=info,persistterm_client=info,persistterm_proto=info",
        )
    })
}

/// Send a URL open request to the browser sideband socket.
/// This is a synchronous helper (no tokio) invoked by `mux open-url <url>`.
fn open_url(url: &str) -> Result<()> {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;

    let sock_path = std::env::var("MUX_BROWSER_SOCKET")
        .context("MUX_BROWSER_SOCKET not set — not inside a mux session?")?;
    let token = std::env::var("MUX_BROWSER_TOKEN")
        .context("MUX_BROWSER_TOKEN not set — not inside a mux session?")?;

    let mut stream = UnixStream::connect(&sock_path)
        .with_context(|| format!("failed to connect to browser socket at {sock_path}"))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;

    writeln!(stream, "{token} {url}")?;
    stream.flush()?;

    let mut reader = std::io::BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;

    let response = response.trim();
    if response == "OK" {
        Ok(())
    } else {
        anyhow::bail!("browser socket rejected request: {response}");
    }
}

fn main() -> Result<()> {
    // Invoked through the `mux-open-url` symlink ($BROWSER helper): the URL is
    // the first argument and there is nothing else to parse.
    let mut args = std::env::args_os();
    let invoked_as = args.next().map(std::path::PathBuf::from);
    if invoked_as.as_deref().and_then(|p| p.file_name())
        == Some(std::ffi::OsStr::new(paths::BROWSER_HELPER_NAME))
    {
        let url = args.next().context("usage: mux-open-url <url>")?;
        let url = url.to_str().context("URL is not valid UTF-8")?;
        return open_url(url);
    }

    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Server {
            session,
            rows,
            cols,
            program,
        }) => {
            paths::validate_session_name(session)?;

            // Server subprocess: init tracing to file (no ANSI)
            tracing_subscriber::fmt()
                .with_env_filter(default_env_filter())
                .with_ansi(false)
                .init();

            daemon::run_server(session, *rows, *cols, program)
        }
        Some(Commands::Bridge {
            session,
            cols,
            rows,
            program,
        }) => {
            paths::validate_session_name(session)?;
            let initial_size = cols.zip(*rows);
            daemon::run_bridge(session, program, initial_size)
        }
        Some(Commands::OpenUrl { url }) => open_url(url),
        None if cli.list => session_mgmt::list_sessions(),
        None if cli.kill.is_some() => {
            let name = cli.kill.as_ref().unwrap();
            paths::validate_session_name(name)?;
            session_mgmt::kill_session(name)
        }
        None => {
            let name = match &cli.name {
                Some(n) => n.clone(),
                None => "default".to_string(),
            };

            match cli::parse_target(&name) {
                SessionTarget::Local(session) => {
                    paths::validate_session_name(&session)?;

                    // Init tracing to client log file
                    paths::ensure_dirs()?;
                    let log_path = paths::client_log_path("local", &session);
                    let log_file = paths::open_log_file(&log_path)?;
                    tracing_subscriber::fmt()
                        .with_env_filter(default_env_filter())
                        .with_ansi(false)
                        .with_writer(log_file)
                        .init();

                    if cli.restart {
                        let _ = session_mgmt::kill_session(&session);
                    }

                    session_mgmt::attach(&session, &cli.program)
                }
                SessionTarget::Remote { host, session } => {
                    paths::validate_session_name(&session)?;

                    // Init tracing to client log file
                    paths::ensure_dirs()?;
                    let log_path = paths::client_log_path(&host, &session);
                    let log_file = paths::open_log_file(&log_path)?;
                    tracing_subscriber::fmt()
                        .with_env_filter(default_env_filter())
                        .with_ansi(false)
                        .with_writer(log_file)
                        .init();

                    session_mgmt::attach_remote(&host, &session, &cli.program)
                }
            }
        }
    }
}
