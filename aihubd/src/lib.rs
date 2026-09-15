//! Foreground Unix socket supervisor. Client lifetimes never own child PTYs.
use aihub_core::*;
use aihub_pty::{PtySize, PtySpawnOptions};
use anyhow::{anyhow, bail, Result};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{broadcast, mpsc, Mutex},
    task::JoinSet,
};

pub type Operation<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

pub fn log_lifecycle(level: &str, event: &str, details: &str) {
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let sec = total_secs % 60;
    let total_mins = total_secs / 60;
    let min = total_mins % 60;
    let total_hours = total_mins / 60;
    let hour = total_hours % 24;
    let mut days = (total_hours / 24) as i64;

    let mut year = 1970;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let days_in_year = if leap { 366 } else { 365 };
        if days >= days_in_year {
            days -= days_in_year;
            year += 1;
        } else {
            break;
        }
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &md in &month_days {
        if days >= md {
            days -= md;
            month += 1;
        } else {
            break;
        }
    }
    let day = days + 1;
    let ts = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, min, sec, millis
    );
    eprintln!("{ts} [{level}] {event}: {details}");
}

/// Plain closure seam for the PTY; the production adapter calls the contract methods.
pub struct Pty {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub scrollback: Vec<u8>,
    pub write: Box<dyn Fn(Vec<u8>) -> Operation<()> + Send + Sync>,
    pub resize: Box<dyn Fn(PtySize) -> Result<()> + Send + Sync>,
    pub wait: Box<dyn Fn() -> Operation<Option<i32>> + Send + Sync>,
    pub kill: Box<dyn Fn() -> Operation<()> + Send + Sync>,
    pub stop: Arc<dyn Fn(Duration) -> Operation<Option<i32>> + Send + Sync>,
    pub try_write: Arc<dyn Fn(Vec<u8>) -> Result<()> + Send + Sync>,
}

pub fn spawn_pty(harness: HarnessId, options: PtySpawnOptions) -> Result<Pty> {
    let handle = if let Ok(cmd) = std::env::var("AIHUB_PTY_COMMAND") {
        Arc::new(aihub_pty::spawn_command(&cmd, &[], options)?)
    } else {
        Arc::new(aihub_pty::spawn_harness(harness, options)?)
    };
    let output = handle.subscribe_output();
    let scrollback = handle.scrollback_snapshot();
    let writer = handle.clone();
    let resizer = handle.clone();
    let waiter = handle.clone();
    let killer = handle.clone();
    let stopper = handle.clone();
    Ok(Pty {
        output,
        scrollback,
        write: Box::new(move |data| {
            let h = writer.clone();
            Box::pin(async move { h.try_write(&data).map_err(Into::into) })
        }),
        resize: Box::new(move |size| resizer.resize(size).map_err(Into::into)),
        wait: Box::new(move || {
            let h = waiter.clone();
            Box::pin(async move { h.wait().await.map_err(Into::into) })
        }),
        kill: Box::new(move || {
            let h = killer.clone();
            Box::pin(async move { h.kill().await.map_err(Into::into) })
        }),
        stop: Arc::new(move |timeout| {
            let h = stopper.clone();
            Box::pin(async move { h.stop_barrier(timeout).await.map_err(Into::into) })
        }),
        try_write: Arc::new(move |data| handle.try_write(&data).map_err(Into::into)),
    })
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
    _lock: std::fs::File,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(m) = std::fs::symlink_metadata(&self.path) {
            if m.dev() == self.device && m.ino() == self.inode {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}
fn bind(path: &Path) -> Result<(UnixListener, SocketGuard)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let meta = std::fs::symlink_metadata(parent)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("socket data directory must be a real directory");
    }
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    // Keep the lock file permanently: unlinking it would let racing processes
    // lock different inodes and steal a just-bound socket during stale cleanup.
    let lock_path = path.with_extension("sock.lock");
    if std::fs::symlink_metadata(&lock_path).is_ok_and(|m| !m.file_type().is_file()) {
        bail!("socket lock must be a regular file");
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)?;
    lock.try_lock()
        .map_err(|_| anyhow!("a live aihubd already holds this socket lock"))?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            bail!("socket path already exists and is not a socket");
        }
        // A closed listener's socket file can still accept one queued
        // connect() from its old kernel backlog for a brief moment after
        // the listening fd is dropped, making a genuinely stale socket look
        // momentarily live. Require repeated successful connects before
        // concluding a live daemon is really there; a lone stale backlog
        // entry won't survive a second attempt a few milliseconds later.
        let mut confirmed_live = false;
        let mut stale = false;
        for attempt in 0..3 {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) if attempt == 2 => confirmed_live = true,
                Ok(_) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    stale = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    log_lifecycle(
                        "ERROR",
                        "bind_check",
                        &format!("stale socket check failed: {e} (kind={:?})", e.kind()),
                    );
                    bail!("cannot establish whether existing socket is stale: {e}");
                }
            }
        }
        if confirmed_live {
            bail!("a live aihubd already holds this socket");
        }
        if stale {
            let current = std::fs::symlink_metadata(path)?;
            if current.ino() != meta.ino() || current.dev() != meta.dev() {
                bail!("socket changed while checking listener");
            }
            std::fs::remove_file(path)?;
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    log_lifecycle(
        "INFO",
        "bind",
        &format!("bound socket to {}", path.display()),
    );
    let meta = std::fs::symlink_metadata(path)?;
    Ok((
        listener,
        SocketGuard {
            path: path.to_owned(),
            device: meta.dev(),
            inode: meta.ino(),
            _lock: lock,
        },
    ))
}

