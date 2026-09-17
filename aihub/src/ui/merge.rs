//! Merge review and colored diff overlay renderer.

use aihub_core::MergeStrategy;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

/// Renders the merge review popup displaying git diff and strategy selector.
pub fn render_merge_review(
    diff: &str,
    message: &str,
    strategy: MergeStrategy,
    scroll: usize,
    area: Rect,
    buf: &mut Buffer,
) {
    let width = (area.width * 95 / 100).max(60).min(area.width.saturating_sub(2));
    let height = (area.height * 90 / 100).max(12).min(area.height.saturating_sub(2));

    if width < 30 || height < 6 {
        return;
    }

    let x = area.left() + (area.width.saturating_sub(width)) / 2;
    let y = area.top() + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    Clear.render(popup_area, buf);

    let strategy_label = match strategy {
        MergeStrategy::Squash => "Squash",
        MergeStrategy::FastForward => "Fast-Forward",
        MergeStrategy::Keep => "Keep",
        MergeStrategy::Discard => "Discard",
    };

    let title = format!(" Revisão de Merge [{}] ", strategy_label);
    let block = Block::default()
        .title(title)
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green))
        .style(Style::default().bg(Color::Rgb(15, 18, 25)));

    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    let mut lines = Vec::new();

    // Daemon instruction message
    if !message.is_empty() {
        lines.push(Line::from(Span::styled(
            message,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    // Git diff lines styled according to diff syntax
    if diff.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "(Nenhuma alteração detectada na worktree)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for line in diff.lines().skip(scroll) {
            let styled_line = if line.starts_with("+++") || line.starts_with("---") {
                Span::styled(line, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else if line.starts_with('+') {
                Span::styled(line, Style::default().fg(Color::Green))
            } else if line.starts_with('-') {
                Span::styled(line, Style::default().fg(Color::Red))
            } else if line.starts_with("@@") {
                Span::styled(line, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            } else if line.starts_with("diff --git") {
                Span::styled(line, Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
            } else {
                Span::styled(line, Style::default().fg(Color::White))
            };
            lines.push(Line::from(styled_line));
        }
    }

    // Strategy choices at bottom
    let line1 = Line::from(vec![
        Span::raw("Estratégia: "),
        highlight_choice("s", "Squash", strategy == MergeStrategy::Squash),
        Span::raw("  "),
        highlight_choice("f", "Fast-Forward", strategy == MergeStrategy::FastForward),
        Span::raw("  "),
        highlight_choice("k", "Keep", strategy == MergeStrategy::Keep),
        Span::raw("  "),
        highlight_choice("d", "Discard", strategy == MergeStrategy::Discard),
    ]);

    let line2 = Line::from(vec![
        Span::styled("[Enter]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" Confirmar  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red)),
        Span::raw(" Cancelar"),
    ]);

    // Reserve bottom 3 lines in inner for controls
    let diff_height = inner.height.saturating_sub(3);
    let diff_area = Rect::new(inner.x, inner.y, inner.width, diff_height);
    let ctrl_area = Rect::new(inner.x, inner.y + diff_height, inner.width, 3);

    Paragraph::new(lines).render(diff_area, buf);
    Paragraph::new(vec![
        Line::from("─".repeat(inner.width as usize)),
        line1,
        line2,
    ])
    .render(ctrl_area, buf);
}

fn highlight_choice(key: &str, label: &str, selected: bool) -> Span<'static> {
    if selected {
        Span::styled(
            format!("[{}] {} (selecionado)", key, label),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!("[{}] {}", key, label),
            Style::default().fg(Color::White),
        )
    }
}
