use crate::ProbeError;
use aihub_core::{
    HarnessId, QuotaSnapshot, QuotaSource, QuotaStatus, QuotaWindow, SlotId, WindowKind,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Credentials stored in ~/.claude/.credentials.json or macOS Keychain service "Claude Code-credentials".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeOAuthCredentials {
    #[serde(rename = "claudeAiOauth")]
    pub claude_ai_oauth: Option<ClaudeAiOAuth>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeAiOAuth {
    #[serde(rename = "accessToken")]
    pub access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<i64>,
}

/// JSON payload from Anthropic API `https://api.anthropic.com/api/oauth/usage`
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicUsageResponse {
    pub five_hour: Option<AnthropicUsageWindow>,
    pub seven_day: Option<AnthropicUsageWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicUsageWindow {
    /// Percentage used (0.0 to 100.0)
    pub utilization: Option<f64>,
    /// ISO 8601 string or timestamp
    pub resets_at: Option<String>,
}

/// Helper: returns candidate config directories for Claude Code.
/// Respects `CLAUDE_CONFIG_DIR` (comma-separated or single path),
/// defaulting to `~/.config/claude` and `~/.claude`.
pub fn claude_config_dirs() -> Vec<PathBuf> {
    if let Ok(val) = std::env::var("CLAUDE_CONFIG_DIR") {
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
        let h = Path::new(&home);
        out.push(h.join(".config").join("claude"));
        out.push(h.join(".claude"));
    }
    out
}

/// Pure helper: checks if credentials are expired.
/// `expires_at` is epoch milliseconds.
pub fn is_claude_expired(creds: &ClaudeOAuthCredentials, now_ms: i64) -> bool {
    match creds.claude_ai_oauth.as_ref().and_then(|o| o.expires_at) {
        Some(exp) => exp < now_ms,
        None => false,
    }
}

/// Pure helper: extracts access token if not expired.
pub fn valid_claude_access_token(
    creds: &ClaudeOAuthCredentials,
    now_ms: i64,
) -> Result<String, &'static str> {
    let oauth = match &creds.claude_ai_oauth {
        Some(o) => o,
        None => return Err("no claudeAiOauth field in credential"),
    };
    if is_claude_expired(creds, now_ms) {
        return Err("credential expired");
    }
    match &oauth.access_token {
        Some(token) if !token.trim().is_empty() => Ok(token.clone()),
        _ => Err("no access_token in credential"),
    }
}

/// Pure parser: parses Claude OAuth usage JSON into quota windows.
///
/// Converts Anthropic `utilization` (which is percent used) directly to `used_pct`.
/// Window durations: five_hour = 18,000s, seven_day = 604,800s.
pub fn parse_claude_usage(json: &str) -> Result<Vec<QuotaWindow>, ProbeError> {
    let resp: AnthropicUsageResponse = serde_json::from_str(json)?;
    let mut windows = Vec::new();

    if let Some(w) = resp.five_hour {
        if let Some(util) = w.utilization {
            let resets_in_s = w.resets_at.as_deref().and_then(parse_resets_in_s);
            windows.push(QuotaWindow::new(
                WindowKind::FiveHour,
                util,
                resets_in_s,
                Some(18000),
            ));
        }
    }

    if let Some(w) = resp.seven_day {
        if let Some(util) = w.utilization {
            let resets_in_s = w.resets_at.as_deref().and_then(parse_resets_in_s);
            windows.push(QuotaWindow::new(
                WindowKind::SevenDay,
                util,
                resets_in_s,
                Some(604800),
            ));
        }
    }

    if windows.is_empty() {
        return Err(ProbeError::Failure("no usage windows in response".into()));
    }

    Ok(windows)
}

/// Helper to parse ISO8601 string or numeric seconds to remaining seconds.
fn parse_resets_in_s(iso_or_epoch: &str) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    // If it's pure digits
    if let Ok(epoch_s) = iso_or_epoch.parse::<u64>() {
        let s = if epoch_s > 100_000_000_000 {
            epoch_s / 1000
        } else {
            epoch_s
        };
        return if s > now { Some(s - now) } else { Some(0) };
    }

    // Try parsing ISO8601 with regex/manual components
    // E.g. "2026-09-15T15:00:00.000Z" or "2026-09-15T15:00:00Z"
    let re = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})").ok()?;
    let caps = re.captures(iso_or_epoch)?;
    let y: u64 = caps[1].parse().ok()?;
    let m: u64 = caps[2].parse().ok()?;
    let d: u64 = caps[3].parse().ok()?;
    let h: u64 = caps[4].parse().ok()?;
    let min: u64 = caps[5].parse().ok()?;
    let s: u64 = caps[6].parse().ok()?;

    // Simple approximate epoch calculation for ISO strings (standard civil timestamp)
    let epoch_s = days_since_epoch(y, m, d) * 86400 + h * 3600 + min * 60 + s;
    if epoch_s > now {
        Some(epoch_s - now)
    } else {
        Some(0)
    }
}

