//! ai-memory adapter for handoff persistence.
//!
//! Citations:
//! - Upstream source: `github.com/akitaonrails/ai-memory` at git tag `v2.2.2`.
//! - Protocol route and tool handler: `crates/ai-memory-mcp/src/server.rs` (`memory_handoff_begin`,
//!   lines 3483-3568) mounted via StreamableHttpService at `POST /mcp` in
//!   `crates/ai-memory-cli/src/commands/serve.rs` (line 1299).
//! - Workspace and project resolution for cwd: `crates/ai-memory-hooks/src/router.rs`
//!   (`resolve_project_ids_inner`, lines 1966-2090) defaulting workspace to `"default"`
//!   (`DEFAULT_WORKSPACE_NAME`) and project to passed identity.
//! - Upstream `NewHandoff` schema: `crates/ai-memory-core/src/handoff.rs` (`v2.2.2`).
//! - Server URL & token environment variables: `docs/install.md` (`AI_MEMORY_SERVER_URL`
//!   defaulting to `http://127.0.0.1:49374`, and `AI_MEMORY_AUTH_TOKEN`).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aihub_core::{paths::default_data_dir, HarnessId, SessionId};
use serde::{Deserialize, Serialize};

use crate::brief::parse_brief_context;
use crate::{BriefPair, HandoffDestination, MemoryError};

/// Default URL for the local ai-memory server per docs/install.md.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

/// Bounded number of spooled handoffs to drain per successful delivery.
pub const MAX_DRAIN_PER_CALL: usize = 20;

/// Whole-delivery deadline (connect, write, read): 5 seconds (N1).
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum allowed response body size: 1 MiB (N1).
pub const MAX_RESPONSE_BODY_SIZE: usize = 1024 * 1024;

/// Maximum spool file size: 10 MiB (N5).
pub const MAX_SPOOL_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum records in spool: 10,000 (N5).
pub const MAX_SPOOL_RECORDS: usize = 10_000;

/// Locally spooled handoff record carrying the immutable sanitized payload (N4).
/// Never stores paths to re-read later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpooledRecord {
    pub session_id: String,
    pub from_harness: String,
    pub to_harness: String,
    pub project: String,
    pub recorded_at: String,
    pub goal: String,
    pub decisions: Vec<String>,
    pub last_output_summary: String,
}

impl SpooledRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: impl Into<String>,
        from_harness: impl Into<String>,
        to_harness: impl Into<String>,
        project: impl Into<String>,
        recorded_at: impl Into<String>,
        goal: impl Into<String>,
        decisions: Vec<String>,
        last_output_summary: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            from_harness: from_harness.into(),
            to_harness: to_harness.into(),
            project: project.into(),
            recorded_at: recorded_at.into(),
            goal: goal.into(),
            decisions,
            last_output_summary: last_output_summary.into(),
        }
    }

    /// Creates a self-contained record by reading the brief file once at creation time (N4).
    pub fn from_brief(
        session_id: &SessionId,
        from: HarnessId,
        to: HarnessId,
        brief: &BriefPair,
        project: &str,
    ) -> Result<Self, MemoryError> {
        let content = std::fs::read_to_string(&brief.brief_path)?;
        let parsed = parse_brief_context(&content);

        Ok(Self {
            session_id: session_id.as_str().to_string(),
            from_harness: harness_label(from).to_string(),
            to_harness: harness_label(to).to_string(),
            project: project.to_string(),
            recorded_at: time_now_rfc3339(),
            goal: parsed.goal,
            decisions: parsed.decisions,
            last_output_summary: parsed.last_output_summary,
        })
    }
}

