use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aihub_core::{
    HarnessId, LaneKind, QuotaLane, QuotaSnapshot, QuotaSource, QuotaStatus, QuotaWindow, SlotId,
    WindowKind,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::ProbeError;

const CURSOR_TOKEN_SQL: &str =
    "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken' LIMIT 1";
const DASHBOARD_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const LOW_REMAINING_PCT: f64 = 20.0;

/// Probes Cursor usage from Cursor IDE `state.vscdb` token and dashboard endpoints.
pub async fn probe() -> Result<QuotaSnapshot, ProbeError> {
    let token = read_cursor_ide_token().or_else(|| {
        for path in cursor_auth_config_paths() {
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Some(jwt) = extract_cursor_jwt(&data) {
                    return Some(jwt);
                }
            }
        }
        None
    });

    let Some(token) = token else {
        return Ok(empty_snapshot(
            QuotaStatus::Unknown,
            Some("no Cursor IDE token in state.vscdb or config".into()),
        ));
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(ProbeError::Http)?;

    let dash = client
        .post(DASHBOARD_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Connect-Protocol-Version", "1")
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(ProbeError::Http)?;

    if dash.status().is_success() {
        let body = dash.text().await.map_err(ProbeError::Http)?;
        let (windows, lanes) = parse_dashboard_usage(&body)?;
        return Ok(build_snapshot(windows, lanes, QuotaSource::OAuth));
    }

    let dash_status = dash.status().as_u16();
    let summary = client
        .get("https://cursor.com/api/usage-summary")
        .header(
            "Cookie",
            format!(
                "WorkosCursorSessionToken={}%3A%3A{}",
                cursor_user_id_from_token(&token).unwrap_or_default(),
                token
            ),
        )
        .header("Origin", "https://cursor.com")
        .header("Referer", "https://cursor.com/dashboard")
        .send()
        .await
        .map_err(ProbeError::Http)?;

    if !summary.status().is_success() {
        return Ok(empty_snapshot(
            QuotaStatus::Unknown,
            Some(format!(
                "dashboard HTTP {dash_status}, usage-summary HTTP {}",
                summary.status().as_u16()
            )),
        ));
    }

    let body = summary.text().await.map_err(ProbeError::Http)?;
    let (windows, lanes) = parse_summary_usage(&body)?;
    Ok(build_snapshot(windows, lanes, QuotaSource::OAuth))
}

/// Pure parser: parses Cursor `GetCurrentPeriodUsage` dashboard response into windows and lanes.
pub fn parse_dashboard_usage(json: &str) -> Result<(Vec<QuotaWindow>, Vec<QuotaLane>), ProbeError> {
    let root: Value = serde_json::from_str(json)?;
    let plan = root
        .get("planUsage")
        .or_else(|| root.get("plan_usage"))
        .cloned()
        .unwrap_or(Value::Null);

    let mut used_pct = plan
        .get("totalPercentUsed")
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)));
    if used_pct.is_none() {
        let limit = plan.get("limit").and_then(|v| v.as_f64());
        if let Some(limit) = limit.filter(|l| *l > 0.0) {
            let used = plan
                .get("used")
                .and_then(|v| v.as_f64())
                .or_else(|| {
                    plan.get("remaining")
                        .and_then(|v| v.as_f64())
                        .map(|rem| limit - rem)
                });
            if let Some(used) = used {
                used_pct = Some((used / limit) * 100.0);
            }
        }
    }

    let used_pct = used_pct.ok_or_else(|| {
        ProbeError::Failure("no plan usage in dashboard response".into())
    })?;

    let start_ms = root
        .get("billingCycleStart")
        .and_then(parse_epoch_ms_field);
    let end_ms = root.get("billingCycleEnd").and_then(parse_epoch_ms_field);

    let window_s = match (start_ms, end_ms) {
        (Some(s), Some(e)) if e > s => Some(((e - s) / 1000) as u64),
        _ => None,
    };
    let resets_in_s = end_ms.and_then(resets_in_from_epoch_ms);

    let windows = vec![QuotaWindow::new(
        WindowKind::Cycle,
        used_pct,
        resets_in_s,
        window_s,
    )];

    let lanes = cursor_lanes(&plan, &root).ok_or_else(|| {
        ProbeError::Failure("incomplete lane split in dashboard response".into())
    })?;

    Ok((windows, lanes))
}