fn days_since_epoch(y: u64, m: u64, d: u64) -> u64 {
    // Days between 1970-01-01 and given year/month/day
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

/// Read credential string from macOS Keychain service "Claude Code-credentials" via security-framework.
#[cfg(target_os = "macos")]
pub fn read_keychain_claude_credentials() -> Option<String> {
    use security_framework::os::macos::keychain::SecKeychain;
    let sec_keychain = SecKeychain::default().ok()?;
    // Claude Code-credentials item is stored with account "" or username
    // Try SecKeychain::find_generic_password with account "" first
    if let Ok((pass, _)) = sec_keychain.find_generic_password("Claude Code-credentials", "") {
        if let Ok(s) = std::str::from_utf8(pass.as_ref()) {
            return Some(s.to_string());
        }
    }
    // Alternatively try command-line security tool as fallback without UI prompt
    None
}

#[cfg(not(target_os = "macos"))]
pub fn read_keychain_claude_credentials() -> Option<String> {
    None
}

/// Reads credentials following the probe order:
/// 1. Candidate config directories `CLAUDE_CONFIG_DIR` (or `~/.config/claude`, `~/.claude`), reading `.credentials.json`.
/// 2. macOS Keychain `Claude Code-credentials`.
///
/// Returns (credentials, source_label, was_expired_seen)
pub fn read_claude_credentials_first_usable(
    now_ms: i64,
) -> (Option<ClaudeOAuthCredentials>, Option<String>, bool) {
    let mut expired_seen = false;

    // 1. Files
    for dir in claude_config_dirs() {
        let cred_path = dir.join(".credentials.json");
        if cred_path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&cred_path) {
                if let Ok(creds) = serde_json::from_str::<ClaudeOAuthCredentials>(&content) {
                    if creds.claude_ai_oauth.is_some() {
                        if is_claude_expired(&creds, now_ms) {
                            expired_seen = true;
                            continue;
                        }
                        return (
                            Some(creds),
                            Some(cred_path.to_string_lossy().to_string()),
                            expired_seen,
                        );
                    }
                }
            }
        }
    }

    // 2. macOS Keychain
    if let Some(raw) = read_keychain_claude_credentials() {
        if let Ok(creds) = serde_json::from_str::<ClaudeOAuthCredentials>(&raw) {
            if creds.claude_ai_oauth.is_some() {
                if is_claude_expired(&creds, now_ms) {
                    expired_seen = true;
                } else {
                    return (
                        Some(creds),
                        Some("macOS Keychain: Claude Code-credentials".to_string()),
                        expired_seen,
                    );
                }
            }
        }
    }

    (None, None, expired_seen)
}

/// Snapshot returned when no usable OAuth credential is available.
pub fn missing_credentials_snapshot(expired_seen: bool) -> QuotaSnapshot {
    let slot = SlotId::default_for(HarnessId::ClaudeCode);
    let note = if expired_seen {
        "every credential found is expired — run `claude` once to refresh"
    } else {
        "no credential found — run `claude` once to log in"
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

/// Probes Claude Code usage from OAuth credentials and Anthropic usage endpoint.
/// Owned by session 02.
pub async fn probe() -> Result<QuotaSnapshot, ProbeError> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let (creds, source_label, expired_seen) =
        tokio::task::spawn_blocking(move || read_claude_credentials_first_usable(now_ms))
            .await
            .map_err(|e| ProbeError::Failure(format!("credential read task failed: {e}")))?;

    let slot = SlotId::default_for(HarnessId::ClaudeCode);

    let Some(creds) = creds else {
        return Ok(missing_credentials_snapshot(expired_seen));
    };

    let token = match valid_claude_access_token(&creds, now_ms) {
        Ok(t) => t,
        Err(_) => {
            return Ok(QuotaSnapshot {
                slot,
                status: QuotaStatus::Unknown,
                source: QuotaSource::OAuth,
                estimated: false,
                note: Some("credential expired — run `claude` once to refresh".to_string()),
                windows: vec![],
                lanes: vec![],
            });
        }
    };

    let client = crate::build_probe_http_client()?;

    let res = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/2.1.183")
        .header("Content-Type", "application/json")
        .send()
        .await;

    match res {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await?;
            let windows = parse_claude_usage(&body)?;
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
