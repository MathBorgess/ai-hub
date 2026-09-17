//! Live ai-memory loop for session 10. Run only with a temp data dir and non-default port:
//! `AI_MEMORY_E2E=1 cargo test -p aihub-memory --test session10_ai_memory_loop -- --nocapture`

use aihub_core::{HarnessId, SessionId};
use aihub_memory::{record_handoff_to, write_brief_pair, HandoffDestination, HandoffTurn};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn aim_binary() -> PathBuf {
    std::env::var("AI_MEMORY_BIN")
        .map(PathBuf::from)
        .expect("AI_MEMORY_BIN must point at the v2.2.2 ai-memory binary")
}

async fn query_mcp_handoff_list(server_url: &str, project: &str) -> Vec<serde_json::Value> {
    let s = server_url.strip_prefix("http://").unwrap_or(server_url);
    let host_port = s.split('/').next().unwrap_or(s);
    let (host, port_str) = host_port.split_once(':').unwrap_or((host_port, "49374"));
    let port: u16 = port_str.parse().expect("valid port");

    let mut stream = TcpStream::connect((host, port))
        .await
        .expect("connect to ai-memory");

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "tools/call",
        "params": {
            "name": "memory_handoff_list",
            "arguments": {
                "workspace": "default",
                "project": project
            }
        }
    });
    let body = serde_json::to_vec(&payload).expect("serialize list payload");

    let req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Accept: application/json, text/event-stream\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    stream
        .write_all(req.as_bytes())
        .await
        .expect("write req headers");
    stream.write_all(&body).await.expect("write req body");
    stream.flush().await.expect("flush");

    let mut resp_bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.take(1024 * 1024).read_to_end(&mut resp_bytes),
    )
    .await
    .expect("read timeout")
    .expect("read response");

    let pos = resp_bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http header boundary");
    let resp_body = &resp_bytes[pos + 4..];

    let json_resp: serde_json::Value =
        serde_json::from_slice(resp_body).expect("parse json response");
    let content_text = json_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("mcp content text");
    let list_data: serde_json::Value =
        serde_json::from_str(content_text).expect("parse handoff list json");

    list_data["handoffs"]
        .as_array()
        .expect("handoffs array")
        .clone()
}

