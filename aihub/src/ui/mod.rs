//! UI orchestration and layout rendering.

pub mod banner;
pub mod footer;
pub mod header;
pub mod merge;
pub mod palette;
pub mod quota_table;
pub mod terminal;

use crate::state::{App, UiMode};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Renders the complete aihub UI onto the buffer and returns cursor position if any.
pub fn render_app(app: &App, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
    if area.height == 0 || area.width == 0 {
        return None;
    }

    // Determine footer height:
    // If recommendation is present in assisted mode, reserve 2 lines (or 3 lines if status message),
    // otherwise 1-2 lines.
    let footer_height: u16 =
        if app.recommendation.is_some() && app.mode == aihub_core::Mode::Assisted {
            if app.status_message.is_some() {
                3
            } else {
                2
            }
        } else if app.status_message.is_some() {
            2
        } else {
            1
        };

    let banner_height: u16 = banner::banner_height(app).min(area.height.saturating_sub(1));

    let header_height: u16 = header::header_height(app, area.width).min(
        area.height
            .saturating_sub(banner_height + footer_height + 1)
            .max(1),
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(banner_height), // Persistent connection banner
            Constraint::Length(header_height), // Header (1 or 2 lines)
            Constraint::Min(1),                // Terminal body
            Constraint::Length(
                footer_height.min(
                    area.height
                        .saturating_sub(banner_height + header_height + 1),
                ),
            ), // Footer
        ])
        .split(area);

    let banner_area = chunks[0];
    let header_area = chunks[1];
    let body_area = chunks[2];
    let footer_area = chunks[3];

    // 1. Render persistent connection banner
    banner::render_banner(app, banner_area, buf);

    // 2. Render Header (statusline)
    header::render_header(app, header_area, buf);

    // 3. Render Terminal Body
    let cursor = terminal::render_terminal(app.vt_parser.screen(), body_area, buf);

    // 4. Render Footer
    footer::render_footer(app, footer_area, buf);

    // 5. Overlays
    match &app.ui_mode {
        UiMode::Normal => {}
        UiMode::Palette { input, .. } => {
            palette::render_palette(input, area, buf);
        }
        UiMode::QuotaTable { scroll } => {
            quota_table::render_quota_table(&app.snapshots, *scroll, area, buf);
        }
        UiMode::MergeReview {
            diff,
            message,
            strategy,
            scroll,
        } => {
            merge::render_merge_review(diff, message, *strategy, *scroll, area, buf);
        }
    }

    cursor
}
