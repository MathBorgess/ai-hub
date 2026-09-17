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
/// Plain closure seam for the PTY; the production adapter calls the contract methods.
pub struct Pty {
    pub output: broadcast::Receiver<Vec<u8>>,
    pub scrollback: Vec<u8>,
    pub write: Box<dyn Fn(Vec<u8>) -> Operation<()> + Send + Sync>,
    pub resize: Box<dyn Fn(PtySize) -> Result<()> + Send + Sync>,
    pub wait: Box<dyn Fn() -> Operation<Option<i32>> + Send + Sync>,
    pub kill: Box<dyn Fn() -> Operation<()> + Send + Sync>,
}
pub fn spawn_pty(harness: HarnessId, options: PtySpawnOptions) -> Result<Pty> {
    let handle = if let Ok(cmd) = std::env::var("AIHUB_PTY_COMMAND") {
        Arc::new(aihub_pty::spawn_command(&cmd, &[], options)?)
    } else {
        Arc::new(aihub_pty::spawn_harness(harness, options)?)
    };
    // ponytail: the PTY contract has no atomic snapshot/subscription cursor;
    // bytes arriving between these calls may be replayed twice at initial spawn.
    // Subsequent client attaches are atomic against the daemon-owned buffer.
    let output = handle.subscribe_output();
    let scrollback = handle.scrollback_snapshot();
    let (writer, resizer, waiter, killer) =
        (handle.clone(), handle.clone(), handle.clone(), handle);
    Ok(Pty {
        output,
        scrollback,
        write: Box::new(move |data| {
            let h = writer.clone();
            Box::pin(async move {
                h.write(&data).await?;
                Ok(())
            })
        }),
        resize: Box::new(move |size| {
            resizer.resize(size)?;
            Ok(())
        }),
        wait: Box::new(move || {
            let h = waiter.clone();
            Box::pin(async move { Ok(h.wait().await?) })
        }),
        kill: Box::new(move || {
            let h = killer.clone();
            Box::pin(async move {
                h.kill().await?;
                Ok(())
            })
        }),
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
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => bail!("a live aihubd already holds this socket"),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                let current = std::fs::symlink_metadata(path)?;
                if current.ino() != meta.ino() || current.dev() != meta.dev() {
                    bail!("socket changed while checking listener");
                }
                std::fs::remove_file(path)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => bail!("cannot establish whether existing socket is stale"),
        }
    }
    let listener = UnixListener::bind(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
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
        self.clients
            .retain(|_, c| c.sender.try_send(message.clone()).is_ok());
    }
    fn session(&mut self, id: &SessionId) -> Result<&mut Session> {
        self.sessions
            .iter_mut()
            .find(|s| &s.summary.session_id == id)
            .ok_or_else(|| anyhow!("session not found"))
    }
}

