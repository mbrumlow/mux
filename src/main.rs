mod cli;
mod config;
mod daemon;
mod paths;
mod session_mgmt;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands, SessionTarget};

/// Default log filter: only mux's own crates. The embedded terminal emulator
/// (wezterm-term) logs raw escape-sequence payloads — i.e. session content —
/// at warn/info level via the `log` bridge, so everything outside our crates
/// stays off unless RUST_LOG explicitly enables it.
fn default_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("off,mux=info,persistterm_server=info,persistterm_client=info,persistterm_proto=info")
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Server { session, rows, cols, program }) => {
            paths::validate_session_name(session)?;

            // Server subprocess: init tracing to file (no ANSI)
            tracing_subscriber::fmt()
                .with_env_filter(default_env_filter())
                .with_ansi(false)
                .init();

            daemon::run_server(session, *rows, *cols, program)
        }
        Some(Commands::Bridge { session, cols, rows, program }) => {
            paths::validate_session_name(session)?;
            let initial_size = cols.zip(*rows);
            daemon::run_bridge(session, program, initial_size)
        }
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
