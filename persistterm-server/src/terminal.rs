use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;

use tattoy_termwiz::color::ColorAttribute;
use tattoy_termwiz::surface::CursorVisibility;
use tattoy_wezterm_term::color::ColorPalette;
use tattoy_wezterm_term::{StableRowIndex, TerminalConfiguration, TerminalSize};

use persistterm_proto::{Cell, CellStyle, Color, CursorState, ScreenSnapshot};

type SequenceNo = tattoy_termwiz::surface::SequenceNo;

/// DEC private modes that should be forwarded to the client terminal.
const FORWARDED_DEC_MODES: &[u16] = &[1000, 1002, 1003, 1004, 1005, 1006, 2004];

/// Events produced by processing PTY output.
pub struct PtyEvents {
    /// If KKP mode changed, the new flags value (0 = disabled).
    pub kkp_changed: Option<u32>,
    /// DEC private mode changes detected in this chunk.
    pub dec_mode_changes: Vec<(u16, bool)>,
    /// Raw OSC bodies to forward to the client (e.g. clipboard).
    pub osc_forwards: Vec<Vec<u8>>,
}

/// Minimal terminal configuration for wezterm-term.
#[derive(Debug)]
struct MuxTermConfig;

impl TerminalConfiguration for MuxTermConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn enable_kitty_keyboard(&self) -> bool {
        true
    }
}

/// Previous frame state for efficient diffing.
struct PrevFrame {
    /// ANSI rendering of each visible line (row-indexed).
    line_renders: Vec<Vec<u8>>,
    /// Seqno of each line at the time it was rendered.
    line_seqnos: Vec<SequenceNo>,
    /// Stable row index of each visible line (for scroll detection).
    stable_ids: Vec<StableRowIndex>,
    /// Cursor state at time of capture.
    cursor_x: usize,
    cursor_y: usize,
    cursor_visible: bool,
}

pub struct Terminal {
    inner: tattoy_wezterm_term::Terminal,
    kkp_stack: Vec<u32>,
    dec_modes: BTreeSet<u16>,
    /// Buffer for OSC sequences that span multiple PTY read chunks.
    pending_osc: Vec<u8>,
    /// Previous frame for computing diffs.
    prev_frame: Option<PrevFrame>,
    /// Whether the application has an active synchronized update (DEC mode 2026).
    app_sync_active: bool,
    size: (u16, u16),
}

impl Terminal {
    pub fn new(rows: u16, cols: u16, pty_writer: Box<dyn Write + Send>) -> Self {
        let size = TerminalSize {
            rows: rows as usize,
            cols: cols as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };

        let inner = tattoy_wezterm_term::Terminal::new(
            size,
            Arc::new(MuxTermConfig),
            "mux",
            "0.1.0",
            pty_writer,
        );

        Self {
            inner,
            kkp_stack: Vec::new(),
            dec_modes: BTreeSet::new(),
            pending_osc: Vec::new(),
            prev_frame: None,
            app_sync_active: false,
            size: (rows, cols),
        }
    }

