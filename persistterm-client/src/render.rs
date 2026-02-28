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
