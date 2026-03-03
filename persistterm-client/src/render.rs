use std::io::Write;

use persistterm_proto::{CellStyle, Color, ScreenSnapshot};

pub fn render_snapshot<W: Write>(out: &mut W, snapshot: &ScreenSnapshot) -> std::io::Result<()> {
    // Hide cursor during repaint
    write!(out, "\x1b[?25l")?;
    // Move to top-left
    write!(out, "\x1b[H")?;

    let mut current_style = CellStyle::default();
    // Reset attributes at start
    write!(out, "\x1b[0m")?;

    let default_cell = persistterm_proto::Cell::default();

    for row in 0..snapshot.height {
        if row > 0 {
            write!(out, "\r\n")?;
        }
        for col in 0..snapshot.width {
            let idx = (row as usize) * (snapshot.width as usize) + (col as usize);
            let cell = snapshot.cells.get(idx).unwrap_or(&default_cell);

            // Skip wide continuation cells (width == 0)
            if cell.width == 0 {
                continue;
            }

            // Emit SGR if style changed
            if cell.style != current_style {
                emit_sgr(out, &cell.style)?;
                current_style = cell.style.clone();
            }

            write!(out, "{}", cell.text)?;
        }
        // Clear to end of line in case previous content was longer
        write!(out, "\x1b[K")?;
    }

    // Reset attributes
    write!(out, "\x1b[0m")?;

    // Restore cursor position (1-based)
    write!(
        out,
        "\x1b[{};{}H",
        snapshot.cursor.y + 1,
        snapshot.cursor.x + 1
    )?;

    // Show/hide cursor based on state
    if snapshot.cursor.visible {
        write!(out, "\x1b[?25h")?;
    }

    out.flush()
}

pub fn render_kicked_overlay<W: Write>(out: &mut W, cols: u16, rows: u16) -> std::io::Result<()> {
    // Clear screen, reset attributes, hide cursor
    write!(out, "\x1b[0m\x1b[2J\x1b[?25l")?;

    let lines: &[&str] = &[
        "Session moved to another client.",
        "",
        "Waiting to reclaim automatically...",
        "",
        "Press SPACE or ENTER to reclaim now.",
        "Press q or ESC to exit.",
    ];

    let start_row = rows.saturating_sub(lines.len() as u16) / 2 + 1;

    for (i, line) in lines.iter().enumerate() {
        let row = start_row + i as u16;
        let col = cols.saturating_sub(line.len() as u16) / 2 + 1;
        write!(out, "\x1b[{row};{col}H")?;
        if i == 0 {
            // Bold for the header
            write!(out, "\x1b[1m{line}\x1b[0m")?;
        } else if i == 2 {
            // Dim for the "waiting" status line
            write!(out, "\x1b[2m{line}\x1b[0m")?;
        } else if i >= 4 {
            // Reverse for the key instructions
            write!(out, "\x1b[7m{line}\x1b[0m")?;
        } else {
            write!(out, "{line}")?;
        }
    }

    out.flush()
}

pub fn render_session_ended_overlay<W: Write>(out: &mut W, cols: u16, rows: u16) -> std::io::Result<()> {
    // Clear screen, reset attributes, hide cursor
    write!(out, "\x1b[0m\x1b[2J\x1b[?25l")?;

    let lines: &[&str] = &[
        "Session has ended.",
        "",
        "Press any key to exit.",
    ];

    let start_row = rows.saturating_sub(lines.len() as u16) / 2 + 1;

    for (i, line) in lines.iter().enumerate() {
        let row = start_row + i as u16;
        let col = cols.saturating_sub(line.len() as u16) / 2 + 1;
        write!(out, "\x1b[{row};{col}H")?;
        if i == 0 {
            write!(out, "\x1b[1m{line}\x1b[0m")?;
        } else if i == 2 {
            write!(out, "\x1b[7m{line}\x1b[0m")?;
        } else {
            write!(out, "{line}")?;
        }
    }

    out.flush()
}

