//! Persistent connection-state banner (design doc §2.5).
//!
//! Unlike `footer`'s `status_message`, this never auto-expires — it reflects
//! `app.connection` for as long as the client is not fully attached, and it is
//! drawn on every frame regardless of whether the daemon socket is currently
//! readable, so the reconnect machine never has to block rendering to show it.

use crate::state::{App, ConnectionState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Paragraph, Widget};

/// Text for the current connection banner, or `None` when fully connected
/// (no banner reserved on screen).
pub fn banner_text(app: &App) -> Option<String> {
    match &app.connection {
        ConnectionState::Connected => None,
        ConnectionState::Connecting => Some("[CONECTANDO] Conectando ao daemon...".to_string()),
        ConnectionState::PairingRequired { code } => Some(format!(
            "[PAREAMENTO NECESSÁRIO] Código: {code} — aprove em {}",
            crate::doctor::GITHUB_APPROVAL_URL
        )),
        ConnectionState::Reconnecting {
            class_text,
            attempt,
        } => Some(format!(
            "[DESCONECTADO] {class_text}. Reconectando (tentativa {attempt})... \
             Sessão continua ativa no servidor. Ctrl+C para sair sem encerrar o trabalho."
        )),
    }
}

/// Height in rows the banner occupies (0 when not shown).
pub fn banner_height(app: &App) -> u16 {
    if banner_text(app).is_some() {
        1
    } else {
        0
    }
}

pub fn render_banner(app: &App, area: Rect, buf: &mut Buffer) {
    if let Some(text) = banner_text(app) {
        Paragraph::new(text)
            .style(Style::default().fg(Color::Black).bg(Color::Yellow))
            .render(area, buf);
    }
}
