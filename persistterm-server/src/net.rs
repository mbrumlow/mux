use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::net::{UnixListener, UnixStream};

pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    pub fn bind(path: &Path) -> Result<Self> {
        // Remove stale socket file if it exists
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let inner = UnixListener::bind(path)?;
        Ok(Self {
            inner,
            path: path.to_path_buf(),
        })
    }

    pub async fn accept(&self) -> Result<UnixStream> {
        let (stream, _addr) = self.inner.accept().await?;
        Ok(stream)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        // Clean up the advisory lock file alongside the socket
        let lock_path = self.path.with_extension("lock");
        let _ = std::fs::remove_file(lock_path);
    }
}
