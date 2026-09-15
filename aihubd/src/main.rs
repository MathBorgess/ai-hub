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
        .run(
            args.socket.unwrap_or_else(aihub_core::default_socket_path),
            aihubd::shutdown_signal(),
        )
        .await
}
