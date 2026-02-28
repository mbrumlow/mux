use std::io::Read;

use tokio::sync::mpsc;

pub struct RawInput {
    _guard: RawModeGuard,
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl RawInput {
    pub fn enable() -> std::io::Result<Self> {
        let guard = RawModeGuard::enable()?;
        Ok(Self { _guard: guard })
    }
}

pub fn spawn_stdin_reader(tx: mpsc::Sender<Vec<u8>>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}
