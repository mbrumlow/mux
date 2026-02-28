mod cli;
mod config;
mod daemon;
mod paths;
mod session_mgmt;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Server { session, rows, cols, program }) => {
            // Server subprocess: init tracing to file (no ANSI)
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .with_ansi(false)
                .init();

            daemon::run_server(session, *rows, *cols, program)
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
                None => {
                    eprintln!("Usage: mux <session-name>");
                    eprintln!("       mux --list");
                    eprintln!("       mux --kill <session>");
                    std::process::exit(1);
                }
            };

            paths::validate_session_name(&name)?;

            if cli.restart {
                let _ = session_mgmt::kill_session(&name);
            }

            session_mgmt::attach(&name, &cli.program)
        }
    }
}
