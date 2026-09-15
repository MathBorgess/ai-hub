use std::collections::HashMap;
use std::env;
use std::time::Duration;

use aihub_core::HarnessId;
use aihub_pty::{harness_recipe, spawn_command, PtySize, PtySpawnOptions};
use tokio::time::{sleep, timeout};

fn test_opts() -> PtySpawnOptions {
    PtySpawnOptions {
        cwd: env::current_dir().expect("cwd"),
        env: HashMap::new(),
        size: PtySize::default(),
        initial_prompt: None,
    }
}

async fn wait_for_scrollback_contains(handle: &aihub_pty::PtyHandle, needle: &[u8]) {
    for _ in 0..100 {
        if handle.scrollback_snapshot().windows(needle.len()).any(|w| w == needle) {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "scrollback missing {:?}, got {:?}",
        needle,
        handle.scrollback_snapshot()
    );
}

#[tokio::test]
async fn echo_output_in_scrollback() {
    let handle = spawn_command("/bin/sh", &["-c", "echo pty-echo-marker"], test_opts())
        .expect("spawn");
    wait_for_scrollback_contains(&handle, b"pty-echo-marker").await;
    let code = handle.wait().await.expect("wait");
    assert_eq!(code, Some(0));
}

#[tokio::test]
async fn late_subscriber_gets_scrollback_then_live() {
    let handle = spawn_command(
        "/bin/sh",
        &["-c", "echo first-line; sleep 0.3; echo second-line"],
        test_opts(),
    )
    .expect("spawn");

    wait_for_scrollback_contains(&handle, b"first-line").await;

    let replay = handle.scrollback_snapshot();
    assert!(
        replay.windows(b"first-line".len()).any(|w| w == b"first-line"),
        "replay should include first line"
    );

    let mut live = handle.subscribe_output();
    wait_for_scrollback_contains(&handle, b"second-line").await;

    let got_live = timeout(Duration::from_secs(2), async {
        loop {
            let chunk = live.recv().await.expect("broadcast");
            if chunk.windows(b"second-line".len()).any(|w| w == b"second-line") {
                return;
            }
        }
    })
    .await;
    assert!(got_live.is_ok(), "expected live second-line on subscribe");

    let code = handle.wait().await.expect("wait");
    assert_eq!(code, Some(0));
}

#[tokio::test]
async fn write_to_pty_stdin() {
    let handle = spawn_command("/bin/sh", &["-c", "read x; echo got:$x"], test_opts())
        .expect("spawn");
    sleep(Duration::from_millis(50)).await;
    handle.write(b"hello-input\n").await.expect("write");
    wait_for_scrollback_contains(&handle, b"got:hello-input").await;
    let code = handle.wait().await.expect("wait");
    assert_eq!(code, Some(0));
}

#[tokio::test]
async fn resize_while_running() {
    let handle = spawn_command("/bin/sh", &["-c", "sleep 0.2"], test_opts()).expect("spawn");
    handle
        .resize(PtySize {
            cols: 120,
            rows: 40,
        })
        .expect("resize");
    let code = handle.wait().await.expect("wait");
    assert_eq!(code, Some(0));
}

#[tokio::test]
async fn exit_status_propagates() {
    let handle = spawn_command("/bin/sh", &["-c", "exit 42"], test_opts()).expect("spawn");
    let code = handle.wait().await.expect("wait");
    assert_eq!(code, Some(42));
}

#[tokio::test]
async fn drop_does_not_leave_running_child() {
    let handle = spawn_command("/bin/sh", &["-c", "sleep 5"], test_opts()).expect("spawn");
    drop(handle);
    sleep(Duration::from_millis(100)).await;
}

#[test]
fn harness_recipes_use_interactive_flags() {
    let claude = harness_recipe(HarnessId::ClaudeCode, Some("hi"));
    assert_eq!(claude.binary, "claude");
    assert_eq!(claude.args, vec!["hi".to_string()]);

    let agy = harness_recipe(HarnessId::Antigravity, Some("hi"));
    assert_eq!(agy.binary, "agy");
    assert_eq!(
        agy.args,
        vec!["--prompt-interactive".to_string(), "hi".to_string()]
    );

    let codex = harness_recipe(HarnessId::Codex, Some("hi"));
    assert_eq!(codex.binary, "codex");
    assert_eq!(codex.args, vec!["hi".to_string()]);

    let cursor = harness_recipe(HarnessId::CursorAgent, Some("hi"));
    assert_eq!(cursor.binary, "cursor-agent");
    assert_eq!(cursor.args, vec!["hi".to_string()]);
}
