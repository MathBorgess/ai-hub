//! Command palette overlay renderer.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

const PALETTE_HELP: &[(&str, &str)] = &[
    ("/switch <harness>", "Troca o harness ativo (claude, agy, codex, cursor-agent)"),
    ("/merge", "Inicia merge assistido com revisão de diff"),
    ("/quota", "Exibe tabela analítica de quotas e janelas"),
    ("/mode", "Alterna entre modo [ASSISTIDO] e [AUTÔNOMO]"),
    ("/detach", "Desconecta do terminal mantendo a sessão no daemon"),
];

/// Renders the command palette centered overlay.
pub fn render_palette(input: &str, area: Rect, buf: &mut Buffer) {
    let width = 74.min(area.width.saturating_sub(4));
    let height = 11.min(area.height.saturating_sub(2));

    if width < 20 || height < 5 {
        return;
    }

    let x = area.left() + (area.width.saturating_sub(width)) / 2;
    let y = area.top() + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    // Clear background
    Clear.render(popup_area, buf);

    let block = Block::default()
        .title(" Paleta de Comandos ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(20, 22, 32)));

    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    let mut lines = Vec::new();

    // Input prompt line
    lines.push(Line::from(vec![
        Span::styled(
            ": ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            input,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("█", Style::default().fg(Color::Cyan)),
    ]));

    lines.push(Line::from(Span::styled(
        "─".repeat(inner.width as usize),
        Style::default().fg(Color::DarkGray),
    )));

    // Matching or standard commands
    let trimmed = input.trim();
    for &(cmd, desc) in PALETTE_HELP {
        let is_match = cmd.starts_with(trimmed) || trimmed.is_empty();
        let cmd_style = if is_match {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let desc_style = if is_match {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{:<20}", cmd), cmd_style),
            Span::styled(desc, desc_style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("[Tab]", Style::default().fg(Color::Cyan)),
        Span::raw(" autocompletar  "),
        Span::styled("[Enter]", Style::default().fg(Color::Green)),
        Span::raw(" executar  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red)),
        Span::raw(" fechar"),
    ]));

    Paragraph::new(lines).render(inner, buf);
}
