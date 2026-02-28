use std::path::Path;

use anyhow::Result;
use tokio::net::UnixStream;

pub async fn connect(path: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(path).await?;
    Ok(stream)
}