/// Records handoff metadata and returns whether it reached the live backend or was spooled locally (§3.5).
pub async fn record_handoff_destination(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
) -> Result<HandoffDestination, MemoryError> {
    let server_url =
        std::env::var("AI_MEMORY_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());
    let auth_token = std::env::var("AI_MEMORY_AUTH_TOKEN").ok();
    let data_dir = std::env::var_os("AIHUB_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);

    record_handoff_to(
        session_id,
        from,
        to,
        brief,
        project,
        &server_url,
        auth_token.as_deref(),
        &data_dir,
    )
    .await
}

/// Convenience alias returning true if delivered to the backend, false if spooled locally.
pub async fn record_handoff_delivered(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
) -> Result<bool, MemoryError> {
    let dest = record_handoff_destination(session_id, from, to, brief, project).await?;
    Ok(dest.is_delivered())
}

/// Internal implementation taking explicit parameters for testing.
#[allow(clippy::too_many_arguments)]
pub async fn record_handoff_to(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
) -> Result<HandoffDestination, MemoryError> {
    let record = SpooledRecord::from_brief(session_id, from, to, brief, project)?;
    record_spooled_record_to(&record, server_url, auth_token, data_dir).await
}

/// Delivers a spooled record or spools it on offline/failure.
pub async fn record_spooled_record_to(
    record: &SpooledRecord,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
) -> Result<HandoffDestination, MemoryError> {
    let spool_path = data_dir.join("handoffs.jsonl");

    // Attempt direct delivery to backend via MCP POST /mcp
    match deliver_record(server_url, auth_token, record).await {
        Ok(()) => {
            // Success: drain previously spooled records and surface drain errors (N3)
            drain_spool(server_url, auth_token, &spool_path, MAX_DRAIN_PER_CALL).await?;
            Ok(HandoffDestination::Delivered)
        }
        Err(_) => {
            // Offline fallback: deadline expired, body over cap, unreachable, or 4xx/5xx/MCP error (N1, N2)
            append_spool(&spool_path, record)?;
            Ok(HandoffDestination::Spooled)
        }
    }
}

pub(crate) fn append_to_spool_file(
    spool_path: &Path,
    record: &SpooledRecord,
) -> Result<(), MemoryError> {
    append_spool(spool_path, record)
}

fn format_summary_payload(record: &SpooledRecord) -> String {
    let mut parts = Vec::new();
    if !record.goal.is_empty() {
        parts.push(record.goal.clone());
    }
    if !record.last_output_summary.is_empty() {
        parts.push(format!("Summary: {}", record.last_output_summary));
    }
    parts.push(format!(
        "Handoff from {} to {} for session {}",
        record.from_harness, record.to_harness, record.session_id
    ));
    parts.join("\n\n")
}

async fn deliver_record(
    server_url: &str,
    auth_token: Option<&str>,
    record: &SpooledRecord,
) -> Result<(), ()> {
    let summary = format_summary_payload(record);

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_handoff_begin",
            "arguments": {
                "workspace": "default",
                "project": record.project,
                "summary": summary,
                "open_questions": Vec::<String>::new(),
                "next_steps": record.decisions,
                "files_touched": Vec::<String>::new(),
                "cwd": null,
                "shared": true
            }
        }
    });

    let body = serde_json::to_vec(&payload).map_err(|_| ())?;
    let (status, resp_bytes) = post_mcp(server_url, auth_token, &body).await?;

    validate_mcp_response(status, &resp_bytes, &serde_json::json!(1))
}

/// Drains oldest spooled records first, bounded by `limit`.
pub async fn drain_spool(
    server_url: &str,
    auth_token: Option<&str>,
    spool_path: &Path,
    limit: usize,
) -> Result<usize, MemoryError> {
    drain_spool_with_hook(server_url, auth_token, spool_path, limit, None).await
}

