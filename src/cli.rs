use clap::{Parser, Subcommand};

const VERSION: &str = match option_env!("MUX_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(name = "mux", version = VERSION)]
pub struct Cli {
    /// Session name to attach to or create
    pub name: Option<String>,

    /// Restart: kill existing session, then create and attach
    #[arg(short = 'r', requires = "name")]
    pub restart: bool,

    /// Kill a session
    #[arg(long, value_name = "SESSION")]
    pub kill: Option<String>,

    /// List active sessions
    #[arg(long, alias = "ls")]
    pub list: bool,

    /// Program and arguments to run instead of $SHELL
    #[arg(last = true)]
    pub program: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Internal: run server (used for daemonization)
    #[command(hide = true)]
    Server {
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(last = true)]
        program: Vec<String>,
    },
    /// Internal: bridge stdin/stdout to a session socket (used by SSH remote)
    #[command(hide = true)]
    Bridge {
        #[arg(long)]
        session: String,
    },
}

pub enum SessionTarget {
    Local(String),
    Remote { host: String, session: String },
}

pub fn parse_target(name: &str) -> SessionTarget {
    match name.split_once('/') {
        Some((host, session)) => SessionTarget::Remote {
            host: host.to_string(),
            session: session.to_string(),
        },
        None => SessionTarget::Local(name.to_string()),
    }
}
