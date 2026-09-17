//! Header (statusline) component rendering quota bars, mode badge, harness, and branch.

use aihub_core::{Mode, WindowKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use crate::colors::quota_color;
use crate::state::App;

/// Renders the top statusline header.
///
/// Contains:
/// - Mode badge: `[ASSISTIDO]` or `[AUTÔNOMO]`
/// - Current harness name
/// - Worktree branch name
/// - Per-slot quota bars covering every window and every lane, colored by how much is used.
pub fn render_header(app: &App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Fill background
    let bg_style = Style::default().bg(Color::Rgb(20, 24, 34));
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(bg_style).set_symbol(" ");
        }
    }

    let mut left_spans: Vec<Span> = Vec::new();

    // Mode badge
    match app.mode {
        Mode::Assisted => {
            left_spans.push(Span::styled(
                "[ASSISTIDO]",
                Style::default()
                    .fg(Color::Cyan)
                    .bg(Color::Rgb(10, 40, 50))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Mode::Autonomous => {
            left_spans.push(Span::styled(
                "[AUTÔNOMO]",
                Style::default()
                    .fg(Color::Magenta)
                    .bg(Color::Rgb(50, 10, 40))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    // Current harness
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        format!("[{}]", app.harness.binary_name()),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));

    // Branch
    left_spans.push(Span::raw(" "));
    left_spans.push(Span::styled(
        format!("({})", app.branch),
        Style::default().fg(Color::DarkGray),
    ));

    left_spans.push(Span::raw(" │ "));

    // Per-slot quota bars covering every window and every lane
    for (i, snapshot) in app.snapshots.iter().enumerate() {
        if i > 0 {
            left_spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }

        // Slot harness name
        left_spans.push(Span::styled(
            format!("{}:", snapshot.slot.harness.binary_name()),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));

        // Top-level windows
        for win in &snapshot.windows {
            let kind_label = match win.kind {
                WindowKind::FiveHour => "5h",
                WindowKind::SevenDay => "7d",
                WindowKind::Cycle => "cyc",
                WindowKind::Custom(ref s) => s.as_str(),
            };
            let color = quota_color(win.used_pct);
            left_spans.push(Span::raw(" "));
            left_spans.push(Span::styled(
                format!("{}:{:.0}%", kind_label, win.used_pct),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }

        // Lanes
        for lane in &snapshot.lanes {
            for win in &lane.windows {
                let color = quota_color(win.used_pct);
                left_spans.push(Span::raw(" "));
                left_spans.push(Span::styled(
                    format!("{}:{:.0}%", lane.name, win.used_pct),
                    Style::default().fg(color),
                ));
            }
        }
    }

    // Draw spans into buffer
    let mut x = area.left();
    let y = area.top();
    for span in left_spans {
        let span_len = span.content.chars().count() as u16;
        if x + span_len > area.right() {
            // Truncate if overflowing width
            let remaining = area.right().saturating_sub(x) as usize;
            if remaining > 0 {
                let truncated: String = span.content.chars().take(remaining).collect();
                buf.set_string(x, y, truncated, span.style);
            }
            break;
        }
        buf.set_string(x, y, &span.content, span.style);
        x += span_len;
    }
}
