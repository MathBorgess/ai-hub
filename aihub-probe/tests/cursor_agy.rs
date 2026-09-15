use aihub_core::{LaneKind, WindowKind};
use aihub_probe::antigravity::{
    discover_ls_bases, parse_csrf_token, parse_lsof_ports, parse_quota_summary,
};
use aihub_probe::cursor::{
    extract_cursor_jwt, parse_dashboard_usage, parse_summary_usage, read_cursor_ide_token_from_path,
};
use rusqlite::Connection;
use std::sync::{Mutex, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> &'static Mutex<()> {
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

fn fixture(path: &str) -> String {
    std::fs::read_to_string(path).expect("fixture")
}

#[test]
fn cursor_dashboard_parses_cycle_and_lanes() {
    let json = fixture("tests/fixtures/cursor/dashboard_usage.json");
    let (windows, lanes) = parse_dashboard_usage(&json).expect("parse");

    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].kind, WindowKind::Cycle);
    assert!((windows[0].used_pct - 42.0).abs() < f64::EPSILON);
    assert_eq!(windows[0].window_s, Some(2_592_000));

    assert_eq!(lanes.len(), 2);
    assert_eq!(lanes[0].name, "cursor-models");
    assert_eq!(lanes[0].kind, LaneKind::Own);
    assert!((lanes[0].windows[0].used_pct - 10.0).abs() < f64::EPSILON);

    assert_eq!(lanes[1].name, "other-models");
    assert_eq!(lanes[1].kind, LaneKind::Frontier);
    assert!((lanes[1].windows[0].used_pct - 100.0).abs() < f64::EPSILON);
}

#[test]
fn cursor_depleted_lane_does_not_empty_slot() {
    let json = fixture("tests/fixtures/cursor/dashboard_usage.json");
    let (windows, lanes) = parse_dashboard_usage(&json).expect("parse");
    assert!(windows[0].remaining_pct() > 0.0);
    assert_eq!(lanes[1].windows[0].remaining_pct(), 0.0);
}

#[test]
fn cursor_summary_fallback_parses() {
    let json = fixture("tests/fixtures/cursor/usage_summary.json");
    let (windows, lanes) = parse_summary_usage(&json).expect("parse");
    assert_eq!(windows.len(), 1);
    assert!((windows[0].used_pct - 30.0).abs() < f64::EPSILON);
    assert_eq!(lanes.len(), 2);
}

#[test]
fn cursor_jwt_extraction() {
    let json = fixture("tests/fixtures/cursor/auth_with_jwt.json");
    assert!(extract_cursor_jwt(&json).is_some());
}

#[test]
fn cursor_reads_token_from_sqlite_readonly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("state.vscdb");
    let conn = Connection::open(&db_path).expect("open");
    conn.execute(
        "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT)",
        [],
    )
    .expect("schema");
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
        ["cursorAuth/accessToken", "fabricated.jwt.token"],
    )
    .expect("insert");
    drop(conn);

    let token = read_cursor_ide_token_from_path(&db_path)
        .expect("read")
        .expect("token");
    assert_eq!(token, "fabricated.jwt.token");
}

#[test]
fn antigravity_quota_summary_lanes_and_windows() {
    let json = fixture("tests/fixtures/antigravity/quota_summary.json");
    let lanes = parse_quota_summary(&json).expect("parse");
    assert_eq!(lanes.len(), 2);

    let gemini = &lanes[0];
    assert_eq!(gemini.name, "gemini");
    assert_eq!(gemini.kind, LaneKind::Own);
    assert_eq!(gemini.windows.len(), 2);
    assert!(gemini
        .windows
        .iter()
        .any(|w| w.kind == WindowKind::FiveHour));
    assert!(gemini
        .windows
        .iter()
        .any(|w| w.kind == WindowKind::SevenDay));

    let third = &lanes[1];
    assert_eq!(third.name, "third-party");
    let five_h = third
        .windows
        .iter()
        .find(|w| w.kind == WindowKind::FiveHour)
        .expect("5h");
    assert!((five_h.used_pct - 100.0).abs() < f64::EPSILON);
}

#[test]
fn antigravity_empty_lane_does_not_empty_slot() {
    let json = fixture("tests/fixtures/antigravity/quota_summary.json");
    let lanes = parse_quota_summary(&json).expect("parse");
    let best_remaining = lanes
        .iter()
        .flat_map(|l| l.windows.iter().map(|w| w.remaining_pct()))
        .fold(0.0, f64::max);
    assert!(best_remaining > 0.0);
    assert_eq!(
        lanes[1]
            .windows
            .iter()
            .find(|w| w.kind == WindowKind::FiveHour)
            .expect("5h")
            .remaining_pct(),
        0.0
    );
}

#[test]
fn antigravity_lsof_port_order() {
    let lsof = fixture("tests/fixtures/antigravity/lsof_pcn.txt");
    let ports = parse_lsof_ports(&lsof);
    assert_eq!(ports, vec![54321, 60001, 54320, 60000]);
}

#[test]
fn antigravity_csrf_from_html() {
    let html = fixture("tests/fixtures/antigravity/index.html");
    assert_eq!(
        parse_csrf_token(&html).as_deref(),
        Some("test-csrf-value-abc")
    );
}

#[test]
fn antigravity_ls_address_override_prepended() {
    let _guard = env_lock().lock().expect("lock");
    unsafe { std::env::set_var("ANTIGRAVITY_LS_ADDRESS", "127.0.0.1:4242") };
    let lsof = fixture("tests/fixtures/antigravity/lsof_pcn.txt");
    let ports = parse_lsof_ports(&lsof);
    let mut bases: Vec<String> = vec!["http://127.0.0.1:4242".into()];
    for port in ports {
        let base = format!("http://127.0.0.1:{port}");
        if !bases.contains(&base) {
            bases.push(base);
        }
    }
    let discovered = discover_ls_bases();
    assert_eq!(
        discovered.first().map(String::as_str),
        Some("http://127.0.0.1:4242")
    );
    assert!(discovered.len() >= bases.len());
    unsafe { std::env::remove_var("ANTIGRAVITY_LS_ADDRESS") };
}
