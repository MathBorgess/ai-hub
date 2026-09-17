use aihub_core::*;
use aihub_router::route;

fn window(used: f64, reset: Option<u64>, duration: Option<u64>) -> QuotaWindow {
    QuotaWindow::new(WindowKind::FiveHour, used, reset, duration)
}
fn slot(harness: HarnessId, windows: Vec<QuotaWindow>) -> QuotaSnapshot {
    QuotaSnapshot {
        slot: SlotId::default_for(harness),
        status: QuotaStatus::Ok,
        source: QuotaSource::Vendor,
        estimated: false,
        note: None,
        windows,
        lanes: vec![],
    }
}

#[test]
fn fullest_supply_wins_without_lanes() {
    let pool = [
        slot(HarnessId::ClaudeCode, vec![window(70., None, None)]),
        slot(HarnessId::Codex, vec![window(20., None, None)]),
    ];
    assert_eq!(
        route(TaskTier::Design, TaskSize::M, &pool, 7200)
            .unwrap()
            .harness,
        HarnessId::Codex
    );
}

fn lane(name: &str, kind: LaneKind, used: f64) -> QuotaLane {
    QuotaLane {
        name: name.into(),
        kind,
        windows: vec![window(used, None, None)],
    }
}

#[test]
fn cursor_exhausted_other_models_never_receives_design() {
    let mut cursor = slot(HarnessId::CursorAgent, vec![window(45., None, None)]);
    cursor.lanes = vec![
        lane("cursor-models", LaneKind::Own, 0.),
        lane("other-models", LaneKind::Frontier, 100.),
    ];
    let claude = slot(HarnessId::ClaudeCode, vec![window(40., None, None)]);
    let result = route(TaskTier::Design, TaskSize::M, &[cursor, claude], 7200).unwrap();
    assert_eq!(result.harness, HarnessId::ClaudeCode);
    assert_eq!(result.lane, None);
}

#[test]
fn codex_full_five_hour_window_is_held_twenty_minutes() {
    let mut codex = slot(
        HarnessId::Codex,
        vec![
            window(100., Some(1200), Some(18000)),
            window(20., Some(200000), Some(604800)),
        ],
    );
    codex.status = QuotaStatus::Empty;
    let result = route(TaskTier::Design, TaskSize::L, &[codex], 7200).unwrap();
    assert_eq!(result.harness, HarnessId::Codex);
    assert_eq!(result.holds_until_s, Some(1200));
    assert!(result.reason.contains("supply 80.0%"));
}

#[test]
fn all_empty_or_no_snapshots_is_a_no_launch_recommendation() {
    let mut empty = slot(
        HarnessId::Codex,
        vec![window(100., Some(8000), Some(18000))],
    );
    empty.status = QuotaStatus::Empty;
    for pool in [vec![empty], vec![]] {
        let result = route(TaskTier::Review, TaskSize::S, &pool, 7200).unwrap();
        assert!(result.reason.starts_with("No available slots:"));
        assert!(result.reason.contains("do not launch"));
        assert_eq!(result.holds_until_s, None);
    }
}

#[test]
fn preferred_lane_and_threefold_penalty_match_script() {
    let mut agy = slot(HarnessId::Antigravity, vec![]);
    agy.lanes = vec![
        lane("gemini", LaneKind::Own, 0.),
        lane("third-party", LaneKind::Frontier, 50.),
    ];
    for tier in [TaskTier::Design, TaskTier::Review] {
        assert_eq!(
            route(tier, TaskSize::M, &[agy.clone()], 7200)
                .unwrap()
                .lane
                .as_deref(),
            Some("third-party")
        );
    }
    assert_eq!(
        route(TaskTier::Mechanical, TaskSize::S, &[agy.clone()], 7200)
            .unwrap()
            .lane
            .as_deref(),
        Some("gemini")
    );
    agy.lanes[1].windows[0].used_pct = 80.;
    let result = route(TaskTier::Design, TaskSize::L, &[agy], 7200).unwrap();
    assert_eq!(result.lane.as_deref(), Some("gemini"));
    assert!(result.reason.contains("nonpreferred"));
}

