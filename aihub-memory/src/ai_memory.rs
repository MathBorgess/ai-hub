//! ai-memory adapter for handoff persistence.
//!
//! Citations:
//! - Upstream source: `github.com/akitaonrails/ai-memory` at git tag `v2.2.2`.
//! - Protocol route and tool handler: `crates/ai-memory-mcp/src/server.rs` (`memory_handoff_begin`,
//!   lines 3483-3568) mounted via StreamableHttpService at `POST /mcp` in
//!   `crates/ai-memory-cli/src/commands/serve.rs` (line 1299).
//! - Workspace and project resolution for cwd: `crates/ai-memory-hooks/src/router.rs`
//!   (`resolve_project_ids_inner`, lines 1966-2090) defaulting workspace to `"default"`
//!   (`DEFAULT_WORKSPACE_NAME`) and project to `basename(cwd)`.
//! - Upstream `NewHandoff` schema: `crates/ai-memory-core/src/handoff.rs` (`v2.2.2`).
//! - Server URL & token environment variables: `docs/install.md` (`AI_MEMORY_SERVER_URL`
//!   defaulting to `http://127.0.0.1:49374`, and `AI_MEMORY_AUTH_TOKEN`).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aihub_core::{paths::default_data_dir, HarnessId, SessionId};
use serde::{Deserialize, Serialize};

use crate::{BriefPair, HandoffDestination, MemoryError};

/// Default URL for the local ai-memory server per docs/install.md.
const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

/// Bounded number of spooled handoffs to drain per successful delivery.
const MAX_DRAIN_PER_CALL: usize = 20;

/// Locally spooled handoff record when the ai-memory server is offline or errors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpooledRecord {
    pub session_id: String,
    pub from_harness: String,
    pub to_harness: String,
    pub brief_path: String,
    pub prompt_path: String,
    pub recorded_at: String,
}

/// Records handoff metadata and returns whether it reached ai-memory or was spooled locally (§3.5).
pub async fn record_handoff_destination(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
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
        &server_url,
        auth_token.as_deref(),
        &data_dir,
    )
    .await
}

/// Convenience alias returning true if delivered to ai-memory, false if spooled locally.
pub async fn record_handoff_delivered(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
) -> Result<bool, MemoryError> {
    let dest = record_handoff_destination(session_id, from, to, brief).await?;
    Ok(dest.reached_ai_memory())
}

/// Internal implementation taking explicit parameters for testing.
pub async fn record_handoff_to(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
) -> Result<HandoffDestination, MemoryError> {
    let record = SpooledRecord {
        session_id: session_id.as_str().to_string(),
        from_harness: harness_label(from).to_string(),
        to_harness: harness_label(to).to_string(),
        brief_path: brief.brief_path.display().to_string(),
        prompt_path: brief.prompt_path.display().to_string(),
        recorded_at: time_now_rfc3339(),
    };

    let spool_path = data_dir.join("handoffs.jsonl");

    // Attempt direct delivery to the ai-memory server via MCP POST /mcp
    match deliver_record(server_url, auth_token, &record).await {
        Ok(()) => {
            // Success: drain any previously spooled records
            let _ = drain_spool(server_url, auth_token, &spool_path, MAX_DRAIN_PER_CALL).await;
            Ok(HandoffDestination::AiMemory)
        }
        Err(_) => {
            // Unreachable or non-2xx: append to local spool
            append_spool(&spool_path, &record)?;
            Ok(HandoffDestination::SpooledLocally)
        }
    }
}

async fn deliver_record(
    server_url: &str,
    auth_token: Option<&str>,
    record: &SpooledRecord,
) -> Result<(), ()> {
    let brief_path = Path::new(&record.brief_path);
    let (cwd, project) = resolve_cwd_and_project(brief_path);
    let summary = extract_summary_from_brief(brief_path).unwrap_or_else(|| {
        format!(
            "Handoff from {} to {} for session {}",
            record.from_harness, record.to_harness, record.session_id
        )
    });

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_handoff_begin",
            "arguments": {
                "workspace": "default",
                "project": project,
                "summary": summary,
                "open_questions": Vec::<String>::new(),
                "next_steps": Vec::<String>::new(),
                "files_touched": Vec::<String>::new(),
                "cwd": cwd.display().to_string(),
                "shared": true
            }
        }
    });

    let body = serde_json::to_vec(&payload).map_err(|_| ())?;
    let (status, resp_bytes) = post_mcp(server_url, auth_token, &body)
        .await
        .map_err(|_| ())?;

    if (200..300).contains(&status) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&resp_bytes) {
            if v.get("error").is_some() {
                return Err(());
            }
        }
        Ok(())
    } else {
        Err(())
    }
}

/// Drains oldest spooled records first, bounded by `limit`.
pub async fn drain_spool(
    server_url: &str,
    auth_token: Option<&str>,
    spool_path: &Path,
    limit: usize,
) -> Result<usize, MemoryError> {
    if !spool_path.exists() {
        return Ok(0);
    }
    let content = std::fs::read_to_string(spool_path)?;
    let mut records: Vec<SpooledRecord> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<SpooledRecord>(trimmed) {
            records.push(rec);
        }
    }

    if records.is_empty() {
        return Ok(0);
    }

    let to_drain = limit.min(records.len());
    let mut drained_count = 0;

    for rec in records.iter().take(to_drain) {
        if deliver_record(server_url, auth_token, rec).await.is_ok() {
            drained_count += 1;
        } else {
            // Stop on first failure
            break;
        }
    }

    if drained_count > 0 {
        let remaining = &records[drained_count..];
        if remaining.is_empty() {
            let _ = std::fs::remove_file(spool_path);
        } else {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(spool_path)?;
            for rec in remaining {
                let line = serde_json::to_string(rec)?;
                writeln!(file, "{line}")?;
            }
        }
    }

    Ok(drained_count)
}

