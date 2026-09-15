//! Terminal hygiene: raw mode management, alternate screen, signal handling, and panic hooks.

use anyhow::Result;
use std::io::{stdout, Write};

/// RAII Guard ensuring raw mode and alternate screen are restored on every exit path.
pub struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    /// Initializes terminal raw mode and enters the alternate screen.
    pub fn new() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = stdout();
        crossterm::execute!(
            out,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        out.flush()?;
        Ok(Self { active: true })
    }

    /// Explicitly restores normal terminal settings.
    pub fn restore(&mut self) {
        if self.active {
            let _ = crossterm::terminal::disable_raw_mode();
            let mut out = stdout();
            let _ = crossterm::execute!(
                out,
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::cursor::Show
            );
            let _ = out.flush();
            self.active = false;
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Sets up a panic hook to guarantee terminal restoration before printing the panic.
pub fn setup_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = stdout();
        let _ = crossterm::execute!(
            out,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = out.flush();
        default_hook(panic_info);
    }));
}

/// Helper future that resolves when SIGTERM or SIGINT is received.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("register SIGTERM");
        let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("register SIGINT");
        tokio::select! {
            _ = term.recv() => (),
            _ = interrupt.recv() => (),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
