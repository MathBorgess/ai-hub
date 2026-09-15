//! Full windows x lanes quota table modal overlay.

use crate::colors::quota_color;
use aihub_core::{QuotaSnapshot, WindowKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Clear, Row, Table, Widget};

fn format_duration(seconds: Option<u64>) -> String {
    match seconds {
        Some(s) if s >= 3600 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        Some(s) if s >= 60 => format!("{}m", s / 60),
        Some(s) => format!("{}s", s),
        None => "—".to_string(),
    }
}

/// Renders the full analytical quota table modal.
pub fn render_quota_table(
    snapshots: &[QuotaSnapshot],
    scroll: usize,
    area: Rect,
    buf: &mut Buffer,
) {
    let width = (area.width * 9 / 10)
        .max(60)
        .min(area.width.saturating_sub(2));
    let height = (area.height * 8 / 10)
        .max(12)
        .min(area.height.saturating_sub(2));

    if width < 30 || height < 6 {
        return;
    }

    let x = area.left() + (area.width.saturating_sub(width)) / 2;
    let y = area.top() + (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    Clear.render(popup_area, buf);

    let block = Block::default()
        .title(" Tabela de Quotas (Janelas × Lanes) ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .style(Style::default().bg(Color::Rgb(16, 20, 30)));

    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    let header_row = Row::new(vec![
        Cell::from("Harness").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Conta").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Tipo").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Janela / Lane").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Uso %").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Status").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Reset em").style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ])
    .bottom_margin(1);

    let mut all_rows = Vec::new();

    for snapshot in snapshots {
        let harness = snapshot.slot.harness.binary_name().to_string();
        let account = snapshot.slot.account.clone();
        let status = snapshot.status.to_string();

        // Top-level windows
        for win in &snapshot.windows {
            let win_label = match win.kind {
                WindowKind::FiveHour => "5h (cinco horas)",
                WindowKind::SevenDay => "7d (sete dias)",
                WindowKind::Cycle => "ciclo mensal",
                WindowKind::Custom(ref s) => s.as_str(),
            };
            let color = quota_color(win.used_pct);

            all_rows.push(Row::new(vec![
                Cell::from(harness.clone()),
                Cell::from(account.clone()),
                Cell::from("Janela").style(Style::default().fg(Color::DarkGray)),
                Cell::from(win_label),
                Cell::from(format!("{:.1}%", win.used_pct))
                    .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Cell::from(status.clone()),
                Cell::from(format_duration(win.resets_in_s)),
            ]));
        }

        // Lanes
        for lane in &snapshot.lanes {
            for win in &lane.windows {
                let color = quota_color(win.used_pct);
                all_rows.push(Row::new(vec![
                    Cell::from(harness.clone()),
                    Cell::from(account.clone()),
                    Cell::from(format!("Lane ({})", lane.kind))
                        .style(Style::default().fg(Color::LightBlue)),
                    Cell::from(lane.name.clone()),
                    Cell::from(format!("{:.1}%", win.used_pct))
                        .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Cell::from(status.clone()),
                    Cell::from(format_duration(win.resets_in_s)),
                ]));
            }
        }
    }

    if all_rows.is_empty() {
        all_rows.push(Row::new(vec![
            Cell::from("Nenhuma quota recebida do daemon ainda."),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]));
    }

    let visible_rows: Vec<Row> = all_rows.into_iter().skip(scroll).collect();

    let widths = [
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(20),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Min(10),
    ];

    let table = Table::new(visible_rows, widths)
        .header(header_row)
        .style(Style::default().fg(Color::White));

    table.render(inner, buf);
}