fn append_spool(spool_path: &Path, record: &SpooledRecord) -> Result<(), MemoryError> {
    if let Some(parent) = spool_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(record)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(spool_path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn resolve_cwd_and_project(brief_path: &Path) -> (PathBuf, String) {
    let mut current = brief_path.parent();
    let mut worktree = None;

    while let Some(dir) = current {
        if dir.file_name().and_then(|n| n.to_str()) == Some(".aihub") {
            worktree = dir.parent().map(Path::to_path_buf);
            break;
        }
        current = dir.parent();
    }

    let cwd = worktree
        .or_else(|| brief_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));

    let project = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default")
        .to_string();

    (cwd, project)
}

fn extract_summary_from_brief(brief_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(brief_path).ok()?;
    if let Some(pos) = content.find("## Goal\n") {
        let after = &content[pos + 8..];
        let end = after.find("\n## ").unwrap_or(after.len());
        let goal = after[..end].trim();
        if !goal.is_empty() {
            return Some(goal.to_string());
        }
    }
    None
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

async fn post_mcp(
    url: &str,
    auth_token: Option<&str>,
    body: &[u8],
) -> Result<(u16, Vec<u8>), std::io::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (host, port) = parse_url(url)?;
    let mut stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;

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

    stream.write_all(req.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    let mut resp_bytes = Vec::new();
    stream.read_to_end(&mut resp_bytes).await?;

    let status_code = parse_http_status(&resp_bytes).unwrap_or(500);
    let body_bytes = parse_http_body(&resp_bytes);

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
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]).to_string();
                req_clone.lock().await.push(req_str);

                let resp = format!(
                    "HTTP/1.1 {} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_code,
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
            3,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-adapter-success-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let worktree = dir.join("my-cool-project");
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nShip ai-memory adapter\n").unwrap();
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
            &server_url,
            Some("fake-token"),
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::AiMemory);
        assert!(dest.reached_ai_memory());
        assert!(!dest.was_spooled_locally());

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains("POST /mcp HTTP/1.1"));
        assert!(recorded[0].contains("Authorization: Bearer fake-token"));
        assert!(recorded[0].contains("\"memory_handoff_begin\""));
        assert!(recorded[0].contains("\"project\":\"my-cool-project\""));
        assert!(recorded[0].contains("\"workspace\":\"default\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_401_unauthorized_spools_locally() {
        let (port, _) = start_fake_server(401, "", 1).await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-adapter-401-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let worktree = dir.join("proj401");
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
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
            &server_url,
            Some("bad-token"),
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::SpooledLocally);
        assert!(dest.was_spooled_locally());

        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-401"));
        assert!(content.contains("claude-code"));
        assert!(content.contains("codex"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_unreachable_spools_locally() {
        // Connect to a port where nothing is listening
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-adapter-unreachable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let worktree = dir.join("projunreach");
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
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
            server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::SpooledLocally);

        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-unreach"));
        assert!(content.contains("cursor-agent"));
        assert!(content.contains("antigravity"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_drain_on_later_success() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-adapter-drain-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let worktree = dir.join("projdrain");
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
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

        // 1. Spool the first handoff while server is unreachable
        let dest1 = record_handoff_to(
            &sid1,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &BriefPair {
                brief_path: brief1,
                prompt_path: prompt1,
            },
            "http://127.0.0.1:49999",
            None,
            &dir,
        )
        .await
        .unwrap();
        assert_eq!(dest1, HandoffDestination::SpooledLocally);

        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let spool_content = std::fs::read_to_string(&spool_file).unwrap();
        assert!(spool_content.contains("sess-spooled-1"));

        // 2. Start fake server
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok"}}"#,
            3,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");

        // 3. Record second handoff successfully -> should drain the spool!
        let dest2 = record_handoff_to(
            &sid2,
            HarnessId::Codex,
            HarnessId::Antigravity,
            &BriefPair {
                brief_path: brief2,
                prompt_path: prompt2,
            },
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest2, HandoffDestination::AiMemory);

        // Server should have received 2 requests: the live one (sess-live-2) and the drained one (sess-spooled-1)
        let mut len = 0usize;
        for _ in 0..50 {
            len = reqs.lock().await.len();
            if len >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(len, 2);
        let recorded = reqs.lock().await;
        let has_live = recorded.iter().any(|r| r.contains("Second live handoff"));
        let has_drained = recorded.iter().any(|r| r.contains("First spooled handoff"));
        assert!(has_live, "server must receive live handoff");
        assert!(has_drained, "server must receive drained handoff");

        // Spool file should be drained (removed or empty)
        if spool_file.exists() {
            let rem = std::fs::read_to_string(&spool_file).unwrap();
            assert!(
                rem.trim().is_empty(),
                "spool must be empty after drain: {rem}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
