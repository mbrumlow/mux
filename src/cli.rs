use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mux")]
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
}
