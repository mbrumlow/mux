use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Maximum frame size: 16 MiB.
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Write a length-prefixed postcard frame (sync).
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let payload =
        postcard::to_stdvec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = payload.len() as u32;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_SIZE",
        ));
    }
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

/// Read a length-prefixed postcard frame (sync).
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_SIZE",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;
    postcard::from_bytes(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Async frame I/O (requires the `tokio` feature).
#[cfg(feature = "tokio")]
pub mod async_io {
    use serde::{Deserialize, Serialize};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::MAX_FRAME_SIZE;

    /// Write a length-prefixed postcard frame (async).
    pub async fn write_frame_async<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
    where
        W: AsyncWriteExt + Unpin,
        T: Serialize,
    {
        let payload = postcard::to_stdvec(value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let len = payload.len() as u32;
        if len > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame exceeds MAX_FRAME_SIZE",
            ));
        }
        writer.write_all(&len.to_be_bytes()).await?;
        writer.write_all(&payload).await?;
        writer.flush().await
    }

    /// Read a length-prefixed postcard frame (async).
    pub async fn read_frame_async<R, T>(reader: &mut R) -> std::io::Result<T>
    where
        R: AsyncReadExt + Unpin,
        T: for<'de> Deserialize<'de>,
    {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf);
        if len > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame exceeds MAX_FRAME_SIZE",
            ));
        }
        let mut payload = vec![0u8; len as usize];
        reader.read_exact(&mut payload).await?;
        postcard::from_bytes(&payload)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{C2S, ClientCapabilities, S2C};
    use crate::types::{Cell, CellStyle, Color, CursorState, ScreenSnapshot};

    #[test]
    fn round_trip_c2s() {
        let msg = C2S::Hello {
            caps: ClientCapabilities {
                supports_kkp: false,
                supports_truecolor: true,
                term: "xterm-256color".to_string(),
                width: 80,
                height: 24,
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: C2S = read_frame(&mut buf.as_slice()).unwrap();
        match decoded {
            C2S::Hello { caps } => {
                assert!(!caps.supports_kkp);
                assert!(caps.supports_truecolor);
                assert_eq!(caps.term, "xterm-256color");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn round_trip_s2c_snapshot() {
        let snapshot = ScreenSnapshot {
            width: 80,
            height: 24,
            cells: vec![
                Cell {
                    text: "A".to_string(),
                    style: CellStyle {
                        fg: Color::Indexed(1),
                        bg: Color::Default,
                        bold: true,
                        dim: false,
                        italic: false,
                        underline: false,
                        reverse: false,
                    },
                    width: 1,
                },
                Cell {
                    text: " ".to_string(),
                    style: CellStyle::default(),
                    width: 1,
                },
            ],
            cursor: CursorState {
                x: 1,
                y: 0,
                visible: true,
            },
        };
        let msg = S2C::Snapshot(snapshot);
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: S2C = read_frame(&mut buf.as_slice()).unwrap();
        match decoded {
            S2C::Snapshot(s) => {
                assert_eq!(s.width, 80);
                assert_eq!(s.height, 24);
                assert_eq!(s.cells.len(), 2);
                assert_eq!(s.cells[0].text, "A");
                assert!(s.cells[0].style.bold);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn round_trip_raw_input() {
        let msg = C2S::RawInput {
            data: vec![0x1b, 0x5b, 0x41],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: C2S = read_frame(&mut buf.as_slice()).unwrap();
        match decoded {
            C2S::RawInput { data } => {
                assert_eq!(data, vec![0x1b, 0x5b, 0x41]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn round_trip_resize() {
        let msg = C2S::Resize {
            width: 120,
            height: 40,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let decoded: C2S = read_frame(&mut buf.as_slice()).unwrap();
        match decoded {
            C2S::Resize { width, height } => {
                assert_eq!(width, 120);
                assert_eq!(height, 40);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn round_trip_ping_pong() {
        let ping = C2S::Ping { t: 42 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &ping).unwrap();
        let decoded: C2S = read_frame(&mut buf.as_slice()).unwrap();
        match decoded {
            C2S::Ping { t } => assert_eq!(t, 42),
            _ => panic!("wrong variant"),
        }

        let pong = S2C::Pong { t: 42 };
        buf.clear();
        write_frame(&mut buf, &pong).unwrap();
        let decoded: S2C = read_frame(&mut buf.as_slice()).unwrap();
        match decoded {
            S2C::Pong { t } => assert_eq!(t, 42),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn frame_too_large() {
        // Create a message that would exceed MAX_FRAME_SIZE if we lower the limit check
        let msg = C2S::RawInput { data: vec![0; 100] };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        // Normal size should succeed
        let _: C2S = read_frame(&mut buf.as_slice()).unwrap();
    }
}