/// Pure parser: parses legacy `usage-summary` JSON response into windows and lanes.
pub fn parse_summary_usage(json: &str) -> Result<(Vec<QuotaWindow>, Vec<QuotaLane>), ProbeError> {
    let root: Value = serde_json::from_str(json)?;
    if root.get("isUnlimited").and_then(|v| v.as_bool()) == Some(true) {
        return Ok((
            vec![QuotaWindow::new(WindowKind::Custom("unlimited".into()), 0.0, None, None)],
            vec![],
        ));
    }

    let plan = root
        .get("individualUsage")
        .and_then(|v| v.get("plan"))
        .or_else(|| root.get("teamUsage").and_then(|v| v.get("plan")))
        .cloned()
        .unwrap_or(Value::Null);

    let used_pct = plan
        .get("totalPercentUsed")
        .or_else(|| plan.get("autoPercentUsed"))
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)))
        .ok_or_else(|| ProbeError::Failure("no plan usage in usage-summary response".into()))?;

    let start_ms = root
        .get("billingCycleStart")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339_ms);
    let end_ms = root
        .get("billingCycleEnd")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339_ms);

    let window_s = match (start_ms, end_ms) {
        (Some(s), Some(e)) if e > s => Some(((e - s) / 1000) as u64),
        _ => None,
    };
    let resets_in_s = end_ms.and_then(resets_in_from_epoch_ms);

    let windows = vec![QuotaWindow::new(
        WindowKind::Cycle,
        used_pct,
        resets_in_s,
        window_s,
    )];

    let lanes = cursor_lanes(&plan, &root).ok_or_else(|| {
        ProbeError::Failure("incomplete lane split in usage-summary response".into())
    })?;

    Ok((windows, lanes))
}

/// Pure parser: traverses a JSON value to find the first JWT string containing a `sub` claim.
pub fn extract_cursor_jwt(json: &str) -> Option<String> {
    let root: Value = serde_json::from_str(json).ok()?;
    find_jwt_in_value(&root, 0)
}

fn build_snapshot(
    windows: Vec<QuotaWindow>,
    lanes: Vec<QuotaLane>,
    source: QuotaSource,
) -> QuotaSnapshot {
    QuotaSnapshot {
        slot: SlotId::default_for(HarnessId::CursorAgent),
        status: snapshot_status(&windows, &lanes),
        source,
        estimated: false,
        note: None,
        windows,
        lanes,
    }
}

fn empty_snapshot(status: QuotaStatus, note: Option<String>) -> QuotaSnapshot {
    QuotaSnapshot {
        slot: SlotId::default_for(HarnessId::CursorAgent),
        status,
        source: QuotaSource::OAuth,
        estimated: false,
        note,
        windows: vec![],
        lanes: vec![],
    }
}

