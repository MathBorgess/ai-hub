use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Default loopback port for the WebSocket remote transport (ADR §8: `127.0.0.1:9920`, free
/// of the box's existing `a2a`/`8787`/`9900`/`9910` ports).
const DEFAULT_WS_ADDR: &str = "127.0.0.1:9920";

#[derive(Parser)]
#[command(about = "Foreground aihub session daemon")]
struct Args {
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Loopback address for the WebSocket remote transport (Fatia 2). The Unix socket path
    /// above keeps working unchanged regardless of this flag (ADR §6). Falls back to
    /// `AIHUB_WS_ADDR`, then `DEFAULT_WS_ADDR` (`clap`'s `env` feature isn't enabled in this
    /// workspace, so the fallback is manual instead of a derive attribute).
    #[arg(long)]
    ws_addr: Option<SocketAddr>,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let ws_addr = args.ws_addr.unwrap_or_else(|| {
        std::env::var("AIHUB_WS_ADDR")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| DEFAULT_WS_ADDR.parse().expect("valid default ws addr"))
    });
    let daemon = aihubd::Daemon::new(aihub_probe::probe_all, aihubd::spawn_pty)
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
        .with_sessions_catalog_path(aihub_core::default_data_dir().join("sessions.json"));

    let socket_path = args.socket.unwrap_or_else(aihub_core::default_socket_path);
    let (stop_uds, stopped_uds) = tokio::sync::oneshot::channel();
    let (stop_ws, stopped_ws) = tokio::sync::oneshot::channel();
    tokio::spawn(async {
        aihubd::shutdown_signal().await;
        let _ = stop_uds.send(());
        let _ = stop_ws.send(());
    });

    let mut uds = {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            daemon
                .run(socket_path, async {
                    stopped_uds.await.ok();
                })
                .await
        })
    };
    let mut ws = {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            daemon
                .run_ws(ws_addr, async {
                    stopped_ws.await.ok();
                })
                .await
        })
    };
    // Either listener failing (e.g. a stale/live socket) tears the other down promptly instead
    // of leaving it running forever waiting on a shutdown signal that will never come — a bind
    // failure must still exit the process quickly, matching pre-existing behavior of `run()`
    // alone (`aihubd/tests/socket.rs::binary_refuses_live_socket_without_probing`).
    tokio::select! {
        res = &mut uds => { ws.abort(); return res?; }
        res = &mut ws => { uds.abort(); return res?; }
    }
}