struct Session {
    summary: SessionSummary,
    base: String,
    originating_checkout: PathBuf,
    prompt: Option<String>,
    pty: Pty,
    generation: u64,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
struct Client {
    sender: mpsc::Sender<DaemonMessage>,
    attached: HashSet<SessionId>,
    // Contract has no separate preview message: repeat a matching request to confirm.
    merge: Option<(SessionId, MergeStrategy, String)>,
}
#[derive(Default)]
struct State {
    sessions: Vec<Session>,
    clients: HashMap<u64, Client>,
    next_id: u64,
}
impl State {
    fn send(&mut self, client: u64, message: DaemonMessage) {
        if let Some(c) = self.clients.get(&client) {
            if c.sender.try_send(message).is_err() {
                self.clients.remove(&client);
            }
        }
    }
    fn broadcast(&mut self, message: DaemonMessage) {
        let session_id = match &message {
            DaemonMessage::PtyOutput { session_id, .. }
            | DaemonMessage::SessionExited { session_id, .. }
            | DaemonMessage::ModeSet { session_id, .. }
            | DaemonMessage::RouteRecommendation { session_id, .. }
            | DaemonMessage::HarnessSwitched { session_id, .. }
            | DaemonMessage::MergeResult { session_id, .. } => Some(session_id),
            _ => None,
        };
        self.clients.retain(|_, c| {
            if let Some(id) = session_id {
                if !c.attached.contains(id) {
                    return true;
                }
            }
            c.sender.try_send(message.clone()).is_ok()
        });
    }
    fn session(&mut self, id: &SessionId) -> Result<&mut Session> {
        self.sessions
            .iter_mut()
            .find(|s| &s.summary.session_id == id)
            .ok_or_else(|| anyhow!("session not found"))
    }
}

type Spawner = dyn Fn(HarnessId, PtySpawnOptions) -> Result<Pty> + Send + Sync;
type Probe =
    dyn Fn() -> Pin<Box<dyn Future<Output = Vec<QuotaSnapshot>> + Send + 'static>> + Send + Sync;
type Classifier = dyn Fn(&str) -> Pin<Box<dyn Future<Output = aihub_router::Classification> + Send + 'static>>
    + Send
    + Sync;
type Router =
    dyn Fn(TaskTier, TaskSize, &[QuotaSnapshot], u64) -> Result<RouteOutcome> + Send + Sync;
type MemoryRecorder = dyn Fn(
        &SessionId,
        HarnessId,
        HarnessId,
        &aihub_memory::BriefPair,
    )
        -> Pin<Box<dyn Future<Output = Result<aihub_memory::HandoffDestination>> + Send + 'static>>
    + Send
    + Sync;
type MemoryExtractor = dyn Fn(
        &SessionId,
        HarnessId,
        &Path,
    ) -> Pin<Box<dyn Future<Output = Result<aihub_memory::HandoffTurn>> + Send + 'static>>
    + Send
    + Sync;
type GitDiff = dyn Fn(&Path, &str, bool) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'static>>
    + Send
    + Sync;
type GitFinish = dyn Fn(
        &Path,
        MergeStrategy,
        &str,
        &str,
        &Path,
    ) -> Pin<Box<dyn Future<Output = Result<aihub_git::MergeOutcome>> + Send + 'static>>
    + Send
    + Sync;

#[derive(Clone)]
pub struct Daemon {
    pub(crate) state: Arc<Mutex<State>>,
    pub(crate) cache: Arc<Mutex<Vec<QuotaSnapshot>>>,
    probe: Arc<Probe>,
    spawner: Arc<Spawner>,
    classifier: Arc<Classifier>,
    router: Arc<Router>,
    memory_recorder: Arc<MemoryRecorder>,
    memory_extractor: Arc<MemoryExtractor>,
    git_diff: Arc<GitDiff>,
    git_finish: Arc<GitFinish>,
    refresh: Arc<tokio::sync::Notify>,
}

impl Daemon {
    pub fn new<P, F, S>(probe: P, spawner: S) -> Self
    where
        P: Fn() -> F + Send + Sync + 'static,
        F: Future<Output = Vec<QuotaSnapshot>> + Send + 'static,
        S: Fn(HarnessId, PtySpawnOptions) -> Result<Pty> + Send + Sync + 'static,
    {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            cache: Arc::new(Mutex::new(vec![])),
            probe: Arc::new(move || Box::pin(probe())),
            spawner: Arc::new(spawner),
            classifier: Arc::new(|prompt: &str| {
                let p = prompt.to_string();
                let p_fallback = p.clone();
                Box::pin(async move {
                    let join = tokio::task::spawn(async move {
                        aihub_router::classify_with_fallback(&p).await
                    })
                    .await;
                    match join {
                        Ok(c) => c,
                        Err(_) => aihub_router::classify(&p_fallback),
                    }
                })
            }),
            router: Arc::new(|tier, size, snapshots, horizon| {
                aihub_router::route_outcome(tier, size, snapshots, horizon).map_err(Into::into)
            }),
            memory_recorder: Arc::new(|session_id, from, to, brief| {
                let id = session_id.clone();
                let b = brief.clone();
                let id_call = id.clone();
                let b_call = b.clone();
                Box::pin(async move {
                    let join = tokio::task::spawn(async move {
                        aihub_memory::record_handoff_destination(&id_call, from, to, &b_call).await
                    })
                    .await;
                    match join {
                        Ok(Ok(dest)) => Ok(dest),
                        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
                        Err(_) => {
                            aihub_memory::record_handoff(&id, from, to, &b).await?;
                            Ok(aihub_memory::HandoffDestination::Spooled)
                        }
                    }
                })
            }),
            memory_extractor: Arc::new(|id, harness, wt_path| {
                let id = id.clone();
                let wt = wt_path.to_path_buf();
                Box::pin(async move {
                    aihub_memory::extract_last_turn(&id, harness, &wt)
                        .await
                        .map_err(Into::into)
                })
            }),
            git_diff: Arc::new(|path, base, cached| {
                let path = path.to_path_buf();
                let base = base.to_string();
                Box::pin(async move {
                    aihub_git::diff(&path, &base, cached)
                        .await
                        .map_err(Into::into)
                })
            }),
            git_finish: Arc::new(|path, strategy, base, branch, origin| {
                let branch = branch.to_string();
                let origin = origin.to_path_buf();
                let path = path.to_path_buf();
                let base = base.to_string();
                Box::pin(async move {
                    aihub_git::finish_session(&path, strategy, &base, &branch, &origin)
                        .await
                        .map_err(Into::into)
                })
            }),
            refresh: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn with_classifier<C, F>(mut self, classifier: C) -> Self
    where
        C: Fn(&str) -> F + Send + Sync + 'static,
        F: Future<Output = aihub_router::Classification> + Send + 'static,
    {
        self.classifier = Arc::new(move |p| Box::pin(classifier(p)));
        self
    }

    pub fn with_router<R>(mut self, router: R) -> Self
    where
        R: Fn(TaskTier, TaskSize, &[QuotaSnapshot], u64) -> Result<RouteOutcome>
            + Send
            + Sync
            + 'static,
    {
        self.router = Arc::new(router);
        self
    }

    pub fn with_memory_recorder<M, F>(mut self, recorder: M) -> Self
    where
        M: Fn(&SessionId, HarnessId, HarnessId, &aihub_memory::BriefPair) -> F
            + Send
            + Sync
            + 'static,
        F: Future<Output = Result<aihub_memory::HandoffDestination>> + Send + 'static,
    {
        self.memory_recorder = Arc::new(move |s, f, t, b| Box::pin(recorder(s, f, t, b)));
        self
    }

    pub fn with_memory_extractor<E, F>(mut self, extractor: E) -> Self
    where
        E: Fn(&SessionId, HarnessId, &Path) -> F + Send + Sync + 'static,
        F: Future<Output = Result<aihub_memory::HandoffTurn>> + Send + 'static,
    {
        self.memory_extractor = Arc::new(move |s, h, p| Box::pin(extractor(s, h, p)));
        self
    }

    pub fn with_git_seams<D, FD, G, FG>(mut self, diff: D, finish: G) -> Self
    where
        D: Fn(&Path, &str, bool) -> FD + Send + Sync + 'static,
        FD: Future<Output = Result<String>> + Send + 'static,
        G: Fn(&Path, MergeStrategy, &str) -> FG + Send + Sync + 'static,
        FG: Future<Output = Result<aihub_git::MergeOutcome>> + Send + 'static,
    {
        self.git_diff = Arc::new(move |p, b, c| Box::pin(diff(p, b, c)));
        self.git_finish = Arc::new(move |p, s, b, _, _| Box::pin(finish(p, s, b)));
        self
    }

    /// Register an already provisioned worktree. Also used by tests without git stubs.
    pub async fn add_session(
        &self,
        repo: PathBuf,
        wt: aihub_git::SessionWorktree,
        harness: HarnessId,
        prompt: Option<String>,
    ) -> Result<SessionId> {
        let mut state = self.state.lock().await;
        self.add_locked(&mut state, repo, wt, harness, prompt)
    }

    /// Explicitly switches a session to a target harness, returning the handoff destination.
    pub async fn switch_session(
        &self,
        id: &SessionId,
        target: HarnessId,
        handoff: bool,
    ) -> Result<Option<aihub_memory::HandoffDestination>> {
        let mut state = self.state.lock().await;
        self.switch(&mut state, id, target, handoff).await
    }

    fn add_locked(
        &self,
        state: &mut State,
        repo: PathBuf,
        wt: aihub_git::SessionWorktree,
        harness: HarnessId,
        prompt: Option<String>,
    ) -> Result<SessionId> {
        if state
            .sessions
            .iter()
            .any(|s| s.summary.session_id == wt.session_id)
        {
            bail!("duplicate session");
        }
        let pty = (self.spawner)(harness, options(wt.path.clone(), prompt.clone()))?;
        let id = wt.session_id.clone();
        let mut session = Session {
            summary: SessionSummary {
                session_id: id.clone(),
                harness,
                mode: Mode::Assisted,
                repo_path: repo,
                worktree_path: wt.path,
                branch: wt.branch,
                active: true,
            },
            base: wt.base_branch,
            originating_checkout: wt.originating_checkout,
            prompt,
            pty,
            generation: 0,
            tasks: vec![],
        };
        self.watch(&mut session);
        log_lifecycle(
            "INFO",
            "session spawn",
            &format!(
                "session_id={} harness={} repo={}",
                id,
                harness,
                session.summary.repo_path.display()
            ),
        );
        state.broadcast(DaemonMessage::SessionCreated {
            session_id: id.clone(),
            harness,
            worktree_path: session.summary.worktree_path.clone(),
            branch: session.summary.branch.clone(),
        });
        state.sessions.push(session);
        Ok(id)
    }

    fn watch(&self, session: &mut Session) {
        let id = session.summary.session_id.clone();
        let generation = session.generation;
        let mut output = session.pty.output.resubscribe();
        // Preserve any bytes queued between spawn and registration.
        std::mem::swap(&mut output, &mut session.pty.output);
        let state = self.state.clone();
        let output_session_id = id.clone();
        session.tasks.push(tokio::spawn(async move {
            loop {
                let data = match output.recv().await {
                    Ok(data) => data,
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let mut state = state.lock().await;
                        state.broadcast(DaemonMessage::Error {
                            code: "pty_lagged".into(),
                            message:
                                "PTY output was lost; reconnect to inspect available scrollback"
                                    .into(),
                        });
                        continue;
                    }
                };
                let mut state = state.lock().await;
                let Ok(s) = state.session(&output_session_id) else {
                    break;
                };
                if s.generation != generation {
                    break;
                }
                s.pty.scrollback.extend_from_slice(&data);
                // Daemon retains only the most recent 1 MiB per session.
                let excess = s.pty.scrollback.len().saturating_sub(1024 * 1024);
                s.pty.scrollback.drain(..excess);
                let message = DaemonMessage::PtyOutput {
                    session_id: output_session_id.clone(),
                    data: data.into(),
                };
                state.clients.retain(|_, c| {
                    !c.attached.contains(&output_session_id)
                        || c.sender.try_send(message.clone()).is_ok()
                });
            }
        }));
        let state = self.state.clone();
        let exit_session_id = id.clone();
        let wait = (session.pty.wait)();
        session.tasks.push(tokio::spawn(async move {
            let exit_code = wait.await.unwrap_or(None);
            let mut state = state.lock().await;
            if let Ok(s) = state.session(&exit_session_id) {
                if s.generation == generation && s.summary.active {
                    s.summary.active = false;
                    log_lifecycle(
                        "INFO",
                        "session stop",
                        &format!("session_id={} exit_code={:?}", exit_session_id, exit_code),
                    );
                    state.broadcast(DaemonMessage::SessionExited {
                        session_id: exit_session_id,
                        exit_code,
                    });
                }
            }
        }));
    }

