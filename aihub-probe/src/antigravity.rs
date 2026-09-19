use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aihub_core::{
    HarnessId, LaneKind, QuotaLane, QuotaSnapshot, QuotaSource, QuotaStatus, QuotaWindow, SlotId,
    WindowKind,
};
use regex::Regex;
use serde_json::Value;

use crate::ProbeError;

const AGY_QUOTA_RPC: &str = "exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";
const AGY_CLOUD_QUOTA: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
];
const LOW_REMAINING_PCT: f64 = 20.0;
const GO_KEYRING_PREFIX: &str = "go-keyring-base64:";

/// Probes Antigravity quota from running language server RPC or saved OS keyring session.
pub async fn probe() -> Result<QuotaSnapshot, ProbeError> {
    let bases = tokio::task::spawn_blocking(discover_ls_bases)
        .await
        .map_err(|e| ProbeError::Failure(format!("antigravity discovery failed: {e}")))?;
    if !bases.is_empty() {
        if let Ok(lanes) = fetch_local_quota(&bases).await {
            return Ok(build_snapshot(lanes, QuotaSource::Vendor, None));
        }
    }

    #[cfg(target_os = "macos")]
    if let Some(session) = tokio::task::spawn_blocking(read_antigravity_session)
        .await
        .map_err(|e| ProbeError::Failure(format!("antigravity session read failed: {e}")))?
    {
        if let Ok(lanes) = fetch_cloud_quota(&session.token).await {
            return Ok(build_snapshot(lanes, QuotaSource::OAuth, None));
        }
    }

    // Additive fallback for hosts without a macOS Keychain (Linux boxes) or where the
    // Keychain lookup above found nothing: a config file under ~/.config/antigravity/
    // (or the legacy ~/.gemini/ Gemini CLI location), then GEMINI_AUTH_TOKEN.
    if let Some(session) = tokio::task::spawn_blocking(read_antigravity_session_fallback)
        .await
        .map_err(|e| {
            ProbeError::Failure(format!("antigravity fallback session read failed: {e}"))
        })?
    {
        if let Ok(lanes) = fetch_cloud_quota(&session.token).await {
            return Ok(build_snapshot(lanes, QuotaSource::OAuth, None));
        }
    }

    Ok(empty_snapshot(
        QuotaStatus::Unknown,
        Some("no Antigravity language server or saved session".into()),
    ))
}

/// Pure parser: parses `RetrieveUserQuotaSummary` JSON into Gemini and 3p lanes.
pub fn parse_quota_summary(json: &str) -> Result<Vec<QuotaLane>, ProbeError> {
    let root: Value = serde_json::from_str(json)?;
    let groups = root
        .get("response")
        .and_then(|r| r.get("groups"))
        .or_else(|| root.get("groups"))
        .and_then(|g| g.as_array())
        .ok_or_else(|| ProbeError::Failure("quota summary has no groups".into()))?;

    let mut by_lane: [Vec<QuotaWindow>; 2] = [vec![], vec![]];
    let mut seen = Vec::new();

    for group in groups {
        let group_name = group
            .get("displayName")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let buckets = group.get("buckets").and_then(|b| b.as_array());
        let Some(buckets) = buckets else {
            continue;
        };
        for bucket in buckets {
            let id = bucket
                .get("bucketId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let win = bucket.get("window").and_then(|v| v.as_str()).unwrap_or("");
            seen.push(if id.is_empty() {
                "<unnamed>".to_string()
            } else {
                id.to_string()
            });

            let weekly = if id.ends_with("weekly") || win == "weekly" {
                Some(true)
            } else if id.ends_with("5h") || win == "5h" {
                Some(false)
            } else {
                None
            };
            let Some(weekly) = weekly else {
                continue;
            };

            let lane_idx = if id.starts_with("gemini") {
                Some(0)
            } else if id.starts_with("3p") {
                Some(1)
            } else if group_name.contains("Gemini") {
                Some(0)
            } else if group_name.contains("Claude") || group_name.contains("GPT") {
                Some(1)
            } else {
                None
            };
            let Some(lane_idx) = lane_idx else {
                continue;
            };

            let frac = bucket.get("remainingFraction").and_then(|v| v.as_f64());
            let Some(frac) = frac.filter(|f| f.is_finite() && (0.0..=1.0).contains(f)) else {
                continue;
            };

            let used_pct = (100.0 - (frac * 100.0).round()).clamp(0.0, 100.0);
            let kind = if weekly {
                WindowKind::SevenDay
            } else {
                WindowKind::FiveHour
            };
            let window_s = if weekly { Some(604_800) } else { Some(18_000) };
            let resets_in_s = bucket
                .get("resetTime")
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339_ms)
                .and_then(resets_in_from_epoch_ms);

            by_lane[lane_idx].push(QuotaWindow::new(kind, used_pct, resets_in_s, window_s));
        }
    }

    let mut lanes = Vec::new();
    if !by_lane[0].is_empty() {
        lanes.push(QuotaLane {
            name: "gemini".into(),
            kind: LaneKind::Own,
            windows: by_lane[0].clone(),
        });
    }
    if !by_lane[1].is_empty() {
        lanes.push(QuotaLane {
            name: "third-party".into(),
            kind: LaneKind::Frontier,
            windows: by_lane[1].clone(),
        });
    }

    if lanes.is_empty() {
        return Err(ProbeError::Failure(format!(
            "no 5h or weekly bucket in the quota summary (it offered: {})",
            if seen.is_empty() {
                "nothing".into()
            } else {
                seen.join(", ")
            }
        )));
    }

    Ok(lanes)
}

