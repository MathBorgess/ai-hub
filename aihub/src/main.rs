//! aihub — Interactive multi-harness terminal supervisor and TUI client.
//! Owned and implemented by Session 09.

use anyhow::Result;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = aihub::cli::Cli::parse();
    aihub::run(cli).await
}