pub fn render_remote_kicked_overlay<W: Write>(
    out: &mut W,
    cols: u16,
    rows: u16,
) -> std::io::Result<()> {
    // Clear screen, reset attributes, hide cursor
    write!(out, "\x1b[0m\x1b[2J\x1b[?25l")?;

    let lines: &[&str] = &[
        "Session taken by another client.",
        "",
        "Press SPACE or ENTER to reclaim.",
        "Press q or ESC to exit.",
    ];

    let start_row = rows.saturating_sub(lines.len() as u16) / 2 + 1;

    for (i, line) in lines.iter().enumerate() {
        let row = start_row + i as u16;
        let col = cols.saturating_sub(line.len() as u16) / 2 + 1;
        write!(out, "\x1b[{row};{col}H")?;
        if i == 0 {
            // Bold for the header
            write!(out, "\x1b[1m{line}\x1b[0m")?;
        } else if i >= 2 {
            // Reverse for the key instructions
            write!(out, "\x1b[7m{line}\x1b[0m")?;
        } else {
            write!(out, "{line}")?;
        }
    }

    out.flush()
}

pub fn render_reconnect_overlay<W: Write>(
    out: &mut W,
    cols: u16,
    rows: u16,
    host: &str,
    session: &str,
    attempt: u32,
    countdown: u32,
) -> std::io::Result<()> {
    // Clear screen, reset attributes, hide cursor
    write!(out, "\x1b[0m\x1b[2J\x1b[?25l")?;

    let header = format!("Connection to {host}/{session} lost.");
    let status = format!("Reconnecting in {countdown}s... (attempt {attempt})");

    let lines: &[&str] = &[
        &header,
        "",
        &status,
        "",
        "Press ENTER to retry now.",
        "Press q or ESC to exit.",
    ];

    let start_row = rows.saturating_sub(lines.len() as u16) / 2 + 1;

    for (i, line) in lines.iter().enumerate() {
        let row = start_row + i as u16;
        let col = cols.saturating_sub(line.len() as u16) / 2 + 1;
        write!(out, "\x1b[{row};{col}H")?;
        if i == 0 {
            // Bold for the header
            write!(out, "\x1b[1m{line}\x1b[0m")?;
        } else if i == 2 {
            // Dim for the status line
            write!(out, "\x1b[2m{line}\x1b[0m")?;
        } else if i >= 4 {
            // Reverse for key instructions
            write!(out, "\x1b[7m{line}\x1b[0m")?;
        } else {
            write!(out, "{line}")?;
        }
    }

    out.flush()
}

