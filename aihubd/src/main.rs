use clap::Parser;
use std::path::PathBuf;
#[derive(Parser)]
#[command(about = "Foreground aihub session daemon")]
struct Args {
    #[arg(long)]
    socket: Option<PathBuf>,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    aihubd::Daemon::new(aihub_probe::probe_all, aihubd::spawn_pty)
        .with_catalog_paths(|harness| match harness {
            aihub_core::HarnessId::CursorAgent => Some(std::path::PathBuf::from("cursor-agent")),
            aihub_core::HarnessId::Antigravity => Some(std::path::PathBuf::from("agy")),
            _ => None,
        })
        .with_drain(|| async {
            aihub_memory::drain_spooled_handoffs(50)
                .await
                .map_err(Into::into)
        })
        .run(
            args.socket.unwrap_or_else(aihub_core::default_socket_path),
            aihubd::shutdown_signal(),
        )
        .await
}
