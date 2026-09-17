//! Header (statusline) component rendering quota bars, mode badge, harness, and branch.

use crate::colors::quota_color;
use crate::state::App;
use aihub_core::{Mode, QuotaSnapshot, WindowKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// Constructs filled and empty parts of a gauge bar for given used percentage.
pub fn make_gauge_bar(used_pct: f64, width: usize) -> (String, String) {
    let filled = ((used_pct / 100.0) * (width as f64))
        .round()
        .clamp(0.0, width as f64) as usize;
    let filled_str: String = "█".repeat(filled);
    let empty_str: String = "░".repeat(width.saturating_sub(filled));
    (filled_str, empty_str)
}

/// Builds prefix spans: Mode badge, current harness, branch.
pub fn build_prefix_spans(app: &App) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    match app.mode {
        Mode::Assisted => {
            spans.push(Span::styled(
                "[ASSISTIDO]",
                Style::default()
                    .fg(Color::Cyan)
                    .bg(Color::Rgb(10, 40, 50))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Mode::Autonomous => {
            spans.push(Span::styled(
                "[AUTÔNOMO]",
                Style::default()
                    .fg(Color::Magenta)
                    .bg(Color::Rgb(50, 10, 40))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("[{}]", app.harness.binary_name()),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));

    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!("({})", app.branch),
        Style::default().fg(Color::DarkGray),
    ));

    spans.push(Span::raw(" │ "));
    spans
}

/// Builds spans for a single quota slot (windows and lanes).
pub fn build_slot_spans(snapshot: &QuotaSnapshot, collapse_lanes: bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();

    spans.push(Span::styled(
        format!("{}:", snapshot.slot.harness.binary_name()),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));

    for win in &snapshot.windows {
        let kind_label = match win.kind {
            WindowKind::FiveHour => "5h",
            WindowKind::SevenDay => "7d",
            WindowKind::Cycle => "cyc",
            WindowKind::Custom(ref s) => s.as_str(),
        };
        let color = quota_color(win.used_pct);
        let (filled, empty) = make_gauge_bar(win.used_pct, 4);

        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{}:{:.0}%[", kind_label, win.used_pct),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            filled,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(empty, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            "]",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }

    if collapse_lanes && !snapshot.lanes.is_empty() {
        if let Some(tightest) = snapshot.tightest_window() {
            let color = quota_color(tightest.used_pct);
            let (filled, empty) = make_gauge_bar(tightest.used_pct, 4);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{:.0}%[", tightest.used_pct),
                Style::default().fg(color),
            ));
            spans.push(Span::styled(filled, Style::default().fg(color)));
            spans.push(Span::styled(empty, Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled("]", Style::default().fg(color)));
        }
    } else {
        for lane in &snapshot.lanes {
            for win in &lane.windows {
                let color = quota_color(win.used_pct);
                let (filled, empty) = make_gauge_bar(win.used_pct, 4);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!("{}:{:.0}%[", lane.name, win.used_pct),
                    Style::default().fg(color),
                ));
                spans.push(Span::styled(filled, Style::default().fg(color)));
                spans.push(Span::styled(empty, Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled("]", Style::default().fg(color)));
            }
        }
    }

    spans
}

fn spans_len(spans: &[Span]) -> u16 {
    spans.iter().map(|s| s.content.chars().count() as u16).sum()
}

/// Computes the required header height (1 or 2 lines) so that every slot stays visible.
pub fn header_height(app: &App, width: u16) -> u16 {
    if app.snapshots.is_empty() || width == 0 {
        return 1;
    }
    let prefix_len = spans_len(&build_prefix_spans(app));
    let mut total_len = prefix_len;
    for (i, snapshot) in app.snapshots.iter().enumerate() {
        if i > 0 {
            total_len += 3; // " │ "
        }
        let slot_len = spans_len(&build_slot_spans(snapshot, false));
        total_len += slot_len;
    }
    if total_len <= width {
        1
    } else {
        2
    }
}

/// Renders the top statusline header.
///
/// Contains:
/// - Mode badge: `[ASSISTIDO]` or `[AUTÔNOMO]`
/// - Current harness name
/// - Worktree branch name
/// - Per-slot quota gauge bars covering every window and every lane, colored by usage.
/// - Wraps to a second line or collapses lanes when needed to keep every slot visible.
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

    let prefix_spans = build_prefix_spans(app);
    let collapse_lanes = area.height == 1 && header_height(app, area.width) > 1;

    let mut current_y = area.top();
    let mut current_x = area.left();

    // Draw prefix spans
    for span in prefix_spans {
        let len = span.content.chars().count() as u16;
        if current_x + len > area.right() {
            break;
        }
        buf.set_string(current_x, current_y, &span.content, span.style);
        current_x += len;
    }

    // Draw slots
    for (i, snapshot) in app.snapshots.iter().enumerate() {
        let slot_spans = build_slot_spans(snapshot, collapse_lanes);
        let slot_len = spans_len(&slot_spans);
        let sep_len = if i > 0 { 3 } else { 0 };

        // Check if slot fits on current line; if not, wrap to next line if available
        if current_x + sep_len + slot_len > area.right() && current_y + 1 < area.bottom() {
            current_y += 1;
            current_x = area.left();
        } else if i > 0 && current_x + sep_len <= area.right() {
            buf.set_string(
                current_x,
                current_y,
                " │ ",
                Style::default().fg(Color::DarkGray),
            );
            current_x += sep_len;
        }

        // Draw slot spans
        for span in slot_spans {
            let len = span.content.chars().count() as u16;
            if current_x + len > area.right() {
                if current_y + 1 < area.bottom() {
                    current_y += 1;
                    current_x = area.left();
                } else {
                    let rem = area.right().saturating_sub(current_x) as usize;
                    if rem > 0 {
                        let trunc: String = span.content.chars().take(rem).collect();
                        buf.set_string(current_x, current_y, trunc, span.style);
                    }
                    break;
                }
            }
            buf.set_string(current_x, current_y, &span.content, span.style);
            current_x += len;
        }
    }
}