/// Pure parser: parses `lsof -nP -iTCP -sTCP:LISTEN -F pcn` machine-readable output for candidate ports.
pub fn parse_lsof_ports(lsof_output: &str) -> Vec<u16> {
    static PROC_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let proc_re = PROC_RE
        .get_or_init(|| Regex::new(r"(^|/)agy$|language_server|antigravity").expect("regex"));

    let mut per_pid: std::collections::BTreeMap<i32, Vec<u16>> = std::collections::BTreeMap::new();
    let mut pid: Option<i32> = None;
    let mut owned: Option<i32> = None;

    for line in lsof_output.lines() {
        if line.is_empty() {
            continue;
        }
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let rest = &line[1..];
        match tag {
            'p' => {
                pid = rest.parse().ok();
                owned = None;
            }
            'c' => {
                owned = pid.filter(|_| proc_re.is_match(rest.trim()));
            }
            'n' if owned.is_some() => {
                if let Some(port) = rest.split(':').next_back().and_then(|p| p.parse().ok()) {
                    if port > 0 {
                        per_pid.entry(owned.unwrap()).or_default().push(port);
                    }
                }
            }
            _ => {}
        }
    }

    agy_probe_order(&per_pid)
}

/// Pure parser: extracts `csrfToken` from HTML served at `/`.
pub fn parse_csrf_token(html: &str) -> Option<String> {
    html.split("csrfToken\":\"")
        .nth(1)?
        .split('"')
        .next()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

pub fn discover_ls_bases() -> Vec<String> {
    let override_addr = std::env::var("ANTIGRAVITY_LS_ADDRESS").ok();
    let output = if which("lsof") {
        std::process::Command::new("lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    } else {
        String::new()
    };
    discover_ls_bases_from(override_addr.as_deref(), &output)
}

/// Discover bases from explicit inputs; no environment access or subprocesses.
pub fn discover_ls_bases_from(override_addr: Option<&str>, lsof_output: &str) -> Vec<String> {
    let mut bases = Vec::new();
    if let Some(override_addr) = override_addr {
        if let Some(base) = normalize_ls_override(override_addr.trim()) {
            bases.push(base);
        }
    }
    for port in parse_lsof_ports(lsof_output) {
        let base = format!("http://127.0.0.1:{port}");
        if !bases.contains(&base) {
            bases.push(base);
        }
    }
    bases
}

fn normalize_ls_override(raw: &str) -> Option<String> {
    let (scheme, rest) = if let Some(rest) = raw.strip_prefix("http://") {
        ("http", rest)
    } else if let Some(rest) = raw.strip_prefix("https://") {
        ("https", rest)
    } else {
        ("http", raw)
    };
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() {
        None
    } else {
        Some(format!("{scheme}://{authority}"))
    }
}

fn agy_probe_order(per_pid: &std::collections::BTreeMap<i32, Vec<u16>>) -> Vec<u16> {
    let groups: Vec<Vec<u16>> = per_pid
        .values()
        .map(|ports| {
            let mut uniq: Vec<u16> = ports
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            uniq.sort_by(|a, b| b.cmp(a));
            uniq
        })
        .collect();

    let mut out = Vec::new();
    let mut rank = 0usize;
    loop {
        let row: Vec<u16> = groups.iter().filter_map(|g| g.get(rank).copied()).collect();
        if row.is_empty() {
            break;
        }
        out.extend(row);
        rank += 1;
    }
    out
}

async fn fetch_local_quota(bases: &[String]) -> Result<Vec<QuotaLane>, ProbeError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(ProbeError::Http)?;

    for base in bases {
        let csrf = match client.get(base.as_str()).send().await {
            Ok(r) if r.status().is_success() => {
                let html = r.text().await.unwrap_or_default();
                parse_csrf_token(&html)
            }
            _ => None,
        };

        let url = format!("{base}/{AGY_QUOTA_RPC}");
        let mut req = client.post(&url).header("Content-Type", "application/json");
        if let Some(token) = &csrf {
            req = req.header("x-codeium-csrf-token", token);
        }
        match req.body("{}").send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.map_err(ProbeError::Http)?;
                return parse_quota_summary(&body);
            }
            _ => continue,
        }
    }

    Err(ProbeError::Failure(
        "no Antigravity language server answered".into(),
    ))
}

