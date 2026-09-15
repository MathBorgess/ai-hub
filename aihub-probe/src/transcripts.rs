use crate::ProbeError;
use aihub_core::{
    HarnessId, QuotaSnapshot, QuotaSource, QuotaStatus, QuotaWindow, SlotId, WindowKind,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const HOUR_MS: u64 = 3600 * 1000;
const BLOCK_MS: u64 = 5 * HOUR_MS;
const DEFAULT_DAYS: u64 = 7;
const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Recorded usage turn from a local session transcript file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptTurn {
    pub timestamp_s: u64,
    pub tokens_used: u64,
    pub session_id: String,
}

/// A 5-hour aggregated window block.
#[derive(Debug, Clone)]
pub struct Block {
    pub start_ms: u64,
    pub end_ms: u64,
    pub last_ts_ms: u64,
    pub tokens: u64,
    pub turns: usize,
}

/// Helper to format tokens like "1.2M tok", "45k tok", "500 tok".
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M tok", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{}k tok", (n as f64 / 1000.0).round() as u64)
    } else {
        format!("{n} tok")
    }
}

/// Pure calculation: builds 5-hour blocks from turns according to ccusage rules.
///
/// ccusage rule:
/// - A window opens at the containing hour of its first turn: `floorHour(turn.ts)`.
/// - A window closes when a turn arrives more than 5 hours after that opening OR
///   more than 5 hours after the previous turn.
pub fn build_blocks_ms(turns: &[TranscriptTurn], block_ms: u64) -> Vec<Block> {
    if turns.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&TranscriptTurn> = turns.iter().collect();
    sorted.sort_by_key(|t| t.timestamp_s);

    let floor_hour = |ms: u64| (ms / HOUR_MS) * HOUR_MS;

    let mut blocks = Vec::new();
    let mut start: Option<u64> = None;
    let mut cur: Vec<&TranscriptTurn> = Vec::new();

    for t in sorted {
        let t_ms = t.timestamp_s.saturating_mul(1000);
        match start {
            None => {
                start = Some(floor_hour(t_ms));
                cur.push(t);
            }
            Some(s) => {
                let last = cur.last().map(|prev| prev.timestamp_s * 1000).unwrap_or(s);
                if t_ms.saturating_sub(s) > block_ms || t_ms.saturating_sub(last) > block_ms {
                    // Close block
                    blocks.push(Block {
                        start_ms: s,
                        end_ms: s + block_ms,
                        last_ts_ms: last,
                        tokens: cur.iter().map(|item| item.tokens_used).sum(),
                        turns: cur.len(),
                    });
                    cur.clear();
                    start = Some(floor_hour(t_ms));
                }
                cur.push(t);
            }
        }
    }

    if let Some(s) = start {
        if !cur.is_empty() {
            let last = cur.last().map(|prev| prev.timestamp_s * 1000).unwrap_or(s);
            blocks.push(Block {
                start_ms: s,
                end_ms: s + block_ms,
                last_ts_ms: last,
                tokens: cur.iter().map(|item| item.tokens_used).sum(),
                turns: cur.len(),
            });
        }
    }

    blocks
}

/// Pure calculation: computes 5-hour rolling windows from historical transcript turns.
///
/// Compares the active 5-hour window against the heaviest completed 5-hour window.
/// Window is labeled with custom kind "five_hour~".
pub fn calculate_rolling_windows(turns: &[TranscriptTurn]) -> Vec<QuotaWindow> {
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let now_ms = now_s * 1000;

    let blocks = build_blocks_ms(turns, BLOCK_MS);
    let active = blocks
        .iter()
        .find(|b| now_ms < b.end_ms && now_ms.saturating_sub(b.last_ts_ms) <= BLOCK_MS);
    let completed: Vec<&Block> = blocks
        .iter()
        .filter(|b| match active {
            Some(a) => b.start_ms != a.start_ms,
            None => true,
        })
        .collect();

    let denominator: u64 = completed.iter().map(|b| b.tokens).max().unwrap_or(0);

    let (used_pct, resets_in_s) = match active {
        None => {
            // Idle window -> 100% remaining -> 0% used
            (0.0, None)
        }
        Some(act) => {
            let resets_in = act.end_ms.saturating_sub(now_ms) / 1000;
            if denominator == 0 {
                // If no completed window, fallback: 0% used or 100% used if tokens exist?
                // In local-usage.mjs: remaining_pct is null, but QuotaWindow requires f64.
                // We use 0.0 with resets_in_s.
                (0.0, Some(resets_in))
            } else {
                let ratio = (act.tokens as f64 / denominator as f64) * 100.0;
                (ratio.clamp(0.0, 100.0), Some(resets_in))
            }
        }
    };

    vec![QuotaWindow::new(
        WindowKind::Custom("five_hour~".to_string()),
        used_pct,
        resets_in_s,
        Some(18000),
    )]
}

