pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod transcripts;
pub mod windows;

use aihub_core::QuotaSnapshot;
use thiserror::Error;

/// Error type encompassing all probe failures.
#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("SQLite database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Probe failed: {0}")]
    Failure(String),
}

/// Runs all provider probes concurrently and aggregates available snapshots.
///
/// Owned and implemented by Session 01, ensuring neither Session 02 nor 03
/// modifies `lib.rs` during parallel work.
pub async fn probe_all() -> Vec<QuotaSnapshot> {
    if std::env::var("AIHUB_NO_PROBES").is_ok() {
        return Vec::new();
    }
    let (claude_res, codex_res, cursor_res, agy_res, transcripts_res) = tokio::join!(
        claude::probe(),
        codex::probe(),
        cursor::probe(),
        antigravity::probe(),
        transcripts::probe(),
    );

    let mut out = Vec::new();
    if let Ok(snap) = claude_res {
        out.push(snap);
    }
    if let Ok(snap) = codex_res {
        out.push(snap);
    }
    if let Ok(snap) = cursor_res {
        out.push(snap);
    }
    if let Ok(snap) = agy_res {
        out.push(snap);
    }
    if let Ok(snaps) = transcripts_res {
        out.extend(snaps);
    }
    out
}
