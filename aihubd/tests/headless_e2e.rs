use aihub_core::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TEMP_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn make_temp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aihub-{}-{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        TEMP_DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn init_git_repo(repo_dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(repo_dir)
            .status()
            .expect("git execution");
        assert!(status.success(), "git command failed: {:?}", args);
    };

    run(&["init"]);
    run(&["config", "user.name", "AIHub Headless Tester"]);
    run(&["config", "user.email", "tester@aihub.local"]);
    run(&["config", "commit.gpgsign", "false"]);

    std::fs::write(repo_dir.join("README.md"), "# Temp Test Repo\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "Initial commit"]);
}

async fn send_msg(s: &mut UnixStream, msg: ClientMessage) {
    s.write_all(&encode_frame(&msg.into()).unwrap())
        .await
        .unwrap();
}

async fn recv_msg_with_timeout(s: &mut UnixStream, timeout: Duration) -> DaemonMessage {
    tokio::time::timeout(timeout, async {
        let n = s.read_u32().await.unwrap();
        let mut data = vec![0; n as usize];
        s.read_exact(&mut data).await.unwrap();
        match serde_json::from_slice::<IpcMessage>(&data).unwrap() {
            IpcMessage::Daemon(m) => m,
            _ => panic!("unexpected message direction"),
        }
    })
    .await
    .expect("recv timeout")
}

async fn recv_msg(s: &mut UnixStream) -> DaemonMessage {
    recv_msg_with_timeout(s, Duration::from_secs(5)).await
}

async fn recv_non_quota(s: &mut UnixStream) -> DaemonMessage {
    loop {
        let msg = recv_msg(s).await;
        if !matches!(msg, DaemonMessage::QuotaPush { .. }) {
            return msg;
        }
    }
}

async fn recv_merge_result(s: &mut UnixStream) -> DaemonMessage {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let msg = recv_msg_with_timeout(s, Duration::from_secs(30)).await;
            if matches!(msg, DaemonMessage::MergeResult { .. }) {
                return msg;
            }
        }
    })
    .await
    .expect("timeout waiting for MergeResult")
}