/// Pure parser: parses a single JSONL transcript line into a `TranscriptTurn`.
///
/// Handles both:
/// 1. Claude Code format:
///    JSON object with optional `sessionId`, `message.id`, and `message.usage`:
///    `input_tokens + output_tokens + cache_creation + cache_read_input_tokens`.
///    Reads timestamp from `timestamp` field.
/// 2. Codex format:
///    JSON object with `type: "event_msg"`, `payload.type: "token_count"`,
///    and `payload.info.last_token_usage` or `total_token_usage`.
///
/// NOTE: Reads only token counts and timestamps, never message text or user content!
pub fn parse_transcript_line(line: &str) -> Result<Option<TranscriptTurn>, ProbeError> {
    let trimmed = line.trim();
    if trimmed.len() < 2 {
        return Ok(None);
    }

    // Pre-filtering for speed
    let is_claude = trimmed.contains("\"usage\"");
    let is_codex = trimmed.contains("token_count");
    if !is_claude && !is_codex {
        return Ok(None);
    }

    let val: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    // Try Claude
    if is_claude {
        if let Some(msg) = val.get("message") {
            if let Some(usage) = msg.get("usage") {
                if let Some(inp) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                    let out = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_create = if let Some(cc) = usage.get("cache_creation") {
                        cc.get("ephemeral_5m_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                            + cc.get("ephemeral_1h_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0)
                    } else {
                        usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                    };
                    let total_tokens = inp + out + cache_read + cache_create;

                    let timestamp_s = val
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .and_then(parse_iso_to_epoch_s)
                        .unwrap_or(0);

                    let session_id = val
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    return Ok(Some(TranscriptTurn {
                        timestamp_s,
                        tokens_used: total_tokens,
                        session_id,
                    }));
                }
            }
        }
    }

    // Try Codex
    if is_codex && val.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
        if let Some(payload) = val.get("payload") {
            if payload.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                let info = payload.get("info");
                let tokens = if let Some(last) = info.and_then(|i| i.get("last_token_usage")) {
                    last.get("total_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_else(|| {
                            let inp = last
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let cached = last
                                .get("cached_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let out = last
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            inp + cached + out
                        })
                } else if let Some(total) = info.and_then(|i| i.get("total_token_usage")) {
                    total
                        .get("total_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_else(|| {
                            let inp = total
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let cached = total
                                .get("cached_input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let out = total
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            inp + cached + out
                        })
                } else {
                    0
                };

                let timestamp_s = val
                    .get("timestamp")
                    .or_else(|| payload.get("timestamp"))
                    .and_then(|v| v.as_str())
                    .and_then(parse_iso_to_epoch_s)
                    .unwrap_or(0);

                let session_id = val
                    .get("session_id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                return Ok(Some(TranscriptTurn {
                    timestamp_s,
                    tokens_used: tokens,
                    session_id,
                }));
            }
        }
    }

    Ok(None)
}

fn parse_iso_to_epoch_s(s: &str) -> Option<u64> {
    let re = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})").ok()?;
    let caps = re.captures(s)?;
    let y: u64 = caps[1].parse().ok()?;
    let m: u64 = caps[2].parse().ok()?;
    let d: u64 = caps[3].parse().ok()?;
    let h: u64 = caps[4].parse().ok()?;
    let min: u64 = caps[5].parse().ok()?;
    let sec: u64 = caps[6].parse().ok()?;

    let days = days_since_epoch(y, m, d);
    Some(days * 86400 + h * 3600 + min * 60 + sec)
}

