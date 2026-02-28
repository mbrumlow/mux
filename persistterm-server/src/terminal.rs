use std::borrow::Cow;
use std::collections::BTreeSet;

use persistterm_proto::{Cell, CellStyle, Color, CursorState, ScreenSnapshot};

/// DEC private modes that should be forwarded to the client terminal.
const FORWARDED_DEC_MODES: &[u16] = &[1000, 1002, 1003, 1004, 1005, 1006, 2004];

/// Events produced by processing PTY output.
pub struct PtyEvents {
    /// Responses to inject back into the PTY (terminal query answers).
    pub pty_responses: Vec<Vec<u8>>,
    /// If KKP mode changed, the new flags value (0 = disabled).
    pub kkp_changed: Option<u32>,
    /// DEC private mode changes detected in this chunk.
    pub dec_mode_changes: Vec<(u16, bool)>,
    /// Raw OSC bodies to forward to the client (e.g. clipboard).
    pub osc_forwards: Vec<Vec<u8>>,
}

pub struct Terminal {
    parser: vt100::Parser,
    kkp_stack: Vec<u32>,
    dec_modes: BTreeSet<u16>,
    /// Buffer for OSC sequences that span multiple PTY read chunks.
    pending_osc: Vec<u8>,
    /// Previous screen state for computing diffs.
    prev_screen: Option<vt100::Screen>,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            kkp_stack: Vec::new(),
            dec_modes: BTreeSet::new(),
            pending_osc: Vec::new(),
            prev_screen: None,
        }
    }

    /// Process PTY output bytes. Returns events that need handling.
    pub fn process(&mut self, data: &[u8]) -> PtyEvents {
        let old_kkp = self.kkp_flags();

        // Scan for terminal queries and KKP sequences before feeding to parser
        let (rows, cols) = self.size();
        let (cursor_row, cursor_col) = self.parser.screen().cursor_position();
        let mut client_forwards = Vec::new();
        let mut dec_mode_changes = Vec::new();
        let pty_responses = scan_pty_output(
            data,
            rows,
            cols,
            cursor_row,
            cursor_col,
            &mut self.kkp_stack,
            &mut self.dec_modes,
            &mut dec_mode_changes,
            &mut client_forwards,
            &mut self.pending_osc,
        );

        // Patch CSI sequences the vt100 crate doesn't handle, then feed to parser
        let patched = patch_csi_final_bytes(data);
        self.parser.process(&patched);

        let new_kkp = self.kkp_flags();
        PtyEvents {
            pty_responses,
            kkp_changed: if old_kkp != new_kkp {
                Some(new_kkp)
            } else {
                None
            },
            dec_mode_changes,
            osc_forwards: client_forwards,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }

    /// Currently active DEC private modes.
    pub fn dec_modes(&self) -> &BTreeSet<u16> {
        &self.dec_modes
    }

    /// Current KKP flags (0 = disabled / legacy mode).
    pub fn kkp_flags(&self) -> u32 {
        self.kkp_stack.last().copied().unwrap_or(0)
    }

    pub fn snapshot(&self) -> ScreenSnapshot {
        let screen = self.parser.screen();
        let (rows, cols) = (screen.size().0, screen.size().1);
        let mut cells = Vec::with_capacity((rows as usize) * (cols as usize));

        for row in 0..rows {
            for col in 0..cols {
                let cell = screen.cell(row, col);
                cells.push(match cell {
                    Some(c) => convert_cell(c),
                    None => Cell::default(),
                });
            }
        }

        let (cur_row, cur_col) = screen.cursor_position();
        let cursor = CursorState {
            x: cur_col,
            y: cur_row,
            visible: !screen.hide_cursor(),
        };

        ScreenSnapshot {
            width: cols,
            height: rows,
            cells,
            cursor,
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(rows, cols);
    }

    /// Full screen contents as ANSI escape sequences.
    pub fn screen_formatted(&self) -> Vec<u8> {
        self.parser.screen().contents_formatted()
    }

    /// Compute a diff against the previous screen. Returns `None` if the diff is empty.
    /// Updates `prev_screen` to the current screen state.
    pub fn screen_diff(&mut self) -> Option<Vec<u8>> {
        let screen = self.parser.screen();
        let diff = match &self.prev_screen {
            Some(prev) => screen.contents_diff(prev),
            None => screen.contents_formatted(),
        };
        self.prev_screen = Some(screen.clone());
        if diff.is_empty() {
            None
        } else {
            Some(diff)
        }
    }

    /// Reset the previous screen to the current state.
    /// Call after sending a full `ScreenData` to establish the diff baseline.
    pub fn reset_prev_screen(&mut self) {
        self.prev_screen = Some(self.parser.screen().clone());
    }
}

fn convert_cell(cell: &vt100::Cell) -> Cell {
    let text = cell.contents();
    let width = if cell.is_wide() {
        2
    } else if cell.is_wide_continuation() {
        0
    } else {
        1
    };

    Cell {
        text: if text.is_empty() {
            " ".to_string()
        } else {
            text
        },
        style: CellStyle {
            fg: convert_color(cell.fgcolor()),
            bg: convert_color(cell.bgcolor()),
            bold: cell.bold(),
            dim: false,
            italic: cell.italic(),
            underline: cell.underline(),
            reverse: cell.inverse(),
        },
        width,
    }
}

fn convert_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Default,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Patch CSI sequences that the vt100 crate doesn't handle.
///
/// - HVP (CSI Ps;Ps f) → CUP (CSI Ps;Ps H): btop uses HVP for all cursor
///   positioning but vt100 only handles CUP.
///
/// Returns a `Cow` to avoid allocating when no patching is needed.
fn patch_csi_final_bytes(data: &[u8]) -> Cow<'_, [u8]> {
    // First pass: check if any patching is needed
    let mut needs_patch = false;
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            i += 2;
            while i < data.len() && data[i] < 0x40 {
                i += 1;
            }
            if i < data.len() {
                if data[i] == b'f' {
                    needs_patch = true;
                    break;
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    if !needs_patch {
        return Cow::Borrowed(data);
    }

    // Second pass: clone and patch
    let mut result = data.to_vec();
    let mut i = 0;
    while i < result.len() {
        if result[i] == 0x1b && i + 1 < result.len() && result[i + 1] == b'[' {
            i += 2;
            while i < result.len() && result[i] < 0x40 {
                i += 1;
            }
            if i < result.len() {
                if result[i] == b'f' {
                    result[i] = b'H';
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    Cow::Owned(result)
}

/// Scan PTY output for terminal queries, KKP sequences, and OSC queries.
/// Returns responses to inject back into the PTY.
fn scan_pty_output(
    data: &[u8],
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    kkp_stack: &mut Vec<u32>,
    dec_modes: &mut BTreeSet<u16>,
    dec_mode_changes: &mut Vec<(u16, bool)>,
    client_forwards: &mut Vec<Vec<u8>>,
    pending_osc: &mut Vec<u8>,
) -> Vec<Vec<u8>> {
    let mut responses = Vec::new();
    let mut i = 0;

    // If we have a pending partial OSC from a previous chunk, try to complete it
    if !pending_osc.is_empty() {
        // Search for a terminator in the new data
        let mut found = false;
        for j in 0..data.len() {
            if data[j] == 0x07 {
                // BEL terminator
                pending_osc.extend_from_slice(&data[..j]);
                handle_osc(pending_osc, &mut responses, client_forwards);
                pending_osc.clear();
                i = j + 1;
                found = true;
                break;
            }
            if data[j] == 0x1b && j + 1 < data.len() && data[j + 1] == b'\\' {
                // ST terminator
                pending_osc.extend_from_slice(&data[..j]);
                handle_osc(pending_osc, &mut responses, client_forwards);
                pending_osc.clear();
                i = j + 2;
                found = true;
                break;
            }
        }
        if !found {
            // Still no terminator — accumulate entire chunk (with a size limit)
            if pending_osc.len() + data.len() < 1024 * 1024 {
                pending_osc.extend_from_slice(data);
            } else {
                // Too large, drop it
                pending_osc.clear();
            }
            return responses;
        }
    }

    while i < data.len() {
        // CSI sequences: ESC [ ...
        if data[i] == 0x1b
            && i + 1 < data.len()
            && data[i + 1] == b'['
            && i + 2 < data.len()
        {
            // ── KKP: CSI ? u — query current keyboard mode ──────────
            if data[i + 2] == b'?' {
                // Check for CSI ? u (query) or CSI ? flags u (not expected in output)
                let mut j = i + 3;
                // Skip digits
                while j < data.len() && data[j].is_ascii_digit() {
                    j += 1;
                }
                if j < data.len() && data[j] == b'u' {
                    // KKP query — respond with current flags
                    let flags = kkp_stack.last().copied().unwrap_or(0);
                    responses.push(format!("\x1b[?{flags}u").into_bytes());
                    i = j + 1;
                    continue;
                }
                // ── DEC private mode set/reset: CSI ? NNNN h/l ─────
                if j < data.len() && (data[j] == b'h' || data[j] == b'l') {
                    if let Ok(mode) = std::str::from_utf8(&data[i + 3..j])
                        .unwrap_or("")
                        .parse::<u16>()
                    {
                        if FORWARDED_DEC_MODES.contains(&mode) {
                            let enabled = data[j] == b'h';
                            let changed = if enabled {
                                dec_modes.insert(mode)
                            } else {
                                dec_modes.remove(&mode)
                            };
                            if changed {
                                dec_mode_changes.push((mode, enabled));
                            }
                            i = j + 1;
                            continue;
                        }
                    }
                }
            }

            // ── KKP: CSI > Ps u — push keyboard mode ───────────────
            if data[i + 2] == b'>' {
                let mut j = i + 3;
                let param_start = j;
                while j < data.len() && data[j].is_ascii_digit() {
                    j += 1;
                }
                if j < data.len() && data[j] == b'u' {
                    let flags: u32 = if j > param_start {
                        std::str::from_utf8(&data[param_start..j])
                            .unwrap_or("0")
                            .parse()
                            .unwrap_or(0)
                    } else {
                        0
                    };
                    kkp_stack.push(flags);
                    i = j + 1;
                    continue;
                }
                // Not a KKP push — check for DA2
                if data[i + 3] == b'c' {
                    responses.push(b"\x1b[>0;300;0c".to_vec());
                    i += 4;
                    continue;
                }
                if i + 4 < data.len()
                    && data[i + 3] == b'0'
                    && data[i + 4] == b'c'
                {
                    responses.push(b"\x1b[>0;300;0c".to_vec());
                    i += 5;
                    continue;
                }
            }

            // ── KKP: CSI < [Ps] u — pop keyboard mode ──────────────
            if data[i + 2] == b'<' {
                let mut j = i + 3;
                let param_start = j;
                while j < data.len() && data[j].is_ascii_digit() {
                    j += 1;
                }
                if j < data.len() && data[j] == b'u' {
                    let count: usize = if j > param_start {
                        std::str::from_utf8(&data[param_start..j])
                            .unwrap_or("1")
                            .parse()
                            .unwrap_or(1)
                    } else {
                        1
                    };
                    for _ in 0..count {
                        kkp_stack.pop();
                    }
                    i = j + 1;
                    continue;
                }
            }

            // ── KKP: CSI = flags ; mode u — set keyboard mode ──────
            if data[i + 2] == b'=' {
                let mut j = i + 3;
                // Skip digits, semicolons, digits
                while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
                    j += 1;
                }
                if j < data.len() && data[j] == b'u' {
                    // Parse "flags;mode"
                    let param_str =
                        std::str::from_utf8(&data[i + 3..j]).unwrap_or("0");
                    let mut parts = param_str.split(';');
                    let flags: u32 = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let mode: u32 = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1);
                    let current = kkp_stack.last().copied().unwrap_or(0);
                    let new_flags = match mode {
                        1 => flags,            // set exactly
                        2 => current | flags,  // OR (set bits)
                        3 => current & !flags, // AND NOT (clear bits)
                        _ => flags,
                    };
                    if kkp_stack.is_empty() {
                        kkp_stack.push(new_flags);
                    } else {
                        *kkp_stack.last_mut().unwrap() = new_flags;
                    }
                    i = j + 1;
                    continue;
                }
            }

            // DA1: \x1b[c or \x1b[0c
            if data[i + 2] == b'c' {
                responses.push(b"\x1b[?62;22c".to_vec());
                i += 3;
                continue;
            }
            if data[i + 2] == b'0' && i + 3 < data.len() && data[i + 3] == b'c' {
                responses.push(b"\x1b[?62;22c".to_vec());
                i += 4;
                continue;
            }
            // DSR status: \x1b[5n
            if data[i + 2] == b'5' && i + 3 < data.len() && data[i + 3] == b'n' {
                responses.push(b"\x1b[0n".to_vec());
                i += 4;
                continue;
            }
            // DSR cursor position: \x1b[6n
            if data[i + 2] == b'6' && i + 3 < data.len() && data[i + 3] == b'n' {
                // Report 1-based cursor position from the parser's current state
                responses.push(
                    format!("\x1b[{};{}R", cursor_row + 1, cursor_col + 1).into_bytes(),
                );
                i += 4;
                continue;
            }
            // XTWINOPS: \x1b[18t — report terminal size in characters
            if data[i + 2] == b'1'
                && i + 4 < data.len()
                && data[i + 3] == b'8'
                && data[i + 4] == b't'
            {
                responses.push(format!("\x1b[8;{};{}t", rows, cols).into_bytes());
                i += 5;
                continue;
            }
            // XTWINOPS: \x1b[14t — report terminal size in pixels (approximate)
            if data[i + 2] == b'1'
                && i + 4 < data.len()
                && data[i + 3] == b'4'
                && data[i + 4] == b't'
            {
                let px_height = rows as u32 * 16;
                let px_width = cols as u32 * 8;
                responses.push(format!("\x1b[4;{};{}t", px_height, px_width).into_bytes());
                i += 5;
                continue;
            }
            i += 2;
            continue;
        }

        // OSC sequences: ESC ] ... (terminated by BEL or ST)
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b']' {
            if let Some((osc_body, end)) = parse_osc(&data[i + 2..]) {
                let end_abs = i + 2 + end;
                handle_osc(osc_body, &mut responses, client_forwards);
                i = end_abs;
                continue;
            } else {
                // No terminator found — buffer the partial OSC for next chunk
                pending_osc.extend_from_slice(&data[i + 2..]);
                break;
            }
        }

        i += 1;
    }
    responses
}

/// Parse an OSC body from the data, returning the body slice and the index
/// past the terminator. OSC is terminated by BEL (0x07) or ST (ESC \).
fn parse_osc(data: &[u8]) -> Option<(&[u8], usize)> {
    for i in 0..data.len() {
        // BEL terminator
        if data[i] == 0x07 {
            return Some((&data[..i], i + 1));
        }
        // ST terminator: ESC backslash
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
            return Some((&data[..i], i + 2));
        }
    }
    None
}

/// Handle recognized OSC queries and push responses.
/// OSC 52 clipboard sequences are forwarded to the client instead.
fn handle_osc(body: &[u8], responses: &mut Vec<Vec<u8>>, client_forwards: &mut Vec<Vec<u8>>) {
    // OSC 52 — clipboard set: forward to client
    if body.starts_with(b"52;") {
        client_forwards.push(body.to_vec());
        return;
    }

    // OSC 10 ; ? ST — query foreground color
    // Respond with a dark-theme default (light gray)
    if body == b"10;?" {
        responses.push(b"\x1b]10;rgb:d4d4/d4d4/d4d4\x1b\\".to_vec());
        return;
    }

    // OSC 11 ; ? ST — query background color
    // Respond with a dark-theme default (near-black)
    if body == b"11;?" {
        responses.push(b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\".to_vec());
        return;
    }

    // OSC 12 ; ? ST — query cursor color
    if body == b"12;?" {
        responses.push(b"\x1b]12;rgb:d4d4/d4d4/d4d4\x1b\\".to_vec());
    }
}
