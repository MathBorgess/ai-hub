use aihub_core::*;
use aihub_router::{route_outcome, CatalogError, ModelCatalog};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};

fn executable(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("list");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn n6_hanging_list_command_times_out_and_reaps_child() {
    let dir = tempfile::tempdir().unwrap();
    let path = executable(&dir, "echo $$ > \"$0.pid\"\nexec /bin/sleep 86400");
    let catalog = ModelCatalog::default();
    let start = Instant::now();
    let result = catalog.refresh(HarnessId::CursorAgent, &path).await;
    assert!(matches!(result, Err(CatalogError::Timeout)));
    assert!(start.elapsed() >= Duration::from_secs(10));
    assert!(start.elapsed() < Duration::from_secs(12));
    let pid: i32 = fs::read_to_string(path.with_extension("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[tokio::test]
async fn n6_oversized_output_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let path = executable(&dir, "exec /usr/bin/yes composer");
    let start = Instant::now();
    assert!(matches!(
        ModelCatalog::default()
            .refresh(HarnessId::CursorAgent, &path)
            .await,
        Err(CatalogError::OutputLimit)
    ));
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn n6_failed_refresh_keeps_last_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = executable(
        &dir,
        "[ \"$1\" = --list-models ] || exit 2\nprintf 'composer\nclaude\n'",
    );
    let catalog = ModelCatalog::default()
        .refresh(HarnessId::CursorAgent, &path)
        .await
        .unwrap();
    executable(&dir, "exit 1");
    assert!(catalog
        .refresh(HarnessId::CursorAgent, &path)
        .await
        .is_err());
    assert_eq!(
        catalog
            .model_for_lane(HarnessId::CursorAgent, Some("cursor-models"))
            .as_deref(),
        Some("composer")
    );
}

#[tokio::test]
async fn n6_route_uses_snapshot_without_io() {
    let dir = tempfile::tempdir().unwrap();
    let path = executable(
        &dir,
        "[ \"$1\" = models ] || exit 2\nprintf 'gemini-test\nclaude-test\n'",
    );
    let catalog = ModelCatalog::default()
        .refresh(HarnessId::Antigravity, &path)
        .await
        .unwrap();
    fs::remove_file(path).unwrap();
    let quota = QuotaSnapshot {
        slot: SlotId::default_for(HarnessId::Antigravity),
        status: QuotaStatus::Ok,
        source: QuotaSource::Vendor,
        estimated: false,
        note: None,
        windows: vec![],
        lanes: vec![QuotaLane {
            name: "gemini".into(),
            kind: LaneKind::Own,
            windows: vec![],
        }],
    };
    let outcome =
        route_outcome(TaskTier::Mechanical, TaskSize::S, &[quota], 7200, &catalog).unwrap();
    assert!(
        matches!(outcome, RouteOutcome::Recommendation { model: Some(ref model), .. } if model == "gemini-test")
    );
}
