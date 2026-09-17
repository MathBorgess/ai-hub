use crate::ProbeError;
use aihub_core::{
    HarnessId, QuotaSnapshot, QuotaSource, QuotaStatus, QuotaWindow, SlotId, WindowKind,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Auth data format in `~/.codex/auth.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexAuth {
    pub tokens: Option<CodexTokens>,
    /// Some formats may store access_token directly
    pub access_token: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexTokens {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
    /// ISO 8601 date string e.g. "2026-09-15T16:00:00.000Z"
    pub expires_at: Option<String>,
}

/// JSON payload from OpenAI WHAM usage endpoint: `https://chatgpt.com/backend-api/wham/usage`
#[derive(Debug, Clone, Deserialize)]
pub struct CodexUsageResponse {
    pub rate_limit: Option<CodexRateLimit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexRateLimit {
    pub primary_window: Option<CodexRateLimitWindow>,
    pub secondary_window: Option<CodexRateLimitWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexRateLimitWindow {
    /// Percentage used (0.0 to 100.0)
    pub used_percent: Option<f64>,
    /// Window duration in seconds (e.g. 18000 or 604800)
    pub limit_window_seconds: Option<u64>,
    /// Epoch timestamp in seconds when the window resets
    pub reset_at: Option<u64>,
}

/// Helper: candidate config directories for Codex.
/// Respects `CODEX_HOME` env var (comma-separated or single directory),
/// defaulting to `~/.codex`.
pub fn codex_homes() -> Vec<PathBuf> {
    if let Ok(val) = std::env::var("CODEX_HOME") {
        let dirs: Vec<PathBuf> = val
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if !dirs.is_empty() {
            return dirs;
        }
    }

    let mut out = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        out.push(Path::new(&home).join(".codex"));
    }
    out
}

/// Pure helper: checks if Codex auth tokens are expired.
pub fn is_codex_expired(auth: &CodexAuth, now_ms: i64) -> bool {
    let exp_str = match auth.tokens.as_ref().and_then(|t| t.expires_at.as_deref()) {
        Some(s) => s,
        None => return false,
    };
    if let Some(exp_ms) = parse_iso_to_epoch_ms(exp_str) {
        exp_ms < now_ms
    } else {
        false
    }
}

/// Pure helper: extracts access token and optional account id if valid.
pub fn valid_codex_tokens(
    auth: &CodexAuth,
    now_ms: i64,
) -> Result<(String, Option<String>), &'static str> {
    if is_codex_expired(auth, now_ms) {
        return Err("credential expired");
    }
    let token = auth
        .tokens
        .as_ref()
        .and_then(|t| t.access_token.clone())
        .or_else(|| auth.access_token.clone());

    let token = match token {
        Some(t) if !t.trim().is_empty() => t,
        _ => return Err("no access token in auth"),
    };

    let account_id = auth
        .tokens
        .as_ref()
        .and_then(|t| t.account_id.clone())
        .or_else(|| auth.account_id.clone());

    Ok((token, account_id))
}

/// Pure parser: parses Codex WHAM usage JSON response into quota windows.
///
/// Ported from handoff.mjs:
/// - Windows: primary_window and secondary_window from `rate_limit`
/// - `used_percent` maps directly to `used_pct`
/// - If `limit_window_seconds == 604800`, kind is SevenDay ("weekly"), otherwise FiveHour ("session")
pub fn parse_codex_usage(json: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
    let resp: CodexUsageResponse = serde_json::from_str(json)?;
    let rl = match resp.rate_limit {
        Some(rl) => rl,
        None => return Err(ProbeError::Failure("no rate_limit in response".into())),
    };

    let mut windows = Vec::new();
    let candidates = [rl.primary_window, rl.secondary_window];

    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for opt_w in candidates.into_iter().flatten() {
        if let Some(used_pct) = opt_w.used_percent {
            let win_s = opt_w.limit_window_seconds;
            let kind = match win_s {
                Some(604800) => WindowKind::SevenDay,
                _ => WindowKind::FiveHour,
            };

            let resets_in_s = opt_w.reset_at.map(|reset| reset.saturating_sub(now_s));

            windows.push(QuotaWindow::new(kind, used_pct, resets_in_s, win_s));
        }
    }

    if windows.is_empty() {
        return Err(ProbeError::Failure(
            "no rate_limit windows in response".into(),
        ));
    }

    Ok(windows)
}

/// Pure parser: reads candidate `auth.json` files and returns the first usable one.
pub fn read_codex_auth_first_usable(now_ms: i64) -> (Option<CodexAuth>, Option<String>, bool) {
    let mut expired_seen = false;

    for dir in codex_homes() {
        let auth_path = dir.join("auth.json");
        if auth_path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&auth_path) {
                if let Ok(auth) = serde_json::from_str::<CodexAuth>(&content) {
                    if auth.tokens.is_some() || auth.access_token.is_some() {
                        if is_codex_expired(&auth, now_ms) {
                            expired_seen = true;
                            continue;
                        }
                        return (
                            Some(auth),
                            Some(auth_path.to_string_lossy().to_string()),
                            expired_seen,
                        );
                    }
                }
            }
        }
    }

    (None, None, expired_seen)
}

