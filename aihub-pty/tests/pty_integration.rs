use std::collections::HashMap;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use aihub_core::HarnessId;
use aihub_pty::{
    harness_recipe, harness_recipe_with_model, spawn_command, spawn_harness, PtyError, PtySize,
    PtySpawnOptions,
};
use tokio::time::{sleep, timeout};

fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn pid_extinct(pid: i32) -> bool {
    unsafe {
        if libc::kill(pid, 0) == 0 {
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
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
        .stop(Duration::from_secs(5))
        .await
        .expect("stop barrier");

    poll_pid_dead(main_pid, Duration::from_secs(2)).await;
    poll_pid_dead(child_pid, Duration::from_secs(2)).await;
}

#[tokio::test]
async fn f4_redirected_descendant_ignoring_term_is_dead_before_stop_returns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("marker");
    let pid_file = dir.path().join("desc.pid");
    let script = dir.path().join("run.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
MARKER={}\n\
PIDFILE={}\n\
trap '' TERM HUP\n\
{{\n\
  while :; do echo x >>\"$MARKER\"; done\n\
}} </dev/null >/dev/null 2>/dev/null &\n\
echo $! >\"$PIDFILE\"\n\
exit 0\n",
            marker.display(),
            pid_file.display(),
        ),
    )
    .expect("write script");
    let mut perms = fs::metadata(&script).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod");

    let opts = PtySpawnOptions {
        cwd: dir.path().to_path_buf(),
        env: HashMap::new(),
        size: PtySize::default(),
        initial_prompt: None,
    };
    let handle = spawn_command("/bin/sh", &[script.to_str().expect("utf8")], opts).expect("spawn");

    let descendant_pid = {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if pid_file.exists() {
                let raw = fs::read_to_string(&pid_file).expect("read pid");
                if let Ok(pid) = raw.trim().parse::<i32>() {
                    break pid;
                }
            }
            if Instant::now() >= deadline {
                panic!("descendant pid file never appeared");
            }
            sleep(Duration::from_millis(20)).await;
        }
    };

    assert!(
        pid_alive(descendant_pid),
        "descendant should be alive before stop"
    );

    handle
        .stop(Duration::from_secs(5))
        .await
        .expect("stop should succeed");

    assert!(
        pid_extinct(descendant_pid),
        "descendant must be dead the moment stop returns"
    );

    let size_after_stop = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    sleep(Duration::from_millis(300)).await;
    let size_later = fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        size_after_stop, size_later,
        "marker file should not grow after stop returned"
    );
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

#[tokio::test]
async fn f7_stalled_writer_does_not_block_caller_or_shutdown() {
    let handle = spawn_command("/bin/sh", &["-c", "exec cat"], test_opts()).expect("spawn");
    sleep(Duration::from_millis(50)).await;

    let chunk = vec![b'x'; 256 * 1024];
    let started = Instant::now();
    let mut got_queue_full = false;
    for _ in 0..200 {
        match handle.try_write(&chunk) {
            Ok(()) => {}
            Err(PtyError::QueueFull) => {
                got_queue_full = true;
                break;
            }
            Err(e) => panic!("unexpected error from try_write: {e}"),
        }
    }
    assert!(got_queue_full, "expected QueueFull from bounded queue");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "try_write should return QueueFull without blocking the caller"
    );

    handle
        .stop(Duration::from_secs(5))
        .await
        .expect("stop should complete while writer thread is blocked in write");
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
fn spawn_harness_forwards_model_to_recipe() {
    let recipe = harness_recipe_with_model(HarnessId::ClaudeCode, Some("hi"), Some("sonnet"));
    let fake = tempfile::tempdir().expect("tempdir");
    let bin = fake.path().join(&recipe.binary);
    fs::write(&bin, "#!/bin/sh\nexit 0\n").expect("fake bin");
    let mut perms = fs::metadata(&bin).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&bin, perms).expect("chmod");

    let mut env = HashMap::new();
    env.insert(
        "PATH".to_string(),
        fake.path().to_string_lossy().into_owned(),
    );
    let opts = PtySpawnOptions {
        cwd: fake.path().to_path_buf(),
        env,
        size: PtySize::default(),
        initial_prompt: Some("hi".to_string()),
    };
    let handle = spawn_harness(HarnessId::ClaudeCode, opts, Some("sonnet")).expect("spawn");
    drop(handle);
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
