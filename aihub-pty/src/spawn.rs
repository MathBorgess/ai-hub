use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize as NativePtySize};
use tokio::sync::Notify;

use crate::scrollback::Scrollback;
use crate::{PtyError, PtySize, PtySpawnOptions};

// ponytail: scrollback retains at most 256 KiB of PTY output
const SCROLLBACK_CAP: usize = 256 * 1024;
const BROADCAST_CAP: usize = 256;

pub(crate) struct PtyInner {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    scrollback: Mutex<Scrollback>,
    output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    exit_code: Mutex<Option<Option<i32>>>,
    exit_notify: Notify,
    killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    running: std::sync::atomic::AtomicBool,
    reader_join: Mutex<Option<JoinHandle<()>>>,
}

impl PtyInner {
    fn set_exit(&self, code: Option<i32>) {
        *self.exit_code.lock().expect("exit lock") = Some(code);
        self.running
            .store(false, std::sync::atomic::Ordering::Release);
        self.exit_notify.notify_waiters();
    }
}

impl Drop for PtyInner {
    fn drop(&mut self) {
        if self.running.load(std::sync::atomic::Ordering::Acquire) {
            if let Some(killer) = self.killer.lock().expect("killer lock").as_mut() {
                let _ = killer.kill();
            }
        }
        if let Some(join) = self.reader_join.lock().expect("join lock").take() {
            let _ = join.join();
        }
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
    let inner = Arc::new(PtyInner {
        writer: Mutex::new(Some(writer)),
        master: Mutex::new(pair.master),
        scrollback: Mutex::new(Scrollback::new(SCROLLBACK_CAP)),
        output_tx,
        exit_code: Mutex::new(None),
        exit_notify: Notify::new(),
        killer: Mutex::new(Some(killer)),
        running: std::sync::atomic::AtomicBool::new(true),
        reader_join: Mutex::new(None),
    });

    let reader_inner = Arc::clone(&inner);
    let join = std::thread::spawn(move || {
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
    });

    *inner.reader_join.lock().expect("join lock") = Some(join);

    Ok(crate::PtyHandle { inner })
}

impl crate::PtyHandle {
    pub async fn write(&self, data: &[u8]) -> Result<(), PtyError> {
        if !self.inner.running.load(std::sync::atomic::Ordering::Acquire) {
            return Err(PtyError::NotRunning);
        }
        let data = data.to_vec();
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.writer.lock().map_err(|_| {
                PtyError::Pty("writer lock poisoned".to_string())
            })?;
            match guard.as_mut() {
                Some(w) => w.write_all(&data).map_err(PtyError::Io),
                None => Err(PtyError::NotRunning),
            }
        })
        .await
        .map_err(|e| PtyError::Pty(e.to_string()))?
    }

    pub fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        if !self.inner.running.load(std::sync::atomic::Ordering::Acquire) {
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
        if !self.inner.running.load(std::sync::atomic::Ordering::Acquire) {
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
