//! Inline renderer — prints ratatui `Line`/`Text` directly to stdout
//! without entering alternate screen or raw mode.
//!
//! This allows the CLI (non-TUI) mode to reuse the same styled widgets
//! that the full-screen TUI uses.

use crossterm::cursor::MoveToColumn;
use crossterm::style::{
    Attribute, Color as CtColor, Print, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use crossterm::ExecutableCommand;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use std::io::{self, Write};

use super::capabilities::should_use_color;

/// Print a single `Line` to stdout. Styling is dropped automatically when
/// the terminal does not support color (NO_COLOR, TERM=dumb, piped output,
/// or `--color=never`).
///
/// Uses `\r\n` (CRLF) instead of just `\n` so output is correct both in
/// cooked mode (terminal driver strips the extra `\r`) and raw mode (where
/// `\n` alone only moves the cursor down without returning to column 0).
pub fn print_line(line: &Line<'_>) {
    let mut stdout = io::stdout();
    let styled = should_use_color();
    for span in &line.spans {
        if styled {
            apply_style(&mut stdout, &span.style);
        }
        let _ = stdout.execute(Print(&span.content));
        if styled {
            let _ = stdout.execute(ResetColor);
            let _ = stdout.execute(SetAttribute(Attribute::Reset));
        }
    }
    let _ = stdout.execute(Print("\r\n"));
    let _ = stdout.flush();
}

/// Print multiple `Line`s to stdout.
pub fn print_lines(lines: &[Line<'_>]) {
    for line in lines {
        print_line(line);
    }
}

/// Print a `Text` block to stdout.
#[allow(dead_code)]
pub fn print_text(text: &Text<'_>) {
    print_lines(&text.lines);
}

/// Print a horizontal rule using a repeated character.
#[allow(dead_code)]
pub fn print_rule(ch: char, style: Style) {
    let width = terminal_width();
    let rule_str: String = std::iter::repeat_n(ch, width).collect();
    let line = Line::from(Span::styled(rule_str, style));
    print_line(&line);
}

/// Print an empty line.
/// Uses `\r\n` for raw mode compatibility.
pub fn print_blank() {
    let mut stdout = io::stdout();
    let _ = stdout.execute(Print("\r\n"));
    let _ = stdout.flush();
}

/// Print a `Line` as a transient status line that overwrites itself on each
/// update. Uses `\r` + clear-line so the cursor stays parked at the start of
/// the line, ready to be overwritten by the next call or finalized by
/// [`clear_transient`] / [`print_line`].
///
/// When stdout is not a TTY (piped output, dumb terminal), this is a no-op so
/// progress noise doesn't pollute logs. Callers should still emit a final
/// `print_line` for the completed state in that case.
pub fn print_transient(line: &Line<'_>) {
    if !is_stdout_tty() {
        return;
    }
    let mut stdout = io::stdout();
    let _ = stdout.execute(MoveToColumn(0));
    let _ = stdout.execute(Clear(ClearType::CurrentLine));
    let styled = should_use_color();
    for span in &line.spans {
        if styled {
            apply_style(&mut stdout, &span.style);
        }
        let _ = stdout.execute(Print(&span.content));
        if styled {
            let _ = stdout.execute(ResetColor);
            let _ = stdout.execute(SetAttribute(Attribute::Reset));
        }
    }
    let _ = stdout.flush();
}

/// Clear the current transient status line. Pair with [`print_transient`]
/// before printing a finalized message.
pub fn clear_transient() {
    if !is_stdout_tty() {
        return;
    }
    let mut stdout = io::stdout();
    let _ = stdout.execute(MoveToColumn(0));
    let _ = stdout.execute(Clear(ClearType::CurrentLine));
    let _ = stdout.flush();
}

/// Replace the last `count` terminal lines with new styled content.
///
/// Moves cursor up `count` lines, clears from cursor down, then prints
/// each new line. Used for re-rendering streamed plaintext as styled
/// markdown once the LLM finishes.
///
/// **Panics** if `count` is 0. No-op when stdout is not a TTY.
pub fn replace_lines(count: u32, new_lines: &[Line<'_>]) {
    if !is_stdout_tty() || count == 0 {
        // When piped, just print the new lines normally.
        for line in new_lines {
            print_line(line);
        }
        return;
    }
    let mut stdout = io::stdout();
    let styled = should_use_color();

    // Move cursor up N lines
    let _ = stdout.execute(crossterm::cursor::MoveUp(count.min(u16::MAX as u32) as u16));
    // Clear from cursor to end of screen
    let _ = stdout.execute(Clear(ClearType::FromCursorDown));

    // Print each new line
    for line in new_lines {
        if styled {
            for span in &line.spans {
                apply_style(&mut stdout, &span.style);
                let _ = stdout.execute(Print(&span.content));
                let _ = stdout.execute(ResetColor);
                let _ = stdout.execute(SetAttribute(Attribute::Reset));
            }
        } else {
            for span in &line.spans {
                let _ = stdout.execute(Print(&span.content));
            }
        }
        let _ = stdout.execute(Print("\r\n"));
    }
    let _ = stdout.flush();
}

/// Get terminal width, defaulting to 80.
pub fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
        .max(40)
}

/// Re-export `is_stdout_tty` from capabilities for use by other modules.
pub fn is_stdout_tty() -> bool {
    super::capabilities::is_stdout_tty()
}

/// Apply a ratatui `Style` to stdout using crossterm commands.
fn apply_style(stdout: &mut io::Stdout, style: &Style) {
    if let Some(fg) = style.fg {
        if let Some(ct_color) = to_crossterm_color(fg) {
            let _ = stdout.execute(SetForegroundColor(ct_color));
        }
    }
    if let Some(bg) = style.bg {
        if let Some(ct_color) = to_crossterm_color(bg) {
            let _ = stdout.execute(SetBackgroundColor(ct_color));
        }
    }

    let mods = style.add_modifier;
    if mods.contains(Modifier::BOLD) {
        let _ = stdout.execute(SetAttribute(Attribute::Bold));
    }
    if mods.contains(Modifier::ITALIC) {
        let _ = stdout.execute(SetAttribute(Attribute::Italic));
    }
    if mods.contains(Modifier::UNDERLINED) {
        let _ = stdout.execute(SetAttribute(Attribute::Underlined));
    }
    if mods.contains(Modifier::DIM) {
        let _ = stdout.execute(SetAttribute(Attribute::Dim));
    }
    if mods.contains(Modifier::CROSSED_OUT) {
        let _ = stdout.execute(SetAttribute(Attribute::CrossedOut));
    }
}

/// Convert ratatui `Color` to crossterm `Color`.
fn to_crossterm_color(color: Color) -> Option<CtColor> {
    match color {
        Color::Reset => None,
        Color::Black => Some(CtColor::Black),
        Color::Red => Some(CtColor::DarkRed),
        Color::Green => Some(CtColor::DarkGreen),
        Color::Yellow => Some(CtColor::DarkYellow),
        Color::Blue => Some(CtColor::DarkBlue),
        Color::Magenta => Some(CtColor::DarkMagenta),
        Color::Cyan => Some(CtColor::DarkCyan),
        Color::Gray => Some(CtColor::Grey),
        Color::DarkGray => Some(CtColor::DarkGrey),
        Color::LightRed => Some(CtColor::Red),
        Color::LightGreen => Some(CtColor::Green),
        Color::LightYellow => Some(CtColor::Yellow),
        Color::LightBlue => Some(CtColor::Blue),
        Color::LightMagenta => Some(CtColor::Magenta),
        Color::LightCyan => Some(CtColor::Cyan),
        Color::White => Some(CtColor::White),
        Color::Rgb(r, g, b) => Some(CtColor::Rgb { r, g, b }),
        Color::Indexed(i) => Some(CtColor::AnsiValue(i)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_crossterm_color_maps_known_variants() {
        assert_eq!(to_crossterm_color(Color::Reset), None);
        assert!(matches!(
            to_crossterm_color(Color::Red),
            Some(CtColor::DarkRed)
        ));
        assert!(matches!(
            to_crossterm_color(Color::Rgb(10, 20, 30)),
            Some(CtColor::Rgb {
                r: 10,
                g: 20,
                b: 30
            })
        ));
        assert!(matches!(
            to_crossterm_color(Color::Indexed(42)),
            Some(CtColor::AnsiValue(42))
        ));
    }

    #[test]
    fn terminal_width_has_minimum_floor() {
        // The width helper enforces a 40-col floor so layout math stays sane
        // even on tiny or unmeasurable terminals.
        assert!(terminal_width() >= 40);
    }
}
