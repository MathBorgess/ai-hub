pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod transcripts;
pub mod windows;

use std::collections::HashMap;
use std::time::Duration;

use aihub_core::{HarnessId, QuotaSnapshot, QuotaStatus};
use thiserror::Error;

/// Shared HTTP timeout for vendor quota endpoints.
pub const PROBE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Builds the shared HTTP client used by vendor probes.
pub fn build_probe_http_client() -> Result<reqwest::Client, ProbeError> {
    reqwest::Client::builder()
        .timeout(PROBE_HTTP_TIMEOUT)
        .build()
        .map_err(ProbeError::Http)
}

/// Turns an HTTP failure into an `Unknown` snapshot note (timeouts never hang the caller).
pub fn http_failure_note(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "HTTP request timed out".to_string()
    } else {
        format!("probe failed: {err}")
    }
}

/// `true` when a vendor probe returned real quota data (not a transcript estimate).
pub fn vendor_has_usable_reading(snap: &QuotaSnapshot) -> bool {
    if snap.estimated {
        return false;
    }
    if !snap.windows.is_empty() || !snap.lanes.is_empty() {
        return true;
    }
    matches!(
        snap.status,
        QuotaStatus::Ok | QuotaStatus::Low | QuotaStatus::Empty
    )
}

/// `true` when transcript fallback must not override or supplement this vendor snapshot.
pub fn slot_blocks_transcript_fallback(snap: &QuotaSnapshot) -> bool {
    if vendor_has_usable_reading(snap) {
        return true;
    }
    snap.note.as_deref().is_some_and(|n| n.contains("expired"))
}

/// Merges vendor probe results with transcript estimates (fallback-only, one row per harness).
pub fn merge_vendor_and_transcript_snapshots(
    mut vendor: Vec<QuotaSnapshot>,
    transcripts: Vec<QuotaSnapshot>,
) -> Vec<QuotaSnapshot> {
    let mut index_by_harness: HashMap<HarnessId, usize> = HashMap::new();
    for (idx, snap) in vendor.iter().enumerate() {
        index_by_harness.insert(snap.slot.harness, idx);
    }

    for tsnap in transcripts {
        let harness = tsnap.slot.harness;
        if let Some(&idx) = index_by_harness.get(&harness) {
            let existing = &vendor[idx];
            if slot_blocks_transcript_fallback(existing) {
                continue;
            }
            vendor[idx] = tsnap;
        } else {
            index_by_harness.insert(harness, vendor.len());
            vendor.push(tsnap);
        }
    }

    vendor
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

    let mut vendor = Vec::new();
    if let Ok(snap) = claude_res {
        vendor.push(snap);
    }
    if let Ok(snap) = codex_res {
        vendor.push(snap);
    }
    if let Ok(snap) = cursor_res {
        vendor.push(snap);
    }
    if let Ok(snap) = agy_res {
        vendor.push(snap);
    }

    let transcripts = transcripts_res.ok().unwrap_or_default();
    merge_vendor_and_transcript_snapshots(vendor, transcripts)
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use aihub_core::{HarnessId, QuotaSource, QuotaWindow, SlotId, WindowKind};

    fn claude_vendor_ok() -> QuotaSnapshot {
        QuotaSnapshot {
            slot: SlotId::default_for(HarnessId::ClaudeCode),
            status: QuotaStatus::Ok,
            source: QuotaSource::OAuth,
            estimated: false,
            note: None,
            windows: vec![QuotaWindow::new(
                WindowKind::FiveHour,
                10.0,
                None,
                Some(18000),
            )],
            lanes: vec![],
        }
    }

    fn claude_transcript_estimate() -> QuotaSnapshot {
        QuotaSnapshot {
            slot: SlotId::default_for(HarnessId::ClaudeCode),
            status: QuotaStatus::Ok,
            source: QuotaSource::Transcript,
            estimated: true,
            note: Some("estimated from 3 turns in transcripts".into()),
            windows: vec![QuotaWindow::new(
                WindowKind::Custom("five_hour~".into()),
                50.0,
                None,
                Some(18000),
            )],
            lanes: vec![],
        }
    }

    #[test]
    fn probe_all_transcript_only_when_no_vendor_reading() {
        let vendor = vec![claude_vendor_ok()];
        let transcripts = vec![claude_transcript_estimate()];
        let merged = merge_vendor_and_transcript_snapshots(vendor, transcripts);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].estimated);
        assert_eq!(merged[0].source, QuotaSource::OAuth);
    }

    #[test]
    fn probe_all_transcript_fills_missing_vendor_reading() {
        let vendor = vec![QuotaSnapshot {
            slot: SlotId::default_for(HarnessId::ClaudeCode),
            status: QuotaStatus::Unknown,
            source: QuotaSource::OAuth,
            estimated: false,
            note: Some("no credential found — run `claude` once to log in".into()),
            windows: vec![],
            lanes: vec![],
        }];
        let transcripts = vec![claude_transcript_estimate()];
        let merged = merge_vendor_and_transcript_snapshots(vendor, transcripts);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].estimated);
    }

    #[test]
    fn probe_all_expired_credentials_never_ok_from_transcripts() {
        let vendor = vec![QuotaSnapshot {
            slot: SlotId::default_for(HarnessId::ClaudeCode),
            status: QuotaStatus::Unknown,
            source: QuotaSource::OAuth,
            estimated: false,
            note: Some("every credential found is expired — run `claude` once to refresh".into()),
            windows: vec![],
            lanes: vec![],
        }];
        let transcripts = vec![claude_transcript_estimate()];
        let merged = merge_vendor_and_transcript_snapshots(vendor, transcripts);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].status, QuotaStatus::Unknown);
        assert!(!merged[0].estimated);
        assert!(merged[0]
            .note
            .as_deref()
            .is_some_and(|n| n.contains("expired")));
    }
}
