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
