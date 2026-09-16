use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize as NativePtySize};
use tokio::sync::Notify;

use crate::scrollback::Scrollback;
use crate::{PtyError, PtySize, PtySpawnOptions};

// ponytail: scrollback retains at most 256 KiB of PTY output
const SCROLLBACK_CAP: usize = 256 * 1024;
const BROADCAST_CAP: usize = 256;
// ponytail: bounded input queue capacity, no config knob until a caller needs one
const INPUT_QUEUE_CAP: usize = 16;
// Drop has no caller-supplied timeout to work with; bound the barrier so it can't hang.
const DROP_STOP_TIMEOUT: Duration = Duration::from_secs(4);
/// After SIGTERM+SIGHUP, wait this long before escalating to SIGKILL.
pub(crate) const STOP_SIGNAL_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct PtyInner {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    scrollback: Mutex<Scrollback>,
    output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_code: Mutex<Option<Option<i32>>>,
    exit_notify: Notify,
    killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    running: std::sync::atomic::AtomicBool,
    /// Process-group id of the child (session leader pid; portable-pty calls `setsid` in pre_exec).
    pgid: Option<i32>,
    /// Direct child pid for wait/reap (same as pgid on unix when setsid succeeded).
    child_pid: Option<i32>,
    /// Serializes `wait`/`waitpid` between the reader thread and the stop barrier.
    reap_lock: Mutex<()>,
    input_tx: Mutex<Option<SyncSender<Vec<u8>>>>,
}

impl PtyInner {
    fn set_exit(&self, code: Option<i32>) {
        *self.exit_code.lock().expect("exit lock") = Some(code);
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
        self.exit_notify.notify_waiters();
    }
}

#[cfg(unix)]
fn signal_pid(pid: i32, sig: libc::c_int) -> Result<(), std::io::Error> {
    unsafe {
        if libc::kill(pid, sig) == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(err)
        }
    }
}

#[cfg(unix)]
fn signal_process_group(pgid: i32, sig: libc::c_int) -> Result<(), std::io::Error> {
    unsafe {
        if libc::kill(-pgid, sig) == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            // Some session leaders reject SIGHUP from outside the session; TERM/KILL still apply.
            Some(libc::EPERM) if sig == libc::SIGHUP => Ok(()),
            // macOS can return EPERM for kill(-pgid) while kill(pgid) reaches the leader.
            Some(libc::EPERM) => signal_pid(pgid, sig),
            _ => Err(err),
        }
    }
}

#[cfg(not(unix))]
fn signal_process_group(_pgid: i32, _sig: i32) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(unix)]
fn process_group_is_extinct(pgid: i32) -> Result<bool, std::io::Error> {
    unsafe {
        if libc::kill(-pgid, 0) == 0 {
            return Ok(false);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(true),
            // Group members exist but this uid cannot signal them yet — not extinct.
            Some(libc::EPERM) => Ok(false),
            _ => Err(err),
        }
    }
}

#[cfg(not(unix))]
fn process_group_is_extinct(_pgid: i32) -> Result<bool, std::io::Error> {
    Ok(true)
}

#[cfg(unix)]
enum ReapOutcome {
    Reaped(Option<i32>),
    StillRunning,
}

#[cfg(unix)]
fn waitpid_status_to_code(status: libc::c_int) -> Option<i32> {
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status) as i32)
    } else if libc::WIFSIGNALED(status) {
        Some(128 + libc::WTERMSIG(status) as i32)
    } else {
        None
    }
}

#[cfg(unix)]
fn try_reap_child_pid(pid: i32) -> Result<ReapOutcome, std::io::Error> {
    unsafe {
        let mut status: libc::c_int = 0;
        let r = libc::waitpid(pid, &mut status, libc::WNOHANG);
        if r == pid {
            return Ok(ReapOutcome::Reaped(waitpid_status_to_code(status)));
        }
        if r == 0 {
            return Ok(ReapOutcome::StillRunning);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ECHILD) {
            return Ok(ReapOutcome::Reaped(None));
        }
        Err(err)
    }
}

