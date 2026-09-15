//! Live ai-memory loop for session 10. Run only with a temp data dir and non-default port:
//! `AI_MEMORY_E2E=1 cargo test -p aihub-memory --test session10_ai_memory_loop -- --nocapture`

use aihub_core::{HarnessId, SessionId};
use aihub_memory::{record_handoff_destination, write_brief_pair, HandoffDestination, HandoffTurn};
use std::path::PathBuf;
use std::process::Command;

fn aim_binary() -> PathBuf {
    std::env::var("AI_MEMORY_BIN")
        .map(PathBuf::from)
        .expect("AI_MEMORY_BIN must point at the v2.2.2 ai-memory binary")
}

#[tokio::test]
async fn session10_ai_memory_live_record_spool_and_drain() {
    if std::env::var("AI_MEMORY_E2E").as_deref() != Ok("1") {
        return;
    }

    let data_dir = PathBuf::from(std::env::var("AI_MEMORY_DATA_DIR").expect("AI_MEMORY_DATA_DIR"));
    let aihub_data = PathBuf::from(std::env::var("AIHUB_DATA_DIR").expect("AIHUB_DATA_DIR"));
    let port = std::env::var("AI_MEMORY_PORT").expect("AI_MEMORY_PORT");
    let server_url = format!("http://127.0.0.1:{port}");
    std::env::set_var("AI_MEMORY_SERVER_URL", &server_url);
    std::env::set_var("AIHUB_DATA_DIR", aihub_data.as_os_str());

    let worktree = data_dir.join("proj-live");
    let handoffs = worktree.join(".aihub").join("handoffs");
    std::fs::create_dir_all(&handoffs).unwrap();
    let turn = HandoffTurn {
        summary: "live handoff".into(),
        last_output: "ok".into(),
        decisions: vec!["ship".into()],
    };
    let brief = write_brief_pair(&handoffs, 1, "session10 live", &turn).unwrap();
    let session_id = SessionId::new("session10-live");

    let dest = record_handoff_destination(
        &session_id,
        HarnessId::ClaudeCode,
        HarnessId::Codex,
        &brief,
    )
    .await
    .expect("record while server up");
    assert_eq!(dest, HandoffDestination::AiMemory);

    let listed = Command::new(aim_binary())
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "handoffs",
            "--project",
            "proj-live",
        ])
        .output()
        .expect("handoffs cli");
    assert!(listed.status.success(), "handoffs: {}", String::from_utf8_lossy(&listed.stderr));
    let out = String::from_utf8_lossy(&listed.stdout);
    assert!(
        out.contains("Open handoffs") && out.contains("proj-live"),
        "handoffs output: {out}"
    );

    // Stop server: record again should spool
    std::env::set_var("AI_MEMORY_SERVER_URL", "http://127.0.0.1:59999");
    let dest2 = record_handoff_destination(
        &SessionId::new("session10-spooled"),
        HarnessId::CursorAgent,
        HarnessId::Antigravity,
        &brief,
    )
    .await
    .expect("record while server down");
    assert_eq!(dest2, HandoffDestination::SpooledLocally);
    let spool = aihub_data.join("handoffs.jsonl");
    assert!(spool.exists());
    let spool_body = std::fs::read_to_string(&spool).unwrap();
    assert!(spool_body.contains("session10-spooled"));

    // Restart URL and record once more to drain spool
    std::env::set_var("AI_MEMORY_SERVER_URL", &server_url);
    let dest3 = record_handoff_destination(
        &SessionId::new("session10-drain-trigger"),
        HarnessId::Codex,
        HarnessId::ClaudeCode,
        &brief,
    )
    .await
    .expect("drain trigger");
    assert_eq!(dest3, HandoffDestination::AiMemory);
    if spool.exists() {
        assert!(
            std::fs::read_to_string(&spool).unwrap().trim().is_empty(),
            "spool should be empty after drain"
        );
    }
}