fn snapshot_status(windows: &[QuotaWindow], lanes: &[QuotaLane]) -> QuotaStatus {
    if let Some(w) = tightest_window(windows) {
        return status_from_used(w.used_pct);
    }
    let best_lane = lanes
        .iter()
        .filter_map(|lane| {
            tightest_window(&lane.windows).map(|w| (lane, w.remaining_pct()))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    if let Some((_, remaining)) = best_lane {
        return status_from_remaining(remaining);
    }
    QuotaStatus::Unknown
}

fn tightest_window(windows: &[QuotaWindow]) -> Option<&QuotaWindow> {
    windows.iter().max_by(|a, b| {
        a.used_pct
            .partial_cmp(&b.used_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn status_from_used(used_pct: f64) -> QuotaStatus {
    status_from_remaining((100.0 - used_pct).max(0.0))
}

fn status_from_remaining(remaining_pct: f64) -> QuotaStatus {
    if remaining_pct <= 0.0 {
        QuotaStatus::Empty
    } else if remaining_pct < LOW_REMAINING_PCT {
        QuotaStatus::Low
    } else {
        QuotaStatus::Ok
    }
}

fn cursor_lanes(plan: &Value, payload: &Value) -> Option<Vec<QuotaLane>> {
    let auto = read_lane_used(plan, payload, "autoPercentUsed", "autoModelSelectedDisplayMessage");
    let api = read_lane_used(plan, payload, "apiPercentUsed", "namedModelSelectedDisplayMessage");
    let (auto, api) = (auto?, api?);
    Some(vec![
        lane_from_used("cursor-models", LaneKind::Own, auto),
        lane_from_used("other-models", LaneKind::Frontier, api),
    ])
}

fn read_lane_used(plan: &Value, payload: &Value, field: &str, message_field: &str) -> Option<f64> {
    plan.get(field)
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)))
        .or_else(|| {
            payload
                .get(message_field)
                .and_then(|v| v.as_str())
                .and_then(percent_from_message)
        })
}

fn lane_from_used(name: &str, kind: LaneKind, used_pct: f64) -> QuotaLane {
    QuotaLane {
        name: name.to_string(),
        kind,
        windows: vec![QuotaWindow::new(WindowKind::Cycle, used_pct, None, None)],
    }
}

fn percent_from_message(msg: &str) -> Option<f64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(\d+(?:\.\d+)?)\s*%").expect("regex"));
    re.captures(msg)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

fn parse_epoch_ms_field(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    v.as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| v.as_str().and_then(parse_rfc3339_ms))
}

fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    // ponytail: parses Z-suffixed timestamps from Cursor/Antigravity fixtures and APIs only.
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

fn find_jwt_in_value(value: &Value, depth: usize) -> Option<String> {
    if depth > 6 {
        return None;
    }
    match value {
        Value::String(s) => {
            if s.split('.').count() == 3 && jwt_claim(s, "sub").is_some() {
                Some(s.clone())
            } else {
                None
            }
        }
        Value::Array(items) => items.iter().find_map(|v| find_jwt_in_value(v, depth + 1)),
        Value::Object(map) => map.values().find_map(|v| find_jwt_in_value(v, depth + 1)),
        _ => None,
    }
}

fn jwt_claim(token: &str, claim: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let padded = payload.replace('-', "+").replace('_', "/");
    let decoded = base64_decode(&padded).ok()?;
    let v: Value = serde_json::from_slice(&decoded).ok()?;
    v.get(claim).and_then(|c| c.as_str()).map(String::from)
}

fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    let mut padded = input.to_string();
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
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
    let bytes = padded.as_bytes();
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
        Err(())
    } else {
        Ok(out)
    }
}

fn cursor_user_id_from_token(token: &str) -> Option<String> {
    jwt_claim(token, "sub").and_then(|sub| cursor_user_id(&sub))
}

fn cursor_user_id(raw_id: &str) -> Option<String> {
    if raw_id.is_empty() {
        return None;
    }
    raw_id
        .split('|')
        .find(|p| p.starts_with("user_"))
        .map(String::from)
        .or_else(|| raw_id.starts_with("user_").then(|| raw_id.to_string()))
}

fn cursor_db_paths() -> Vec<PathBuf> {
    let home = dirs_home();
    vec![
        home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"),
        home.join(".config/Cursor/User/globalStorage/state.vscdb"),
        home.join("AppData/Roaming/Cursor/User/globalStorage/state.vscdb"),
    ]
}

fn cursor_auth_config_paths() -> Vec<PathBuf> {
    let home = dirs_home();
    vec![
        home.join(".config/cursor/auth.json"),
        home.join(".config/cursor-agent/auth.json"),
        home.join("Library/Application Support/cursor/auth.json"),
        home.join("Library/Application Support/Cursor/auth.json"),
        home.join(".cursor/auth.json"),
        home.join(".cursor/cli-config.json"),
        home.join(".cursor/credentials.json"),
        home.join(".cursor-agent/auth.json"),
    ]
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn read_cursor_ide_token_from_path(db_path: &Path) -> Result<Option<String>, ProbeError> {
    if !db_path.is_file() {
        return Ok(None);
    }
    let uri = format!("file:{}?mode=ro", db_path.display());
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut stmt = conn.prepare(CURSOR_TOKEN_SQL)?;
    let token: Option<String> = stmt.query_row([], |row| row.get(0)).ok();
    Ok(token.filter(|t| !t.is_empty()))
}

fn read_cursor_ide_token() -> Option<String> {
    for path in cursor_db_paths() {
        if let Ok(Some(token)) = read_cursor_ide_token_from_path(&path) {
            return Some(token);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_extraction_finds_nested_token() {
        let json = include_str!("../tests/fixtures/cursor/auth_with_jwt.json");
        let jwt = extract_cursor_jwt(json).expect("jwt");
        assert!(jwt.contains("user_test") || jwt_claim(&jwt, "sub") == Some("user_test".into()));
    }
}