/// Terminate/wait/escalate/reap barrier for the child's process group (F4).
///
/// Success requires the direct child to be reaped and `kill(-pgid, 0)` to return `ESRCH`.
/// Liveness is never inferred from PTY EOF or from `running` alone.
///
/// Descendants that leave the group via `setsid` or `setpgid` are not covered by this barrier.
///
/// Blocking; run from `spawn_blocking` or a drop path, never from an async task directly.
fn stop_barrier_blocking(
    inner: &PtyInner,
    timeout: Duration,
    signal_group: fn(i32, libc::c_int) -> Result<(), std::io::Error>,
    try_reap: fn(i32) -> Result<ReapOutcome, std::io::Error>,
) -> Result<Option<i32>, PtyError> {
    #[cfg(not(unix))]
    {
        return inner
            .exit_code
            .lock()
            .expect("exit lock")
            .and_then(|c| c)
            .ok_or(PtyError::Pty(
                "stop barrier unsupported on this platform".to_string(),
            ));
    }

    #[cfg(unix)]
    {
        let pgid = inner
            .pgid
            .ok_or(PtyError::Pty("missing process group id".to_string()))?;
        let child_pid = inner
            .child_pid
            .ok_or(PtyError::Pty("missing child pid".to_string()))?;

        let started = Instant::now();
        let deadline = started + timeout;
        let escalate_at = started + STOP_SIGNAL_GRACE.min(timeout);
        let mut sent_kill = false;

        loop {
            let now = Instant::now();
            if now >= deadline {
                let group_gone = match process_group_is_extinct(pgid) {
                    Ok(v) => v,
                    Err(e) => return Err(PtyError::StopSignal(e.to_string())),
                };
                if group_gone {
                    return Err(PtyError::StopReap(
                        "stop deadline reached before child was reaped".to_string(),
                    ));
                }
                return Err(PtyError::StopTimeout);
            }

            let group_gone = match process_group_is_extinct(pgid) {
                Ok(v) => v,
                Err(e) => return Err(PtyError::StopSignal(e.to_string())),
            };

            if !group_gone {
                if now < escalate_at {
                    signal_group(pgid, libc::SIGTERM)
                        .map_err(|e| PtyError::StopSignal(e.to_string()))?;
                    signal_group(pgid, libc::SIGHUP)
                        .map_err(|e| PtyError::StopSignal(e.to_string()))?;
                } else if !sent_kill {
                    signal_group(pgid, libc::SIGKILL)
                        .map_err(|e| PtyError::StopSignal(e.to_string()))?;
                    sent_kill = true;
                }
            }

            let reaped = {
                let _guard = inner.reap_lock.lock().expect("reap lock");
                match try_reap(child_pid) {
                    Ok(ReapOutcome::Reaped(code)) => {
                        if inner.exit_code.lock().expect("exit lock").is_none() {
                            inner.set_exit(code);
                        }
                        true
                    }
                    Ok(ReapOutcome::StillRunning) => false,
                    Err(e) => return Err(PtyError::StopReap(e.to_string())),
                }
            };

            let group_gone = match process_group_is_extinct(pgid) {
                Ok(v) => v,
                Err(e) => return Err(PtyError::StopSignal(e.to_string())),
            };

            if group_gone && reaped {
                let stored = *inner.exit_code.lock().expect("exit lock");
                return match stored {
                    Some(code) => Ok(code),
                    None => Err(PtyError::StopReap(
                        "child exited but exit code was not recorded".to_string(),
                    )),
                };
            }

            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn stop_barrier_blocking_default(
    inner: &PtyInner,
    timeout: Duration,
) -> Result<Option<i32>, PtyError> {
    stop_barrier_blocking(inner, timeout, signal_process_group, try_reap_child_pid)
}

impl Drop for crate::PtyHandle {
    fn drop(&mut self) {
        let _ = stop_barrier_blocking_default(&self.inner, DROP_STOP_TIMEOUT);
    }
}

fn to_native_size(size: PtySize) -> NativePtySize {
    NativePtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn exit_status_to_i32(status: portable_pty::ExitStatus) -> Option<i32> {
    let code = status.exit_code();
    if code > i32::MAX as u32 {
        None
    } else {
        Some(code as i32)
    }
}

pub fn spawn_command(
    cmd: &str,
    args: &[&str],
    opts: PtySpawnOptions,
) -> Result<crate::PtyHandle, PtyError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(to_native_size(opts.size))
        .map_err(|e| PtyError::Pty(e.to_string()))?;

    let mut command = CommandBuilder::new(cmd);
    for arg in args {
        command.arg(arg);
    }
    command.cwd(&opts.cwd);
    for (k, v) in &opts.env {
        command.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|e| PtyError::Pty(e.to_string()))?;

    let killer = child.clone_killer();
    let child_pid = child.process_id().map(|pid| pid as i32);
    let pgid = child_pid;
    let mut child = child;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| PtyError::Pty(e.to_string()))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| PtyError::Pty(e.to_string()))?;

    let (output_tx, _) = tokio::sync::broadcast::channel(BROADCAST_CAP);
    let (input_tx, input_rx) = sync_channel::<Vec<u8>>(INPUT_QUEUE_CAP);
    let inner = Arc::new(PtyInner {
        writer: Mutex::new(Some(writer)),
        master: Mutex::new(pair.master),
        scrollback: Mutex::new(Scrollback::new(SCROLLBACK_CAP)),
        output_tx,
        exit_code: Mutex::new(None),
        exit_notify: Notify::new(),
        killer: Mutex::new(Some(killer)),
        running: std::sync::atomic::AtomicBool::new(true),
        pgid,
        child_pid,
        reap_lock: Mutex::new(()),
        input_tx: Mutex::new(Some(input_tx)),
    });

    let writer_inner = Arc::clone(&inner);
    std::thread::spawn(move || {
        while let Ok(data) = input_rx.recv() {
            let mut guard = match writer_inner.writer.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            match guard.as_mut() {
                Some(w) => {
                    if w.write_all(&data).is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    });

    let reader_inner = Arc::clone(&inner);
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    reader_inner
                        .scrollback
                        .lock()
                        .expect("scrollback lock")
                        .append(&chunk);
                    let _ = reader_inner.output_tx.send(chunk);
                }
                Err(_) => break,
            }
        }

        let exit = {
            let _guard = reader_inner.reap_lock.lock().expect("reap lock");
            if let Some(code) = *reader_inner.exit_code.lock().expect("exit lock") {
                code
            } else {
                match child.wait() {
                    Ok(status) => exit_status_to_i32(status),
                    Err(_) => None,
                }
            }
        };
        reader_inner.set_exit(exit);
        *reader_inner.killer.lock().expect("killer lock") = None;
        // Drop the sender so the dedicated writer thread's recv() unblocks and it exits.
        *reader_inner.input_tx.lock().expect("input tx lock") = None;
    });

    Ok(crate::PtyHandle { inner })
}