pub fn render_session_info_overlay<W: Write>(
    out: &mut W,
    cols: u16,
    rows: u16,
    session_name: &str,
    server_version: &str,
    client_version: &str,
    program: &[String],
    uptime_secs: u64,
    terminal_size: (u16, u16),
    pid: u32,
    child_pid: Option<u32>,
    attached_secs: u64,
    waiting_clients: usize,
    latency_ms: Option<u64>,
) -> std::io::Result<()> {
    // Clear screen, reset attributes, hide cursor
    write!(out, "\x1b[0m\x1b[2J\x1b[?25l")?;

    let command = if program.is_empty() {
        "$SHELL".to_string()
    } else {
        program.join(" ")
    };

    let format_duration = |total_secs: u64| -> String {
        let days = total_secs / 86400;
        let hours = (total_secs % 86400) / 3600;
        let mins = (total_secs % 3600) / 60;
        let secs = total_secs % 60;
        if days > 0 {
            format!("{days}d {hours}h {mins}m {secs}s")
        } else if hours > 0 {
            format!("{hours}h {mins}m {secs}s")
        } else if mins > 0 {
            format!("{mins}m {secs}s")
        } else {
            format!("{secs}s")
        }
    };

    let uptime = format_duration(uptime_secs);
    let attached = format_duration(attached_secs);

    let size_str = format!("{}x{}", terminal_size.0, terminal_size.1);
    let server_val = format!("mux {server_version}");
    let client_val = format!("mux {client_version}");
    let pid_str = pid.to_string();
    let child_pid_str = child_pid.map_or("N/A".to_string(), |p| p.to_string());
    let waiting_str = waiting_clients.to_string();
    let latency_str = latency_ms.map_or("N/A".to_string(), |ms| format!("{ms}ms"));

    // Key-value pairs (without Command): right-align keys, align ':'
    let kv: &[(&str, &str)] = &[
        ("Session", session_name),
        ("Server", &server_val),
        ("Client", &client_val),
        ("Uptime", &uptime),
        ("Attached", &attached),
        ("Size", &size_str),
        ("PID", &pid_str),
        ("Child PID", &child_pid_str),
        ("Waiting", &waiting_str),
        ("Latency", &latency_str),
    ];

    // Longest key determines the column position of ':'
    let key_width = kv.iter().map(|(k, _)| k.len()).max().unwrap_or(0);

    let hint_line = "Press any key to return.";

    // Total line count: data lines + blank + hint
    let total = kv.len() + 2;
    let start_row = rows.saturating_sub(total as u16) / 2 + 1;

    // Place the ':' at the screen center. Keys go left, values go right.
    let colon_col = (cols / 2 + 1) as u16;

    for (i, (key, val)) in kv.iter().enumerate() {
        let row = start_row + i as u16;
        let key_col = colon_col.saturating_sub(key_width as u16 + 1);
        write!(out, "\x1b[{row};{key_col}H")?;
        let line = format!("{:>width$} : {}", key, val, width = key_width);
        if i == 0 {
            write!(out, "\x1b[1m{line}\x1b[0m")?;
        } else {
            write!(out, "{line}")?;
        }
    }

    // Hint line centered on screen
    let hint_row = start_row + kv.len() as u16 + 1;
    let hint_col = (cols.saturating_sub(hint_line.len() as u16) + 1) / 2 + 1;
    write!(out, "\x1b[{hint_row};{hint_col}H")?;
    write!(out, "\x1b[7m{hint_line}\x1b[0m")?;

    // Command at the bottom-left, truncated to screen width
    let cmd_label = format!("Command: {command}");
    let cmd_display = if cmd_label.len() > cols as usize {
        &cmd_label[..cols as usize]
    } else {
        &cmd_label
    };
    write!(out, "\x1b[{rows};1H")?;
    write!(out, "\x1b[2m{cmd_display}\x1b[0m")?;

    out.flush()
}

fn emit_sgr<W: Write>(out: &mut W, style: &CellStyle) -> std::io::Result<()> {
    // Reset then apply
    write!(out, "\x1b[0")?;

    if style.bold {
        write!(out, ";1")?;
    }
    if style.dim {
        write!(out, ";2")?;
    }
    if style.italic {
        write!(out, ";3")?;
    }
    if style.underline {
        write!(out, ";4")?;
    }
    if style.reverse {
        write!(out, ";7")?;
    }

    emit_color(out, &style.fg, true)?;
    emit_color(out, &style.bg, false)?;

    write!(out, "m")
}

fn emit_color<W: Write>(out: &mut W, color: &Color, foreground: bool) -> std::io::Result<()> {
    match color {
        Color::Default => {
            // Default is handled by the reset at the start of SGR
        }
        Color::Indexed(idx) => {
            if foreground {
                write!(out, ";38;5;{idx}")?;
            } else {
                write!(out, ";48;5;{idx}")?;
            }
        }
        Color::Rgb(r, g, b) => {
            if foreground {
                write!(out, ";38;2;{r};{g};{b}")?;
            } else {
                write!(out, ";48;2;{r};{g};{b}")?;
            }
        }
    }
    Ok(())
}