/// Drains spool with an optional hook called before renaming remainder (narrow seam for N3 testing).
pub async fn drain_spool_with_hook(
    server_url: &str,
    auth_token: Option<&str>,
    spool_path: &Path,
    limit: usize,
    before_rename: Option<&(dyn Fn() -> Result<(), MemoryError> + Send + Sync)>,
) -> Result<usize, MemoryError> {
    let parent = spool_path.parent().unwrap_or_else(|| Path::new("."));
    ensure_spool_dir(parent)?;

    let lock_path = parent.join("handoffs.lock");
    let _lock = SpoolLock::acquire(&lock_path)?;

    if !spool_path.exists() {
        return Ok(0);
    }

    let file = std::fs::File::open(spool_path)?;
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(file);

    let lines_iter = reader.lines();
    let mut drained_count = 0usize;
    let mut stop_delivery = false;
    let mut undelivered_records: Vec<String> = Vec::new();

    for line_res in lines_iter {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if !stop_delivery && drained_count < limit {
            match serde_json::from_str::<SpooledRecord>(trimmed) {
                Ok(rec) => {
                    if deliver_record(server_url, auth_token, &rec).await.is_ok() {
                        drained_count += 1;
                    } else {
                        // Delivery failed: stop draining and keep this record plus the rest
                        stop_delivery = true;
                        undelivered_records.push(line);
                    }
                }
                Err(_) => {
                    // Malformed record line: preserve it and stop
                    stop_delivery = true;
                    undelivered_records.push(line);
                }
            }
        } else {
            undelivered_records.push(line);
        }
    }

    if drained_count == 0 {
        return Ok(0);
    }

    // Atomic rewrite of remainder (N3):
    let temp_file_name = format!(
        ".handoffs.jsonl.tmp.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let temp_path = parent.join(temp_file_name);

    let write_res = (|| -> Result<(), MemoryError> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut temp_file = options.open(&temp_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o600));
        }

        for rem in &undelivered_records {
            writeln!(temp_file, "{rem}")?;
        }
        temp_file.sync_all()?;

        if let Some(hook) = before_rename {
            hook()?;
        }

        std::fs::rename(&temp_path, spool_path)?;

        let dir_file = std::fs::File::open(parent)?;
        dir_file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    Ok(drained_count)
}

fn append_spool(spool_path: &Path, record: &SpooledRecord) -> Result<(), MemoryError> {
    let parent = spool_path.parent().unwrap_or_else(|| Path::new("."));
    ensure_spool_dir(parent)?;

    let lock_path = parent.join("handoffs.lock");
    let _lock = SpoolLock::acquire(&lock_path)?;

    let line = serde_json::to_string(record)?;
    let line_bytes = line.len() as u64 + 1; // +1 for newline

    if spool_path.exists() {
        let meta = std::fs::metadata(spool_path)?;
        let current_size = meta.len();
        if current_size + line_bytes > MAX_SPOOL_BYTES {
            return Err(MemoryError::SpoolFull(format!(
                "spool size limit (10 MiB) reached: current {} bytes, adding {} bytes",
                current_size, line_bytes
            )));
        }

        // Count records by streaming lines
        let file = std::fs::File::open(spool_path)?;
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);
        let mut count = 0usize;
        for l in reader.lines() {
            let l = l?;
            if !l.trim().is_empty() {
                count += 1;
            }
        }
        if count >= MAX_SPOOL_RECORDS {
            return Err(MemoryError::SpoolFull(format!(
                "spool record count limit (10,000) reached: {} records",
                count
            )));
        }
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(spool_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(spool_path, std::fs::Permissions::from_mode(0o600));
    }
    writeln!(file, "{line}")?;
    file.sync_all()?;
    Ok(())
}

/// Ensures directory exists with 0700 permissions (N3, Permissions).
pub(crate) fn ensure_spool_dir(dir: &Path) -> Result<(), MemoryError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Cross-process exclusive file lock for spool access (N3).
pub(crate) struct SpoolLock {
    file: std::fs::File,
}

impl SpoolLock {
    pub(crate) fn acquire(lock_path: &Path) -> Result<Self, MemoryError> {
        if let Some(parent) = lock_path.parent() {
            ensure_spool_dir(parent)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(lock_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(lock_path, std::fs::Permissions::from_mode(0o600));
        }
        file.lock()?;
        Ok(Self { file })
    }
}

impl Drop for SpoolLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn harness_label(h: HarnessId) -> &'static str {
    match h {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::CursorAgent => "cursor-agent",
        HarnessId::Antigravity => "antigravity",
    }
}