impl crate::PtyHandle {
    pub fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        if !self
            .inner
            .running
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(PtyError::NotRunning);
        }
        self.inner
            .master
            .lock()
            .map_err(|_| PtyError::Pty("master lock poisoned".to_string()))?
            .resize(to_native_size(size))
            .map_err(|e| PtyError::Pty(e.to_string()))
    }

    pub fn subscribe_output(&self) -> tokio::sync::broadcast::Receiver<Vec<u8>> {
        self.inner.output_tx.subscribe()
    }

    pub fn scrollback_snapshot(&self) -> Vec<u8> {
        self.inner
            .scrollback
            .lock()
            .expect("scrollback lock")
            .snapshot()
    }

    pub async fn wait(&self) -> Result<Option<i32>, PtyError> {
        loop {
            if let Some(code) = *self.inner.exit_code.lock().expect("exit lock") {
                return Ok(code);
            }
            self.inner.exit_notify.notified().await;
        }
    }

    pub async fn kill(&self) -> Result<(), PtyError> {
        if !self
            .inner
            .running
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(PtyError::NotRunning);
        }
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            if let Some(killer) = inner.killer.lock().expect("killer lock").as_mut() {
                killer.kill().map_err(PtyError::Io)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| PtyError::Pty(e.to_string()))?
    }

    /// Bounded terminate/wait/escalate/reap stop barrier for child process group (F4).
    pub async fn stop(&self, timeout: std::time::Duration) -> Result<Option<i32>, PtyError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || stop_barrier_blocking_default(&inner, timeout))
            .await
            .map_err(|e| PtyError::Pty(e.to_string()))?
    }

    /// Alias for `stop`.
    pub async fn stop_barrier(
        &self,
        timeout: std::time::Duration,
    ) -> Result<Option<i32>, PtyError> {
        self.stop(timeout).await
    }

    /// Submits input without blocking the caller via a bounded queue drained by a writer thread (F7).
    ///
    /// Returns `PtyError::QueueFull` if the queue capacity is exceeded.
    pub fn try_write(&self, data: &[u8]) -> Result<(), PtyError> {
        if !self
            .inner
            .running
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(PtyError::NotRunning);
        }
        let guard = self.inner.input_tx.lock().expect("input tx lock");
        match guard.as_ref() {
            Some(tx) => match tx.try_send(data.to_vec()) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => Err(PtyError::QueueFull),
                Err(TrySendError::Disconnected(_)) => Err(PtyError::NotRunning),
            },
            None => Err(PtyError::NotRunning),
        }
    }

    /// Alias for `try_write`.
    pub fn write_nonblocking(&self, data: &[u8]) -> Result<(), PtyError> {
        self.try_write(data)
    }
}

