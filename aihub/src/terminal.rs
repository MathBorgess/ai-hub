//! Terminal hygiene: raw mode management, alternate screen, signal handling, and panic hooks.

use anyhow::Result;
use std::io::{stdout, Write};

/// RAII Guard ensuring raw mode and alternate screen are restored on every exit path.
pub struct TerminalGuard {
    active: bool,
    restore_fn: Option<Box<dyn FnMut() + Send>>,
}

impl TerminalGuard {
    /// Initializes terminal raw mode and enters the alternate screen.
    pub fn new() -> Result<Self> {
        Self::new_with_writer(stdout())
    }

    /// Seam allowing custom writer for terminal initialization.
    /// Delegates to `new_with_seam` so both production and test paths
    /// share a single unified initializer.
    pub fn new_with_writer<W: Write>(writer: W) -> Result<Self> {
        Self::new_with_seam(writer, crossterm::terminal::enable_raw_mode, || {
            let _ = crossterm::terminal::disable_raw_mode();
        })
    }

    /// Testable seam for custom enable/disable closures and writer.
    pub fn new_with_seam<W: Write, E, D>(
        mut writer: W,
        enable_raw: E,
        mut disable_raw: D,
    ) -> Result<Self>
    where
        E: FnOnce() -> std::io::Result<()>,
        D: FnMut() + Send + 'static,
    {
        enable_raw()?;
        let mut guard = Self {
            active: true,
            restore_fn: Some(Box::new(move || {
                disable_raw();
            })),
        };

        if let Err(e) = crossterm::execute!(
            writer,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        ) {
            guard.restore();
            return Err(e.into());
        }

        if let Err(e) = writer.flush() {
            guard.restore();
            return Err(e.into());
        }

        Ok(guard)
    }

    /// Explicitly restores normal terminal settings.
    pub fn restore(&mut self) {
        if self.active {
            if let Some(mut custom_restore) = self.restore_fn.take() {
                custom_restore();
            } else {
                let _ = crossterm::terminal::disable_raw_mode();
            }
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
        let term_res = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        let int_res = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt());
        match (term_res, int_res) {
            (Ok(mut term), Ok(mut interrupt)) => {
                tokio::select! {
                    _ = term.recv() => (),
                    _ = interrupt.recv() => (),
                }
            }
            _ => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
