use aihub_core::HarnessId;
use aihub_router::{lane_to_model_id_from_list, parse_cli_model_list};

#[test]
fn lane_to_model_id_prefers_named_own_model_over_auto() {
    let fixture = "\
* auto
  composer-1.5
  claude-4-sonnet
";
    assert_eq!(
        lane_to_model_id_from_list(HarnessId::CursorAgent, "cursor-models", fixture),
        Some("composer-1.5".into())
    );
    assert_eq!(
        lane_to_model_id_from_list(HarnessId::CursorAgent, "other-models", fixture),
        Some("claude-4-sonnet".into())
    );
}

#[test]
fn agy_lane_mapping_uses_fixture_list() {
    let fixture = "gemini-2.5-pro\ngpt-5.2\n";
    assert_eq!(
        lane_to_model_id_from_list(HarnessId::Antigravity, "gemini", fixture),
        Some("gemini-2.5-pro".into())
    );
    assert_eq!(
        lane_to_model_id_from_list(HarnessId::Antigravity, "third-party", fixture),
        Some("gpt-5.2".into())
    );
}

#[test]
fn parse_cli_model_list_skips_bullets_and_blank_lines() {
    let ids = parse_cli_model_list("- sonnet\n\n* auto\n");
    assert_eq!(ids, vec!["sonnet", "auto"]);
}
