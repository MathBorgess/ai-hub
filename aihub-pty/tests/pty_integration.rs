use std::collections::HashMap;
use std::env;
use std::time::Duration;

use aihub_core::HarnessId;
use aihub_pty::{
    harness_recipe, harness_recipe_with_model, spawn_command, PtyError, PtySize, PtySpawnOptions,
};
use tokio::time::{sleep, timeout};

fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn extract_pid(text: &str, prefix: &str) -> i32 {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no line starting with {prefix:?} in {text:?}"))
        .trim()
        .parse()
        .expect("valid pid")
}

async fn poll_pid_dead(pid: i32, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("pid {pid} still alive after {timeout:?}");
}

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
        if handle
            .scrollback_snapshot()
            .windows(needle.len())
            .any(|w| w == needle)
        {
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
    let handle =
        spawn_command("/bin/sh", &["-c", "echo pty-echo-marker"], test_opts()).expect("spawn");
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
        replay
            .windows(b"first-line".len())
            .any(|w| w == b"first-line"),
        "replay should include first line"
    );

    let mut live = handle.subscribe_output();
    wait_for_scrollback_contains(&handle, b"second-line").await;

    let got_live = timeout(Duration::from_secs(2), async {
        loop {
            let chunk = live.recv().await.expect("broadcast");
            if chunk
                .windows(b"second-line".len())
                .any(|w| w == b"second-line")
            {
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
    let handle =
        spawn_command("/bin/sh", &["-c", "read x; echo got:$x"], test_opts()).expect("spawn");
    sleep(Duration::from_millis(50)).await;
    handle.try_write(b"hello-input\n").expect("write");
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

#[tokio::test]
async fn f4_stop_barrier_terminates_full_group_including_sigterm_ignoring_descendant() {
    let handle = spawn_command(
        "/bin/sh",
        &[
            "-c",
            "trap '' TERM; echo MAIN:$$; (trap '' TERM; sleep 5) & echo CHILD:$!; sleep 5",
        ],
        test_opts(),
    )
    .expect("spawn");

    wait_for_scrollback_contains(&handle, b"CHILD:").await;
    let text = String::from_utf8_lossy(&handle.scrollback_snapshot()).into_owned();
    let main_pid = extract_pid(&text, "MAIN:");
    let child_pid = extract_pid(&text, "CHILD:");
    assert!(pid_alive(main_pid), "main pid should be alive before stop");
    assert!(
        pid_alive(child_pid),
        "descendant pid should be alive before stop"
    );

    handle
        .stop(Duration::from_millis(300))
        .await
        .expect("stop barrier");

    poll_pid_dead(main_pid, Duration::from_secs(2)).await;
    poll_pid_dead(child_pid, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn f6_drop_without_stop_leaves_no_live_child_pid() {
    let handle =
        spawn_command("/bin/sh", &["-c", "echo PID:$$; sleep 5"], test_opts()).expect("spawn");
    wait_for_scrollback_contains(&handle, b"PID:").await;
    let text = String::from_utf8_lossy(&handle.scrollback_snapshot()).into_owned();
    let pid = extract_pid(&text, "PID:");
    assert!(pid_alive(pid), "child pid should be alive before drop");

    drop(handle);

    poll_pid_dead(pid, Duration::from_secs(2)).await;
}

#[test]
fn f7_full_input_queue_returns_error_without_blocking_sender() {
    let handle = spawn_command("/bin/sh", &["-c", "sleep 5"], test_opts()).expect("spawn");

    let payload = vec![b'x'; 64];
    let mut got_queue_full = false;
    for _ in 0..200 {
        match handle.try_write(&payload) {
            Ok(()) => {}
            Err(PtyError::QueueFull) => {
                got_queue_full = true;
                break;
            }
            Err(e) => panic!("unexpected error from try_write: {e}"),
        }
    }
    assert!(
        got_queue_full,
        "expected the bounded queue to fill and try_write to return QueueFull"
    );
}

#[test]
fn harness_recipe_with_model_uses_each_clis_model_flag() {
    let claude = harness_recipe_with_model(HarnessId::ClaudeCode, Some("hi"), Some("sonnet"));
    assert_eq!(
        claude.args,
        vec![
            "--model".to_string(),
            "sonnet".to_string(),
            "hi".to_string()
        ]
    );

    let agy = harness_recipe_with_model(HarnessId::Antigravity, Some("hi"), Some("sonnet"));
    assert_eq!(
        agy.args,
        vec![
            "--model".to_string(),
            "sonnet".to_string(),
            "--prompt-interactive".to_string(),
            "hi".to_string()
        ]
    );

    let codex = harness_recipe_with_model(HarnessId::Codex, Some("hi"), Some("sonnet"));
    assert_eq!(
        codex.args,
        vec![
            "--model".to_string(),
            "sonnet".to_string(),
            "hi".to_string()
        ]
    );

    let cursor = harness_recipe_with_model(HarnessId::CursorAgent, Some("hi"), Some("sonnet"));
    assert_eq!(
        cursor.args,
        vec![
            "--model".to_string(),
            "sonnet".to_string(),
            "hi".to_string()
        ]
    );

    let no_model = harness_recipe_with_model(HarnessId::ClaudeCode, None, None);
    assert_eq!(no_model.args, Vec::<String>::new());
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
