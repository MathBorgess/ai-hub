//! Virtual terminal body renderer mapping vt100 state to Ratatui buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Converts a vt100 color to a ratatui color.
pub fn to_ratatui_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Renders the virtual terminal screen into the specified area of the buffer.
/// Returns the absolute cursor coordinates `Some((x, y))` if the cursor should be visible.
pub fn render_terminal(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
    if area.height == 0 || area.width == 0 {
        return None;
    }

    let (rows, cols) = screen.size();
    let max_r = area.height.min(rows);
    let max_c = area.width.min(cols);

    for r in 0..max_r {
        for c in 0..max_c {
            let buf_x = area.x + c;
            let buf_y = area.y + r;
            if buf_x >= area.right() || buf_y >= area.bottom() {
                continue;
            }

            if let Some(cell) = screen.cell(r, c) {
                if cell.is_wide_continuation() {
                    continue;
                }

                let buf_cell = &mut buf[(buf_x, buf_y)];
                if cell.has_contents() {
                    buf_cell.set_symbol(&cell.contents());
                } else {
                    buf_cell.set_symbol(" ");
                }

                let mut style = Style::default();
                let fg = to_ratatui_color(cell.fgcolor());
                if fg != Color::Reset {
                    style = style.fg(fg);
                }
                let bg = to_ratatui_color(cell.bgcolor());
                if bg != Color::Reset {
                    style = style.bg(bg);
                }

                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                buf_cell.set_style(style);
            }
        }
    }

    // Determine cursor visibility and position
    if !screen.hide_cursor() {
        let (c_r, c_c) = screen.cursor_position();
        if c_r < area.height && c_c < area.width {
            return Some((area.x + c_c, area.y + c_r));
        }
    }

    None
}