fn days_since_epoch(y: u64, m: u64, d: u64) -> u64 {
    let mut days = 0;
    for year in 1970..y {
        days += if is_leap_year(year) { 366 } else { 365 };
    }
    let mdays = if is_leap_year(y) {
        [0, 31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    for month in 1..m {
        days += mdays[month as usize];
    }
    days + (d - 1)
}

fn is_leap_year(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

/// Recursively discovers `.jsonl` files under `root` up to `depth` 8.
fn walk_jsonl(root: &Path, depth: usize, out: &mut Vec<(PathBuf, u64, u64)>) {
    if depth > 8 || !root.exists() {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_symlink() {
            continue;
        }
        if p.is_dir() {
            walk_jsonl(&p, depth + 1, out);
        } else if p.is_file() && p.extension().is_some_and(|ext| ext == "jsonl") {
            if let Ok(meta) = entry.metadata() {
                let mtime_s = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                out.push((p, mtime_s, meta.len()));
            }
        }
    }
}

/// Helper: estimates usage for a given provider from disk transcripts.
pub fn estimate_provider_usage(harness: HarnessId, roots: &[PathBuf]) -> Option<QuotaSnapshot> {
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let cutoff_s = now_s.saturating_sub(DEFAULT_DAYS * 86400);

    let mut files = Vec::new();
    for r in roots {
        walk_jsonl(r, 0, &mut files);
    }
    files.sort_by_key(|a| std::cmp::Reverse(a.1)); // Newest first

    let mut picked = Vec::new();
    let mut total_bytes = 0;
    for (p, mtime, size) in files {
        if mtime < cutoff_s {
            break;
        }
        if total_bytes + size > DEFAULT_MAX_BYTES {
            break;
        }
        total_bytes += size;
        picked.push(p);
    }

    if picked.is_empty() {
        return None;
    }

    let mut turns = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for p in picked {
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for line in text.lines() {
            if let Ok(Some(turn)) = parse_transcript_line(line) {
                // Deduplicate if session_id and turn are repetitive
                let key = format!("{}:{}", turn.session_id, turn.timestamp_s);
                if seen_ids.insert(key) {
                    turns.push(turn);
                }
            }
        }
    }

    if turns.is_empty() {
        return None;
    }

    let windows = calculate_rolling_windows(&turns);
    let tightest = crate::windows::pick_tightest_window(&windows);
    let status = match tightest {
        Some(w) => crate::windows::bucket_for_usage(w.used_pct, 20.0),
        None => QuotaStatus::Unknown,
    };

    let slot = SlotId::default_for(harness);

    Some(QuotaSnapshot {
        slot,
        status,
        source: QuotaSource::Transcript,
        estimated: true,
        note: Some(format!(
            "estimated from {} turns in transcripts",
            turns.len()
        )),
        windows,
        lanes: vec![],
    })
}

fn probe_transcripts_blocking() -> Vec<QuotaSnapshot> {
    let mut out = Vec::new();

    let claude_roots: Vec<PathBuf> = crate::claude::claude_config_dirs()
        .into_iter()
        .map(|d| d.join("projects"))
        .collect();
    if let Some(snap) = estimate_provider_usage(HarnessId::ClaudeCode, &claude_roots) {
        out.push(snap);
    }

    let codex_roots: Vec<PathBuf> = crate::codex::codex_homes()
        .into_iter()
        .flat_map(|d| vec![d.join("sessions"), d.join("archived_sessions")])
        .collect();
    if let Some(snap) = estimate_provider_usage(HarnessId::Codex, &codex_roots) {
        out.push(snap);
    }

    out
}

/// Probes local transcript JSONL logs to compute rolling usage estimates.
/// Owned by session 02.
pub async fn probe() -> Result<Vec<QuotaSnapshot>, ProbeError> {
    tokio::task::spawn_blocking(probe_transcripts_blocking)
        .await
        .map_err(|e| ProbeError::Failure(format!("transcript probe failed: {e}")))
}