type Spawner = dyn Fn(HarnessId, PtySpawnOptions) -> Result<Pty> + Send + Sync;
type Probe = dyn Fn() -> Pin<Box<dyn Future<Output = Vec<QuotaSnapshot>> + Send>> + Send + Sync;
#[derive(Clone)]
pub struct Daemon {
    state: Arc<Mutex<State>>,
    cache: Arc<Mutex<Vec<QuotaSnapshot>>>,
    probe: Arc<Probe>,
    spawner: Arc<Spawner>,
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
            refresh: Arc::new(tokio::sync::Notify::new()),
        }
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
            prompt,
            pty,
            generation: 0,
            tasks: vec![],
        };
        self.watch(&mut session);
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
                let Ok(s) = state.session(&id) else {
                    break;
                };
                if s.generation != generation {
                    break;
                }
                s.pty.scrollback.extend_from_slice(&data);
                // ponytail: daemon retains only the most recent 1 MiB per session.
                let excess = s.pty.scrollback.len().saturating_sub(1024 * 1024);
                s.pty.scrollback.drain(..excess);
                let message = DaemonMessage::PtyOutput {
                    session_id: id.clone(),
                    data: data.into(),
                };
                state.clients.retain(|_, c| {
                    !c.attached.contains(&id) || c.sender.try_send(message.clone()).is_ok()
                });
            }
        }));
        let state = self.state.clone();
        let id = session.summary.session_id.clone();
        let wait = (session.pty.wait)();
        session.tasks.push(tokio::spawn(async move {
            let exit_code = wait.await.unwrap_or(None);
            let mut state = state.lock().await;
            if let Ok(s) = state.session(&id) {
                if s.generation == generation {
                    s.summary.active = false;
                    state.broadcast(DaemonMessage::SessionExited {
                        session_id: id,
                        exit_code,
                    });
                }
            }
        }));
    }
    pub async fn run(&self, path: PathBuf, shutdown: impl Future<Output = ()>) -> Result<()> {
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
                    Err(error) => break Err(error.into()),
                },
                Some(done) = tasks.join_next() => { if done.is_err() { break Err(anyhow!("daemon worker failed")); } }
            }
        };
        // Let an in-flight registry mutation finish before cancelling clients.
        let mut state = self.state.lock().await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        state.clients.clear();
        let mut kill_failed = false;
        for s in &mut state.sessions {
            for t in s.tasks.drain(..) {
                t.abort();
            }
            if s.summary.active && (s.pty.kill)().await.is_err() {
                kill_failed = true;
            }
            s.summary.active = false;
        }
        if kill_failed {
            bail!("one or more child PTYs could not be stopped");
        }
        result
    }
    async fn quota_loop(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(120));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = interval.tick() => (), _ = self.refresh.notified() => () }
            // Probe returns sanitized snapshots, never errors formatted into IPC.
            let snapshots = (self.probe)().await;
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
                let (id, scrollback) = (s.summary.session_id.clone(), s.pty.scrollback.clone());
                if let Some(c) = state.clients.get_mut(&client) {
                    c.attached.insert(id.clone());
                }
                state.send(
                    client,
                    DaemonMessage::Attached {
                        session_id: id,
                        scrollback: scrollback.into(),
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
                let s = state.session(&session_id)?;
                (s.pty.write)(data.into_inner()).await?;
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
                    .await?
            }
            ClientMessage::RouteRequest {
                prompt,
                repo_path,
                size_hint,
            } => {
                let id = state
                    .sessions
                    .iter()
                    .rev()
                    .find(|s| {
                        s.summary.active
                            && repo_path
                                .as_ref()
                                .is_some_and(|p| p == &s.summary.repo_path)
                    })
                    .map(|s| s.summary.session_id.clone());
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
                let s = state.session(&session_id)?;
                let (path, base) = (s.summary.worktree_path.clone(), s.base.clone());
                let diff = aihub_git::diff(&path, &base, true).await?;
                let c = state
                    .clients
                    .get_mut(&client)
                    .ok_or_else(|| anyhow!("client disconnected"))?;
                let confirmation = (session_id.clone(), strategy, diff.clone());
                if c.merge.as_ref() != Some(&confirmation) {
                    c.merge = Some(confirmation);
                    state.send(client, DaemonMessage::MergeResult { session_id, success: false, diff, message: "Review diff; repeat MergeRequest with the same strategy to confirm. A changed diff requires review again.".into() });
                } else {
                    c.merge = None;
                    let s = state.session(&session_id)?;
                    if s.summary.active {
                        (s.pty.kill)().await?;
                        s.summary.active = false;
                    }
                    // The agent may have written between preview validation and kill.
                    let stopped_diff = aihub_git::diff(&path, &base, true).await?;
                    if stopped_diff != diff {
                        if let Some(c) = state.clients.get_mut(&client) {
                            c.merge = Some((session_id.clone(), strategy, stopped_diff.clone()));
                        }
                        state.send(client, DaemonMessage::MergeResult { session_id, success: false, diff: stopped_diff, message: "Agent stopped; final diff changed. Review and repeat MergeRequest to confirm.".into() });
                        return Ok(());
                    }
                    let outcome = aihub_git::finish(&path, strategy, &base).await?;
                    state.broadcast(DaemonMessage::MergeResult {
                        session_id,
                        success: outcome.success,
                        diff: outcome.diff,
                        message: outcome.message,
                    });
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
        let classification = aihub_router::classify(prompt);
        let r = aihub_router::route(classification.tier, size, snapshots, 120)?;
        state.broadcast(DaemonMessage::RouteRecommendation {
            tier: r.tier,
            harness: r.harness,
            lane: r.lane,
            holds_until_s: r.holds_until_s,
            confidence: classification.confidence,
            reason: r.reason,
        });
        if let Some(id) = id {
            let s = state.session(&id)?;
            if s.summary.mode == Mode::Autonomous
                && s.summary.harness != r.harness
                && r.holds_until_s.unwrap_or(0) == 0
            {
                self.switch(state, &id, r.harness, true).await?;
            }
        }
        Ok(())
    }
    async fn switch(
        &self,
        state: &mut State,
        id: &SessionId,
        target: HarnessId,
        handoff: bool,
    ) -> Result<()> {
        let s = state.session(id)?;
        if s.summary.harness == target {
            return Ok(());
        }
        let old = s.summary.harness;
        let brief = if handoff {
            let turn = aihub_memory::extract_last_turn(id, old, &s.summary.worktree_path).await?;
            Some(aihub_memory::write_brief_pair(
                &s.summary.worktree_path.join(".aihub/handoffs"),
                s.generation as u32 + 1,
                s.prompt.as_deref().unwrap_or("Continue the session"),
                &turn,
            )?)
        } else {
            None
        };
        let prompt = match &brief {
            Some(b) => Some(std::fs::read_to_string(&b.prompt_path)?),
            None => s.prompt.clone(),
        };
        let incoming = (self.spawner)(target, options(s.summary.worktree_path.clone(), prompt))?;
        if (s.pty.kill)().await.is_err() {
            let _ = (incoming.kill)().await;
            bail!("outgoing PTY could not be stopped");
        }
        for task in s.tasks.drain(..) {
            task.abort();
        }
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
        if let Some(b) = brief {
            if aihub_memory::record_handoff(id, old, target, &b)
                .await
                .is_err()
            {
                state.broadcast(error(
                    "handoff_record",
                    "Harness switched, but handoff metadata could not be recorded",
                ));
            }
        }
        Ok(())
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
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("register SIGTERM");
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("register SIGINT");
    tokio::select! { _ = term.recv() => (), _ = interrupt.recv() => () }
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
                .write_all(&encode_frame(&ClientMessage::Hello { version: 1 }.into()).unwrap())
                .await
                .unwrap();
            assert!(matches!(
                receive(&mut client).await,
                DaemonMessage::Hello { version: 1 }
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
    async fn explicit_switch_starts_in_same_worktree_before_killing_outgoing() {
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
                    kill: Box::new(move || {
                        let calls = calls.clone();
                        let kills = kills.clone();
                        Box::pin(async move {
                            assert_eq!(calls.load(Ordering::SeqCst), 2);
                            kills.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                    }),
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
