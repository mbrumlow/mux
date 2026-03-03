use std::io::{Read, Write};

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

pub struct PtyHandle {
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    child_pid: Option<u32>,
}

impl PtyHandle {
    pub fn spawn(
        rows: u16,
        cols: u16,
        program: &[String],
        session_name: &str,
        extra_env: &[(String, String)],
    ) -> Result<(Self, Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = if program.is_empty() {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            CommandBuilder::new(&shell)
        } else {
            let mut cmd = CommandBuilder::new(&program[0]);
            for arg in &program[1..] {
                cmd.arg(arg);
            }
            cmd
        };
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("MUX_SESSION", session_name);
        cmd.env_remove("SSH_TTY");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let child_pid = child.process_id();

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let handle = PtyHandle {
            master: pair.master,
            _child: child,
            child_pid,
        };

        Ok((handle, reader, writer))
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}