async fn fetch_cloud_quota(token: &str) -> Result<Vec<QuotaLane>, ProbeError> {
    let client = crate::build_probe_http_client()?;

    for url in AGY_CLOUD_QUOTA {
        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("User-Agent", "antigravity")
            .body("{}")
            .send()
            .await
            .map_err(ProbeError::Http)?;

        if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
            return Err(ProbeError::Failure(
                "saved Google session was rejected".into(),
            ));
        }
        if !resp.status().is_success() {
            continue;
        }
        let body = resp.text().await.map_err(ProbeError::Http)?;
        return parse_quota_summary(&body);
    }

    Err(ProbeError::Failure(
        "no Cloud Code endpoint answered".into(),
    ))
}

#[cfg(target_os = "macos")]
fn read_antigravity_session() -> Option<AntigravitySession> {
    let raw = read_keychain_raw("gemini", "antigravity")?;
    parse_antigravity_session(&raw)
}

#[cfg(not(target_os = "macos"))]
fn read_antigravity_session() -> Option<AntigravitySession> {
    None
}

/// Pure parser: extracts the bearer token from a saved Antigravity/Gemini CLI session
/// file (`~/.config/antigravity/*.json` or `~/.gemini/oauth_creds.json`), independent
/// of the macOS Keychain and of any subprocess/network access.
pub fn parse_antigravity_session_token(raw: &str) -> Option<String> {
    parse_antigravity_session(raw).map(|s| s.token)
}

/// Reads and parses a saved Antigravity/Gemini CLI session file from an explicit path.
/// Used by the non-macOS config-file fallback and directly by tests.
pub fn read_antigravity_session_file(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_antigravity_session_token(&raw)
}

fn antigravity_config_paths() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    vec![
        home.join(".config/antigravity/session.json"),
        home.join(".config/antigravity/auth.json"),
        home.join(".gemini/oauth_creds.json"),
    ]
}

/// Non-Keychain session lookup: config files under `~/.config/antigravity/` (or the
/// legacy `~/.gemini/`), then `GEMINI_AUTH_TOKEN`. Additive alternative to the macOS
/// Keychain read above, tried on every platform when it comes up empty.
fn read_antigravity_session_fallback() -> Option<AntigravitySession> {
    for path in antigravity_config_paths() {
        if let Some(token) = read_antigravity_session_file(&path) {
            return Some(AntigravitySession {
                token,
                expires_at: None,
            });
        }
    }
    std::env::var("GEMINI_AUTH_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .map(|token| AntigravitySession {
            token,
            expires_at: None,
        })
}

struct AntigravitySession {
    token: String,
    #[allow(dead_code)]
    expires_at: Option<i64>,
}