pub fn merge_spawn_opts(
    cwd: PathBuf,
    size: PtySize,
    base_env: HashMap<String, String>,
    extra_env: HashMap<String, String>,
    initial_prompt: Option<String>,
) -> PtySpawnOptions {
    let mut env = base_env;
    env.extend(extra_env);
    PtySpawnOptions {
        cwd,
        env,
        size,
        initial_prompt,
    }
}

#[cfg(test)]
mod stop_barrier_tests {
    use super::*;
    fn minimal_inner(pgid: i32, child_pid: i32) -> PtyInner {
        PtyInner {
            writer: Mutex::new(None),
            master: Mutex::new(
                native_pty_system()
                    .openpty(NativePtySize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .expect("openpty")
                    .master,
            ),
            scrollback: Mutex::new(Scrollback::new(1024)),
            output_tx: tokio::sync::broadcast::channel(4).0,
            exit_code: Mutex::new(None),
            exit_notify: Notify::new(),
            killer: Mutex::new(None),
            running: std::sync::atomic::AtomicBool::new(true),
            pgid: Some(pgid),
            child_pid: Some(child_pid),
            reap_lock: Mutex::new(()),
            input_tx: Mutex::new(None),
        }
    }

    #[test]
    fn f4_stop_error_when_signal_fails() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        let opts = PtySpawnOptions {
            cwd: PathBuf::from("."),
            env: HashMap::new(),
            size: PtySize::default(),
            initial_prompt: None,
        };
        let handle = spawn_command("/bin/sh", &["-c", "sleep 60"], opts).expect("spawn pty child");

        fn fail_signal(_pgid: i32, _sig: libc::c_int) -> Result<(), std::io::Error> {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }
        fn reap_ok(_pid: i32) -> Result<ReapOutcome, std::io::Error> {
            Ok(ReapOutcome::StillRunning)
        }

        let err = stop_barrier_blocking(
            &handle.inner,
            Duration::from_millis(200),
            fail_signal,
            reap_ok,
        )
        .expect_err("signal failure should error");
        assert!(matches!(err, PtyError::StopSignal(_)));

        let _ = stop_barrier_blocking_default(&handle.inner, Duration::from_secs(2));
    }

    #[test]
    fn f4_reap_failure_is_an_error() {
        fn noop_signal(_pgid: i32, _sig: libc::c_int) -> Result<(), std::io::Error> {
            Ok(())
        }
        fn fail_reap(_pid: i32) -> Result<ReapOutcome, std::io::Error> {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }

        let inner = minimal_inner(42_001, 42_001);
        let err = stop_barrier_blocking(&inner, Duration::from_millis(200), noop_signal, fail_reap)
            .expect_err("reap failure should error");
        assert!(matches!(err, PtyError::StopReap(_)));
    }

    #[test]
    fn f4_unconfirmed_reap_at_deadline_is_an_error() {
        fn noop_signal(_pgid: i32, _sig: libc::c_int) -> Result<(), std::io::Error> {
            Ok(())
        }
        fn never_reap(_pid: i32) -> Result<ReapOutcome, std::io::Error> {
            Ok(ReapOutcome::StillRunning)
        }

        let inner = minimal_inner(42_002, 42_002);
        let err = stop_barrier_blocking(&inner, Duration::from_millis(50), noop_signal, never_reap)
            .expect_err("unconfirmed reap should error");
        assert!(matches!(err, PtyError::StopReap(_)));
    }
}
