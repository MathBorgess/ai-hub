//! Unix domain socket client, framing, and daemon autostart lifecycle.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use aihub_core::{
    encode_frame, ClientMessage, DaemonMessage, IpcMessage, PROTOCOL_VERSION,
};
use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// Connects to the daemon socket, automatically launching `aihubd` detached if not running.
pub async fn connect_or_start_daemon(socket_path: &Path) -> Result<UnixStream> {
    match UnixStream::connect(socket_path).await {
        Ok(stream) => Ok(stream),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
            || e.kind() == std::io::ErrorKind::NotFound =>
        {
            start_daemon_detached(socket_path)?;
            // Retry connecting for up to 5 seconds
            let start = Instant::now();
            let timeout = Duration::from_secs(5);
            while start.elapsed() < timeout {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Ok(stream) = UnixStream::connect(socket_path).await {
                    return Ok(stream);
                }
            }
            bail!(
                "Timed out waiting for aihubd to bind socket at {:?}",
                socket_path
            );
        }
        Err(e) => Err(e).context(format!("Failed to connect to socket at {:?}", socket_path)),
    }
}

/// Spawns `aihubd` detached from the same install directory as the running `aihub` binary.
pub fn start_daemon_detached(socket_path: &Path) -> Result<()> {
    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("Failed to get parent directory of {:?}", current_exe))?;

    let mut aihubd_path = exe_dir.join("aihubd");
    if !aihubd_path.exists() {
        #[cfg(target_os = "windows")]
        {
            aihubd_path = exe_dir.join("aihubd.exe");
        }
    }

    if !aihubd_path.exists() {
        // Fallback: check PATH
        if let Ok(path) = which("aihubd") {
            aihubd_path = path;
        } else {
            bail!(
                "aihubd executable not found in install directory {:?} or in PATH",
                exe_dir
            );
        }
    }

    let mut cmd = std::process::Command::new(&aihubd_path);
    cmd.arg("--socket").arg(socket_path);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.spawn()
        .context(format!("Failed to spawn aihubd from {:?}", aihubd_path))?;
    Ok(())
}

fn which(binary: &str) -> Result<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!("binary {} not found in PATH", binary)
}

/// Sends a ClientMessage over the framed socket writer.
pub async fn send_msg(writer: &mut OwnedWriteHalf, msg: &ClientMessage) -> Result<()> {
    let frame = encode_frame(&msg.clone().into())
        .map_err(|e| anyhow!("Failed to encode IPC frame: {}", e))?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads the next DaemonMessage from the framed socket reader.
pub async fn recv_msg(reader: &mut OwnedReadHalf) -> Result<DaemonMessage> {
    let len = reader.read_u32().await? as usize;
    if len > aihub_core::DEFAULT_MAX_FRAME_LENGTH {
        bail!("Frame size {} exceeds maximum allowed", len);
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let ipc: IpcMessage = serde_json::from_slice(&buf)?;
    match ipc {
        IpcMessage::Daemon(m) => Ok(m),
        IpcMessage::Client(_) => bail!("Unexpected client message received from daemon"),
    }
}

/// Performs the initial protocol handshake: sends `Hello`, verifies response `Hello`.
pub async fn perform_handshake(
    writer: &mut OwnedWriteHalf,
    reader: &mut OwnedReadHalf,
) -> Result<()> {
    send_msg(
        writer,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;

    let response = tokio::time::timeout(Duration::from_secs(5), recv_msg(reader))
        .await
        .context("Timeout waiting for Hello from daemon")??;

    match response {
        DaemonMessage::Hello { version } => {
            if version != PROTOCOL_VERSION {
                bail!(
                    "Protocol version mismatch: daemon is {}, client is {}",
                    version,
                    PROTOCOL_VERSION
                );
            }
            Ok(())
        }
        DaemonMessage::Error { code, message } => {
            bail!("Daemon returned error during handshake: [{}] {}", code, message)
        }
        other => bail!("Expected Hello from daemon, received: {:?}", other),
    }
}