#[tokio::test]
async fn session10_ai_memory_live_record_spool_and_drain() {
    if std::env::var("AI_MEMORY_E2E").as_deref() != Ok("1") {
        return;
    }

    // Parameters are read once here and threaded explicitly (never mutating process env)
    let data_dir = PathBuf::from(std::env::var("AI_MEMORY_DATA_DIR").expect("AI_MEMORY_DATA_DIR"));
    let aihub_data = PathBuf::from(std::env::var("AIHUB_DATA_DIR").expect("AIHUB_DATA_DIR"));
    let port = std::env::var("AI_MEMORY_PORT").expect("AI_MEMORY_PORT");
    let server_url = format!("http://127.0.0.1:{port}");
    let project = "proj-live";

    let worktree = data_dir.join(project);
    let handoffs = worktree.join(".aihub").join("handoffs");
    std::fs::create_dir_all(&handoffs).unwrap();

    // 1. Live record while server is up
    let turn1 = HandoffTurn {
        summary: "live handoff turn".into(),
        last_output: "live output ok".into(),
        decisions: vec!["decision-live-ship".into()],
    };
    let brief1 = write_brief_pair(&handoffs, 1, "session10 live goal", &turn1).unwrap();
    let session_id1 = SessionId::new("session10-live");

    let dest1 = record_handoff_to(
        &session_id1,
        HarnessId::ClaudeCode,
        HarnessId::Codex,
        &brief1,
        project,
        &server_url,
        None,
        &aihub_data,
    )
    .await
    .expect("record while server up");
    assert_eq!(dest1, HandoffDestination::Delivered);

    // 2. Point at offline port: record again -> must spool
    let turn2 = HandoffTurn {
        summary: "spooled handoff turn".into(),
        last_output: "spooled output ok".into(),
        decisions: vec!["decision-spooled-offline".into()],
    };
    let brief2 = write_brief_pair(&handoffs, 2, "session10 spooled goal", &turn2).unwrap();
    let session_id2 = SessionId::new("session10-spooled");

    let offline_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let offline_port = offline_listener.local_addr().unwrap().port();
    drop(offline_listener);
    let offline_url = format!("http://127.0.0.1:{offline_port}");

    let dest2 = record_handoff_to(
        &session_id2,
        HarnessId::CursorAgent,
        HarnessId::Antigravity,
        &brief2,
        project,
        &offline_url,
        None,
        &aihub_data,
    )
    .await
    .expect("record while server down");
    assert_eq!(dest2, HandoffDestination::Spooled);

    let spool = aihub_data.join("handoffs.jsonl");
    assert!(spool.exists());
    let spool_body = std::fs::read_to_string(&spool).unwrap();
    assert!(spool_body.contains("session10-spooled"));

    // 3. Point back at live server: record once more to drain the spool
    let turn3 = HandoffTurn {
        summary: "drain trigger turn".into(),
        last_output: "drain trigger ok".into(),
        decisions: vec!["decision-drain-online".into()],
    };
    let brief3 = write_brief_pair(&handoffs, 3, "session10 drain goal", &turn3).unwrap();
    let session_id3 = SessionId::new("session10-drain-trigger");

    let dest3 = record_handoff_to(
        &session_id3,
        HarnessId::Codex,
        HarnessId::ClaudeCode,
        &brief3,
        project,
        &server_url,
        None,
        &aihub_data,
    )
    .await
    .expect("drain trigger");
    assert_eq!(dest3, HandoffDestination::Delivered);

    // Spool must be drained (empty or removed)
    if spool.exists() {
        assert!(
            std::fs::read_to_string(&spool).unwrap().trim().is_empty(),
            "spool should be empty after drain"
        );
    }

    // 4. Verify contents via ai-memory's CLI
    let listed = Command::new(aim_binary())
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "handoffs",
            "--project",
            project,
            "--json",
        ])
        .output()
        .expect("handoffs cli");
    assert!(
        listed.status.success(),
        "handoffs cli failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let cli_json: serde_json::Value =
        serde_json::from_slice(&listed.stdout).expect("parse cli json output");
    let cli_items = cli_json.as_array().expect("cli items array");
    assert_eq!(
        cli_items.len(),
        3,
        "CLI must show exactly 3 handoffs (live, drained, trigger)"
    );

    // 5. Verify exact contents (goals, decisions, project identity, no duplicates) via ai-memory API
    let api_handoffs = query_mcp_handoff_list(&server_url, project).await;
    assert_eq!(
        api_handoffs.len(),
        3,
        "API must show exactly 3 handoffs in project {project}"
    );

    let mut found_ids = HashSet::new();
    let mut found_goals = HashSet::new();
    let mut found_decisions = HashSet::new();

    for h in &api_handoffs {
        let id = h["id"].as_str().expect("handoff id").to_string();
        assert!(found_ids.insert(id.clone()), "duplicate handoff id: {id}");

        let summary = h["summary"].as_str().unwrap_or("");
        if summary.contains("session10 live goal") {
            found_goals.insert("session10 live goal");
        }
        if summary.contains("session10 spooled goal") {
            found_goals.insert("session10 spooled goal");
        }
        if summary.contains("session10 drain goal") {
            found_goals.insert("session10 drain goal");
        }

        if let Some(steps) = h["next_steps"].as_array() {
            for step in steps {
                if let Some(s) = step.as_str() {
                    if s.contains("decision-live-ship") {
                        found_decisions.insert("decision-live-ship");
                    }
                    if s.contains("decision-spooled-offline") {
                        found_decisions.insert("decision-spooled-offline");
                    }
                    if s.contains("decision-drain-online") {
                        found_decisions.insert("decision-drain-online");
                    }
                }
            }
        }
    }

    assert_eq!(
        found_goals.len(),
        3,
        "all 3 goals must land in ai-memory: found {found_goals:?}"
    );
    assert_eq!(
        found_decisions.len(),
        3,
        "all 3 decisions must land in ai-memory: found {found_decisions:?}"
    );
    assert_eq!(found_ids.len(), 3, "must have 3 unique handoff ids");
}