    pub async fn run(&self, path: PathBuf, shutdown: impl Future<Output = ()>) -> Result<()> {
        log_lifecycle("INFO", "start", "daemon starting");
        let (listener, _guard) = bind(&path)?;
        let mut tasks = JoinSet::new();
        let daemon = self.clone();
        tasks.spawn(async move {
            daemon.quota_loop().await;
        });
        tokio::pin!(shutdown);
        let result = loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => { let daemon = self.clone(); tasks.spawn(async move { let _ = daemon.client(stream).await; }); },
                    Err(error) => {
                        if error.kind() == std::io::ErrorKind::ConnectionAborted
                            || error.kind() == std::io::ErrorKind::Interrupted
                        {
                            continue;
                        }
                        break Err(error.into());
                    }
                },
                Some(done) = tasks.join_next() => { if done.is_err() { break Err(anyhow!("daemon worker failed")); } }
            }
        };
        // Let an in-flight registry mutation finish before cancelling clients.
        let mut state = self.state.lock().await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        state.clients.clear();
        let mut stop_failed = false;
        for s in &mut state.sessions {
            for t in s.tasks.drain(..) {
                t.abort();
            }
            if s.summary.active {
                let stop_fn = s.pty.stop.clone();
                if stop_fn(Duration::from_secs(5)).await.is_err() {
                    stop_failed = true;
                }
                s.summary.active = false;
                log_lifecycle(
                    "INFO",
                    "session stop",
                    &format!("session_id={} stopped on shutdown", s.summary.session_id),
                );
            }
        }
        if stop_failed {
            bail!("one or more child PTYs could not be stopped");
        }
        log_lifecycle("INFO", "shutdown", "daemon shutdown complete");
        result
    }

    async fn quota_loop(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(120));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = interval.tick() => (), _ = self.refresh.notified() => () }
            // Probe returns sanitized snapshots, never errors formatted into IPC.
            let snapshots = (self.probe)().await;
            log_lifecycle(
                "INFO",
                "probe refresh",
                &format!("refreshed {} quota snapshots", snapshots.len()),
            );
            let mut state = self.state.lock().await;
            *self.cache.lock().await = snapshots.clone();
            state.broadcast(DaemonMessage::QuotaPush {
                snapshots: snapshots.clone(),
            });
            let requests: Vec<_> = state
                .sessions
                .iter()
                .filter(|s| s.summary.active)
                .filter_map(|s| s.prompt.clone().map(|p| (s.summary.session_id.clone(), p)))
                .collect();
            for (id, prompt) in requests {
                let _ = self
                    .recommend(&mut state, &prompt, Some(id), TaskSize::M, &snapshots)
                    .await;
            }
        }
    }
    async fn client(&self, stream: UnixStream) -> Result<()> {
        let (mut reader, mut writer) = stream.into_split();
        let hello =
            tokio::time::timeout(Duration::from_secs(10), read_message(&mut reader)).await??;
        writer
            .write_all(&encode_frame(
                &DaemonMessage::Hello {
                    version: PROTOCOL_VERSION,
                }
                .into(),
            )?)
            .await?;
        if !matches!(
            hello,
            ClientMessage::Hello {
                version: PROTOCOL_VERSION
            }
        ) {
            writer
                .write_all(&encode_frame(
                    &error("protocol", "expected Hello with protocol version 1").into(),
                )?)
                .await?;
            return Ok(());
        }
        let (tx, mut rx) = mpsc::channel(128);
        let id;
        {
            let mut state = self.state.lock().await;
            state.next_id += 1;
            id = state.next_id;
            tx.try_send(DaemonMessage::QuotaPush {
                snapshots: self.cache.lock().await.clone(),
            })?;
            state.clients.insert(
                id,
                Client {
                    sender: tx,
                    attached: HashSet::new(),
                    merge: None,
                },
            );
        }
        let mut writing = JoinSet::new();
        writing.spawn(async move {
            while let Some(msg) = rx.recv().await {
                writer.write_all(&encode_frame(&msg.into())?).await?;
            }
            Ok::<(), anyhow::Error>(())
        });
        loop {
            let message = tokio::select! {
                _ = writing.join_next() => break,
                message = read_message(&mut reader) => match message { Ok(m) => m, Err(_) => break },
            };
            // A disconnect must not cancel a switch after the incoming PTY starts.
            if self.handle(id, message).await.is_err() {
                self.state.lock().await.send(
                    id,
                    error("request_failed", "request could not be completed"),
                );
            }
        }
        self.state.lock().await.clients.remove(&id);
        Ok(())
    }
    async fn handle(&self, client: u64, message: ClientMessage) -> Result<()> {
        if matches!(message, ClientMessage::RequestQuota) {
            self.refresh.notify_one();
            return Ok(());
        }
        let mut state = self.state.lock().await;
        match message {
            ClientMessage::Hello { .. } => state.send(
                client,
                error("protocol", "Hello is only valid at connection start"),
            ),
            ClientMessage::ListSessions => {
                let sessions = state.sessions.iter().map(|s| s.summary.clone()).collect();
                state.send(client, DaemonMessage::SessionList { sessions });
            }
            ClientMessage::NewSession {
                harness,
                repo_path,
                initial_prompt,
            } => {
                state.next_id += 1;
                let id = SessionId::new(format!(
                    "{}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos(),
                    state.next_id
                ));
                let wt = aihub_git::create_session_worktree(&repo_path, &id, None).await?;
                self.add_locked(&mut state, repo_path, wt, harness, initial_prompt)?;
            }
            ClientMessage::Attach { target } => {
                let s = state
                    .sessions
                    .iter()
                    .rev()
                    .find(|s| match &target {
                        SessionTarget::Id(id) => &s.summary.session_id == id,
                        SessionTarget::LatestForRepo(repo) => &s.summary.repo_path == repo,
                    })
                    .ok_or_else(|| anyhow!("session not found"))?;
                let (id, scrollback, summary) = (
                    s.summary.session_id.clone(),
                    s.pty.scrollback.clone(),
                    s.summary.clone(),
                );
                if let Some(c) = state.clients.get_mut(&client) {
                    c.attached.insert(id.clone());
                }
                state.send(
                    client,
                    DaemonMessage::Attached {
                        session_id: id,
                        scrollback: scrollback.into(),
                        summary,
                    },
                );
            }
            ClientMessage::Detach { session_id } => {
                if let Some(c) = state.clients.get_mut(&client) {
                    c.attached.remove(&session_id);
                }
                state.send(client, DaemonMessage::Detached { session_id });
            }
            ClientMessage::PtyInput { session_id, data } => {
                let writer = match state.session(&session_id) {
                    Ok(s) => Ok(s.pty.try_write.clone()),
                    Err(e) => Err(e),
                };
                // Drop state lock before performing PTY I/O (F7)
                drop(state);
                match writer {
                    Ok(try_write) => {
                        if let Err(e) = try_write(data.into_inner()) {
                            let mut state = self.state.lock().await;
                            log_lifecycle(
                                "ERROR",
                                "pty_input_error",
                                &format!("session_id={} error={}", session_id, e),
                            );
                            state.send(
                                client,
                                error("pty_input_error", &format!("PTY input error: {e}")),
                            );
                        }
                    }
                    Err(e) => {
                        let mut state = self.state.lock().await;
                        state.send(client, error("session_not_found", &e.to_string()));
                    }
                }
            }
            ClientMessage::PtyResize {
                session_id,
                cols,
                rows,
            } => {
                if cols == 0 || rows == 0 {
                    bail!("invalid PTY size");
                }
                (state.session(&session_id)?.pty.resize)(PtySize { cols, rows })?;
            }
            ClientMessage::SetMode { session_id, mode } => {
                state.session(&session_id)?.summary.mode = mode;
                state.broadcast(DaemonMessage::ModeSet { session_id, mode });
            }
            ClientMessage::SwitchHarness {
                session_id,
                target,
                with_handoff,
            } => {
                self.switch(&mut state, &session_id, target, with_handoff)
                    .await?;
            }
            ClientMessage::RouteRequest {
                prompt,
                repo_path,
                size_hint,
            } => {
                let id = {
                    let client_attached = state
                        .clients
                        .get(&client)
                        .and_then(|c| c.attached.iter().next().cloned());
                    client_attached.or_else(|| {
                        state
                            .sessions
                            .iter()
                            .rev()
                            .find(|s| {
                                s.summary.active
                                    && repo_path
                                        .as_ref()
                                        .is_some_and(|p| p == &s.summary.repo_path)
                            })
                            .map(|s| s.summary.session_id.clone())
                    })
                };
                let snapshots = self.cache.lock().await.clone();
                self.recommend(
                    &mut state,
                    &prompt,
                    id,
                    size_hint.unwrap_or(TaskSize::M),
                    &snapshots,
                )
                .await?;
            }
            ClientMessage::MergeRequest {
                session_id,
                strategy,
            } => {
                let (path, base, branch, origin) = {
                    let s = state.session(&session_id)?;
                    (
                        s.summary.worktree_path.clone(),
                        s.base.clone(),
                        s.summary.branch.clone(),
                        s.originating_checkout.clone(),
                    )
                };
                let diff = (self.git_diff)(&path, &base, true).await?;
                let c = state
                    .clients
                    .get_mut(&client)
                    .ok_or_else(|| anyhow!("client disconnected"))?;
                let confirmation = (session_id.clone(), strategy, diff.clone());
                if c.merge.as_ref() != Some(&confirmation) {
                    c.merge = Some(confirmation);
                    state.send(client, DaemonMessage::MergeResult {
                        session_id,
                        success: false,
                        diff,
                        message: "Review diff; repeat MergeRequest with the same strategy to confirm. A changed diff requires review again.".into()
                    });
                } else {
                    c.merge = None;
                    // F4: Quiesce and reap through the stop barrier
                    let s = state.session(&session_id)?;
                    if s.summary.active {
                        let stop_fn = s.pty.stop.clone();
                        stop_fn(Duration::from_secs(5)).await?;
                        s.summary.active = false;
                        for task in s.tasks.drain(..) {
                            task.abort();
                        }
                        log_lifecycle(
                            "INFO",
                            "session stop",
                            &format!("session_id={} stopped for merge", session_id),
                        );
                    }
                    // Only after the child group is reaped do they take the final diff
                    let stopped_diff = (self.git_diff)(&path, &base, true).await?;
                    if stopped_diff != diff {
                        if let Some(c) = state.clients.get_mut(&client) {
                            c.merge = Some((session_id.clone(), strategy, stopped_diff.clone()));
                        }
                        state.send(client, DaemonMessage::MergeResult {
                            session_id,
                            success: false,
                            diff: stopped_diff,
                            message: "Agent stopped; final diff changed. Review and repeat MergeRequest to confirm.".into(),
                        });
                        return Ok(());
                    }
                    let outcome =
                        (self.git_finish)(&path, strategy, &base, &branch, &origin).await?;
                    log_lifecycle(
                        "INFO",
                        "session merge",
                        &format!(
                            "session_id={} strategy={} success={}",
                            session_id, strategy, outcome.success
                        ),
                    );
                    state.broadcast(DaemonMessage::MergeResult {
                        session_id,
                        success: outcome.success,
                        diff: outcome.diff,
                        message: outcome.message,
                    });
                }
            }
            ClientMessage::SubmitTask { session_id, task } => {
                let (snapshots, mode, current_harness) = {
                    let s = state.session(&session_id)?;
                    s.prompt = Some(task.clone());
                    (
                        self.cache.lock().await.clone(),
                        s.summary.mode,
                        s.summary.harness,
                    )
                };
                let classification = (self.classifier)(&task).await;
                let outcome = (self.router)(classification.tier, TaskSize::M, &snapshots, 120)?;
                state.send(
                    client,
                    DaemonMessage::RouteRecommendation {
                        session_id: session_id.clone(),
                        outcome: outcome.clone(),
                    },
                );
                if mode == Mode::Autonomous {
                    if let RouteOutcome::Recommendation {
                        harness,
                        holds_until_s,
                        ..
                    } = &outcome
                    {
                        if *harness != current_harness && holds_until_s.unwrap_or(0) == 0 {
                            self.switch(&mut state, &session_id, *harness, true).await?;
                        }
                    }
                    // NoCapacity is never dispatched (F10)
                }
            }
            ClientMessage::RequestQuota => unreachable!(),
        }
        Ok(())
    }

    async fn recommend(
        &self,
        state: &mut State,
        prompt: &str,
        id: Option<SessionId>,
        size: TaskSize,
        snapshots: &[QuotaSnapshot],
    ) -> Result<()> {
        let classification = (self.classifier)(prompt).await;
        let outcome = (self.router)(classification.tier, size, snapshots, 120)?;
        let session_id = id.clone().unwrap_or_else(|| SessionId::new("unknown"));
        state.broadcast(DaemonMessage::RouteRecommendation {
            session_id: session_id.clone(),
            outcome: outcome.clone(),
        });
        if let Some(id) = id {
            if let Ok(s) = state.session(&id) {
                if s.summary.mode == Mode::Autonomous {
                    if let RouteOutcome::Recommendation {
                        harness,
                        holds_until_s,
                        ..
                    } = &outcome
                    {
                        if s.summary.harness != *harness && holds_until_s.unwrap_or(0) == 0 {
                            self.switch(state, &id, *harness, true).await?;
                        }
                    }
                    // NoCapacity is never dispatched (F10)
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn switch(
        &self,
        state: &mut State,
        id: &SessionId,
        target: HarnessId,
        handoff: bool,
    ) -> Result<Option<aihub_memory::HandoffDestination>> {
        let s = state.session(id)?;
        if s.summary.harness == target {
            return Ok(None);
        }
        let old = s.summary.harness;
        let wt_path = s.summary.worktree_path.clone();
        let prompt_fallback = s.prompt.clone();
        let generation = s.generation;

        // 1. Quiesce outgoing harness through the stop barrier (F5)
        if s.summary.active {
            let stop_fn = s.pty.stop.clone();
            stop_fn(Duration::from_secs(5)).await?;
            s.summary.active = false;
            for task in s.tasks.drain(..) {
                task.abort();
            }
            log_lifecycle(
                "INFO",
                "session stop",
                &format!("session_id={} quiesced for switch", id),
            );
        }

        // 2. Extract final context
        let brief = if handoff {
            let turn = (self.memory_extractor)(id, old, &wt_path).await?;
            // 3. Write the brief
            Some(aihub_memory::write_brief_pair(
                &wt_path.join(".aihub/handoffs"),
                generation as u32 + 1,
                prompt_fallback.as_deref().unwrap_or("Continue the session"),
                &turn,
            )?)
        } else {
            None
        };

        let prompt = match &brief {
            Some(b) => Some(std::fs::read_to_string(&b.prompt_path)?),
            None => prompt_fallback,
        };

        // 4. Launch incoming harness (F5)
        let incoming = match (self.spawner)(target, options(wt_path.clone(), prompt)) {
            Ok(pty) => pty,
            Err(e) => {
                // If incoming spawn fails, session ends in recoverable stopped state (F5)
                log_lifecycle(
                    "ERROR",
                    "session switch failure",
                    &format!("session_id={} incoming spawn failed: {}", id, e),
                );
                let s = state.session(id)?;
                s.summary.active = false;
                state.broadcast(DaemonMessage::SessionExited {
                    session_id: id.clone(),
                    exit_code: None,
                });
                return Err(e);
            }
        };

        let s = state.session(id)?;
        let old_scrollback = std::mem::take(&mut s.pty.scrollback);
        s.pty = incoming;
        s.pty.scrollback.splice(..0, old_scrollback);
        s.generation += 1;
        s.summary.harness = target;
        s.summary.active = true;
        self.watch(s);

        let handoff_path = brief.as_ref().map(|b| b.brief_path.clone());
        state.broadcast(DaemonMessage::HarnessSwitched {
            session_id: id.clone(),
            old_harness: old,
            new_harness: target,
            handoff_path,
        });

        let mut destination = None;
        if let Some(b) = brief {
            match (self.memory_recorder)(id, old, target, &b).await {
                Ok(dest) => {
                    destination = Some(dest);
                    log_lifecycle(
                        "INFO",
                        "session switch",
                        &format!(
                            "session_id={} from={} to={} handoff_dest={:?}",
                            id, old, target, dest
                        ),
                    );
                }
                Err(err) => {
                    log_lifecycle(
                        "ERROR",
                        "handoff_record",
                        &format!("session_id={} handoff record failed: {}", id, err),
                    );
                    state.broadcast(error(
                        "handoff_record",
                        "Harness switched, but handoff metadata could not be recorded",
                    ));
                }
            }
        } else {
            log_lifecycle(
                "INFO",
                "session switch",
                &format!(
                    "session_id={} from={} to={} handoff_dest=none",
                    id, old, target
                ),
            );
        }

        Ok(destination)
    }
}
fn options(cwd: PathBuf, initial_prompt: Option<String>) -> PtySpawnOptions {
    PtySpawnOptions {
        cwd,
        initial_prompt,
        env: HashMap::new(),
        size: PtySize::default(),
    }
}
fn error(code: &str, message: &str) -> DaemonMessage {
    DaemonMessage::Error {
        code: code.into(),
        message: message.into(),
    }
}
async fn read_message(reader: &mut (impl AsyncReadExt + Unpin)) -> Result<ClientMessage> {
    let length = reader.read_u32().await? as usize;
    if length > DEFAULT_MAX_FRAME_LENGTH {
        bail!("frame too large");
    }
    let mut data = vec![0; length];
    reader.read_exact(&mut data).await?;
    match serde_json::from_slice(&data)? {
        IpcMessage::Client(message) => Ok(message),
        _ => bail!("wrong message direction"),
    }
}
/// Shared by the foreground binary and subprocess lifecycle tests.
pub async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut interrupt =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    tokio::select! {
        _ = async {
            if let Some(ref mut s) = term {
                s.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => (),
        _ = async {
            if let Some(ref mut s) = interrupt {
                s.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        } => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn connected_clients_get_hello_cache_and_broadcast() {
        async fn receive(stream: &mut UnixStream) -> DaemonMessage {
            let n = stream.read_u32().await.unwrap();
            let mut data = vec![0; n as usize];
            stream.read_exact(&mut data).await.unwrap();
            match serde_json::from_slice(&data).unwrap() {
                IpcMessage::Daemon(m) => m,
                _ => panic!("wrong direction"),
            }
        }
        let daemon = Daemon::new(|| async { vec![] }, |_, _| panic!("no spawn"));
        let mut workers = JoinSet::new();
        let mut clients = vec![];
        for _ in 0..2 {
            let (mut client, server) = UnixStream::pair().unwrap();
            let d = daemon.clone();
            workers.spawn(async move {
                d.client(server).await.unwrap();
            });
            client
                .write_all(
                    &encode_frame(
                        &ClientMessage::Hello {
                            version: PROTOCOL_VERSION,
                        }
                        .into(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                receive(&mut client).await,
                DaemonMessage::Hello {
                    version: PROTOCOL_VERSION
                }
            ));
            assert!(matches!(
                receive(&mut client).await,
                DaemonMessage::QuotaPush { .. }
            ));
            client
                .write_all(&encode_frame(&ClientMessage::ListSessions.into()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                receive(&mut client).await,
                DaemonMessage::SessionList { sessions: vec![] }
            );
            clients.push(client);
        }
        daemon
            .state
            .lock()
            .await
            .broadcast(DaemonMessage::QuotaPush { snapshots: vec![] });
        for client in &mut clients {
            assert!(matches!(
                receive(client).await,
                DaemonMessage::QuotaPush { .. }
            ));
        }
        drop(clients);
        while workers.join_next().await.is_some() {}
        assert!(daemon.state.lock().await.clients.is_empty());
    }

    #[tokio::test]
    async fn framing_survives_fragmentation_and_rejects_oversized_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let frame = encode_frame(&ClientMessage::ListSessions.into()).unwrap();
        let task = tokio::spawn(async move {
            for byte in frame {
                writer.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            writer
                .write_u32(DEFAULT_MAX_FRAME_LENGTH as u32 + 1)
                .await
                .unwrap();
        });
        assert!(matches!(
            read_message(&mut reader).await.unwrap(),
            ClientMessage::ListSessions
        ));
        assert!(read_message(&mut reader).await.is_err());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_switch_starts_in_same_worktree_after_quiescing_outgoing() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let calls = starts.clone();
        let kills = stops.clone();
        let daemon = Daemon::new(
            || async { vec![] },
            move |harness, opts| {
                assert_eq!(opts.cwd, PathBuf::from("/fake/wt"));
                let order = calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    harness,
                    if order == 0 {
                        HarnessId::Codex
                    } else {
                        HarnessId::ClaudeCode
                    }
                );
                // When incoming harness starts (order == 1), outgoing must ALREADY be stopped (no overlap!)
                if order == 1 {
                    assert_eq!(
                        kills.load(Ordering::SeqCst),
                        1,
                        "outgoing harness must be stopped before incoming launches"
                    );
                }
                let (tx, output) = broadcast::channel(16);
                let calls = calls.clone();
                let kills = kills.clone();
                Ok(Pty {
                    output,
                    scrollback: vec![],
                    write: Box::new(|_| Box::pin(async { Ok(()) })),
                    resize: Box::new(|_| Ok(())),
                    wait: Box::new(move || {
                        let tx = tx.clone();
                        Box::pin(async move {
                            let _keep = tx;
                            std::future::pending().await
                        })
                    }),
                    kill: {
                        let calls = calls.clone();
                        let kills = kills.clone();
                        Box::new(move || {
                            assert_eq!(calls.load(Ordering::SeqCst), 1);
                            kills.fetch_add(1, Ordering::SeqCst);
                            Box::pin(async { Ok(()) })
                        })
                    },
                    stop: {
                        let calls = calls.clone();
                        let kills = kills.clone();
                        Arc::new(move |_| {
                            assert_eq!(calls.load(Ordering::SeqCst), 1);
                            kills.fetch_add(1, Ordering::SeqCst);
                            Box::pin(async { Ok(Some(0)) })
                        })
                    },
                    try_write: Arc::new(|_| Ok(())),
                })
            },
        );
        let id = SessionId::new("switch");
        daemon
            .add_session(
                PathBuf::from("/fake/repo"),
                aihub_git::SessionWorktree {
                    session_id: id.clone(),
                    path: PathBuf::from("/fake/wt"),
                    branch: id.branch_name(),
                    base_branch: "main".into(),
                    originating_checkout: PathBuf::from("/fake/repo"),
                },
                HarnessId::Codex,
                None,
            )
            .await
            .unwrap();
        let mut state = daemon.state.lock().await;
        daemon
            .switch(&mut state, &id, HarnessId::ClaudeCode, false)
            .await
            .unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.session(&id).unwrap().summary.harness,
            HarnessId::ClaudeCode
        );
        for s in &mut state.sessions {
            for task in s.tasks.drain(..) {
                task.abort();
            }
        }
    }

    #[tokio::test]
    async fn quota_refresh_is_cached_and_probe_never_blocks_registry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let daemon = Daemon::new(
            move || {
                let n = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    vec![QuotaSnapshot {
                        slot: SlotId::default_for(HarnessId::Codex),
                        status: QuotaStatus::Unknown,
                        source: QuotaSource::Vendor,
                        estimated: false,
                        note: Some(format!("unavailable {n}")),
                        windows: vec![],
                        lanes: vec![],
                    }]
                }
            },
            |_, _| panic!("no PTY requested"),
        );
        let (tx, mut rx) = mpsc::channel(16);
        daemon.state.lock().await.clients.insert(
            1,
            Client {
                sender: tx,
                attached: HashSet::new(),
                merge: None,
            },
        );
        let d = daemon.clone();
        let task = tokio::spawn(async move {
            d.quota_loop().await;
        });
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, DaemonMessage::QuotaPush { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            daemon.cache.lock().await[0].note.as_deref(),
            Some("unavailable 0")
        );
        daemon.handle(1, ClientMessage::RequestQuota).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        task.abort();
    }
}