#[test]
fn windows_take_minimum_after_refills_not_before() {
    let codex = slot(
        HarnessId::Codex,
        vec![
            window(85., Some(600), Some(18000)),
            window(70., Some(200000), Some(604800)),
        ],
    );
    let claude = slot(HarnessId::ClaudeCode, vec![window(75., None, None)]);
    let result = route(
        TaskTier::Design,
        TaskSize::M,
        &[codex.clone(), claude.clone()],
        7200,
    )
    .unwrap();
    assert_eq!(result.harness, HarnessId::Codex);
    assert_eq!(result.holds_until_s, Some(600));
    assert_eq!(
        route(TaskTier::Design, TaskSize::M, &[codex, claude], 300)
            .unwrap()
            .harness,
        HarnessId::ClaudeCode
    );
}

#[test]
fn a_nonrefilling_exhausted_weekly_gate_blocks_even_with_five_hour_reset() {
    let codex = slot(
        HarnessId::Codex,
        vec![
            window(100., Some(600), Some(18000)),
            window(100., Some(200000), Some(604800)),
        ],
    );
    assert!(route(TaskTier::Review, TaskSize::M, &[codex], 7200)
        .unwrap()
        .reason
        .starts_with("No available slots:"));
}

#[test]
fn latest_blocked_reset_and_multiple_refills_match_script() {
    let codex = slot(
        HarnessId::Codex,
        vec![
            window(95., Some(600), Some(18000)),
            window(90., Some(1200), Some(604800)),
        ],
    );
    let result = route(TaskTier::Design, TaskSize::M, &[codex], 7200).unwrap();
    assert_eq!(result.holds_until_s, Some(1200));
    assert!(result.reason.contains("supply 105.0%"));
    let fast = slot(HarnessId::Codex, vec![window(100., Some(0), Some(3600))]);
    let result = route(TaskTier::Mechanical, TaskSize::S, &[fast], 7200).unwrap();
    assert!(result.reason.contains("supply 300.0%"));
}

#[test]
fn independent_lane_windows_replace_aggregate_and_can_hold() {
    let mut agy = slot(HarnessId::Antigravity, vec![window(100., None, None)]);
    agy.status = QuotaStatus::Empty;
    agy.lanes = vec![QuotaLane {
        name: "third-party".into(),
        kind: LaneKind::Frontier,
        windows: vec![
            window(100., Some(1200), Some(18000)),
            window(40., None, None),
        ],
    }];
    let result = route(TaskTier::Design, TaskSize::M, &[agy], 7200).unwrap();
    assert_eq!(result.lane.as_deref(), Some("third-party"));
    assert_eq!(result.holds_until_s, Some(1200));
    assert!(result.reason.contains("supply 60.0%"));
}

#[test]
fn low_slots_are_last_resort_unknown_slots_have_neutral_supply() {
    let mut low = slot(HarnessId::Codex, vec![window(90., None, None)]);
    low.status = QuotaStatus::Low;
    let mut unknown = slot(HarnessId::ClaudeCode, vec![]);
    unknown.status = QuotaStatus::Unknown;
    assert_eq!(
        route(
            TaskTier::Mechanical,
            TaskSize::M,
            &[low.clone(), unknown],
            7200
        )
        .unwrap()
        .harness,
        HarnessId::ClaudeCode
    );
    let result = route(TaskTier::Mechanical, TaskSize::L, &[low], 7200).unwrap();
    assert_eq!(result.harness, HarnessId::Codex);
    assert!(result.reason.contains("demand exceeds"));
}

#[test]
fn missing_or_zero_window_duration_does_not_invent_refill() {
    for duration in [None, Some(0)] {
        let empty = slot(HarnessId::Codex, vec![window(100., Some(1200), duration)]);
        assert!(route(TaskTier::Design, TaskSize::M, &[empty], 7200)
            .unwrap()
            .reason
            .starts_with("No available slots:"));
    }
}

#[test]
fn equal_scores_preserve_input_order() {
    let pool = [
        slot(HarnessId::Codex, vec![window(50., None, None)]),
        slot(HarnessId::ClaudeCode, vec![window(50., None, None)]),
    ];
    assert_eq!(
        route(TaskTier::Review, TaskSize::M, &pool, 7200)
            .unwrap()
            .harness,
        HarnessId::Codex
    );
}