    /// Process PTY output bytes. Returns events that need handling.
    pub fn process(&mut self, data: &[u8]) -> PtyEvents {
        let old_kkp = self.kkp_flags();

        // Scan for KKP/DEC/OSC changes (detection only)
        let mut client_forwards = Vec::new();
        let mut dec_mode_changes = Vec::new();
        scan_pty_output(
            data,
            &mut self.kkp_stack,
            &mut self.dec_modes,
            &mut dec_mode_changes,
            &mut client_forwards,
            &mut self.pending_osc,
            &mut self.app_sync_active,
        );

        // wezterm-term handles ALL VT emulation and query responses
        self.inner.advance_bytes(data);

        let new_kkp = self.kkp_flags();
        PtyEvents {
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
        self.size
    }

    /// Currently active DEC private modes.
    pub fn dec_modes(&self) -> &BTreeSet<u16> {
        &self.dec_modes
    }

    /// Current KKP flags (0 = disabled / legacy mode).
    pub fn kkp_flags(&self) -> u32 {
        self.kkp_stack.last().copied().unwrap_or(0)
    }

    /// Whether the application has an active synchronized update (DEC 2026).
    pub fn is_app_sync_active(&self) -> bool {
        self.app_sync_active
    }

    pub fn snapshot(&mut self) -> ScreenSnapshot {
        let (rows, cols) = (self.size.0 as usize, self.size.1 as usize);
        let mut cells = Vec::with_capacity(rows * cols);

        let screen = self.inner.screen_mut();
        let vis_range = 0..rows as i64;
        screen.with_phys_lines_mut(screen.phys_range(&vis_range), |lines| {
            for line in lines {
                for col in 0..cols {
                    if let Some(cell_ref) = line.get_cell(col) {
                        cells.push(convert_cell_ref(&cell_ref));
                    } else {
                        cells.push(Cell::default());
                    }
                }
            }
        });

        while cells.len() < rows * cols {
            cells.push(Cell::default());
        }

        let cursor_pos = self.inner.cursor_pos();
        let cursor = CursorState {
            x: cursor_pos.x as u16,
            y: cursor_pos.y as u16,
            visible: cursor_pos.visibility == CursorVisibility::Visible,
        };

        ScreenSnapshot {
            width: cols as u16,
            height: rows as u16,
            cells,
            cursor,
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.inner.resize(TerminalSize {
            rows: rows as usize,
            cols: cols as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        });
        self.size = (rows, cols);
        self.prev_frame = None;
    }

    /// Full screen contents as ANSI escape sequences.
    pub fn screen_formatted(&mut self) -> Vec<u8> {
        let (rows, cols) = (self.size.0 as usize, self.size.1 as usize);
        let mut out = Vec::with_capacity(rows * cols * 3);

        // Reset scroll region, attrs, home cursor, clear screen
        out.extend_from_slice(b"\x1b[r\x1b[0m\x1b[H\x1b[2J");

        let screen = self.inner.screen_mut();
        let vis_range = 0..rows as i64;
        screen.with_phys_lines_mut(screen.phys_range(&vis_range), |lines| {
            for (row, line) in lines.iter().enumerate() {
                write_cup(&mut out, row, 0);
                render_line_ansi(&mut out, line, cols);
            }
        });

        out.extend_from_slice(b"\x1b[0m");
        let cursor_pos = self.inner.cursor_pos();
        write_cup(&mut out, cursor_pos.y as usize, cursor_pos.x);
        if cursor_pos.visibility == CursorVisibility::Visible {
            out.extend_from_slice(b"\x1b[?25h");
        } else {
            out.extend_from_slice(b"\x1b[?25l");
        }

        out
    }

    /// Compute a diff against the previous frame. Returns `None` if nothing changed.
    pub fn screen_diff(&mut self) -> Option<Vec<u8>> {
        let (rows, cols) = (self.size.0 as usize, self.size.1 as usize);
        let cursor_pos = self.inner.cursor_pos();
        let cursor_x = cursor_pos.x;
        let cursor_y = cursor_pos.y as usize;
        let cursor_visible = cursor_pos.visibility == CursorVisibility::Visible;

        let vis_range = 0..rows as i64;

        // Gather current stable IDs for all visible rows (immutable access)
        let mut cur_stable_ids = Vec::with_capacity(rows);
        {
            let screen = self.inner.screen();
            for vis_row in 0..rows as i64 {
                cur_stable_ids.push(screen.visible_row_to_stable_row(vis_row));
            }
        }

        let has_valid_prev = matches!(&self.prev_frame, Some(prev) if prev.line_renders.len() == rows);

        if !has_valid_prev {
            // No valid previous frame — capture and return full screen
            let mut frame = PrevFrame {
                line_renders: Vec::with_capacity(rows),
                line_seqnos: Vec::with_capacity(rows),
                stable_ids: cur_stable_ids,
                cursor_x,
                cursor_y,
                cursor_visible,
            };
            let screen = self.inner.screen_mut();
            screen.with_phys_lines_mut(screen.phys_range(&vis_range), |lines| {
                for line in lines {
                    let mut buf = Vec::with_capacity(cols * 2);
                    render_line_ansi(&mut buf, line, cols);
                    frame.line_renders.push(buf);
                    frame.line_seqnos.push(line.current_seqno());
                }
            });
            while frame.line_renders.len() < rows {
                frame.line_renders.push(Vec::new());
                frame.line_seqnos.push(0);
            }
            self.prev_frame = Some(frame);
            return Some(self.screen_formatted());
        }

        // ── Scroll detection ──
        let scroll = detect_scroll(
            &self.prev_frame.as_ref().unwrap().stable_ids,
            &cur_stable_ids,
            rows,
        );

        let mut out = Vec::new();

        // Take ownership of prev_frame (confirmed Some with correct size above).
        let mut prev_frame = self.prev_frame.take().unwrap();
        let prev_cursor_x = prev_frame.cursor_x;
        let prev_cursor_y = prev_frame.cursor_y;
        let prev_cursor_visible = prev_frame.cursor_visible;

        // Apply scroll if detected
        if let Some((region_top, region_bot, shift)) = scroll {
            let abs_shift = shift.unsigned_abs();

            if shift > 0 {
                // Scroll up: lines moved up by `shift` rows
                // Emit: set scroll region, scroll up, reset scroll region
                write_scroll_region(&mut out, region_top, region_bot);
                write_cup(&mut out, region_top, 0);
                let _ = write!(out, "\x1b[{}S", abs_shift); // scroll up
                out.extend_from_slice(b"\x1b[r"); // reset scroll region

                // Shift cached renders to match
                let mut new_renders: Vec<Option<Vec<u8>>> = vec![None; rows];
                let mut new_seqnos = vec![0usize; rows];

                for row in 0..rows {
                    if row >= region_top && row < region_bot {
                        let src = row + abs_shift;
                        if src < region_bot && src < prev_frame.line_renders.len() {
                            new_renders[row] = Some(std::mem::take(&mut prev_frame.line_renders[src]));
                            new_seqnos[row] = prev_frame.line_seqnos[src];
                        }
                        // else: new line at bottom of region, will be rendered below
                    } else if row < prev_frame.line_renders.len() {
                        // Outside scroll region: unchanged
                        new_renders[row] = Some(std::mem::take(&mut prev_frame.line_renders[row]));
                        new_seqnos[row] = prev_frame.line_seqnos[row];
                    }
                }
                prev_frame.line_renders = new_renders.into_iter().map(|r| r.unwrap_or_default()).collect();
                prev_frame.line_seqnos = new_seqnos;
            } else if shift < 0 {
                // Scroll down: lines moved down by |shift| rows
                write_scroll_region(&mut out, region_top, region_bot);
                write_cup(&mut out, region_top, 0);
                let _ = write!(out, "\x1b[{}T", abs_shift); // scroll down
                out.extend_from_slice(b"\x1b[r");

                let mut new_renders: Vec<Option<Vec<u8>>> = vec![None; rows];
                let mut new_seqnos = vec![0usize; rows];

                for row in 0..rows {
                    if row >= region_top && row < region_bot {
                        if row >= region_top + abs_shift {
                            let src = row - abs_shift;
                            if src >= region_top && src < prev_frame.line_renders.len() {
                                new_renders[row] = Some(std::mem::take(&mut prev_frame.line_renders[src]));
                                new_seqnos[row] = prev_frame.line_seqnos[src];
                            }
                        }
                        // else: new line at top of region
                    } else if row < prev_frame.line_renders.len() {
                        new_renders[row] = Some(std::mem::take(&mut prev_frame.line_renders[row]));
                        new_seqnos[row] = prev_frame.line_seqnos[row];
                    }
                }
                prev_frame.line_renders = new_renders.into_iter().map(|r| r.unwrap_or_default()).collect();
                prev_frame.line_seqnos = new_seqnos;
            }
        }

        // ── Line-by-line diff (catches scroll-introduced new lines + other changes) ──
        let mut new_frame = PrevFrame {
            line_renders: Vec::with_capacity(rows),
            line_seqnos: Vec::with_capacity(rows),
            stable_ids: cur_stable_ids,
            cursor_x,
            cursor_y,
            cursor_visible,
        };

        {
            let screen = self.inner.screen_mut();
            screen.with_phys_lines_mut(screen.phys_range(&vis_range), |lines| {
                for (row, line) in lines.iter().enumerate() {
                    if row >= rows {
                        break;
                    }
                    let line_seqno = line.current_seqno();

                    // Fast path: seqno unchanged means line content is identical
                    if row < prev_frame.line_seqnos.len()
                        && !line.changed_since(prev_frame.line_seqnos[row])
                        && !prev_frame.line_renders[row].is_empty()
                    {
                        new_frame.line_renders.push(
                            std::mem::take(&mut prev_frame.line_renders[row]),
                        );
                        new_frame.line_seqnos.push(prev_frame.line_seqnos[row]);
                        continue;
                    }

                    // Render current line
                    let mut buf = Vec::with_capacity(cols * 2);
                    render_line_ansi(&mut buf, line, cols);

                    // Compare with cached render
                    if row < prev_frame.line_renders.len()
                        && buf == prev_frame.line_renders[row]
                    {
                        // Content identical even though seqno changed
                        new_frame.line_renders.push(buf);
                        new_frame.line_seqnos.push(line_seqno);
                        continue;
                    }

                    // Emit changed line: CUP + content (no erase — render pads to full width)
                    write_cup(&mut out, row, 0);
                    out.extend_from_slice(&buf);
                    new_frame.line_renders.push(buf);
                    new_frame.line_seqnos.push(line_seqno);
                }
            });
        }

        // Pad if fewer lines than expected
        while new_frame.line_renders.len() < rows {
            new_frame.line_renders.push(Vec::new());
            new_frame.line_seqnos.push(0);
        }

        // ── Cursor ──
        if !out.is_empty() {
            // Wrap in synchronized output so the terminal renders everything
            // in one atomic frame — prevents cursor flicker during line writes.
            let mut framed = Vec::with_capacity(out.len() + 64);
            framed.extend_from_slice(b"\x1b[?2026h");
            framed.append(&mut out);
            // Reposition cursor after content writes
            write_cup(&mut framed, cursor_y, cursor_x);
            if cursor_visible != prev_cursor_visible {
                if cursor_visible {
                    framed.extend_from_slice(b"\x1b[?25h");
                } else {
                    framed.extend_from_slice(b"\x1b[?25l");
                }
            }
            framed.extend_from_slice(b"\x1b[?2026l");
            out = framed;
        } else if cursor_visible != prev_cursor_visible {
            // No content changes, but cursor visibility changed
            if cursor_visible {
                out.extend_from_slice(b"\x1b[?25h");
            } else {
                out.extend_from_slice(b"\x1b[?25l");
            }
        } else if cursor_x != prev_cursor_x || cursor_y != prev_cursor_y {
            // No content changes, but cursor moved
            write_cup(&mut out, cursor_y, cursor_x);
        }

        self.prev_frame = Some(new_frame);

        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// Reset the previous screen to the current state.
    pub fn reset_prev_screen(&mut self) {
        let (rows, cols) = (self.size.0 as usize, self.size.1 as usize);
        let cursor_pos = self.inner.cursor_pos();

        let vis_range = 0..rows as i64;

        let mut frame = PrevFrame {
            line_renders: Vec::with_capacity(rows),
            line_seqnos: Vec::with_capacity(rows),
            stable_ids: Vec::with_capacity(rows),
            cursor_x: cursor_pos.x,
            cursor_y: cursor_pos.y as usize,
            cursor_visible: cursor_pos.visibility == CursorVisibility::Visible,
        };

        {
            let screen = self.inner.screen();
            for vis_row in 0..rows as i64 {
                frame
                    .stable_ids
                    .push(screen.visible_row_to_stable_row(vis_row));
            }
        }

        let screen = self.inner.screen_mut();
        screen.with_phys_lines_mut(screen.phys_range(&vis_range), |lines| {
            for line in lines {
                let mut buf = Vec::with_capacity(cols * 2);
                render_line_ansi(&mut buf, line, cols);
                frame.line_renders.push(buf);
                frame.line_seqnos.push(line.current_seqno());
            }
        });

        while frame.line_renders.len() < rows {
            frame.line_renders.push(Vec::new());
            frame.line_seqnos.push(0);
        }

        self.prev_frame = Some(frame);
    }
}

/// Detect a uniform scroll within a contiguous region of the screen.
///
/// Compares previous and current stable row IDs to find a shift.
/// Returns `Some((region_top, region_bot_exclusive, shift))` where shift > 0
/// means scroll up (content moved up), shift < 0 means scroll down.
fn detect_scroll(
    prev_stable: &[StableRowIndex],
    cur_stable: &[StableRowIndex],
    rows: usize,
) -> Option<(usize, usize, isize)> {
    if prev_stable.len() != rows || cur_stable.len() != rows || rows < 2 {
        return None;
    }

    // For each current row, find where its stable ID was in the previous frame.
    // Compute the shift (prev_pos - cur_pos) for each. The dominant non-zero
    // shift across a contiguous region is our scroll.
    let mut shifts = vec![0isize; rows];
    let mut has_shift = vec![false; rows];

    for cur_row in 0..rows {
        let stable = cur_stable[cur_row];
        // Look for this stable ID in the previous frame
        if let Some(prev_row) = prev_stable.iter().position(|&s| s == stable) {
            let s = prev_row as isize - cur_row as isize;
            if s != 0 {
                shifts[cur_row] = s;
                has_shift[cur_row] = true;
            }
        }
    }

    // Find the longest contiguous run of the same non-zero shift
    let mut best_shift: isize = 0;
    let mut best_top = 0;
    let mut best_len = 0;

    let mut run_shift: isize = 0;
    let mut run_start = 0;
    let mut run_len = 0;

    for row in 0..rows {
        if has_shift[row] && shifts[row] == run_shift && run_len > 0 {
            run_len += 1;
        } else if has_shift[row] && shifts[row] != 0 {
            // Start new run
            run_shift = shifts[row];
            run_start = row;
            run_len = 1;
        } else {
            // Row didn't shift or shift is 0 — break the run
            if run_len > best_len {
                best_len = run_len;
                best_shift = run_shift;
                best_top = run_start;
            }
            run_len = 0;
        }
    }
    if run_len > best_len {
        best_len = run_len;
        best_shift = run_shift;
        best_top = run_start;
    }

    // Only use scroll optimization if it covers a meaningful portion
    // (at least 3 lines shifted the same way)
    if best_len < 3 || best_shift == 0 {
        return None;
    }

    // Expand the region to include the new lines introduced by the scroll.
    // For scroll-up by N: the region extends N rows below the shifted block
    // (those are the newly scrolled-in lines at the bottom of the region).
    let abs_shift = best_shift.unsigned_abs();
    let (region_top, region_bot) = if best_shift > 0 {
        // Scroll up: content moved up. The region includes the shifted lines
        // plus the new lines that appeared at the bottom.
        let top = best_top.saturating_sub(abs_shift);
        let bot = (best_top + best_len + abs_shift).min(rows);
        (top, bot)
    } else {
        // Scroll down: content moved down.
        let top = best_top.saturating_sub(abs_shift);
        let bot = (best_top + best_len + abs_shift).min(rows);
        (top, bot)
    };

    if region_bot <= region_top + 1 {
        return None;
    }

    Some((region_top, region_bot, best_shift))
}

/// Write a DECSTBM (set top and bottom margins / scroll region) sequence.
fn write_scroll_region(out: &mut Vec<u8>, top: usize, bot: usize) {
    let _ = write!(out, "\x1b[{};{}r", top + 1, bot);
}

/// Render a single line's visible cells to ANSI bytes.
/// Emits SGR only when attributes change from the previous cell.
/// Pads to `cols` width so no separate erase-line is needed.
fn render_line_ansi(
    out: &mut Vec<u8>,
    line: &tattoy_wezterm_term::Line,
    cols: usize,
) {
    let mut last_attrs: Option<tattoy_termwiz::cell::CellAttributes> = None;
    let mut col_count = 0usize;

    for cell_ref in line.visible_cells() {
        let idx = cell_ref.cell_index();
        if idx >= cols {
            break;
        }
        let attrs = cell_ref.attrs();
        let need_sgr = match &last_attrs {
            None => true,
            Some(prev) => prev != attrs,
        };
        if need_sgr {
            write_sgr(out, attrs);
            last_attrs = Some(attrs.clone());
        }
        let text = cell_ref.str();
        if text.is_empty() {
            out.push(b' ');
        } else {
            out.extend_from_slice(text.as_bytes());
        }
        col_count = idx + cell_ref.width().max(1);
    }

    // Pad remaining columns with spaces
    if col_count < cols {
        let default_attrs = tattoy_termwiz::cell::CellAttributes::default();
        if last_attrs.as_ref() != Some(&default_attrs) {
            out.extend_from_slice(b"\x1b[0m");
        }
        for _ in col_count..cols {
            out.push(b' ');
        }
    }
}

/// Write a CUP (cursor position) escape: ESC [ row+1 ; col+1 H
#[inline]
fn write_cup(out: &mut Vec<u8>, row: usize, col: usize) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
}

/// Write SGR for the given cell attributes.
fn write_sgr(out: &mut Vec<u8>, attrs: &tattoy_termwiz::cell::CellAttributes) {
    use tattoy_termwiz::cell::{Intensity, Underline};

    out.extend_from_slice(b"\x1b[0");

    match attrs.intensity() {
        Intensity::Bold => out.extend_from_slice(b";1"),
        Intensity::Half => out.extend_from_slice(b";2"),
        Intensity::Normal => {}
    }

    if attrs.italic() {
        out.extend_from_slice(b";3");
    }

    match attrs.underline() {
        Underline::None => {}
        _ => out.extend_from_slice(b";4"),
    }

    if attrs.reverse() {
        out.extend_from_slice(b";7");
    }

    if attrs.strikethrough() {
        out.extend_from_slice(b";9");
    }

    write_color_sgr(out, attrs.foreground(), true);
    write_color_sgr(out, attrs.background(), false);

    out.push(b'm');
}

/// Append color parameters to an SGR sequence.
fn write_color_sgr(out: &mut Vec<u8>, color: ColorAttribute, is_fg: bool) {
    let base: u8 = if is_fg { 30 } else { 40 };
    match color {
        ColorAttribute::Default => {
            // SGR reset (\x1b[0...) already sets default colors, no need to emit ;39/;49
        }
        ColorAttribute::PaletteIndex(idx) => {
            if idx < 8 {
                let _ = write!(out, ";{}", base + idx);
            } else if idx < 16 {
                let _ = write!(out, ";{}", base + 60 + idx - 8);
            } else {
                let _ = write!(out, ";{};5;{}", base + 8, idx);
            }
        }
        ColorAttribute::TrueColorWithDefaultFallback(c) => {
            let (r, g, b, _) = c.as_rgba_u8();
            let _ = write!(out, ";{};2;{};{};{}", base + 8, r, g, b);
        }
        ColorAttribute::TrueColorWithPaletteFallback(c, _) => {
            let (r, g, b, _) = c.as_rgba_u8();
            let _ = write!(out, ";{};2;{};{};{}", base + 8, r, g, b);
        }
    }
}

fn convert_cell_ref(cell_ref: &tattoy_wezterm_term::CellRef<'_>) -> Cell {
    let text = cell_ref.str();
    let width = cell_ref.width() as u8;
    let attrs = cell_ref.attrs();

    Cell {
        text: if text.is_empty() {
            " ".to_string()
        } else {
            text.to_string()
        },
        style: CellStyle {
            fg: convert_color_attr(attrs.foreground()),
            bg: convert_color_attr(attrs.background()),
            bold: attrs.intensity() == tattoy_termwiz::cell::Intensity::Bold,
            dim: attrs.intensity() == tattoy_termwiz::cell::Intensity::Half,
            italic: attrs.italic(),
            underline: attrs.underline() != tattoy_termwiz::cell::Underline::None,
            reverse: attrs.reverse(),
        },
        width,
    }
}

fn convert_color_attr(color: ColorAttribute) -> Color {
    match color {
        ColorAttribute::Default => Color::Default,
        ColorAttribute::PaletteIndex(idx) => Color::Indexed(idx),
        ColorAttribute::TrueColorWithDefaultFallback(c) => {
            let (r, g, b, _a) = c.as_rgba_u8();
            Color::Rgb(r, g, b)
        }
        ColorAttribute::TrueColorWithPaletteFallback(c, _idx) => {
            let (r, g, b, _a) = c.as_rgba_u8();
            Color::Rgb(r, g, b)
        }
    }
}

fn parse_osc(data: &[u8]) -> Option<(&[u8], usize)> {
    for i in 0..data.len() {
        if data[i] == 0x07 {
            return Some((&data[..i], i + 1));
        }
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
            return Some((&data[..i], i + 2));
        }
    }
    None
}

fn handle_osc(body: &[u8], client_forwards: &mut Vec<Vec<u8>>) {
    if body.starts_with(b"52;") {
        client_forwards.push(body.to_vec());
    }
}

/// Scan PTY output for KKP sequences, DEC private mode changes, and OSC clipboard.
fn scan_pty_output(
    data: &[u8],
    kkp_stack: &mut Vec<u32>,
    dec_modes: &mut BTreeSet<u16>,
    dec_mode_changes: &mut Vec<(u16, bool)>,
    client_forwards: &mut Vec<Vec<u8>>,
    pending_osc: &mut Vec<u8>,
    app_sync_active: &mut bool,
) {
    let mut i = 0;

    if !pending_osc.is_empty() {
        let mut found = false;
        for j in 0..data.len() {
            if data[j] == 0x07 {
                pending_osc.extend_from_slice(&data[..j]);
                handle_osc(pending_osc, client_forwards);
                pending_osc.clear();
                i = j + 1;
                found = true;
                break;
            }
            if data[j] == 0x1b && j + 1 < data.len() && data[j + 1] == b'\\' {
                pending_osc.extend_from_slice(&data[..j]);
                handle_osc(pending_osc, client_forwards);
                pending_osc.clear();
                i = j + 2;
                found = true;
                break;
            }
        }
        if !found {
            if pending_osc.len() + data.len() < 1024 * 1024 {
                pending_osc.extend_from_slice(data);
            } else {
                pending_osc.clear();
            }
            return;
        }
    }

    while i < data.len() {
        if data[i] == 0x1b
            && i + 1 < data.len()
            && data[i + 1] == b'['
            && i + 2 < data.len()
        {
            if data[i + 2] == b'?' {
                // Scan past digits and semicolons to find the final byte
                let mut j = i + 3;
                while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
                    j += 1;
                }
                if j < data.len() && data[j] == b'u' {
                    i = j + 1;
                    continue;
                }
                if j < data.len() && (data[j] == b'h' || data[j] == b'l') {
                    let enabled = data[j] == b'h';
                    // Parse semicolon-separated mode numbers
                    if let Ok(params_str) = std::str::from_utf8(&data[i + 3..j]) {
                        for param in params_str.split(';') {
                            if let Ok(mode) = param.parse::<u16>() {
                                if mode == 2026 {
                                    // Synchronized update — track internally,
                                    // don't forward to client (we handle it).
                                    *app_sync_active = enabled;
                                } else if FORWARDED_DEC_MODES.contains(&mode) {
                                    let changed = if enabled {
                                        dec_modes.insert(mode)
                                    } else {
                                        dec_modes.remove(&mode)
                                    };
                                    if changed {
                                        dec_mode_changes.push((mode, enabled));
                                    }
                                }
                            }
                        }
                    }
                    i = j + 1;
                    continue;
                }
            }

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
            }

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

            if data[i + 2] == b'=' {
                let mut j = i + 3;
                while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
                    j += 1;
                }
                if j < data.len() && data[j] == b'u' {
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
                        1 => flags,
                        2 => current | flags,
                        3 => current & !flags,
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

            i += 2;
            continue;
        }

        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b']' {
            if let Some((osc_body, end)) = parse_osc(&data[i + 2..]) {
                let end_abs = i + 2 + end;
                handle_osc(osc_body, client_forwards);
                i = end_abs;
                continue;
            } else {
                pending_osc.extend_from_slice(&data[i + 2..]);
                break;
            }
        }

        i += 1;
    }
}