/// Helper: converts ISO8601 string to epoch ms.
fn parse_iso_to_epoch_ms(s: &str) -> Option<i64> {
    let re = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})").ok()?;
    let caps = re.captures(s)?;
    let y: u64 = caps[1].parse().ok()?;
    let m: u64 = caps[2].parse().ok()?;
    let d: u64 = caps[3].parse().ok()?;
    let h: u64 = caps[4].parse().ok()?;
    let min: u64 = caps[5].parse().ok()?;
    let sec: u64 = caps[6].parse().ok()?;

    let days = days_since_epoch(y, m, d);
    let epoch_s = days * 86400 + h * 3600 + min * 60 + sec;
    Some((epoch_s * 1000) as i64)
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

/// Snapshot returned when no usable auth token is available.
pub fn missing_credentials_snapshot(expired_seen: bool) -> QuotaSnapshot {
    let slot = SlotId::default_for(HarnessId::Codex);
    let note = if expired_seen {
        "every credential found is expired — run `codex` once to refresh"
    } else {
        "no credential found — run `codex` once to log in"
    };

    QuotaSnapshot {
        slot,
        status: QuotaStatus::Unknown,
        source: QuotaSource::OAuth,
        estimated: false,
        note: Some(note.to_string()),
        windows: vec![],
        lanes: vec![],
    }
}

/// Probes OpenAI Codex usage from local auth tokens and usage endpoint.
/// Owned by session 02.
pub async fn probe() -> Result<QuotaSnapshot, ProbeError> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let (auth, source_label, expired_seen) =
        tokio::task::spawn_blocking(move || read_codex_auth_first_usable(now_ms))
            .await
            .map_err(|e| ProbeError::Failure(format!("auth read task failed: {e}")))?;

    let slot = SlotId::default_for(HarnessId::Codex);

    let Some(auth) = auth else {
        return Ok(missing_credentials_snapshot(expired_seen));
    };

    let (token, account_id) = match valid_codex_tokens(&auth, now_ms) {
        Ok(res) => res,
        Err(_) => {
            return Ok(QuotaSnapshot {
                slot,
                status: QuotaStatus::Unknown,
                source: QuotaSource::OAuth,
                estimated: false,
                note: Some("credential expired — run `codex` once to refresh".to_string()),
                windows: vec![],
                lanes: vec![],
            });
        }
    };

    let client = crate::build_probe_http_client()?;

    let mut req = client
        .get("https://chatgpt.com/backend-api/wham/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", "codex-cli");

    if let Some(acc) = account_id {
        req = req.header("ChatGPT-Account-Id", acc);
    }

    let res = req.send().await;

    match res {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await?;
            let windows = parse_codex_usage(&body)?;
            let tightest = crate::windows::pick_tightest_window(&windows);
            let status = match tightest {
                Some(w) => crate::windows::bucket_for_usage(w.used_pct, 20.0),
                None => QuotaStatus::Unknown,
            };

            Ok(QuotaSnapshot {
                slot,
                status,
                source: QuotaSource::OAuth,
                estimated: false,
                note: source_label.map(|l| format!("via {l}")),
                windows,
                lanes: vec![],
            })
        }
        Ok(resp) => {
            let status_code = resp.status();
            Ok(QuotaSnapshot {
                slot,
                status: QuotaStatus::Unknown,
                source: QuotaSource::OAuth,
                estimated: false,
                note: Some(format!("HTTP {status_code}")),
                windows: vec![],
                lanes: vec![],
            })
        }
        Err(e) => Ok(QuotaSnapshot {
            slot,
            status: QuotaStatus::Unknown,
            source: QuotaSource::OAuth,
            estimated: false,
            note: Some(crate::http_failure_note(&e)),
            windows: vec![],
            lanes: vec![],
        }),
    }
}