fn parse_antigravity_session(raw: &str) -> Option<AntigravitySession> {
    let mut json = raw.trim().to_string();
    if json.starts_with(GO_KEYRING_PREFIX) {
        json = decode_base64_json(json.strip_prefix(GO_KEYRING_PREFIX)?.trim())?;
    }
    let root: Value = serde_json::from_str(&json).ok()?;
    let t = root
        .get("token")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(root);
    let token = [
        "access_token",
        "accessToken",
        "token",
        "id_token",
        "idToken",
        "bearerToken",
        "auth_token",
        "authToken",
    ]
    .iter()
    .find_map(|k| t.get(k).and_then(|v| v.as_str()))
    .filter(|s| !s.trim().is_empty())
    .map(String::from)?;

    let expiry = ["expiry", "expires_at", "expiresAt"]
        .iter()
        .find_map(|k| t.get(k))
        .and_then(|v| {
            if let Some(n) = v.as_i64() {
                Some(epoch_to_ms(n))
            } else {
                v.as_str().and_then(parse_rfc3339_ms)
            }
        });

    Some(AntigravitySession {
        token,
        expires_at: expiry,
    })
}

fn decode_base64_json(b64: &str) -> Option<String> {
    let bytes = standard_base64_decode(b64.trim())?;
    String::from_utf8(bytes).ok()
}

fn standard_base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 256] = &{
        let mut t = [255u8; 256];
        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 {
            t[chars[i] as usize] = i as u8;
            i += 1;
        }
        t
    };
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let b0 = TABLE[bytes[i] as usize];
        let b1 = TABLE[bytes[i + 1] as usize];
        let b2 = TABLE[bytes[i + 2] as usize];
        let b3 = TABLE[bytes[i + 3] as usize];
        if b0 == 255 || b1 == 255 {
            break;
        }
        out.push((b0 << 2) | (b1 >> 4));
        if bytes[i + 2] == b'=' {
            break;
        }
        if b2 == 255 {
            break;
        }
        out.push(((b1 & 0x0f) << 4) | (b2 >> 2));
        if bytes[i + 3] == b'=' {
            break;
        }
        if b3 == 255 {
            break;
        }
        out.push(((b2 & 0x03) << 6) | b3);
        i += 4;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(target_os = "macos")]
fn read_keychain_raw(service: &str, account: &str) -> Option<String> {
    use security_framework::passwords::get_generic_password;
    get_generic_password(service, account)
        .ok()
        .and_then(|buf| String::from_utf8(buf).ok())
}

fn epoch_to_ms(n: i64) -> i64 {
    if n >= 100_000_000_000 {
        n
    } else {
        n * 1000
    }
}

fn build_snapshot(
    lanes: Vec<QuotaLane>,
    source: QuotaSource,
    note: Option<String>,
) -> QuotaSnapshot {
    QuotaSnapshot {
        slot: SlotId::default_for(HarnessId::Antigravity),
        status: snapshot_status(&lanes),
        source,
        estimated: false,
        note,
        windows: vec![],
        lanes,
    }
}

fn empty_snapshot(status: QuotaStatus, note: Option<String>) -> QuotaSnapshot {
    QuotaSnapshot {
        slot: SlotId::default_for(HarnessId::Antigravity),
        status,
        source: QuotaSource::Vendor,
        estimated: false,
        note,
        windows: vec![],
        lanes: vec![],
    }
}

fn snapshot_status(lanes: &[QuotaLane]) -> QuotaStatus {
    let best = lanes
        .iter()
        .filter_map(|lane| {
            lane.windows
                .iter()
                .max_by(|a, b| {
                    a.used_pct
                        .partial_cmp(&b.used_pct)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|w| w.remaining_pct())
        })
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    if let Some(remaining) = best {
        if remaining <= 0.0 {
            QuotaStatus::Empty
        } else if remaining < LOW_REMAINING_PCT {
            QuotaStatus::Low
        } else {
            QuotaStatus::Ok
        }
    } else {
        QuotaStatus::Unknown
    }
}

fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 20 || !s.ends_with('Z') {
        return None;
    }
    let date = &s[..10];
    let time = &s[11..19];
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let mut tp = time.split(':');
    let hour: i64 = tp.next()?.parse().ok()?;
    let min: i64 = tp.next()?.parse().ok()?;
    let sec: i64 = tp.next()?.parse().ok()?;
    let days = unix_days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec;
    Some(secs * 1000)
}

fn unix_days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let mut y = year;
    let mut m = month;
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn resets_in_from_epoch_ms(end_ms: i64) -> Option<u64> {
    if end_ms <= 0 {
        return None;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    let delta_ms = end_ms - now_ms;
    Some(if delta_ms > 0 {
        (delta_ms / 1000) as u64
    } else {
        0
    })
}

fn which(bin: &str) -> bool {
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
