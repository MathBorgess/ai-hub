use aihub_core::{QuotaStatus, WindowKind};
use aihub_probe::claude::{
    is_claude_expired, parse_claude_usage, valid_claude_access_token, ClaudeOAuthCredentials,
};
use aihub_probe::codex::{
    is_codex_expired, parse_codex_usage, valid_codex_tokens, CodexAuth,
};
use aihub_probe::transcripts::{
    calculate_rolling_windows, parse_transcript_line, TranscriptTurn,
};
use aihub_probe::windows::{bucket_for_usage, pick_tightest_window};

#[test]
fn test_claude_usage_parse() {
    let fixture = include_str!("fixtures/claude/usage_ok.json");
    let windows = parse_claude_usage(fixture).expect("parse claude usage");
    assert_eq!(windows.len(), 2);

    let w5 = windows.iter().find(|w| w.kind == WindowKind::FiveHour).expect("five_hour");
    assert_eq!(w5.used_pct, 42.5);
    assert_eq!(w5.window_s, Some(18000));
    assert_eq!(w5.remaining_pct(), 57.5);

    let w7 = windows.iter().find(|w| w.kind == WindowKind::SevenDay).expect("seven_day");
    assert_eq!(w7.used_pct, 78.0);
    assert_eq!(w7.window_s, Some(604800));
    assert_eq!(w7.remaining_pct(), 22.0);

    let tightest = pick_tightest_window(&windows).expect("tightest");
    assert_eq!(tightest.kind, WindowKind::SevenDay);
    assert_eq!(tightest.used_pct, 78.0);
}

#[test]
fn test_claude_credentials_valid_and_expired() {
    let valid_json = include_str!("fixtures/claude/credentials_valid.json");
    let valid_creds: ClaudeOAuthCredentials = serde_json::from_str(valid_json).expect("valid creds json");
    let now_ms = 1750000000000; // before expiresAt (1789400000000)
    assert!(!is_claude_expired(&valid_creds, now_ms));
    let token = valid_claude_access_token(&valid_creds, now_ms).expect("valid token");
    assert_eq!(token, "fabricated-token-active-12345");

    let expired_json = include_str!("fixtures/claude/credentials_expired.json");
    let expired_creds: ClaudeOAuthCredentials = serde_json::from_str(expired_json).expect("expired creds json");
    assert!(is_claude_expired(&expired_creds, now_ms));
    let err = valid_claude_access_token(&expired_creds, now_ms).expect_err("should be expired");
    assert_eq!(err, "credential expired");
}

#[test]
fn test_codex_usage_parse() {
    let fixture = include_str!("fixtures/codex/usage_ok.json");
    let windows = parse_codex_usage(fixture).expect("parse codex usage");
    assert_eq!(windows.len(), 2);

    let primary = windows.iter().find(|w| w.kind == WindowKind::FiveHour).expect("primary");
    assert_eq!(primary.used_pct, 15.0);
    assert_eq!(primary.window_s, Some(18000));

    let secondary = windows.iter().find(|w| w.kind == WindowKind::SevenDay).expect("secondary");
    assert_eq!(secondary.used_pct, 65.5);
    assert_eq!(secondary.window_s, Some(604800));

    let tightest = pick_tightest_window(&windows).expect("tightest");
    assert_eq!(tightest.kind, WindowKind::SevenDay);
    assert_eq!(tightest.used_pct, 65.5);
}

#[test]
fn test_codex_auth_valid_and_expired() {
    let valid_json = include_str!("fixtures/codex/auth_valid.json");
    let valid_auth: CodexAuth = serde_json::from_str(valid_json).expect("valid auth json");
    let now_ms = 1750000000000;
    assert!(!is_codex_expired(&valid_auth, now_ms));
    let (token, account_id) = valid_codex_tokens(&valid_auth, now_ms).expect("valid tokens");
    assert_eq!(token, "fabricated-codex-token-12345");
    assert_eq!(account_id.as_deref(), Some("fabricated-account-abc"));

    let expired_json = include_str!("fixtures/codex/auth_expired.json");
    let expired_auth: CodexAuth = serde_json::from_str(expired_json).expect("expired auth json");
    assert!(is_codex_expired(&expired_auth, now_ms));
    let err = valid_codex_tokens(&expired_auth, now_ms).expect_err("should be expired");
    assert_eq!(err, "credential expired");
}

#[test]
fn test_claude_transcript_parse_and_rolling_window() {
    let fixture = include_str!("fixtures/claude/transcript.jsonl");
    let mut turns = Vec::new();
    for line in fixture.lines() {
        if let Some(turn) = parse_transcript_line(line).expect("parse line") {
            turns.push(turn);
        }
    }
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].tokens_used, 1200 + 300 + 150 + 50); // 1700
    assert_eq!(turns[1].tokens_used, 2500 + 500 + 200);      // 3200

    // Also test calculation with turns across multiple 5h windows
    let mut multi_turns = Vec::new();
    // Block 1 (day 1, 10:00:00)
    let t1 = 1750000000;
    multi_turns.push(TranscriptTurn {
        timestamp_s: t1,
        tokens_used: 10_000,
        session_id: "s1".into(),
    });
    // Block 2 (day 2, 10:00:00 - 24 hours later)
    let t2 = t1 + 86400;
    multi_turns.push(TranscriptTurn {
        timestamp_s: t2,
        tokens_used: 50_000,
        session_id: "s2".into(),
    });
    // Current active block (recent turn)
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    multi_turns.push(TranscriptTurn {
        timestamp_s: now_s - 1800, // 30 min ago
        tokens_used: 25_000,
        session_id: "s3".into(),
    });

    let windows = calculate_rolling_windows(&multi_turns);
    assert_eq!(windows.len(), 1);
    // Active tokens = 25_000, completed heaviest = 50_000 -> used_pct = 50%
    assert_eq!(windows[0].used_pct, 50.0);
    assert!(windows[0].resets_in_s.is_some());
    assert_eq!(bucket_for_usage(windows[0].used_pct, 20.0), QuotaStatus::Ok);
}

#[test]
fn test_codex_transcript_parse() {
    let fixture = include_str!("fixtures/codex/transcript.jsonl");
    let mut turns = Vec::new();
    for line in fixture.lines() {
        if let Some(turn) = parse_transcript_line(line).expect("parse line") {
            turns.push(turn);
        }
    }
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].tokens_used, 800 + 100 + 200);  // 1100
    assert_eq!(turns[1].tokens_used, 1500 + 300 + 400); // 2200
    assert_eq!(turns[0].session_id, "codex-sess-1");
}
