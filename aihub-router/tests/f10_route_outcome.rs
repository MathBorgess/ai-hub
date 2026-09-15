use aihub_core::*;
use aihub_router::route_outcome;

fn window(used: f64, reset: Option<u64>, duration: Option<u64>) -> QuotaWindow {
    QuotaWindow::new(WindowKind::FiveHour, used, reset, duration)
}

fn slot(harness: HarnessId, status: QuotaStatus, windows: Vec<QuotaWindow>) -> QuotaSnapshot {
    QuotaSnapshot {
        slot: SlotId::default_for(harness),
        status,
        source: QuotaSource::Vendor,
        estimated: false,
        note: None,
        windows,
        lanes: vec![],
    }
}

#[test]
fn f10_exhausted_supply_returns_no_capacity_not_a_placeholder_harness() {
    let exhausted = slot(
        HarnessId::Codex,
        QuotaStatus::Ok,
        vec![window(100., Some(8000), Some(18000))],
    );
    let outcome = route_outcome(TaskTier::Review, TaskSize::S, &[exhausted], 7200).unwrap();
    match outcome {
        RouteOutcome::NoCapacity { reason } => {
            assert!(reason.contains("No available slots:"));
            assert!(reason.contains("do not launch"));
        }
        RouteOutcome::Recommendation { .. } => panic!("expected NoCapacity"),
    }
}

#[test]
fn f10_unknown_slots_are_never_candidates() {
    let mut unknown = slot(HarnessId::ClaudeCode, QuotaStatus::Unknown, vec![]);
    unknown.windows = vec![window(10., None, None)];
    let outcome = route_outcome(TaskTier::Mechanical, TaskSize::M, &[unknown], 7200).unwrap();
    assert!(matches!(outcome, RouteOutcome::NoCapacity { .. }));
}