fn time_now_rfc3339() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", dur.as_secs())
}

fn parse_url(raw: &str) -> Result<(String, u16), std::io::Error> {
    let s = raw.strip_prefix("http://").unwrap_or(raw);
    let host_port = s.split('/').next().unwrap_or(s);
    let (host, port_str) = host_port.split_once(':').unwrap_or((host_port, "49374"));
    let port: u16 = port_str
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    Ok((host.to_string(), port))
}

async fn post_mcp(url: &str, auth_token: Option<&str>, body: &[u8]) -> Result<(u16, Vec<u8>), ()> {
    tokio::time::timeout(DELIVERY_TIMEOUT, post_mcp_raw(url, auth_token, body))
        .await
        .map_err(|_| ())?
}

async fn post_mcp_raw(
    url: &str,
    auth_token: Option<&str>,
    body: &[u8],
) -> Result<(u16, Vec<u8>), ()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (host, port) = parse_url(url).map_err(|_| ())?;
    let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|_| ())?;

    let auth_header = match auth_token {
        Some(tok) if !tok.is_empty() => format!("Authorization: Bearer {tok}\r\n"),
        _ => String::new(),
    };

    let req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Accept: application/json, text/event-stream\r\n\
         {auth_header}\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    stream.write_all(req.as_bytes()).await.map_err(|_| ())?;
    stream.write_all(body).await.map_err(|_| ())?;
    stream.flush().await.map_err(|_| ())?;

    let mut resp_bytes = Vec::new();
    let mut buf = [0u8; 8192];
    let mut header_end = None;

    loop {
        let n = stream.read(&mut buf).await.map_err(|_| ())?;
        if n == 0 {
            break;
        }
        resp_bytes.extend_from_slice(&buf[..n]);

        if header_end.is_none() {
            if let Some(pos) = resp_bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = Some(pos + 4);
                if let Ok(headers_str) = std::str::from_utf8(&resp_bytes[..pos]) {
                    for line in headers_str.lines() {
                        if let Some(val) = line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            if let Ok(len) = val.trim().parse::<usize>() {
                                if len > MAX_RESPONSE_BODY_SIZE {
                                    return Err(());
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(end) = header_end {
            let body_len = resp_bytes.len().saturating_sub(end);
            if body_len > MAX_RESPONSE_BODY_SIZE {
                return Err(());
            }
        }
    }

    let status_code = parse_http_status(&resp_bytes).ok_or(())?;
    let body_bytes = parse_http_body(&resp_bytes);
    if body_bytes.len() > MAX_RESPONSE_BODY_SIZE {
        return Err(());
    }

    Ok((status_code, body_bytes))
}

fn parse_http_status(resp: &[u8]) -> Option<u16> {
    let header_text = std::str::from_utf8(resp).ok()?;
    let first_line = header_text.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _proto = parts.next()?;
    let code_str = parts.next()?;
    code_str.parse::<u16>().ok()
}

fn parse_http_body(resp: &[u8]) -> Vec<u8> {
    if let Some(pos) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
        resp[pos + 4..].to_vec()
    } else {
        resp.to_vec()
    }
}

/// Validates MCP delivery response per N2:
/// 2xx status, valid JSON or SSE framing, id match, no top-level error, result without isError: true.
pub(crate) fn validate_mcp_response(
    status: u16,
    body: &[u8],
    expected_id: &serde_json::Value,
) -> Result<(), ()> {
    if !(200..300).contains(&status) {
        return Err(());
    }
    if body.is_empty() {
        return Err(());
    }

    let json_val = parse_json_or_sse(body)?;

    if json_val.get("id") != Some(expected_id) {
        return Err(());
    }

    if let Some(err) = json_val.get("error") {
        if !err.is_null() {
            return Err(());
        }
    }

    let result = json_val.get("result").ok_or(())?;
    if let Some(is_error) = result.get("isError").and_then(|v| v.as_bool()) {
        if is_error {
            return Err(());
        }
    }

    Ok(())
}

fn parse_json_or_sse(body: &[u8]) -> Result<serde_json::Value, ()> {
    // Try raw JSON framing
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if v.is_object()
            && (v.get("jsonrpc").is_some() || v.get("result").is_some() || v.get("error").is_some())
        {
            return Ok(v);
        }
    }

    // Try SSE framing where data: lines carry JSON-RPC message
    let text = std::str::from_utf8(body).map_err(|_| ())?;
    let mut sse_data = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("data:") {
            let data_str = rest.trim();
            if !data_str.is_empty() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(data_str) {
                    if v.is_object()
                        && (v.get("jsonrpc").is_some()
                            || v.get("result").is_some()
                            || v.get("error").is_some())
                    {
                        return Ok(v);
                    }
                }
                sse_data.push(data_str);
            }
        }
    }

    if !sse_data.is_empty() {
        let joined = sse_data.join("\n");
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&joined) {
            if v.is_object() {
                return Ok(v);
            }
        }
    }

    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    async fn start_fake_server(
        status_code: u16,
        return_body: &'static str,
        content_type: &'static str,
        max_accepts: usize,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let _ = ready_tx.send(());
            for _ in 0..max_accepts {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };
                if let Some(header_end) = header_end {
                    let content_length = String::from_utf8_lossy(&buf[..header_end])
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        let mut chunk = [0u8; 4096];
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
                let req_str = String::from_utf8_lossy(&buf).to_string();
                req_clone.lock().await.push(req_str);

                let resp = format!(
                    "HTTP/1.1 {} Status\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_code,
                    content_type,
                    return_body.len(),
                    return_body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        ready_rx
            .await
            .expect("fake ai-memory server task ended early");
        (port, requests)
    }

    #[tokio::test]
    async fn test_adapter_success_delivers_to_ai_memory() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-123"}}"#,
            "application/json",
            3,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-success-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let worktree = dir.join("my-cool-project");
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nShip ai-memory adapter\n## Constraints\n- Context: initial\n- Decision: use rust\n## Last turn (outgoing harness)\nDone shipping\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-success");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "my-cool-project",
            &server_url,
            Some("fake-token"),
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Delivered);
        assert!(dest.is_delivered());
        assert!(!dest.is_spooled());

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains("POST /mcp HTTP/1.1"));
        assert!(recorded[0].contains("Authorization: Bearer fake-token"));
        assert!(recorded[0].contains("\"memory_handoff_begin\""));
        assert!(recorded[0].contains("\"project\":\"my-cool-project\""));
        assert!(recorded[0].contains("\"workspace\":\"default\""));
        assert!(recorded[0].contains("Decision: use rust"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_401_unauthorized_spools_locally() {
        let (port, _) = start_fake_server(401, "", "application/json", 1).await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-401-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest 401 fallback\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-401");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj401",
            &server_url,
            Some("bad-token"),
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-401"));
        assert!(content.contains("proj401"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_unreachable_spools_locally() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-unreach-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest unreachable\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-unreach");
        let server_url = "http://127.0.0.1:49999";

        let dest = record_handoff_to(
            &session_id,
            HarnessId::CursorAgent,
            HarnessId::Antigravity,
            &brief,
            "projunreach",
            server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-unreach"));
        assert!(content.contains("projunreach"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_drain_on_later_success() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-drain-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief1 = handoffs_dir.join("01.md");
        std::fs::write(&brief1, "## Goal\nFirst spooled handoff\n").unwrap();
        let prompt1 = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt1, "p1").unwrap();

        let brief2 = handoffs_dir.join("02.md");
        std::fs::write(&brief2, "## Goal\nSecond live handoff\n").unwrap();
        let prompt2 = handoffs_dir.join("02.prompt.md");
        std::fs::write(&prompt2, "p2").unwrap();

        let sid1 = SessionId::new("sess-spooled-1");
        let sid2 = SessionId::new("sess-live-2");

        // 1. Spool first handoff while offline
        let dest1 = record_handoff_to(
            &sid1,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &BriefPair {
                brief_path: brief1,
                prompt_path: prompt1,
            },
            "projdrain",
            "http://127.0.0.1:49999",
            None,
            &dir,
        )
        .await
        .unwrap();
        assert_eq!(dest1, HandoffDestination::Spooled);

        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let spool_content = std::fs::read_to_string(&spool_file).unwrap();
        assert!(spool_content.contains("sess-spooled-1"));

        // 2. Start fake server
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok"}}"#,
            "application/json",
            3,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");

        // 3. Record second handoff successfully -> drains spool!
        let dest2 = record_handoff_to(
            &sid2,
            HarnessId::Codex,
            HarnessId::Antigravity,
            &BriefPair {
                brief_path: brief2,
                prompt_path: prompt2,
            },
            "projdrain",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest2, HandoffDestination::Delivered);

        let mut len = 0usize;
        for _ in 0..50 {
            len = reqs.lock().await.len();
            if len >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(len, 2);
        let recorded = reqs.lock().await;
        let has_live = recorded.iter().any(|r| r.contains("Second live handoff"));
        let has_drained = recorded.iter().any(|r| r.contains("First spooled handoff"));
        assert!(has_live);
        assert!(has_drained);

        if spool_file.exists() {
            let rem = std::fs::read_to_string(&spool_file).unwrap();
            assert!(rem.trim().is_empty());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Regression Findings Tests ---

    #[tokio::test]
    async fn n1_silent_server_spools_within_deadline() {
        // Silent server: accepts TCP connection but never sends any bytes back
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let _ = ready_tx.send(());
            if let Ok((mut socket, _)) = listener.accept().await {
                // Read request but never respond, keep open until timeout
                let mut buf = [0u8; 1024];
                while socket.read(&mut buf).await.unwrap_or(0) > 0 {}
            }
        });
        ready_rx.await.unwrap();

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n1-silent-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nSilent server test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let start = tokio::time::Instant::now();
        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n1-silent"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n1",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        let elapsed = start.elapsed();
        // Deadline is 5 seconds. Must complete within ~6 seconds and spool!
        assert!(elapsed >= Duration::from_secs(4));
        assert!(elapsed < Duration::from_secs(7));
        assert_eq!(dest, HandoffDestination::Spooled);

        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-n1-silent"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n1_response_body_over_cap_spools_record() {
        // Server sends response body over 1 MiB
        let oversized_body = "x".repeat(MAX_RESPONSE_BODY_SIZE + 1024);
        let (port, _) = start_fake_server(
            200,
            Box::leak(oversized_body.into_boxed_str()),
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n1-cap-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nBody over cap test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n1-cap"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n1",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_tool_is_error_true_spools_record() {
        // 200 with result.isError = true
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"failed"}]}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-iserror-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTool isError test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n2-iserror"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_empty_body_and_malformed_spools_record() {
        // 200 with empty body
        let (port, _) = start_fake_server(200, "", "application/json", 1).await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-empty-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nEmpty body test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n2-empty"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_sse_error_spools_and_sse_success_delivers() {
        // 1. SSE carrying RPC error
        let (port_err, _) = start_fake_server(
            200,
            "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32603,\"message\":\"internal\"}}\r\n\r\n",
            "text/event-stream",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-sse-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nSSE test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port_err}");
        let dest_err = record_handoff_to(
            &SessionId::new("sess-n2-sse-err"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest_err, HandoffDestination::Spooled);

        // 2. SSE carrying valid result
        let (port_ok, _) = start_fake_server(
            200,
            "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"handoff_id\":\"h-sse\",\"isError\":false}}\r\n\r\n",
            "text/event-stream",
            2,
        )
        .await;
        let server_url_ok = format!("http://127.0.0.1:{port_ok}");
        let dest_ok = record_handoff_to(
            &SessionId::new("sess-n2-sse-ok"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url_ok,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest_ok, HandoffDestination::Delivered);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_mismatched_id_spools_record() {
        // Server responds with mismatched ID 999 instead of 1
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":999,"result":{"handoff_id":"wrong-id"}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-id-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nID mismatch test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n2-id"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n3_interrupted_rewrite_keeps_pending_records() {
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok"}}"#,
            "application/json",
            5,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n3-rewrite-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");

        let rec1 = SpooledRecord::new(
            "s1",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g1",
            vec![],
            "sum1",
        );
        let rec2 = SpooledRecord::new(
            "s2",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g2",
            vec![],
            "sum2",
        );
        append_spool(&spool_path, &rec1).unwrap();
        append_spool(&spool_path, &rec2).unwrap();

        let initial_spool = std::fs::read_to_string(&spool_path).unwrap();

        // Inject failure between temp file write and rename
        let server_url = format!("http://127.0.0.1:{port}");
        let hook = || -> Result<(), MemoryError> {
            Err(MemoryError::Io(std::io::Error::other(
                "injected disk-full crash before rename",
            )))
        };

        let res = drain_spool_with_hook(&server_url, None, &spool_path, 1, Some(&hook)).await;
        assert!(res.is_err(), "interrupted rewrite must return error");

        // The original spool MUST be completely intact!
        let current_spool = std::fs::read_to_string(&spool_path).unwrap();
        assert_eq!(
            current_spool, initial_spool,
            "original spool must be intact after crash before rename"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n3_cross_process_lock_serializes_append_and_drain() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n3-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let lock_path = dir.join("handoffs.lock");

        let lock1 = SpoolLock::acquire(&lock_path).expect("first lock acquire");

        // Second lock attempt from same/another process should be blocked or fail
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let try_res = f.try_lock();
        assert!(
            try_res.is_err(),
            "second lock acquire must fail when lock is held"
        );

        drop(lock1);
        f.try_lock()
            .expect("lock acquire succeeds after first lock released");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n4_deleted_brief_delivers_full_content_from_spool() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n4-deleted-brief-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(
            &brief_path,
            "## Goal\nRecoverable goal\n## Constraints\n- Context: ctx\n- Decision: argon2\n- Decision: strict\n## Last turn (outgoing harness)\nFinal words\n",
        )
        .unwrap();
        let brief = BriefPair {
            brief_path: brief_path.clone(),
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        // 1. Record while backend is offline -> spooled
        let dest = record_handoff_to(
            &SessionId::new("sess-n4-deleted"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "project-n4",
            "http://127.0.0.1:49999",
            None,
            &dir,
        )
        .await
        .unwrap();
        assert_eq!(dest, HandoffDestination::Spooled);

        // 2. Delete the brief file completely from disk!
        std::fs::remove_file(&brief_path).unwrap();
        assert!(!brief_path.exists());

        // 3. Start server and drain spool
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-n4"}}"#,
            "application/json",
            2,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");
        let spool_path = dir.join("handoffs.jsonl");

        let drained = drain_spool(&server_url, None, &spool_path, 10)
            .await
            .unwrap();
        assert_eq!(drained, 1);

        // 4. Verify delivered payload carries the full content (goal, decisions, project identity)
        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        let req = &recorded[0];
        assert!(
            req.contains("Recoverable goal"),
            "payload must carry original goal: {req}"
        );
        assert!(
            req.contains("argon2"),
            "payload must carry original decisions: {req}"
        );
        assert!(
            req.contains("project-n4"),
            "payload must carry original project identity: {req}"
        );
        assert!(
            req.contains("Final words"),
            "payload must carry last output: {req}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n4_online_delivery_carries_decisions_and_identity() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-n4-online"}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n4-online-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(
            &brief_path,
            "## Goal\nOnline handoff\n## Constraints\n- Context: ctx\n- Decision: sha256\n## Last turn (outgoing harness)\nDone\n",
        )
        .unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n4-online"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "project-n4-online",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Delivered);

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        let req = &recorded[0];
        assert!(req.contains("\"project\":\"project-n4-online\""));
        assert!(req.contains("\"next_steps\":[\"Decision: sha256\"]"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n5_append_over_limit_returns_spool_full_error() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n5-limit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");

        // Pre-create spool at the 10 MiB limit
        ensure_spool_dir(&dir).unwrap();
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&spool_path)
                .unwrap();
            let padding = vec![b' '; MAX_SPOOL_BYTES as usize];
            f.write_all(&padding).unwrap();
            f.sync_all().unwrap();
        }

        let rec = SpooledRecord::new(
            "s-over",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g",
            vec![],
            "sum",
        );
        let res = append_spool(&spool_path, &rec);

        assert!(res.is_err(), "append over limit must return error");
        match res.unwrap_err() {
            MemoryError::SpoolFull(msg) => {
                assert!(msg.contains("spool size limit"));
            }
            other => panic!("expected SpoolFull, got: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n5_timed_retry_drain_without_new_handoff() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-retry"}}"#,
            "application/json",
            3,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n5-timed-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");

        let rec1 = SpooledRecord::new(
            "s1",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g1",
            vec![],
            "sum1",
        );
        let rec2 = SpooledRecord::new(
            "s2",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g2",
            vec![],
            "sum2",
        );
        append_spool(&spool_path, &rec1).unwrap();
        append_spool(&spool_path, &rec2).unwrap();

        // Run independent public drain with NO new handoff
        let server_url = format!("http://127.0.0.1:{port}");
        let drained = crate::record::drain_spooled_handoffs_to(&server_url, None, &dir, 10)
            .await
            .unwrap();

        assert_eq!(drained, 2);
        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].contains("g1"));
        assert!(recorded[1].contains("g2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n10_real_session_path_layout_uses_passed_project_identity() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-n10"}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n10-path-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // Real session worktree layout: .../worktrees/<session-id>/.aihub/handoffs/01.md
        let session_id = "sess-uuid-777-abc";
        let worktree = dir.join("worktrees").join(session_id);
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nReal session path test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let sid = SessionId::new(session_id);
        let server_url = format!("http://127.0.0.1:{port}");
        let passed_repo_identity = "my-originating-repo";

        let dest = record_handoff_to(
            &sid,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            passed_repo_identity,
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Delivered);

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        let req = &recorded[0];
        // Must record passed repository identity, NOT the session-id directory name
        assert!(
            req.contains("\"project\":\"my-originating-repo\""),
            "request must carry passed project identity: {req}"
        );
        assert!(
            !req.contains("\"project\":\"sess-uuid-777-abc\""),
            "request must NOT use cwd session basename as project: {req}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_spool_permissions_0700_dir_0600_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-perms-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let spool_path = dir.join("handoffs.jsonl");
        let rec = SpooledRecord::new("s", "claude-code", "codex", "p", "0Z", "g", vec![], "sum");
        append_spool(&spool_path, &rec).unwrap();

        let dir_meta = std::fs::metadata(&dir).unwrap();
        let dir_mode = dir_meta.permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "directory mode must be 0700");

        let spool_meta = std::fs::metadata(&spool_path).unwrap();
        let spool_mode = spool_meta.permissions().mode() & 0o777;
        assert_eq!(spool_mode, 0o600, "spool file mode must be 0600");

        let lock_path = dir.join("handoffs.lock");
        let lock_meta = std::fs::metadata(&lock_path).unwrap();
        let lock_mode = lock_meta.permissions().mode() & 0o777;
        assert_eq!(lock_mode, 0o600, "lock file mode must be 0600");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