#[tokio::test]
async fn test_headless_end_to_end() {
    let test_dir = make_temp_dir("e2e-main");
    let repo_dir = test_dir.join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);

    let sock_dir = PathBuf::from(format!("/tmp/ah10-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sock_dir);
    std::fs::create_dir_all(&sock_dir).unwrap();
    let sock_path = sock_dir.join("s");

    // 1. Start `aihubd --socket <temp path>`
    let mut daemon_proc = tokio::process::Command::new(env!("CARGO_BIN_EXE_aihubd"))
        .arg("--socket")
        .arg(&sock_path)
        .env("AIHUB_PTY_COMMAND", "/bin/sh")
        .env("AIHUB_NO_PROBES", "1")
        .spawn()
        .expect("spawn aihubd");

    // Wait for socket to appear and accept connections
    let mut connected_stream = None;
    for _ in 0..100 {
        if sock_path.exists() {
            if let Ok(stream) = UnixStream::connect(&sock_path).await {
                connected_stream = Some(stream);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut client = connected_stream.expect("failed to connect to aihubd socket");

    // Confirm socket exists with private permissions
    assert!(sock_path.exists());
    let sock_meta = std::fs::symlink_metadata(&sock_path).expect("sock metadata");
    let dir_meta = std::fs::symlink_metadata(&sock_dir).expect("sock dir metadata");
    let dir_mode = dir_meta.permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "socket directory should have 0o700 permissions"
    );
    let sock_mode = sock_meta.permissions().mode() & 0o777;
    assert_eq!(
        sock_mode, 0o600,
        "socket file should have 0o600 private permissions"
    );

    // Handshake
    send_msg(
        &mut client,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            credential: None,
        },
    )
    .await;
    let hello = recv_msg(&mut client).await;
    assert_eq!(
        hello,
        DaemonMessage::Hello {
            version: PROTOCOL_VERSION
        }
    );
    let quota = recv_msg(&mut client).await;
    assert!(matches!(quota, DaemonMessage::QuotaPush { .. }));

    // 2. Connect client and open a session whose command is `/bin/sh`
    send_msg(
        &mut client,
        ClientMessage::NewSession {
            harness: HarnessId::ClaudeCode,
            repo_path: repo_dir.clone(),
            initial_prompt: None,
        },
    )
    .await;

    let (session_id, worktree_path) = match recv_non_quota(&mut client).await {
        DaemonMessage::SessionCreated {
            session_id,
            harness: _,
            worktree_path,
            branch,
            ..
        } => {
            assert!(worktree_path.exists(), "worktree must exist on disk");
            assert!(branch.starts_with("session/"));
            (session_id, worktree_path)
        }
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Attach client to session
    send_msg(
        &mut client,
        ClientMessage::Attach {
            target: SessionTarget::Id(session_id.clone()),
            last_seen_offset: None,
        },
    )
    .await;

    match recv_non_quota(&mut client).await {
        DaemonMessage::Attached {
            session_id: attached_id,
            ..
        } => {
            assert_eq!(attached_id, session_id);
        }
        other => panic!("expected Attached, got {:?}", other),
    }

    // 3. Send input, detach, reattach, and confirm the scrollback replays
    let marker = "HELLO_AIHUB_HEADLESS_E2E";
    let cmd = format!("echo '{}'\n", marker);
    send_msg(
        &mut client,
        ClientMessage::PtyInput {
            session_id: session_id.clone(),
            data: Base64Bytes::new(cmd.into_bytes()),
        },
    )
    .await;

    // Collect output until marker arrives
    let mut found_marker = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(300), recv_msg(&mut client)).await {
            Ok(DaemonMessage::PtyOutput { data, .. }) => {
                let text = String::from_utf8_lossy(data.as_slice());
                if text.contains(marker) {
                    found_marker = true;
                    break;
                }
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(found_marker, "marker was not found in PTY output stream");

    // Detach
    send_msg(
        &mut client,
        ClientMessage::Detach {
            session_id: session_id.clone(),
        },
    )
    .await;
    match recv_non_quota(&mut client).await {
        DaemonMessage::Detached {
            session_id: detached_id,
        } => {
            assert_eq!(detached_id, session_id);
        }
        other => panic!("expected Detached, got {:?}", other),
    }

    // Reattach and confirm scrollback replays
    send_msg(
        &mut client,
        ClientMessage::Attach {
            target: SessionTarget::Id(session_id.clone()),
            last_seen_offset: None,
        },
    )
    .await;

    match recv_non_quota(&mut client).await {
        DaemonMessage::Attached {
            session_id: reattached_id,
            scrollback,
            ..
        } => {
            assert_eq!(reattached_id, session_id);
            let scrollback_str = String::from_utf8_lossy(scrollback.as_slice());
            assert!(
                scrollback_str.contains(marker),
                "reattached scrollback must contain the marker, got: {:?}",
                scrollback_str
            );
        }
        other => panic!(
            "expected Attached with replayed scrollback, got {:?}",
            other
        ),
    }

    // 4. Exercise worktree create → change → squash merge in a temp repo, through the daemon's merge flow
    let feature_file = worktree_path.join("new_feature.txt");
    std::fs::write(&feature_file, "verified end-to-end squash merge\n").unwrap();

    // First MergeRequest: returns preview diff with success: false
    send_msg(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: session_id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;

    match recv_merge_result(&mut client).await {
        DaemonMessage::MergeResult {
            session_id: merge_id,
            success,
            diff,
            message,
        } => {
            assert_eq!(merge_id, session_id);
            assert!(
                !success,
                "first merge request must be a preview requiring confirmation"
            );
            assert!(
                diff.contains("new_feature.txt") || diff.contains("verified end-to-end"),
                "diff must show new feature change: {}",
                diff
            );
            assert!(
                message.contains("Review diff"),
                "message must instruct to review diff: {}",
                message
            );
        }
        other => panic!("expected MergeResult preview, got {:?}", other),
    }

    // Repeating the same strategy confirms the merge
    send_msg(
        &mut client,
        ClientMessage::MergeRequest {
            session_id: session_id.clone(),
            strategy: MergeStrategy::Squash,
        },
    )
    .await;

    match recv_merge_result(&mut client).await {
        DaemonMessage::MergeResult {
            session_id: merge_id,
            success,
            diff: _,
            message: _,
        } => {
            assert_eq!(merge_id, session_id);
            assert!(success, "confirmed merge must succeed");
        }
        other => panic!("expected MergeResult success, got {:?}", other),
    }

    // Verify squash merge reflected in main repository
    let merged_file_in_main = repo_dir.join("new_feature.txt");
    assert!(
        merged_file_in_main.exists(),
        "squashed feature file must exist in main checkout"
    );
    assert_eq!(
        std::fs::read_to_string(&merged_file_in_main).unwrap(),
        "verified end-to-end squash merge\n"
    );

    // Terminate daemon and verify socket cleanup
    let _ = daemon_proc.kill().await;
    let _ = daemon_proc.wait().await;

    // Clean up test directories
    let _ = std::fs::remove_dir_all(&test_dir);
    let _ = std::fs::remove_dir_all(&sock_dir);
}

// Real PTYs, git worktrees, router and daemon; only quota data and shell workload
// are fixtures. No provider executable, credentials, or default memory port.
struct RealSystem {
    root: PathBuf,
    socket_dir: PathBuf,
    socket: PathBuf,
    daemon: aihubd::Daemon,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    launches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RealSystem {
    async fn start() -> Self {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let root = make_temp_dir("cross-crate");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_git_repo(&repo);
        std::fs::write(repo.join(".gitignore"), ".scratch/\n").unwrap();
        assert!(StdCommand::new("git")
            .current_dir(&repo)
            .args(["add", ".gitignore"])
            .status()
            .unwrap()
            .success());
        assert!(StdCommand::new("git")
            .current_dir(&repo)
            .args(["commit", "-m", "Ignore scratch"])
            .status()
            .unwrap()
            .success());
        let script = root.join("writer.sh");
        std::fs::write(&script, r#"#!/bin/sh
mkdir -p .scratch
file=.scratch/writes-$$
trap 'i=0; while [ "$i" -lt 5 ]; do echo stopping >> "$file"; i=$((i+1)); sleep 0.04; done; exit 0' TERM HUP
while :; do echo running >> "$file"; sleep 0.02; done
"#).unwrap();
        let launches = Arc::new(AtomicUsize::new(0));
        let count = launches.clone();
        let daemon = aihubd::Daemon::new(
            || async {
                HarnessId::all()
                    .iter()
                    .map(|h| QuotaSnapshot {
                        slot: SlotId::default_for(*h),
                        status: QuotaStatus::Empty,
                        source: QuotaSource::Vendor,
                        estimated: false,
                        note: None,
                        windows: vec![QuotaWindow::new(
                            WindowKind::FiveHour,
                            100.,
                            Some(3600),
                            Some(18000),
                        )],
                        lanes: vec![],
                    })
                    .collect()
            },
            move |_, mut opts, _model| {
                // The actual PTY adapter; no fake lifecycle methods.
                opts.env
                    .insert("HOME".into(), opts.cwd.display().to_string());
                let h = Arc::new(aihub_pty::spawn_command(
                    "/bin/sh",
                    &[script.to_str().unwrap()],
                    opts,
                )?);
                count.fetch_add(1, Ordering::SeqCst);
                let writer = h.clone();
                let resize = h.clone();
                let wait = h.clone();
                let kill = h.clone();
                let stop = h.clone();
                Ok(aihubd::Pty {
                    output: h.subscribe_output(),
                    scrollback: h.scrollback_snapshot(),
                    write: Box::new(move |data| {
                        let h = writer.clone();
                        Box::pin(async move { h.try_write(&data).map_err(Into::into) })
                    }),
                    resize: Box::new(move |size| resize.resize(size).map_err(Into::into)),
                    wait: Box::new(move || {
                        let h = wait.clone();
                        Box::pin(async move { h.wait().await.map_err(Into::into) })
                    }),
                    kill: Box::new(move || {
                        let h = kill.clone();
                        Box::pin(async move { h.kill().await.map_err(Into::into) })
                    }),
                    stop: std::sync::Arc::new(move |timeout| {
                        let h = stop.clone();
                        Box::pin(async move { h.stop_barrier(timeout).await.map_err(Into::into) })
                    }),
                    try_write: std::sync::Arc::new(move |data| {
                        h.try_write(&data).map_err(Into::into)
                    }),
                })
            },
        );
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let socket_dir = PathBuf::from(format!(
            "/tmp/ah10-real-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket = socket_dir.join("s");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let d = daemon.clone();
        let path = socket.clone();
        let task = tokio::spawn(async move {
            d.run(path, async {
                let _ = rx.await;
            })
            .await
        });
        Self {
            root,
            socket_dir,
            socket,
            daemon,
            shutdown: Some(tx),
            task: Some(task),
            launches,
        }
    }

    async fn session(&self, name: &str) -> (SessionId, PathBuf, UnixStream) {
        let id = SessionId::new(name);
        let repo = self.root.join("repo");
        let wt = aihub_git::create_session_worktree(&repo, &id, Some(&self.root.join("worktrees")))
            .await
            .unwrap();
        let path = wt.path.clone();
        self.daemon
            .add_session(repo, wt, HarnessId::Codex, None)
            .await
            .unwrap();
        let mut client = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(s) = UnixStream::connect(&self.socket).await {
                    break s;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        send_msg(
            &mut client,
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                credential: None,
            },
        )
        .await;
        assert!(matches!(
            recv_msg(&mut client).await,
            DaemonMessage::Hello { .. }
        ));
        assert!(matches!(
            recv_msg(&mut client).await,
            DaemonMessage::QuotaPush { .. }
        ));
        send_msg(
            &mut client,
            ClientMessage::Attach {
                target: SessionTarget::Id(id.clone()),
                last_seen_offset: None,
            },
        )
        .await;
        loop {
            if matches!(recv_msg(&mut client).await, DaemonMessage::Attached { .. }) {
                break;
            }
        }
        wait_for_writes(&path, 1).await;
        (id, path, client)
    }

    async fn stop(mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        tokio::time::timeout(Duration::from_secs(10), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
impl Drop for RealSystem {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.socket_dir);
    }
}
fn write_files(path: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(path.join(".scratch"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("writes-")
        })
        .collect()
}
async fn wait_for_writes(path: &std::path::Path, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if write_files(path)
                .iter()
                .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 0))
                .count()
                >= count
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn wait_for_stable_files(files: &[PathBuf], stable_for: Duration, deadline: Duration) {
    tokio::time::timeout(deadline, async {
        let mut last: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let mut stable_since = std::time::Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let current: Vec<Vec<u8>> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
            if current == last {
                if stable_since.elapsed() >= stable_for {
                    return;
                }
            } else {
                last = current;
                stable_since = std::time::Instant::now();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("files never stabilized for {:?}", stable_for));
}

async fn assert_quiescent(files: &[PathBuf]) {
    let before: Vec<_> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert!(
        before
            .iter()
            .all(|b| String::from_utf8_lossy(b).contains("stopping")),
        "SIGTERM handler must actually execute"
    );
    wait_for_stable_files(files, Duration::from_millis(350), Duration::from_secs(5)).await;
    let after: Vec<_> = files.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(
        before, after,
        "outgoing harness wrote after the stop barrier returned"
    );
}

async fn wait_for_file_growth(path: &std::path::Path, min_exclusive: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::metadata(path).is_ok_and(|m| m.len() > min_exclusive) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "file {} never grew past {} bytes",
            path.display(),
            min_exclusive
        )
    });
}

#[tokio::test]
async fn f5_real_switch_waits_for_sigterm_writes() {
    let system = RealSystem::start().await;
    let (id, path, mut client) = system.session("f5-real").await;
    let outgoing = write_files(&path);
    send_msg(
        &mut client,
        ClientMessage::SwitchHarness {
            session_id: id,
            target: HarnessId::ClaudeCode,
            with_handoff: false,
            model: None,
        },
    )
    .await;
    loop {
        if matches!(
            recv_msg(&mut client).await,
            DaemonMessage::HarnessSwitched { .. }
        ) {
            break;
        }
    }
    wait_for_writes(&path, 2).await;
    assert_quiescent(&outgoing).await;
    assert_eq!(system.launches.load(std::sync::atomic::Ordering::SeqCst), 2);
    system.stop().await;
}

#[tokio::test]
async fn f2_f4_real_keep_and_merge_preserve_ignored_files_after_stop() {
    for strategy in [
        MergeStrategy::Keep,
        MergeStrategy::Squash,
        MergeStrategy::FastForward,
    ] {
        let system = RealSystem::start().await;
        let (id, path, mut client) = system.session("f2-f4-real").await;
        std::fs::write(path.join("README.md"), "tracked change\n").unwrap();
        std::fs::write(path.join(".scratch/owner-data"), b"keep this ignored work").unwrap();
        let files = write_files(&path);
        for expected_success in [false, true] {
            send_msg(
                &mut client,
                ClientMessage::MergeRequest {
                    session_id: id.clone(),
                    strategy,
                },
            )
            .await;
            match recv_merge_result(&mut client).await {
                DaemonMessage::MergeResult {
                    success, message, ..
                } => assert_eq!(success, expected_success, "{message}"),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            std::fs::read(path.join(".scratch/owner-data")).unwrap(),
            b"keep this ignored work"
        );
        assert_quiescent(&files).await;
        if strategy != MergeStrategy::Keep {
            assert_eq!(
                std::fs::read_to_string(system.root.join("repo/README.md")).unwrap(),
                "tracked change\n"
            );
        }
        system.stop().await;
    }
}

#[tokio::test]
async fn f10_real_router_exhausted_autonomous_dispatches_nothing() {
    let system = RealSystem::start().await;
    let (id, path, mut client) = system.session("f10-real").await;
    send_msg(
        &mut client,
        ClientMessage::SetMode {
            session_id: id.clone(),
            mode: Mode::Autonomous,
        },
    )
    .await;
    loop {
        if matches!(recv_msg(&mut client).await, DaemonMessage::ModeSet { .. }) {
            break;
        }
    }
    send_msg(&mut client, ClientMessage::RequestQuota).await;
    loop {
        if let DaemonMessage::QuotaPush { snapshots } = recv_msg(&mut client).await {
            assert_eq!(snapshots.len(), 4);
            assert!(snapshots.iter().all(|s| s.status == QuotaStatus::Empty));
            break;
        }
    }
    let before = std::fs::metadata(&write_files(&path)[0]).unwrap().len();
    send_msg(
        &mut client,
        ClientMessage::SubmitTask {
            session_id: id,
            task: "fix typo".into(),
        },
    )
    .await;
    loop {
        if let DaemonMessage::RouteRecommendation { outcome, .. } = recv_msg(&mut client).await {
            assert!(matches!(outcome, RouteOutcome::NoCapacity { .. }));
            break;
        }
    }
    assert_eq!(system.launches.load(std::sync::atomic::Ordering::SeqCst), 1);
    wait_for_file_growth(&write_files(&path)[0], before).await;
    system.stop().await;
}

#[tokio::test]
async fn f13_real_clients_receive_only_attached_session_events() {
    let system = RealSystem::start().await;
    let (id_a, _, mut a) = system.session("f13-a").await;
    let (id_b, _, mut b) = system.session("f13-b").await;
    for (id, client) in [(id_a.clone(), &mut a), (id_b.clone(), &mut b)] {
        send_msg(
            client,
            ClientMessage::SetMode {
                session_id: id.clone(),
                mode: Mode::Autonomous,
            },
        )
        .await;
        loop {
            match recv_msg(client).await {
                DaemonMessage::ModeSet { session_id, .. } => {
                    assert_eq!(session_id, id);
                    break;
                }
                DaemonMessage::QuotaPush { .. }
                | DaemonMessage::SessionCreated { .. }
                | DaemonMessage::PtyOutput { .. } => (),
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    send_msg(
        &mut b,
        ClientMessage::SubmitTask {
            session_id: id_b.clone(),
            task: "fix typo".into(),
        },
    )
    .await;
    loop {
        if let DaemonMessage::RouteRecommendation { session_id, .. } = recv_msg(&mut b).await {
            assert_eq!(session_id, id_b);
            break;
        }
    }
    send_msg(
        &mut b,
        ClientMessage::SwitchHarness {
            session_id: id_b,
            target: HarnessId::ClaudeCode,
            with_handoff: false,
            model: None,
        },
    )
    .await;
    loop {
        if matches!(
            recv_msg(&mut b).await,
            DaemonMessage::HarnessSwitched { .. }
        ) {
            break;
        }
    }
    // A round trip to A is a deterministic fence after B's scoped events.
    send_msg(&mut a, ClientMessage::ListSessions).await;
    loop {
        match recv_msg(&mut a).await {
            DaemonMessage::SessionList { sessions } => {
                assert_eq!(sessions.len(), 2);
                break;
            }
            DaemonMessage::QuotaPush { .. } | DaemonMessage::SessionCreated { .. } => (),
            DaemonMessage::PtyOutput { session_id, .. } => assert_eq!(session_id, id_a),
            other => panic!("session B leaked to A: {other:?}"),
        }
    }
    system.stop().await;
}
