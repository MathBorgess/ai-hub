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
const DROP_STOP_TIMEOUT: Duration = Duration::from_millis(300);

pub(crate) struct PtyInner {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    scrollback: Mutex<Scrollback>,
    output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_code: Mutex<Option<Option<i32>>>,
    exit_notify: Notify,
    killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    running: std::sync::atomic::AtomicBool,
    /// Process-group id of the child (== pid; portable-pty makes it a session leader on unix).
    pgid: Option<i32>,
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
fn signal_group(pgid: i32, sig: libc::c_int) {
    unsafe {
        libc::kill(-pgid, sig);
    }
}

#[cfg(not(unix))]
fn signal_group(_pgid: i32, _sig: i32) {}

/// Polls `inner.running` until it goes false or `timeout` elapses. Returns whether it stopped.
fn poll_until_stopped(inner: &PtyInner, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !inner.running.load(std::sync::atomic::Ordering::Acquire) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Terminate/wait/escalate/reap barrier for the child's process group (F4).
///
/// Blocking; run from `spawn_blocking` or a drop path, never from an async task directly.
fn stop_barrier_blocking(inner: &PtyInner, timeout: Duration) -> Option<i32> {
    if inner.running.load(std::sync::atomic::Ordering::Acquire) {
        if let Some(pgid) = inner.pgid {
            signal_group(pgid, libc::SIGTERM);
            if !poll_until_stopped(inner, timeout) {
                signal_group(pgid, libc::SIGKILL);
                poll_until_stopped(inner, timeout);
            }
        }
    }
    inner.exit_code.lock().expect("exit lock").and_then(|c| c)
}

impl Drop for crate::PtyHandle {
    fn drop(&mut self) {
        stop_barrier_blocking(&self.inner, DROP_STOP_TIMEOUT);
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
    let pgid = child.process_id().map(|pid| pid as i32);
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

        let exit = match child.wait() {
            Ok(status) => exit_status_to_i32(status),
            Err(_) => None,
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
        tokio::task::spawn_blocking(move || stop_barrier_blocking(&inner, timeout))
            .await
            .map_err(|e| PtyError::Pty(e.to_string()))
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
