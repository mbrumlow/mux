use serde::{Deserialize, Serialize};

use crate::types::{Frame, KeyEvent, ScreenSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCapabilities {
    pub supports_kkp: bool,
    pub supports_truecolor: bool,
    pub term: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum C2S {
    Hello { caps: ClientCapabilities },
    Input { seq: u64, events: Vec<KeyEvent> },
    RawInput { data: Vec<u8> },
    Resize { width: u16, height: u16 },
    Ping { t: u64 },
    RequestSnapshot,
    KillSession,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum S2C {
    Welcome { session_id: String },
    Snapshot(ScreenSnapshot),
    Frame(Frame),
    Kicked { reason: String },
    Pong { t: u64 },
    SetKkpMode { flags: u32 },
    SetDecMode { mode: u16, enabled: bool },
    Clipboard { params: String, data: String },
    ScreenData { data: Vec<u8> },
    ScreenDiff { data: Vec<u8> },
}
