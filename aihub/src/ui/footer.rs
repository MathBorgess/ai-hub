//! Footer component rendering assisted-mode recommendation banner and prefix shortcuts hint.

use crate::state::{App, RecommendationState};
use aihub_core::Mode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Renders the footer banner, prefix hint, and status message.
pub fn render_footer(app: &App, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Fill background
    let bg_style = Style::default().bg(Color::Rgb(15, 18, 26));
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(bg_style).set_symbol(" ");
        }
    }

    let mut current_y = area.top();

    // 1. Assisted-mode recommendation banner (if present and in Assisted mode)
    if app.mode == Mode::Assisted {
        if let Some(rec) = &app.recommendation {
            if current_y < area.bottom() {
                let banner_line = match rec {
                    RecommendationState::Recommended {
                        harness,
                        reason,
                        confidence,
                        ..
                    } => Line::from(vec![
                        Span::styled(
                            "💡 [RECOMENDAÇÃO] ",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("Trocar para "),
                        Span::styled(
                            harness.binary_name(),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            " ({}) [confiança: {:.0}%] — ",
                            reason,
                            confidence * 100.0
                        )),
                        Span::styled(
                            "Pressione ^] seguido de Enter para aceitar",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    RecommendationState::NoCapacity { reason } => Line::from(vec![
                        Span::styled(
                            "ℹ️ [SEM CAPACIDADE] ",
                            Style::default()
                                .fg(Color::LightYellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!("{} — Aceitar indisponível", reason)),
                    ]),
                };

                buf.set_line(area.left(), current_y, &banner_line, area.width);
                current_y += 1;
            }
        }
    }

    // 2. Status message (if active)
    if let Some((msg, _)) = &app.status_message {
        if current_y < area.bottom() {
            let status_line = Line::from(vec![Span::styled(
                format!("💬 {}", msg),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::ITALIC),
            )]);
            buf.set_line(area.left(), current_y, &status_line, area.width);
            current_y += 1;
        }
    }

    // 3. Prefix shortcuts hint (always shown on bottom line)
    if current_y < area.bottom() {
        let hint_line = if app.prefix_active {
            Line::from(vec![
                Span::styled(
                    " PREFIXO ATIVO (^]) ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" Escolha: "),
                Span::styled(
                    "[p|:]",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" paleta  "),
                Span::styled(
                    "[Enter]",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" aceitar  "),
                Span::styled(
                    "[Tab]",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" alternar  "),
                Span::styled(
                    "[m]",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" modo  "),
                Span::styled(
                    "[q]",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" quotas  "),
                Span::styled(
                    "[d]",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" desconectar  "),
                Span::styled(
                    "[^]]",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" enviar ^]"),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    "^]",
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(40, 50, 70))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" prefixo: "),
                Span::styled("[p|:]", Style::default().fg(Color::Cyan)),
                Span::raw(" paleta  "),
                Span::styled("[Enter]", Style::default().fg(Color::Green)),
                Span::raw(" aceitar rec  "),
                Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
                Span::raw(" alternar harness  "),
                Span::styled("[m]", Style::default().fg(Color::Magenta)),
                Span::raw(" modo  "),
                Span::styled("[q]", Style::default().fg(Color::Blue)),
                Span::raw(" quotas  "),
                Span::styled("[d]", Style::default().fg(Color::Red)),
                Span::raw(" desconectar"),
            ])
        };

        buf.set_line(area.left(), current_y, &hint_line, area.width);
    }
}
